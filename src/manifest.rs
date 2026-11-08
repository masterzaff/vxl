//! The snapshot manifest: the worlds in a snapshot, every file in each world and,
//! for region files, every chunk and the object that holds it. Stored as
//! zstd-compressed JSON in `snapshots/<id>.json.zst`, where `id` is the BLAKE3 of
//! the JSON.
//!
//! Version 1 had a single world's file list at the top level; it is read as a
//! snapshot with one world named after its source folder (see [`Snapshot::upgrade`]).
//! Version 3 (repository format 3) may store a region file's chunk list as its own
//! object, a *table* (see [`encode_table`]): a region whose chunks didn't change gives the
//! same table, so it isn't stored again. [`crate::repo::Repo::load_snapshot`] fills the
//! chunk lists back in, so code using a loaded snapshot never sees the difference.

use std::path::{Component, Path, PathBuf};

use anyhow::{Result, bail, ensure};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use crate::repo::ObjectId;

/// 2: a snapshot holds several worlds (`worlds`), each with its own file list.
/// 3: region chunk lists may be stored as table objects.
pub const FORMAT_VERSION: u32 = 3;

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
    /// The folder that was backed up: a world, or a server folder holding worlds.
    pub source: String,
    /// Totals across all worlds.
    pub stats: Stats,
    /// Names of the worlds in this snapshot, in manifest order.
    #[serde(default)]
    pub world_names: Vec<String>,
}

impl SnapshotHeader {
    /// Fill in what version 1 didn't record.
    pub fn upgrade(&mut self) {
        if self.version < 2 && self.world_names.is_empty() {
            self.world_names.push(v1_world_name(&self.source));
        }
    }
}

/// A version-1 snapshot is one world; name it after the folder it came from.
fn v1_world_name(source: &str) -> String {
    Path::new(source)
        .file_name()
        .and_then(|n| n.to_str())
        .filter(|n| is_valid_world_name(n))
        .unwrap_or("world")
        .to_string()
}

