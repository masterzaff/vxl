//! The repository: config, lock, pack-based object store, snapshots.
//!
//! ```text
//! repo/
//!   config.json             format version, zstd level, pack size
//!   lock                    OS file lock (backup/prune exclusive, restore/verify shared)
//!   packs/<id>.pack         many zstd-compressed objects + an index (see `pack.rs`)
//!   snapshots/<id>.json.zst manifests
//!   cache/headers/<id>.json snapshot headers, so `list` needn't read whole manifests
//!   cache/packs.idx         every pack's index in one file (see `index_cache.rs`)
//!   tmp/                    staging area for atomic writes
//! ```
//!
//! Objects are named by the BLAKE3 of their *uncompressed* content. On open we
//! read every pack's index into memory: object id → (pack, offset, length).
//!
//! Format 3 adds group objects for chunks (see `group.rs`): named by the chunk they
//! decode to, and possibly reading sections from other group objects. [`Repo::get`]
//! decodes them transparently; [`Repo::deps`] says what one needs, for `prune`. Packs
//! are written strictly in the order they were filled, so after a crash every written
//! object's sources are on disk too.

use std::collections::{HashMap, HashSet};
use std::fmt;
use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, RwLock};

use anyhow::{Context, Result, anyhow, bail};
use serde::{Deserialize, Serialize};

use crate::group::{self, Body};
use crate::index_cache;
use crate::manifest::{Snapshot, SnapshotHeader};
use crate::pack::{self, Builder, Entry};
use crate::util::{atomic_write, sync_dir};

/// How long `open` waits for a lock someone else holds before reporting the repo busy.
pub const LOCK_GRACE: std::time::Duration = std::time::Duration::from_secs(2);
/// Newest repository format this vxl reads and writes. v1 (loose object files) only
/// existed in early dev builds; v2 stores every chunk whole; v3 also stores chunks as
/// group objects (`group.rs`).
pub const REPO_VERSION: u32 = 3;
/// Format `vxl init` creates unless told otherwise.
pub const DEFAULT_REPO_VERSION: u32 = 3;
/// Group objects kept decoded in memory, for reading sections out of them.
const BODY_CACHE: usize = 1024;
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
    /// No lock (`list`, `show`, `diff`: they only read snapshots, which never change).
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
    /// Further copies of an object: a repaired group object (`chunk_store.rs`), a chunk
    /// `migrate` converted, or a crash during repack. Reads try every copy.
    extra: HashMap<ObjectId, Vec<Loc>>,
}

impl Index {
    /// `newest`: the pack was just written, so its copies become the primary ones.
    fn add(&mut self, info: PackInfo, newest: bool) {
        for e in &info.entries {
            let loc = Loc { pack: info.id, offset: e.offset, len: e.len };
            match self.objects.get_mut(&e.id) {
                None => {
                    self.objects.insert(e.id, loc);
                }
                Some(cur) if cur.pack == info.id => {}
                Some(cur) => {
                    let other = if newest { std::mem::replace(cur, loc) } else { loc };
                    self.extra.entry(e.id).or_default().push(other);
                }
            }
        }
        self.packs.insert(info.id, info);
    }

    fn copies(&self, id: &ObjectId) -> Vec<Loc> {
        self.objects.get(id).into_iter().chain(self.extra.get(id).into_iter().flatten()).copied().collect()
    }

    fn remove_pack(&mut self, pack: &ObjectId) {
        let Some(info) = self.packs.remove(pack) else { return };
        for e in info.entries {
            if let Some(extra) = self.extra.get_mut(&e.id) {
                extra.retain(|l| l.pack != *pack);
            }
            if self.objects.get(&e.id).is_some_and(|l| l.pack == *pack) {
                match self.extra.get_mut(&e.id).and_then(|x| x.pop()) {
                    Some(l) => self.objects.insert(e.id, l),
                    None => self.objects.remove(&e.id),
                };
            }
            if self.extra.get(&e.id).is_some_and(|x| x.is_empty()) {
                self.extra.remove(&e.id);
            }
        }
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
    /// Held while writing a full pack; taken before the open pack is released, so packs
    /// reach the disk in the order they were filled.
    write_order: Mutex<()>,
    /// Recently used group objects, decoded (see [`Repo::source_body`]).
    bodies: Mutex<HashMap<ObjectId, Arc<Body>>>,
    /// Open file handles, so restoring 100k chunks doesn't mean 100k `open()` calls.
    handles: Mutex<HashMap<ObjectId, Arc<File>>>,
    /// Problems found while loading (e.g. an unreadable pack).
    pub warnings: Vec<String>,
    _lock: Option<File>,
}

impl Repo {
    /// Create a new, empty repository at `path` (must not exist or be empty) in the
    /// default format.
    pub fn init(path: &Path, zstd_level: i32, pack_size: u64) -> Result<()> {
        Self::init_format(path, zstd_level, pack_size, DEFAULT_REPO_VERSION)
    }

