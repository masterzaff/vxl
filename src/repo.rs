//! The repository: config, lock, content-addressed object store, snapshots.
//!
//! ```text
//! repo/
//!   config.json             format version + zstd level
//!   lock                    flock'd by every command that reads or writes objects
//!   objects/ab/cdef…        zstd(data), named by BLAKE3(data)
//!   snapshots/<id>.json.zst manifests
//!   tmp/                    staging area for atomic writes
//! ```

use std::collections::HashSet;
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::manifest::{FORMAT_VERSION, Snapshot, SnapshotHeader};
use crate::util::{atomic_write, sync_dir};

/// Default zstd level for objects. Chosen by `examples/zstd_levels.rs`: past
/// this point each extra level costs a lot of speed for very little space.
pub const DEFAULT_ZSTD_LEVEL: i32 = 6;
const MANIFEST_ZSTD_LEVEL: i32 = 9;

// ---------------------------------------------------------------------------
// ObjectId

/// BLAKE3 hash of an object's uncompressed content.
#[derive(Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct ObjectId(pub [u8; 32]);

impl ObjectId {
    pub fn of(data: &[u8]) -> Self {
        ObjectId(*blake3::hash(data).as_bytes())
    }

    pub fn hex(&self) -> String {
        blake3::Hash::from_bytes(self.0).to_hex().to_string()
    }

    pub fn from_hex(s: &str) -> Result<Self> {
        Ok(ObjectId(*blake3::Hash::from_hex(s).map_err(|e| anyhow!("bad hash {s:?}: {e}"))?.as_bytes()))
    }
}

impl fmt::Display for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.hex())
    }
}

impl fmt::Debug for ObjectId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "ObjectId({})", &self.hex()[..12])
    }
}

impl Serialize for ObjectId {
    fn serialize<S: serde::Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.serialize_str(&self.hex())
    }
}

impl<'de> Deserialize<'de> for ObjectId {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let s = <&str>::deserialize(d)?;
        ObjectId::from_hex(s).map_err(serde::de::Error::custom)
    }
}

// ---------------------------------------------------------------------------
// Repo

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub zstd_level: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// No lock (only for `list`, which reads nothing but manifests).
    None,
    /// Reading objects (restore, verify). Many can run at once.
    Shared,
    /// Writing or deleting (backup, prune). Excludes everyone else.
    Exclusive,
}

/// Result of [`Repo::put`].
pub struct Put {
    pub id: ObjectId,
    /// `Some(compressed size)` if this call stored a new object.
    pub stored: Option<u64>,
}

pub struct Repo {
    root: PathBuf,
    pub config: Config,
    /// Objects known to exist (or claimed by a thread that's writing them) —
    /// saves a filesystem lookup per chunk and stops two threads storing the same chunk.
    known: Mutex<HashSet<ObjectId>>,
    _lock: Option<File>,
}

impl Repo {
    /// Create a new, empty repository at `path` (must not exist or be empty).
    pub fn init(path: &Path, zstd_level: i32) -> Result<()> {
        if !(1..=22).contains(&zstd_level) {
            bail!("zstd level must be between 1 and 22");
        }
        if path.exists() && fs::read_dir(path)?.next().is_some() {
            bail!("{} already exists and is not empty", path.display());
        }
        for i in 0..=255u8 {
            fs::create_dir_all(path.join("objects").join(format!("{i:02x}")))?;
        }
        fs::create_dir_all(path.join("snapshots"))?;
        fs::create_dir_all(path.join("tmp"))?;
        File::create(path.join("lock"))?;
        let config = Config { version: FORMAT_VERSION, zstd_level };
        atomic_write(&path.join("config.json"), &path.join("tmp"), &serde_json::to_vec_pretty(&config)?)?;
        sync_dir(path)?;
        Ok(())
    }

