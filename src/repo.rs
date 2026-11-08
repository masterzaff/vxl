//! The repository: config, lock, pack-based object store, snapshots.
//!
//! ```text
//! repo/
//!   config.json             format version, zstd level, pack size
//!   lock                    OS file lock (backup/prune exclusive, restore/verify shared)
//!   packs/<id>.pack         many zstd-compressed objects + an index (see `pack.rs`)
//!   snapshots/<id>.json.zst manifests
//!   cache/headers/<id>.json snapshot headers, so `list` needn't read whole manifests
//!   tmp/                    staging area for atomic writes
//! ```
//!
//! Objects are named by the BLAKE3 of their *uncompressed* content. On open we
//! read every pack's index into memory: object id → (pack, offset, length).

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::manifest::{Snapshot, SnapshotHeader};
use crate::pack::{self, Builder, Entry};
use crate::util::{atomic_write, sync_dir};

/// Repository layout version. v1 (loose object files) only existed in early dev builds.
pub const REPO_VERSION: u32 = 2;
/// Default zstd level for objects. Chosen by `examples/zstd_levels.rs`: past
/// this point each extra level costs a lot of speed for very little space.
pub const DEFAULT_ZSTD_LEVEL: i32 = 6;
/// Target pack size. Big enough that a large world is a few hundred files,
/// small enough that repacking one during prune is quick.
pub const DEFAULT_PACK_SIZE: u64 = 16 << 20;
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