    pub fn init_format(path: &Path, zstd_level: i32, pack_size: u64, version: u32) -> Result<()> {
        if !(2..=REPO_VERSION).contains(&version) {
            bail!("repository format must be 2 or 3");
        }
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
        let config = Config { version, zstd_level, pack_size };
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
        if version < 2 {
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
                // Wait a moment before giving up: the lock may be held only briefly (a
                // command just finishing, or a freshly forked child that hasn't exec'd yet
                // and still holds a copy of the descriptor).
                let deadline = std::time::Instant::now() + LOCK_GRACE;
                loop {
                    let res = if mode == LockMode::Exclusive { f.try_lock() } else { f.try_lock_shared() };
                    match res {
                        Ok(()) => break Some(f),
                        Err(TryLockError::WouldBlock) if std::time::Instant::now() < deadline => {
                            std::thread::sleep(std::time::Duration::from_millis(25));
                        }
                        Err(TryLockError::WouldBlock) => {
                            bail!("repository is busy: another vxl command is using it right now")
                        }
                        Err(TryLockError::Error(e)) => return Err(e).context("locking repository"),
                    }
                }
            }
        };
        let mut repo = Repo {
            root: path.to_path_buf(),
            config,
            index: RwLock::default(),
            known: Mutex::default(),
            open_pack: Mutex::default(),
            write_order: Mutex::default(),
            bodies: Mutex::default(),
            handles: Mutex::default(),
            warnings: Vec::new(),
            _lock: lock_file,
        };
        repo.load_index()?;
        Ok(repo)
    }

    /// Load every pack's index, from `cache/packs.idx` where it's still valid (see
    /// [`index_cache`]); packs the cache doesn't know are read, and the cache is rewritten
    /// if anything changed (best effort: a read-only repo just doesn't get one).
    fn load_index(&mut self) -> Result<()> {
        let cache_path = self.root.join("cache").join("packs.idx");
        let mut cached = index_cache::read(&cache_path).unwrap_or_default();
        let had = cached.len();
        let mut index = Index::default();
        let mut changed = false;
        for e in fs::read_dir(self.root.join("packs"))? {
            let e = e?;
            let path = e.path();
            let Some(id) = path
                .file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.strip_suffix(".pack"))
                .and_then(|h| ObjectId::from_hex(h).ok())
            else {
                continue;
            };
            let size = e.metadata()?.len();
            if let Some((cached_size, entries)) = cached.remove(&id)
                && cached_size == size
            {
                index.add(PackInfo { id, size, entries }, false);
                continue;
            }
            changed = true;
            match pack::read_index(&path) {
                Ok((size, entries)) => index.add(PackInfo { id, size, entries }, false),
                Err(e) => self.warnings.push(format!("ignoring unreadable pack {}: {e:#}", path.display())),
            }
        }
        // Whatever is left in `cached` is for packs that no longer exist.
        if changed || !cached.is_empty() || had != index.packs.len() {
            let packs = index.packs.values().map(|p| (&p.id, p.size, p.entries.as_slice()));
            let _ = index_cache::write(&cache_path, &self.tmp_dir(), packs);
        }
        self.index = RwLock::new(index);
        Ok(())
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Format 3: chunks are stored as group objects.
    pub fn grouped(&self) -> bool {
        self.config.version >= 3
    }

    /// Switch the repository's format (for `migrate`). Needs the exclusive lock.
    pub fn set_format(&mut self, version: u32) -> Result<()> {
        if !(2..=REPO_VERSION).contains(&version) {
            bail!("no repository format {version}");
        }
        if self.config.version == version {
            return Ok(());
        }
        let config = Config { version, ..self.config.clone() };
        atomic_write(&self.root.join("config.json"), &self.tmp_dir(), &serde_json::to_vec_pretty(&config)?)?;
        sync_dir(&self.root)?;
        self.config = config;
        Ok(())
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
        self.add_stored(id, &packed)?;
        Ok(Put { id, stored: Some(packed.len() as u64) })
    }