/// A world name becomes a folder name on restore, so it must be one plain path component.
pub fn is_valid_world_name(name: &str) -> bool {
    !name.is_empty()
        && name != "."
        && name != ".."
        && !name.contains(['/', '\\'])
        && safe_rel_path(name).is_ok()
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    #[serde(flatten)]
    pub header: SnapshotHeader,
    /// Kept last in the JSON so the header is cheap to parse.
    #[serde(default)]
    pub worlds: Vec<World>,
    /// Version 1 only: the single world's files. Moved into `worlds` by [`Snapshot::upgrade`].
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<FileEntry>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct World {
    /// Folder name of the world; what it's restored as and what `--world` selects.
    pub name: String,
    /// Where it was found, relative to the snapshot's source (`.` if the source was this world).
    pub path: String,
    pub stats: Stats,
    /// Paths are relative to the world folder, exactly like a single-world backup.
    pub files: Vec<FileEntry>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum FileEntry {
    /// A file stored whole (level.dat, playerdata/…, broken region files).
    Blob { path: String, size: u64, hash: ObjectId },
    /// A region file stored chunk by chunk. With `table`, `chunks` is stored in that
    /// object rather than in the manifest (and is empty in the stored JSON).
    Region {
        path: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        table: Option<ObjectId>,
        chunks: Vec<ChunkEntry>,
    },
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
            FileEntry::Region { table, chunks, .. } => (*table, chunks.as_slice()),
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
        self.worlds.iter().flat_map(|w| &w.files).flat_map(|f| f.objects())
    }

    /// Bring an older manifest into the current shape (in memory only; the stored
    /// bytes, and so the snapshot id, never change).
    pub fn upgrade(&mut self) {
        self.header.upgrade();
        if self.header.version < 2 && self.worlds.is_empty() {
            self.worlds.push(World {
                name: self.header.world_names[0].clone(),
                path: ".".into(),
                stats: self.header.stats.clone(),
                files: std::mem::take(&mut self.files),
            });
        }
    }

    pub fn world(&self, name: &str) -> Result<&World> {
        self.worlds.iter().find(|w| w.name == name).ok_or_else(|| {
            let names: Vec<&str> = self.worlds.iter().map(|w| w.name.as_str()).collect();
            anyhow::anyhow!("this snapshot has no world {name:?} (it has: {})", names.join(", "))
        })
    }
}

const TABLE_MAGIC: &[u8; 4] = b"VXT\x01";
const TABLE_ENTRY: usize = 2 + 4 + 1 + 1 + 32;

/// A region's chunk list as stored in a table object:
/// `"VXT\x01" | count:u32 | count × (slot:u16 ts:u32 comp:u8 flags:u8 hash[32])` (little-endian;
/// flag 1 = verbatim).
pub fn encode_table(chunks: &[ChunkEntry]) -> Vec<u8> {
    let mut b = Vec::with_capacity(8 + chunks.len() * TABLE_ENTRY);
    b.extend_from_slice(TABLE_MAGIC);
    b.extend_from_slice(&(chunks.len() as u32).to_le_bytes());
    for c in chunks {
        b.extend_from_slice(&c.slot.to_le_bytes());
        b.extend_from_slice(&c.ts.to_le_bytes());
        b.push(c.comp);
        b.push(c.verbatim as u8);
        b.extend_from_slice(&c.hash.0);
    }
    b
}

pub fn decode_table(b: &[u8]) -> Result<Vec<ChunkEntry>> {
    let rest = b.strip_prefix(TABLE_MAGIC).ok_or_else(|| anyhow::anyhow!("not a chunk table"))?;
    ensure!(rest.len() >= 4, "truncated chunk table");
    let n = u32::from_le_bytes(rest[..4].try_into().unwrap()) as usize;
    let rest = &rest[4..];
    ensure!(n.checked_mul(TABLE_ENTRY) == Some(rest.len()), "chunk table has the wrong length");
    rest.as_chunks::<TABLE_ENTRY>()
        .0
        .iter()
        .map(|e| {
            ensure!(e[7] <= 1, "bad chunk table flags");
            Ok(ChunkEntry {
                slot: u16::from_le_bytes([e[0], e[1]]),
                ts: u32::from_le_bytes(e[2..6].try_into().unwrap()),
                comp: e[6],
                verbatim: e[7] == 1,
                hash: ObjectId(e[8..].try_into().unwrap()),
            })
        })
        .collect()
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

    #[test]
    fn world_names() {
        assert!(is_valid_world_name("world_nether"));
        assert!(!is_valid_world_name("a/b"));
        assert!(!is_valid_world_name(".."));
        assert!(!is_valid_world_name("."));
        assert!(!is_valid_world_name(""));
        assert_eq!(v1_world_name("/srv/mc/survival"), "survival");
        assert_eq!(v1_world_name("/"), "world");
    }

    #[test]
    fn tables_roundtrip_and_refuse_damage() {
        let chunks: Vec<ChunkEntry> = (0..5u16)
            .map(|i| ChunkEntry {
                slot: i * 100,
                ts: 1_700_000_000 + i as u32,
                comp: 2,
                hash: ObjectId::of(&[i as u8]),
                verbatim: i == 3,
            })
            .collect();
        let t = encode_table(&chunks);
        assert_eq!(decode_table(&t).unwrap(), chunks);
        assert_eq!(decode_table(&encode_table(&[])).unwrap(), []);
        assert!(decode_table(&t[..t.len() - 1]).is_err());
        assert!(decode_table(b"VXT\x01\xff\xff\xff\xff").is_err());
        let mut bad = t.clone();
        bad[8 + 7] = 2;
        assert!(decode_table(&bad).is_err());
    }

    #[test]
    fn v1_manifest_reads_as_one_world() {
        let json = r#"{"version":1,"created":"2026-11-08T08:00:00Z","message":null,
            "source":"/srv/mc/world","stats":{"files":1,"chunks":0,"new_chunks":0,
            "new_objects":1,"stored_bytes":10,"world_bytes":10},
            "files":[{"kind":"blob","path":"level.dat","size":10,
            "hash":"0000000000000000000000000000000000000000000000000000000000000000"}]}"#;
        let mut snap: Snapshot = serde_json::from_str(json).unwrap();
        snap.upgrade();
        assert_eq!(snap.header.world_names, ["world"]);
        assert_eq!(snap.worlds.len(), 1);
        assert_eq!(snap.worlds[0].path, ".");
        assert_eq!(snap.worlds[0].files.len(), 1);
        assert!(snap.files.is_empty());
        let mut h: SnapshotHeader = serde_json::from_str(json).unwrap();
        h.upgrade();
        assert_eq!(h.world_names, ["world"]);
    }
}