// ---------------------------------------------------------------------------
// Repo

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Config {
    pub version: u32,
    pub zstd_level: i32,
    pub pack_size: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum LockMode {
    /// No lock and no pack index (only for `list`, which reads nothing but manifests).
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

/// A pack as listed in the in-memory index.
#[derive(Clone, Debug)]
pub struct PackInfo {
    pub id: ObjectId,
    /// File size on disk.
    pub size: u64,
    pub entries: Vec<Entry>,
}

#[derive(Clone, Copy)]
struct Loc {
    pack: ObjectId,
    offset: u64,
    len: u32,
}

#[derive(Default)]
struct Index {
    packs: HashMap<ObjectId, PackInfo>,
    objects: HashMap<ObjectId, Loc>,
}

impl Index {
    fn add(&mut self, info: PackInfo) {
        for e in &info.entries {
            // A crash during repack can leave an object in two packs; either copy is fine.
            self.objects.insert(e.id, Loc { pack: info.id, offset: e.offset, len: e.len });
        }
        self.packs.insert(info.id, info);
    }
}

pub struct Repo {
    root: PathBuf,
    pub config: Config,
    index: RwLock<Index>,
    /// Ids stored (or being stored) by this process but maybe not in a written pack yet.
    known: Mutex<HashSet<ObjectId>>,
    /// The pack currently being filled.
    open_pack: Mutex<Builder>,
    /// Open file handles, so restoring 100k chunks doesn't mean 100k `open()` calls.
    handles: Mutex<HashMap<ObjectId, Arc<File>>>,
    /// Problems found while loading (e.g. an unreadable pack).
    pub warnings: Vec<String>,
    _lock: Option<File>,
}

impl Repo {
    /// Create a new, empty repository at `path` (must not exist or be empty).
    pub fn init(path: &Path, zstd_level: i32, pack_size: u64) -> Result<()> {
        if !(1..=22).contains(&zstd_level) {
            bail!("zstd level must be between 1 and 22");
        }
        if !(64 << 10..=1 << 30).contains(&pack_size) {
            bail!("pack size must be between 64 KiB and 1 GiB");
        }
        if path.exists() && fs::read_dir(path)?.next().is_some() {
            bail!("{} already exists and is not empty", path.display());
        }
        for d in ["packs", "snapshots", "tmp"] {
            fs::create_dir_all(path.join(d))?;
        }
        File::create(path.join("lock"))?;
        let config = Config { version: REPO_VERSION, zstd_level, pack_size };
        atomic_write(&path.join("config.json"), &path.join("tmp"), &serde_json::to_vec_pretty(&config)?)?;
        sync_dir(path)?;
        Ok(())
    }

    pub fn open(path: &Path, lock: LockMode) -> Result<Repo> {
        let cfg_path = path.join("config.json");
        let raw = fs::read(&cfg_path)
            .with_context(|| format!("{} is not a vxl repository (run `vxl init` first)", path.display()))?;
        let version = serde_json::from_slice::<serde_json::Value>(&raw)
            .context("reading config.json")?
            .get("version")
            .and_then(|v| v.as_u64())
            .unwrap_or(0);
        if version > REPO_VERSION as u64 {
            bail!("repository format v{version} is newer than this vxl (v{REPO_VERSION}); upgrade vxl");
        }
        if version < REPO_VERSION as u64 {
            bail!(
                "repository format v{version} is from an early development build of vxl and \
                 can't be read any more; create a new repository with `vxl init`"
            );
        }
        let config: Config = serde_json::from_slice(&raw).context("reading config.json")?;
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
        let mut repo = Repo {
            root: path.to_path_buf(),
            config,
            index: RwLock::default(),
            known: Mutex::default(),
            open_pack: Mutex::default(),
            handles: Mutex::default(),
            warnings: Vec::new(),
            _lock: lock_file,
        };
        if lock != LockMode::None {
            repo.load_index()?;
        }
        Ok(repo)
    }

    fn load_index(&mut self) -> Result<()> {
        let mut index = Index::default();
        for e in fs::read_dir(self.root.join("packs"))? {
            let path = e?.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".pack"))
                .and_then(|h| ObjectId::from_hex(h).ok())
            else {
                continue;
            };
            match pack::read_index(&path) {
                Ok((size, entries)) => index.add(PackInfo { id, size, entries }),
                Err(e) => self.warnings.push(format!("ignoring unreadable pack {}: {e:#}", path.display())),
            }
        }
        self.index = RwLock::new(index);
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn tmp_dir(&self) -> PathBuf {
        self.root.join("tmp")
    }

    fn pack_path(&self, id: &ObjectId) -> PathBuf {
        self.root.join("packs").join(format!("{}.pack", id.hex()))
    }

    // -- writing ---------------------------------------------------------------

    /// Store `data` unless an identical object already exists. The object goes
    /// into the open pack; call [`Repo::flush`] before relying on it being on disk.
    pub fn put(&self, data: &[u8]) -> Result<Put> {
        let id = ObjectId::of(data);
        if self.index.read().unwrap().objects.contains_key(&id) {
            return Ok(Put { id, stored: None });
        }
        // `insert` returns false if this id was already stored (or claimed by another thread).
        if !self.known.lock().unwrap().insert(id) {
            return Ok(Put { id, stored: None });
        }
        let packed = zstd::bulk::compress(data, self.config.zstd_level)?;
        let full = {
            let mut open = self.open_pack.lock().unwrap();
            open.add(id, &packed)?;
            (open.data_len() >= self.config.pack_size).then(|| std::mem::take(&mut *open))
        };
        // Write a full pack outside the lock so other threads keep filling a new one.
        if let Some(b) = full {
            self.write_pack(b)?;
        }
        Ok(Put { id, stored: Some(packed.len() as u64) })
    }

    fn write_pack(&self, b: Builder) -> Result<ObjectId> {
        let done = b.finish();
        atomic_write(&self.pack_path(&done.id), &self.tmp_dir(), &done.bytes)
            .with_context(|| format!("writing pack {}", done.id))?;
        let info = PackInfo { id: done.id, size: done.bytes.len() as u64, entries: done.entries };
        self.index.write().unwrap().add(info);
        Ok(done.id)
    }

    /// Write the partly filled pack and make all new packs durable.
    pub fn flush(&self) -> Result<()> {
        let b = std::mem::take(&mut *self.open_pack.lock().unwrap());
        if !b.is_empty() {
            self.write_pack(b)?;
        }
        sync_dir(&self.root.join("packs"))
    }

    // -- reading ---------------------------------------------------------------

    fn handle(&self, pack: &ObjectId) -> Result<Arc<File>> {
        if let Some(f) = self.handles.lock().unwrap().get(pack) {
            return Ok(f.clone());
        }
        let f =
            Arc::new(File::open(self.pack_path(pack)).with_context(|| format!("pack {pack} is missing"))?);
        self.handles.lock().unwrap().insert(*pack, f.clone());
        Ok(f)
    }

    /// The compressed bytes of an object, unverified.
    pub fn read_raw(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let loc =
            *self.index.read().unwrap().objects.get(id).ok_or_else(|| anyhow!("object {id} is missing"))?;
        let mut buf = vec![0u8; loc.len as usize];
        pack::read_at(&*self.handle(&loc.pack)?, &mut buf, loc.offset)
            .with_context(|| format!("reading object {id} from pack {}", loc.pack))?;
        Ok(buf)
    }

    /// Load and verify an object.
    pub fn get(&self, id: &ObjectId) -> Result<Vec<u8>> {
        decode_verified(id, &self.read_raw(id)?)
    }

    /// All objects with their compressed size.
    pub fn list_objects(&self) -> Vec<(ObjectId, u64)> {
        let index = self.index.read().unwrap();
        index.objects.iter().map(|(id, loc)| (*id, loc.len as u64)).collect()
    }

    pub fn packs(&self) -> Vec<PackInfo> {
        let mut packs: Vec<_> = self.index.read().unwrap().packs.values().cloned().collect();
        packs.sort_by_key(|p| p.id);
        packs
    }

    /// Where an object lives on disk: (pack file, offset). For tests and debugging.
    #[doc(hidden)]
    pub fn locate(&self, id: &ObjectId) -> Option<(PathBuf, u64)> {
        let loc = *self.index.read().unwrap().objects.get(id)?;
        Some((self.pack_path(&loc.pack), loc.offset))
    }

    // -- garbage collection ------------------------------------------------------

    /// Copy the `keep` objects out of `packs` into new packs (verifying each
    /// one), make those durable, then delete the old packs. Needs the exclusive lock.
    /// Returns (new packs written, their total size).
    pub fn repack(&self, packs: &[PackInfo], keep: &HashSet<ObjectId>) -> Result<(u64, u64)> {
        let mut copied = HashSet::new();
        let mut b = Builder::default();
        let mut written = Vec::new();
        for p in packs {
            let f = self.handle(&p.id)?;
            for e in &p.entries {
                if !keep.contains(&e.id) || !copied.insert(e.id) {
                    continue;
                }
                let mut raw = vec![0u8; e.len as usize];
                pack::read_at(&f, &mut raw, e.offset)?;
                decode_verified(&e.id, &raw).context("refusing to repack a damaged object")?;
                b.add(e.id, &raw)?;
                if b.data_len() >= self.config.pack_size {
                    written.push(self.write_pack(std::mem::take(&mut b))?);
                }
            }
        }
        if !b.is_empty() {
            written.push(self.write_pack(b)?);
        }
        // New copies must be durable before the old ones go away.
        sync_dir(&self.root.join("packs"))?;
        for p in packs {
            if !written.contains(&p.id) {
                self.delete_pack(&p.id)?;
            }
        }
        let index = self.index.read().unwrap();
        let bytes = written.iter().filter_map(|id| index.packs.get(id)).map(|p| p.size).sum();
        Ok((written.len() as u64, bytes))
    }

    pub fn delete_pack(&self, id: &ObjectId) -> Result<()> {
        self.handles.lock().unwrap().remove(id);
        fs::remove_file(self.pack_path(id)).with_context(|| format!("deleting pack {id}"))?;
        let mut index = self.index.write().unwrap();
        if let Some(info) = index.packs.remove(id) {
            for e in info.entries {
                if index.objects.get(&e.id).is_some_and(|l| l.pack == *id) {
                    index.objects.remove(&e.id);
                }
            }
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
        let mut snap: Snapshot =
            serde_json::from_slice(&json).with_context(|| format!("parsing snapshot {id}"))?;
        check_version(id, snap.header.version)?;
        snap.upgrade();
        Ok(snap)
    }

    /// A snapshot's header. Served from `cache/headers/` when possible: a manifest of a
    /// big world is tens of MB, and `list`/`prune` need only its header. Snapshots never
    /// change (the id is the hash of the content), so a cached header can't go stale; a
    /// missing or unreadable cache entry just means reading the manifest.
    pub fn load_header(&self, id: &ObjectId) -> Result<SnapshotHeader> {
        let cached = self.header_cache_path(id);
        if let Some(mut h) =
            fs::read(&cached).ok().and_then(|b| serde_json::from_slice::<SnapshotHeader>(&b).ok())
            && self.snapshot_path(id).exists()
        {
            check_version(id, h.version)?;
            h.upgrade();
            return Ok(h);
        }
        let json = self.read_snapshot_json(id)?;
        let mut h: SnapshotHeader =
            serde_json::from_slice(&json).with_context(|| format!("parsing snapshot {id}"))?;
        check_version(id, h.version)?;
        h.upgrade();
        // Best effort: a read-only repo or a concurrent prune just means no cache.
        if let Ok(bytes) = serde_json::to_vec(&h) {
            let _ = fs::create_dir_all(cached.parent().unwrap())
                .map_err(anyhow::Error::from)
                .and_then(|()| atomic_write(&cached, &self.tmp_dir(), &bytes));
        }
        Ok(h)
    }

    fn header_cache_path(&self, id: &ObjectId) -> PathBuf {
        self.root.join("cache").join("headers").join(format!("{}.json", id.hex()))
    }

    pub fn delete_snapshot(&self, id: &ObjectId) -> Result<()> {
        fs::remove_file(self.snapshot_path(id))?;
        let _ = fs::remove_file(self.header_cache_path(id));
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

/// Refuse manifests from a newer vxl rather than misreading them.
fn check_version(id: &ObjectId, version: u32) -> Result<()> {
    if version > crate::manifest::FORMAT_VERSION {
        bail!(
            "snapshot {id} uses manifest format {version}, but this vxl only knows up to {}; upgrade vxl",
            crate::manifest::FORMAT_VERSION
        );
    }
    Ok(())
}

fn decode_verified(id: &ObjectId, packed: &[u8]) -> Result<Vec<u8>> {
    let data = zstd::stream::decode_all(packed)
        .with_context(|| format!("object {id} is corrupt (bad zstd data)"))?;
    if ObjectId::of(&data) != *id {
        bail!("object {id} is corrupt (hash mismatch)");
    }
    Ok(data)
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

    const SMALL: u64 = 64 << 10;

    fn fresh(pack_size: u64) -> (tempfile::TempDir, Repo) {
        let dir = tempfile::tempdir().unwrap();
        Repo::init(dir.path(), 3, pack_size).unwrap();
        let repo = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
        (dir, repo)
    }

    fn noise(seed: u64, n: usize) -> Vec<u8> {
        let mut x = seed * 2 + 1;
        (0..n)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect()
    }

    #[test]
    fn put_get_dedup_across_reopen() {
        let (dir, repo) = fresh(DEFAULT_PACK_SIZE);
        let a = repo.put(b"chunk data").unwrap();
        assert!(a.stored.is_some());
        let b = repo.put(b"chunk data").unwrap();
        assert_eq!(a.id, b.id);
        assert!(b.stored.is_none());
        repo.flush().unwrap();
        assert_eq!(repo.get(&a.id).unwrap(), b"chunk data");
        assert_eq!(repo.packs().len(), 1);

        drop(repo);
        let repo = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
        assert!(repo.warnings.is_empty());
        assert!(repo.put(b"chunk data").unwrap().stored.is_none());
        assert_eq!(repo.get(&a.id).unwrap(), b"chunk data");
    }

    #[test]
    fn unflushed_objects_are_not_on_disk() {
        let (dir, repo) = fresh(DEFAULT_PACK_SIZE);
        repo.put(b"never flushed").unwrap();
        drop(repo);
        let repo = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
        assert!(repo.list_objects().is_empty());
    }

    #[test]
    fn packs_roll_over_at_target_size() {
        let (_dir, repo) = fresh(SMALL);
        let ids: Vec<_> = (0..40).map(|i| repo.put(&noise(i, 10_000)).unwrap().id).collect();
        repo.flush().unwrap();
        let packs = repo.packs();
        assert!(packs.len() >= 6, "{} packs", packs.len());
        assert!(packs.iter().all(|p| p.size < SMALL + 20_000));
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(repo.get(id).unwrap(), noise(i as u64, 10_000));
        }
    }

    #[test]
    fn corruption_detected() {
        let (_dir, repo) = fresh(DEFAULT_PACK_SIZE);
        let id = repo.put(&noise(1, 5000)).unwrap().id;
        repo.flush().unwrap();
        let (path, offset) = repo.locate(&id).unwrap();
        let mut bytes = fs::read(&path).unwrap();
        bytes[offset as usize + 100] ^= 0xFF;
        fs::write(&path, bytes).unwrap();
        assert!(repo.get(&id).unwrap_err().to_string().contains("corrupt"));
    }

    #[test]
    fn unreadable_pack_is_a_warning() {
        let (dir, repo) = fresh(DEFAULT_PACK_SIZE);
        repo.put(b"x").unwrap();
        repo.flush().unwrap();
        let path = repo.packs()[0].id;
        drop(repo);
        fs::write(dir.path().join("packs").join(format!("{}.pack", path.hex())), b"junk").unwrap();
        let repo = Repo::open(dir.path(), LockMode::Shared).unwrap();
        assert_eq!(repo.warnings.len(), 1);
        assert!(repo.list_objects().is_empty());
    }

    #[test]
    fn repack_keeps_only_live_objects() {
        let (dir, repo) = fresh(SMALL);
        let ids: Vec<_> = (0..20).map(|i| repo.put(&noise(i, 8_000)).unwrap().id).collect();
        repo.flush().unwrap();
        let keep: HashSet<_> = ids.iter().step_by(3).copied().collect();
        let old = repo.packs();
        let (written, _) = repo.repack(&old, &keep).unwrap();
        assert!(written >= 1);
        assert_eq!(repo.list_objects().len(), keep.len());
        for p in &old {
            assert!(!dir.path().join("packs").join(format!("{}.pack", p.id.hex())).exists());
        }
        drop(repo);
        let repo = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
        for (i, id) in ids.iter().enumerate() {
            assert_eq!(repo.get(id).is_ok(), keep.contains(id), "object {i}");
        }
    }

    #[test]
    fn exclusive_lock_blocks_others() {
        let (dir, _w) = fresh(DEFAULT_PACK_SIZE);
        assert!(Repo::open(dir.path(), LockMode::Shared).is_err());
        assert!(Repo::open(dir.path(), LockMode::None).is_ok());
    }

    #[test]
    fn init_validates() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("x"), b"").unwrap();
        assert!(Repo::init(dir.path(), 3, DEFAULT_PACK_SIZE).is_err());
        let empty = tempfile::tempdir().unwrap();
        assert!(Repo::init(empty.path(), 3, 10).is_err());
    }

    #[test]
    fn old_format_refused() {
        let dir = tempfile::tempdir().unwrap();
        fs::write(dir.path().join("config.json"), r#"{"version":1,"zstd_level":6}"#).unwrap();
        let err = Repo::open(dir.path(), LockMode::Shared).err().unwrap();
        assert!(err.to_string().contains("early development build"));
    }

    #[test]
    fn short_ids() {
        let a = ObjectId::from_hex(&format!("abcdef0{}", "0".repeat(57))).unwrap();
        let b = ObjectId::from_hex(&format!("abcdef1{}", "0".repeat(57))).unwrap();
        assert_eq!(short_len(&[a]), 6);
        assert_eq!(short_len(&[a, b]), 7);
    }

    #[test]
    fn headers_are_cached_and_survive_a_bad_cache() {
        use crate::manifest::{FORMAT_VERSION, Stats};
        let (dir, repo) = fresh(DEFAULT_PACK_SIZE);
        let snap = Snapshot {
            header: SnapshotHeader {
                version: FORMAT_VERSION,
                created: chrono::Utc::now(),
                message: Some("hi".into()),
                source: "/srv/mc".into(),
                stats: Stats::default(),
                world_names: vec!["world".into()],
            },
            worlds: vec![],
            files: vec![],
        };
        let id = repo.write_snapshot(&snap).unwrap();
        let cache = dir.path().join("cache/headers").join(format!("{}.json", id.hex()));
        assert!(!cache.exists());
        assert_eq!(repo.load_header(&id).unwrap().message.as_deref(), Some("hi"));
        assert!(cache.exists(), "first read fills the cache");
        assert_eq!(repo.load_header(&id).unwrap().message.as_deref(), Some("hi"));

        fs::write(&cache, b"{not json").unwrap();
        assert_eq!(repo.load_header(&id).unwrap().message.as_deref(), Some("hi"), "bad cache: read the manifest");

        repo.delete_snapshot(&id).unwrap();
        assert!(!cache.exists());
        assert!(repo.load_header(&id).is_err());
    }
}