    /// Store an object whose stored form is already made (a group object). With
    /// `replace`, it's stored even if the repo has the id already (that copy is broken,
    /// or plain and being converted): reads then try every copy.
    pub fn put_stored(&self, id: ObjectId, stored: &[u8], replace: bool) -> Result<Put> {
        if !replace && self.index.read().unwrap().objects.contains_key(&id) {
            return Ok(Put { id, stored: None });
        }
        if !self.known.lock().unwrap().insert(id) {
            return Ok(Put { id, stored: None });
        }
        self.bodies.lock().unwrap().remove(&id);
        self.add_stored(id, stored)?;
        Ok(Put { id, stored: Some(stored.len() as u64) })
    }

    /// Whether the repo has `id` (in a written pack).
    pub fn has(&self, id: &ObjectId) -> bool {
        self.index.read().unwrap().objects.contains_key(id)
    }

    /// Whether this process stored (or is storing) `id`.
    pub fn stored_here(&self, id: &ObjectId) -> bool {
        self.known.lock().unwrap().contains(id)
    }

    fn add_stored(&self, id: ObjectId, stored: &[u8]) -> Result<()> {
        let mut open = self.open_pack.lock().unwrap();
        open.add(id, stored)?;
        if open.data_len() >= self.config.pack_size {
            let full = std::mem::take(&mut *open);
            // Queue for writing before letting anyone fill the next pack: packs then reach
            // the disk in order, and an object never lands before one it reads from.
            let order = self.write_order.lock().unwrap();
            drop(open);
            self.write_pack(full)?;
            drop(order);
        }
        Ok(())
    }

    fn write_pack(&self, b: Builder) -> Result<ObjectId> {
        let done = b.finish();
        atomic_write(&self.pack_path(&done.id), &self.tmp_dir(), &done.bytes)
            .with_context(|| format!("writing pack {}", done.id))?;
        if self.grouped() {
            // Make this pack's rename durable before any later pack's.
            sync_dir(&self.root.join("packs"))?;
        }
        let info = PackInfo { id: done.id, size: done.bytes.len() as u64, entries: done.entries };
        self.index.write().unwrap().add(info, true);
        Ok(done.id)
    }

    /// Write the partly filled pack and make all new packs durable.
    pub fn flush(&self) -> Result<()> {
        let mut open = self.open_pack.lock().unwrap();
        let b = std::mem::take(&mut *open);
        let _order = self.write_order.lock().unwrap();
        drop(open);
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

    /// The stored bytes of an object (its primary copy), unverified.
    pub fn read_raw(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let loc =
            *self.index.read().unwrap().objects.get(id).ok_or_else(|| anyhow!("object {id} is missing"))?;
        self.read_loc(id, loc, loc.len as usize)
    }

    /// The first `max` stored bytes of the copy at `loc`.
    fn read_loc(&self, id: &ObjectId, loc: Loc, max: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; (loc.len as usize).min(max)];
        pack::read_at(&*self.handle(&loc.pack)?, &mut buf, loc.offset)
            .with_context(|| format!("reading object {id} from pack {}", loc.pack))?;
        Ok(buf)
    }

    fn copies(&self, id: &ObjectId) -> Result<Vec<Loc>> {
        let copies = self.index.read().unwrap().copies(id);
        if copies.is_empty() {
            bail!("object {id} is missing");
        }
        Ok(copies)
    }

    /// Load and verify an object: its content hashes to `id` (for a group object, the
    /// chunk it decodes to does). Tries every copy until one is good.
    pub fn get(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let mut err = None;
        for loc in self.copies(id)? {
            match self.read_loc(id, loc, usize::MAX).and_then(|raw| self.decode(id, &raw)) {
                Ok(data) => return Ok(data),
                Err(e) => err = Some(e),
            }
        }
        Err(err.unwrap())
    }

