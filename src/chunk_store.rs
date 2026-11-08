//! Storing chunks in a format-3 repository: as group objects (`group.rs`) whose
//! unchanged sections point at where those sections are already stored.
//!
//! Where a section might already be: in the previous version of the same chunk (read
//! back and hashed before use, see [`Repo::group_locations`]), or in a group object this
//! run stored. Nothing else is consulted, and `group::encode` decodes every object
//! before it's stored, so a wrong guess can only cost space.
//!
//! Reusing an existing object is safe only if it decodes. Objects the latest snapshots
//! use do (prune keeps what they need); any other object is decoded first. One that
//! exists but doesn't decode (it was garbage, and prune dropped its sources) is stored
//! again alongside the old copy. The new copy keeps the old copy's sections in their
//! slots and appends what it needs, so objects pointing into the old copy stay valid.

use std::collections::{HashMap, HashSet};
use std::sync::Mutex;

use anyhow::Result;

use crate::group::{self, Location};
use crate::repo::{ObjectId, Put, Repo};

const SHARDS: usize = 16;

pub struct ChunkStore<'a> {
    repo: &'a Repo,
    /// Chunk objects the latest snapshots use: known to decode, so reused without a check.
    trusted: HashSet<ObjectId>,
    /// Sections this run stored: section id → where.
    run: Vec<Mutex<HashMap<ObjectId, Location>>>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    /// Not in the repo.
    New,
    /// In the repo, but no copy decodes: store a repaired copy.
    Repair,
    /// In the repo as a plain object (`migrate`): add a group copy, or leave it plain.
    Convert,
}

impl<'a> ChunkStore<'a> {
    pub fn new(repo: &'a Repo, trusted: HashSet<ObjectId>) -> Self {
        ChunkStore { repo, trusted, run: (0..SHARDS).map(|_| Mutex::default()).collect() }
    }

    fn shard(&self, id: &ObjectId) -> &Mutex<HashMap<ObjectId, Location>> {
        &self.run[id.0[0] as usize % SHARDS]
    }

    fn seen(&self, section: &ObjectId) -> Option<Location> {
        self.shard(section).lock().unwrap().get(section).copied()
    }

    fn remember(&self, obj: ObjectId, sections: &[ObjectId]) {
        for (k, s) in sections.iter().enumerate() {
            self.shard(s).lock().unwrap().entry(*s).or_insert((obj, k as u32));
        }
    }

    /// Store a chunk's (decompressed) NBT. `prev`: the object holding the previous version
    /// of this chunk, whose sections the new version can point at.
    pub fn put(&self, nbt: &[u8], prev: Option<&ObjectId>) -> Result<Put> {
        let id = ObjectId::of(nbt);
        if self.repo.stored_here(&id) {
            return Ok(Put { id, stored: None });
        }
        let mode = if !self.repo.has(&id) {
            Mode::New
        } else if self.trusted.contains(&id) || self.repo.get(&id).is_ok() {
            return Ok(Put { id, stored: None });
        } else {
            Mode::Repair
        };
        self.store(id, nbt, prev, mode)
    }

    /// For `migrate`: give a chunk stored as a plain object a group copy, unless it
    /// already has one that decodes. A chunk that can't be grouped stays plain.
    pub fn convert(&self, nbt: &[u8]) -> Result<Put> {
        let id = ObjectId::of(nbt);
        if self.repo.stored_here(&id) || self.repo.get_group(&id).is_ok() {
            return Ok(Put { id, stored: None });
        }
        self.store(id, nbt, None, Mode::Convert)
    }

    fn store(&self, id: ObjectId, nbt: &[u8], prev: Option<&ObjectId>, mode: Mode) -> Result<Put> {
        let Some(split) = group::split(nbt) else { return self.plain(id, nbt, mode) };
        let base = if mode == Mode::New { None } else { self.repo.source_body(&id).ok() };
        let near: HashMap<ObjectId, Location> = match prev {
            Some(p) if *p != id => self.repo.group_locations(p).unwrap_or_default().into_iter().collect(),
            _ => HashMap::new(),
        };
        let level = self.repo.config.zstd_level;
        let find = |s: &ObjectId| near.get(s).copied().or_else(|| self.seen(s));
        let Ok(enc) = group::encode(nbt, split, base.as_deref(), find, level) else {
            return self.plain(id, nbt, mode);
        };
        let put = self.repo.put_stored(id, &enc.stored, mode != Mode::New)?;
        if put.stored.is_some() {
            self.remember(id, &enc.new);
        }
        Ok(put)
    }

    /// Store `nbt` whole, as in format 2.
    fn plain(&self, id: ObjectId, nbt: &[u8], mode: Mode) -> Result<Put> {
        match mode {
            Mode::New => self.repo.put(nbt),
            Mode::Repair => {
                let packed = zstd::bulk::compress(nbt, self.repo.config.zstd_level)?;
                self.repo.put_stored(id, &packed, true)
            }
            Mode::Convert => Ok(Put { id, stored: None }),
        }
    }
}