    pub fn open(path: &Path, lock: LockMode) -> Result<Repo> {
        let cfg_path = path.join("config.json");
        let raw = fs::read(&cfg_path)
            .with_context(|| format!("{} is not a vxl repository (run `vxl init` first)", path.display()))?;
        let config: Config = serde_json::from_slice(&raw).context("reading config.json")?;
        if config.version > FORMAT_VERSION {
            bail!(
                "repository format v{} is newer than this vxl (v{FORMAT_VERSION}); upgrade vxl",
                config.version
            );
        }
        let lock_file = match lock {
            LockMode::None => None,
            mode => {
                let f = File::options().create(true).truncate(false).write(true).open(path.join("lock"))?;
                let res = if mode == LockMode::Exclusive { f.try_lock() } else { f.try_lock_shared() };
                match res {
                    Ok(()) => Some(f),
                    Err(TryLockError::WouldBlock) => {
                        bail!("repository is busy: another vxl command is using it right now")
                    }
                    Err(TryLockError::Error(e)) => return Err(e).context("locking repository"),
                }
            }
        };
        Ok(Repo { root: path.to_path_buf(), config, known: Mutex::default(), _lock: lock_file })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn object_path(&self, id: &ObjectId) -> PathBuf {
        let hex = id.hex();
        self.root.join("objects").join(&hex[..2]).join(&hex[2..])
    }

    // -- objects -------------------------------------------------------------

    /// Store `data` unless an identical object already exists.
    pub fn put(&self, data: &[u8]) -> Result<Put> {
        let id = ObjectId::of(data);
        if self.known.lock().unwrap().contains(&id) {
            return Ok(Put { id, stored: None });
        }
        // (filesystem check outside the mutex so threads don't queue up on it)
        let on_disk = self.object_path(&id).exists();
        // `insert` returns false if another thread claimed this id in the meantime.
        if !self.known.lock().unwrap().insert(id) || on_disk {
            return Ok(Put { id, stored: None });
        }
        let packed = zstd::bulk::compress(data, self.config.zstd_level)?;
        let res = atomic_write(&self.object_path(&id), &self.tmp_dir(), &packed);
        if res.is_err() {
            self.known.lock().unwrap().remove(&id);
        }
        res.with_context(|| format!("storing object {id}"))?;
        Ok(Put { id, stored: Some(packed.len() as u64) })
    }

    /// Load and verify an object.
    pub fn get(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let path = self.object_path(id);
        let packed = fs::read(&path).with_context(|| format!("object {id} is missing"))?;
        let data = zstd::stream::decode_all(packed.as_slice())
            .with_context(|| format!("object {id} is corrupt (bad zstd data)"))?;
        if ObjectId::of(&data) != *id {
            bail!("object {id} is corrupt (hash mismatch)");
        }
        Ok(data)
    }

    pub fn has(&self, id: &ObjectId) -> bool {
        self.object_path(id).exists()
    }

    /// All objects with their on-disk (compressed) size.
    pub fn list_objects(&self) -> Result<Vec<(ObjectId, u64)>> {
        let mut out = Vec::new();
        for i in 0..=255u8 {
            let prefix = format!("{i:02x}");
            let dir = self.root.join("objects").join(&prefix);
            let Ok(entries) = fs::read_dir(&dir) else { continue };
            for e in entries {
                let e = e?;
                let name = e.file_name();
                let Some(name) = name.to_str() else { continue };
                if let Ok(id) = ObjectId::from_hex(&format!("{prefix}{name}")) {
                    out.push((id, e.metadata()?.len()));
                }
            }
        }
        Ok(out)
    }

    pub fn delete_object(&self, id: &ObjectId) -> Result<()> {
        fs::remove_file(self.object_path(id)).with_context(|| format!("deleting object {id}"))
    }

    /// Make renames of new objects durable (call before committing a snapshot).
    pub fn sync_objects(&self) -> Result<()> {
        for i in 0..=255u8 {
            sync_dir(&self.root.join("objects").join(format!("{i:02x}")))?;
        }
        Ok(())
    }

    /// Remove leftovers from interrupted writes. Needs the exclusive lock.
    pub fn clean_tmp(&self) -> Result<u64> {
        let mut n = 0;
        for e in fs::read_dir(self.tmp_dir())? {
            fs::remove_file(e?.path())?;
            n += 1;
        }
        Ok(n)
    }

    // -- snapshots -----------------------------------------------------------

    fn snapshot_path(&self, id: &ObjectId) -> PathBuf {
        self.root.join("snapshots").join(format!("{}.json.zst", id.hex()))
    }

    /// Commit a snapshot. This is the moment a backup "exists".
    pub fn write_snapshot(&self, snap: &Snapshot) -> Result<ObjectId> {
        let json = serde_json::to_vec(snap)?;
        let id = ObjectId::of(&json);
        let packed = zstd::bulk::compress(&json, MANIFEST_ZSTD_LEVEL)?;
        atomic_write(&self.snapshot_path(&id), &self.tmp_dir(), &packed)?;
        sync_dir(&self.root.join("snapshots"))?;
        Ok(id)
    }

    pub fn snapshot_ids(&self) -> Result<Vec<ObjectId>> {
        let mut ids = Vec::new();
        for e in fs::read_dir(self.root.join("snapshots"))? {
            let name = e?.file_name();
            if let Some(hex) = name.to_str().and_then(|n| n.strip_suffix(".json.zst"))
                && let Ok(id) = ObjectId::from_hex(hex)
            {
                ids.push(id);
            }
        }
        ids.sort();
        Ok(ids)
    }

    fn read_snapshot_json(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let packed = fs::read(self.snapshot_path(id)).with_context(|| format!("snapshot {id} not found"))?;
        let json = zstd::stream::decode_all(packed.as_slice())
            .with_context(|| format!("snapshot {id} is corrupt"))?;
        if ObjectId::of(&json) != *id {
            bail!("snapshot {id} is corrupt (hash mismatch)");
        }
        Ok(json)
    }

    pub fn load_snapshot(&self, id: &ObjectId) -> Result<Snapshot> {
        let json = self.read_snapshot_json(id)?;
        serde_json::from_slice(&json).with_context(|| format!("parsing snapshot {id}"))
    }

    pub fn load_header(&self, id: &ObjectId) -> Result<SnapshotHeader> {
        let json = self.read_snapshot_json(id)?;
        serde_json::from_slice(&json).with_context(|| format!("parsing snapshot {id}"))
    }

    pub fn delete_snapshot(&self, id: &ObjectId) -> Result<()> {
        fs::remove_file(self.snapshot_path(id))?;
        Ok(())
    }

    /// Resolve a (possibly abbreviated) snapshot id, git-style.
    pub fn resolve(&self, prefix: &str) -> Result<ObjectId> {
        let prefix = prefix.to_ascii_lowercase();
        if prefix.is_empty() || !prefix.bytes().all(|b| b.is_ascii_hexdigit()) {
            bail!("{prefix:?} is not a snapshot id");
        }
        let matches: Vec<_> =
            self.snapshot_ids()?.into_iter().filter(|id| id.hex().starts_with(&prefix)).collect();
        match matches.as_slice() {
            [id] => Ok(*id),
            [] => bail!("no snapshot matches {prefix:?} (see `vxl list`)"),
            _ => bail!("{prefix:?} is ambiguous ({} snapshots match); use more characters", matches.len()),
        }
    }
}

/// Shortest prefix length (min 6) that keeps every id in `ids` unique.
pub fn short_len(ids: &[ObjectId]) -> usize {
    let mut hexes: Vec<String> = ids.iter().map(|i| i.hex()).collect();
    hexes.sort();
    let common = hexes
        .windows(2)
        .map(|w| w[0].bytes().zip(w[1].bytes()).take_while(|(a, b)| a == b).count())
        .max()
        .unwrap_or(0);
    (common + 1).max(6)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn put_get_dedup() {
        let dir = tempfile::tempdir().unwrap();
        Repo::init(dir.path(), 3).unwrap();
        let repo = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
        let a = repo.put(b"chunk data").unwrap();
        assert!(a.stored.is_some());
        let b = repo.put(b"chunk data").unwrap();
        assert_eq!(a.id, b.id);
        assert!(b.stored.is_none());
        assert_eq!(repo.get(&a.id).unwrap(), b"chunk data");
        assert_eq!(repo.list_objects().unwrap().len(), 1);

        // a fresh handle (empty cache) still dedups via the filesystem
        drop(repo);
        let repo = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
        assert!(repo.put(b"chunk data").unwrap().stored.is_none());
    }

    #[test]
    fn corruption_detected() {
        let dir = tempfile::tempdir().unwrap();
        Repo::init(dir.path(), 3).unwrap();
        let repo = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
        let id = repo.put(b"original").unwrap().id;
        let other = zstd::bulk::compress(b"tampered", 3).unwrap();
        fs::write(repo.object_path(&id), other).unwrap();
        assert!(repo.get(&id).unwrap_err().to_string().contains("hash mismatch"));
    }

    #[test]
    fn exclusive_lock_blocks_others() {
        let dir = tempfile::tempdir().unwrap();
        Repo::init(dir.path(), 3).unwrap();
        let _w = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
        assert!(Repo::open(dir.path(), LockMode::Shared).is_err());
        assert!(Repo::open(dir.path(), LockMode::None).is_ok());
    }

    #[test]
    fn init_refuses_non_empty() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x"), b"").unwrap();
        assert!(Repo::init(dir.path(), 3).is_err());
    }

    #[test]
    fn short_ids() {
        let a = ObjectId::from_hex(&format!("abcdef0{}", "0".repeat(57))).unwrap();
        let b = ObjectId::from_hex(&format!("abcdef1{}", "0".repeat(57))).unwrap();
        assert_eq!(short_len(&[a]), 6);
        assert_eq!(short_len(&[a, b]), 7);
    }
}