    fn decode(&self, id: &ObjectId, raw: &[u8]) -> Result<Vec<u8>> {
        if !group::is_group(raw) {
            return decode_verified(id, raw);
        }
        let body = Body::decode(raw).with_context(|| format!("object {id} is corrupt"))?;
        let data = body
            .chunk(|i, k| self.source_slot(&body.sources[i], k))
            .with_context(|| format!("object {id} can't be decoded"))?;
        if ObjectId::of(&data) != *id {
            bail!("object {id} is corrupt (decodes to the wrong content)");
        }
        Ok(data)
    }

    /// Section `k` of group object `x`.
    pub fn source_slot(&self, x: &ObjectId, k: u32) -> Result<Vec<u8>> {
        let body = self.source_body(x)?;
        body.slot(k).map(<[u8]>::to_vec).ok_or_else(|| anyhow!("object {x} has no section {k}"))
    }

    /// Group object `x`, decoded: the intact copy with the most sections. Copies of a
    /// group object only ever add sections (`chunk_store.rs` repairs by appending), so
    /// that copy holds every section any copy does.
    pub fn source_body(&self, x: &ObjectId) -> Result<Arc<Body>> {
        if let Some(b) = self.bodies.lock().unwrap().get(x) {
            return Ok(b.clone());
        }
        let mut best: Option<Body> = None;
        let mut err = None;
        for loc in self.copies(x)? {
            match self.read_loc(x, loc, usize::MAX).and_then(|raw| Body::decode(&raw)) {
                Ok(b) if best.as_ref().is_none_or(|c| b.slots() > c.slots()) => best = Some(b),
                Ok(_) => {}
                Err(e) => err = Some(e),
            }
        }
        let body = Arc::new(best.ok_or_else(|| {
            err.unwrap_or_else(|| anyhow!("object {x} is not a group object")).context(format!("object {x}"))
        })?);
        let mut cache = self.bodies.lock().unwrap();
        if cache.len() >= BODY_CACHE {
            cache.clear();
        }
        cache.insert(*x, body.clone());
        Ok(body)
    }

    /// The objects `id` reads sections from (any copy); empty for plain objects.
    /// Reads only the start of each copy. `prune` keeps these alive along with `id`.
    pub fn deps(&self, id: &ObjectId) -> Result<Vec<ObjectId>> {
        let copies = self.index.read().unwrap().copies(id);
        let mut out = Vec::new();
        for loc in copies {
            let head = self.read_loc(id, loc, group::MAX_HEADER_LEN)?;
            if group::is_group(&head) {
                out.extend(group::sources(&head).with_context(|| format!("object {id} is corrupt"))?);
            }
        }
        out.sort();
        out.dedup();
        Ok(out)
    }

