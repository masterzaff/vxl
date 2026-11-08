//! The snapshot manifest: a list of every file in the world and, for region
//! files, every chunk and the object that holds it. Stored as zstd-compressed
//! JSON in `snapshots/<id>.json.zst`, where `id` is the BLAKE3 of the JSON.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::repo::ObjectId;

pub const FORMAT_VERSION: u32 = 1;

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Stats {
    /// Files in the world (region files count once).
    pub files: u64,
    /// Chunks across all region files.
    pub chunks: u64,
    /// Chunks whose content wasn't in the repo yet.
    pub new_chunks: u64,
    /// New objects of any kind (chunks + whole files).
    pub new_objects: u64,
    /// Compressed bytes this snapshot added to the repo.
    pub stored_bytes: u64,
    /// Size of the world on disk when it was backed up.
    pub world_bytes: u64,
}

/// Everything about a snapshot except its file list (cheap to load for `list`).
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SnapshotHeader {
    pub version: u32,
    pub created: DateTime<Utc>,
    pub message: Option<String>,
    pub source: String,
    pub stats: Stats,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(flatten)]
    pub header: SnapshotHeader,
    /// Kept last in the JSON so the header is cheap to parse.
    pub files: Vec<FileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FileEntry {
    /// A file stored whole (level.dat, playerdata/…, broken region files).
    Blob { path: String, size: u64, hash: ObjectId },
    /// A region file stored chunk by chunk.
    Region { path: String, chunks: Vec<ChunkEntry> },
}

impl FileEntry {
    pub fn path(&self) -> &str {
        match self {
            FileEntry::Blob { path, .. } | FileEntry::Region { path, .. } => path,
        }
    }

    /// Every object this entry needs.
    pub fn objects(&self) -> impl Iterator<Item = ObjectId> + '_ {
        let (blob, chunks) = match self {
            FileEntry::Blob { hash, .. } => (Some(*hash), &[][..]),
            FileEntry::Region { chunks, .. } => (None, chunks.as_slice()),
        };
        blob.into_iter().chain(chunks.iter().map(|c| c.hash))
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkEntry {
    pub slot: u16,
    /// Region-header timestamp (restored as-is).
    pub ts: u32,
    /// Original compression byte (1 gzip, 2 zlib, 3 none, 4 lz4).
    pub comp: u8,
    /// Object holding the *decompressed* NBT — or, if `verbatim`, the original payload.
    pub hash: ObjectId,
    /// The chunk couldn't be decompressed, so its compressed bytes were kept as-is.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub verbatim: bool,
}

impl Snapshot {
    pub fn objects(&self) -> impl Iterator<Item = ObjectId> + '_ {
        self.files.iter().flat_map(|f| f.objects())
    }
}

/// Turn a manifest path (`region/r.0.0.mca`) into a relative `PathBuf`,
/// refusing anything that could escape the restore folder.
pub fn safe_rel_path(p: &str) -> Result<PathBuf> {
    let path = Path::new(p);
    if p.is_empty() || !path.components().all(|c| matches!(c, Component::Normal(_))) {
        bail!("refusing unsafe path in manifest: {p:?}");
    }
    Ok(path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unsafe_paths_rejected() {
        assert!(safe_rel_path("region/r.0.0.mca").is_ok());
        assert!(safe_rel_path("../etc/passwd").is_err());
        assert!(safe_rel_path("/etc/passwd").is_err());
        assert!(safe_rel_path("a/./b").is_ok()); // `.` components are normalised away
        assert!(safe_rel_path("").is_err());
    }
}