    /// For a group object that decodes correctly: where each section it uses is stored
    /// (section id → group object, slot), so a newer version of the chunk can point at
    /// them. Everything returned was read back and hashed. `None` if no copy is a group
    /// object that decodes.
    pub fn group_locations(&self, id: &ObjectId) -> Option<Vec<(ObjectId, group::Location)>> {
        for loc in self.copies(id).ok()? {
            let Ok(raw) = self.read_loc(id, loc, usize::MAX) else { continue };
            if !group::is_group(&raw) {
                continue;
            }
            let Ok(body) = Body::decode(&raw) else { continue };
            let mut out = Vec::new();
            let decoded = body.chunk(|i, k| {
                let x = body.sources[i];
                let s = self.source_slot(&x, k)?;
                out.push((ObjectId::of(&s), (x, k)));
                Ok(s)
            });
            if !decoded.is_ok_and(|d| ObjectId::of(&d) == *id) {
                continue;
            }
            out.extend((0..body.slots() as u32).map(|k| (ObjectId::of(body.slot(k).unwrap()), (*id, k))));
            return Some(out);
        }
        None
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

    /// Copy the `keep` objects out of `packs` into new packs (checking each one), make
    /// those durable, then delete the old packs. Needs the exclusive lock.
    /// Returns (new packs written, their total size).
    ///
    /// Where an object has several copies in `packs`, one is kept: the group copy with
    /// the most sections (see [`Repo::source_body`]), else a plain one. A plain copy is
    /// also kept unless some group copy of the object, anywhere, decodes correctly.
    pub fn repack(&self, packs: &[PackInfo], keep: &HashSet<ObjectId>) -> Result<(u64, u64)> {
        self.repack_with(packs, keep, false)
    }

    /// [`Repo::repack`]; with `plain`, an object with a good plain copy keeps only that
    /// (for `migrate --to 2`).
    pub fn repack_with(
        &self,
        packs: &[PackInfo],
        keep: &HashSet<ObjectId>,
        plain: bool,
    ) -> Result<(u64, u64)> {
        let mut candidates: Vec<(ObjectId, Vec<Loc>)> = Vec::new();
        let mut slot: HashMap<ObjectId, usize> = HashMap::new();
        for p in packs {
            for e in p.entries.iter().filter(|e| keep.contains(&e.id)) {
                let i = *slot.entry(e.id).or_insert_with(|| {
                    candidates.push((e.id, Vec::new()));
                    candidates.len() - 1
                });
                candidates[i].1.push(Loc { pack: p.id, offset: e.offset, len: e.len });
            }
        }
        let mut b = Builder::default();
        let mut written = Vec::new();
        for (id, locs) in &candidates {
            for raw in self.choose_copies(id, locs, plain).context("refusing to repack a damaged object")? {
                b.add(*id, &raw)?;
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

    /// Which of an object's copies at `locs` repack keeps (see [`Repo::repack`]).
    fn choose_copies(&self, id: &ObjectId, locs: &[Loc], prefer_plain: bool) -> Result<Vec<Vec<u8>>> {
        let mut best_group: Option<(usize, Vec<u8>)> = None;
        let mut plain: Option<Vec<u8>> = None;
        let mut err = None;
        for &loc in locs {
            let raw = self.read_loc(id, loc, usize::MAX)?;
            if group::is_group(&raw) {
                match Body::decode(&raw) {
                    Ok(b) if best_group.as_ref().is_none_or(|(n, _)| b.slots() > *n) => {
                        best_group = Some((b.slots(), raw))
                    }
                    Ok(_) => {}
                    Err(e) => err = Some(e),
                }
            } else {
                match decode_verified(id, &raw) {
                    Ok(_) if plain.is_none() => plain = Some(raw),
                    Ok(_) => {}
                    Err(e) => err = Some(e),
                }
            }
        }
        if prefer_plain && let Some(raw) = plain {
            return Ok(vec![raw]);
        }
        let mut out = Vec::new();
        if let Some((_, raw)) = best_group {
            out.push(raw);
        }
        if let Some(raw) = plain {
            // Drop the plain copy only if the chunk provably decodes from a group copy
            // (here or in a pack that stays).
            if self.get_group(id).is_err() {
                out.push(raw);
            } else if out.is_empty() {
                return Ok(out);
            }
        }
        if out.is_empty() {
            return Err(err.unwrap_or_else(|| anyhow!("object {id} has no readable copy")));
        }
        Ok(out)
    }

    /// Load `id` from a plain copy only (group copies don't count).
    pub fn get_plain(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let mut err = anyhow!("object {id} has no plain copy");
        for loc in self.copies(id)? {
            let raw = self.read_loc(id, loc, usize::MAX)?;
            if !group::is_group(&raw) {
                match decode_verified(id, &raw) {
                    Ok(d) => return Ok(d),
                    Err(e) => err = e,
                }
            }
        }
        Err(err)
    }

    /// Whether any copy of any object is a group object.
    pub fn has_group_objects(&self) -> Result<bool> {
        let locs: Vec<(ObjectId, Loc)> = {
            let index = self.index.read().unwrap();
            index.objects.keys().flat_map(|id| index.copies(id).into_iter().map(|l| (*id, l))).collect()
        };
        for (id, loc) in locs {
            if group::is_group(&self.read_loc(&id, loc, group::MAGIC.len())?) {
                return Ok(true);
            }
        }
        Ok(false)
    }

    /// Decode `id` from a group copy only (plain copies don't count).
    pub fn get_group(&self, id: &ObjectId) -> Result<Vec<u8>> {
        let mut err = anyhow!("object {id} has no group copy");
        for loc in self.copies(id)? {
            let raw = self.read_loc(id, loc, usize::MAX)?;
            if group::is_group(&raw) {
                match self.decode(id, &raw) {
                    Ok(d) => return Ok(d),
                    Err(e) => err = e,
                }
            }
        }
        Err(err)
    }

    /// Re-read every pack's index from disk (after a bulk rewrite like `migrate`).
    pub fn reload_index(&mut self) -> Result<()> {
        self.handles.lock().unwrap().clear();
        self.bodies.lock().unwrap().clear();
        self.load_index()
    }

    pub fn delete_pack(&self, id: &ObjectId) -> Result<()> {
        self.handles.lock().unwrap().remove(id);
        fs::remove_file(self.pack_path(id)).with_context(|| format!("deleting pack {id}"))?;
        self.index.write().unwrap().remove_pack(id);
        self.bodies.lock().unwrap().clear();
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
    /// Chunk lists that are in a table object are left out of the stored JSON.
    pub fn write_snapshot(&self, snap: &Snapshot) -> Result<ObjectId> {
        let has_tables = snap
            .worlds
            .iter()
            .flat_map(|w| &w.files)
            .any(|f| matches!(f, crate::manifest::FileEntry::Region { table: Some(_), .. }));
        let json = if has_tables {
            if snap.header.version < 3 {
                bail!("chunk tables need manifest format 3");
            }
            let mut s = snap.clone();
            for f in s.worlds.iter_mut().flat_map(|w| &mut w.files) {
                if let crate::manifest::FileEntry::Region { table: Some(_), chunks, .. } = f {
                    chunks.clear();
                }
            }
            serde_json::to_vec(&s)?
        } else {
            serde_json::to_vec(snap)?
        };
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
        for f in snap.worlds.iter_mut().flat_map(|w| &mut w.files) {
            if let crate::manifest::FileEntry::Region { path, table: Some(t), chunks } = f {
                let bytes = self.get(t).with_context(|| format!("snapshot {id}: chunk table of {path}"))?;
                *chunks = crate::manifest::decode_table(&bytes)
                    .with_context(|| format!("snapshot {id}: chunk table of {path}"))?;
            }
        }
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
        assert_eq!(
            repo.load_header(&id).unwrap().message.as_deref(),
            Some("hi"),
            "bad cache: read the manifest"
        );

        repo.delete_snapshot(&id).unwrap();
        assert!(!cache.exists());
        assert!(repo.load_header(&id).is_err());
    }

    #[test]
    fn pack_index_cache_follows_the_packs() {
        let dir = tempfile::tempdir().unwrap();
        Repo::init(dir.path(), 3, 64 << 10).unwrap();
        let cache = dir.path().join("cache/packs.idx");
        let blobs: Vec<Vec<u8>> = (0..40).map(|i| noise(i, 20_000)).collect();
        {
            let repo = Repo::open(dir.path(), LockMode::Exclusive).unwrap();
            for b in &blobs {
                repo.put(b).unwrap();
            }
            repo.flush().unwrap();
        }
        assert!(!cache.exists(), "nothing to cache before the first open with packs");
        let ids = {
            let repo = Repo::open(dir.path(), LockMode::Shared).unwrap();
            repo.list_objects()
        };
        assert!(cache.exists());
        let packs: Vec<PathBuf> =
            fs::read_dir(dir.path().join("packs")).unwrap().map(|e| e.unwrap().path()).collect();
        assert!(packs.len() > 1);

        // Served from the cache: a pack whose index area is damaged afterwards still opens
        // cleanly and every object still reads (and hash-checks).
        let mut bytes = fs::read(&packs[0]).unwrap();
        let n = bytes.len();
        bytes[n - 10] ^= 0xFF;
        fs::write(&packs[0], &bytes).unwrap();
        let repo = Repo::open(dir.path(), LockMode::Shared).unwrap();
        assert!(repo.warnings.is_empty(), "{:?}", repo.warnings);
        assert_eq!(repo.list_objects().len(), ids.len());
        for b in &blobs {
            assert_eq!(&repo.get(&ObjectId::of(b)).unwrap(), b);
        }
        drop(repo);
        bytes[n - 10] ^= 0xFF;
        fs::write(&packs[0], &bytes).unwrap();

        // A pack that disappears is dropped from the cache; a corrupt cache is ignored.
        fs::remove_file(&packs[1]).unwrap();
        let fewer = Repo::open(dir.path(), LockMode::Shared).unwrap().list_objects().len();
        assert!(fewer < ids.len());
        let cached = index_cache::read(&cache).unwrap();
        assert_eq!(cached.len(), packs.len() - 1);
        fs::write(&cache, b"garbage").unwrap();
        let repo = Repo::open(dir.path(), LockMode::Shared).unwrap();
        assert_eq!(repo.list_objects().len(), fewer);
        assert!(index_cache::read(&cache).is_some(), "rewritten");
    }
}
