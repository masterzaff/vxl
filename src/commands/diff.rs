//! `vxl diff`: what changed between two snapshots, by world and dimension, without
//! restoring anything. Chunks compare by content (their object hash), so a chunk
//! that was only re-saved with a new timestamp doesn't count as changed.

use std::collections::BTreeMap;

use crate::manifest::{ChunkEntry, FileEntry, Snapshot, World};
use crate::repo::ObjectId;
pub use crate::world::dimension;

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Counts {
    pub changed: u64,
    pub added: u64,
    pub removed: u64,
}

impl Counts {
    pub fn is_zero(&self) -> bool {
        self.changed == 0 && self.added == 0 && self.removed == 0
    }

    fn add(&mut self, o: &Counts) {
        self.changed += o.changed;
        self.added += o.added;
        self.removed += o.removed;
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Change {
    Added,
    Removed,
    Changed,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    pub path: String,
    pub change: Change,
    /// For region files: chunk-level changes (all of a removed/added region's chunks
    /// count as removed/added).
    pub chunks: Option<Counts>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WorldDiff {
    pub name: String,
    /// `None`: in both snapshots.
    pub only_in: Option<Side>,
    /// Chunk changes per dimension (`overworld`, `the_nether`, `the_end`, `mod:name`),
    /// over its region, entities and poi files.
    pub dimensions: BTreeMap<String, Counts>,
    /// Every file that differs, sorted by path.
    pub files: Vec<FileChange>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Side {
    A,
    B,
}

impl WorldDiff {
    pub fn chunks(&self) -> Counts {
        let mut c = Counts::default();
        self.dimensions.values().for_each(|d| c.add(d));
        c
    }

    pub fn other_files(&self) -> usize {
        self.files.iter().filter(|f| f.chunks.is_none()).count()
    }

    pub fn is_same(&self) -> bool {
        self.only_in.is_none() && self.files.is_empty()
    }
}

/// Compare snapshot `a` (older, usually) with `b`. Worlds are matched by name.
pub fn diff(a: &Snapshot, b: &Snapshot) -> Vec<WorldDiff> {
    let mut names: Vec<&str> = a.worlds.iter().chain(&b.worlds).map(|w| w.name.as_str()).collect();
    names.sort();
    names.dedup();
    names
        .into_iter()
        .map(|name| {
            let wa = a.worlds.iter().find(|w| w.name == name);
            let wb = b.worlds.iter().find(|w| w.name == name);
            let only_in = match (wa, wb) {
                (Some(_), None) => Some(Side::A),
                (None, Some(_)) => Some(Side::B),
                _ => None,
            };
            diff_world(name, wa, wb, only_in)
        })
        .collect()
}

fn diff_world(name: &str, a: Option<&World>, b: Option<&World>, only_in: Option<Side>) -> WorldDiff {
    let (fa, fb) = (files_of(a), files_of(b));
    let mut paths: Vec<&str> = fa.keys().chain(fb.keys()).copied().collect();
    paths.sort();
    paths.dedup();

    let mut out = WorldDiff { name: name.into(), only_in, dimensions: BTreeMap::new(), files: Vec::new() };
    for path in paths {
        let (ea, eb) = (fa.get(path).copied(), fb.get(path).copied());
        let change = match (ea, eb) {
            (Some(_), None) => Change::Removed,
            (None, Some(_)) => Change::Added,
            (Some(x), Some(y)) if x == y => continue,
            _ => Change::Changed,
        };
        let chunks = chunk_counts(ea, eb);
        if let Some(c) = &chunks {
            if change == Change::Changed && c.is_zero() {
                continue; // only timestamps or chunk order differ
            }
            out.dimensions.entry(dimension(path)).or_default().add(c);
        }
        out.files.push(FileChange { path: path.into(), change, chunks });
    }
    out
}

fn files_of(w: Option<&World>) -> BTreeMap<&str, &FileEntry> {
    w.map(|w| w.files.iter().map(|f| (f.path(), f)).collect()).unwrap_or_default()
}

/// Chunk-level changes if either side is a region stored chunk by chunk.
fn chunk_counts(a: Option<&FileEntry>, b: Option<&FileEntry>) -> Option<Counts> {
    let chunks = |e: Option<&FileEntry>| -> Option<BTreeMap<u16, ObjectId>> {
        match e {
            Some(FileEntry::Region { chunks, .. }) => {
                Some(chunks.iter().map(|c: &ChunkEntry| (c.slot, c.hash)).collect())
            }
            _ => None,
        }
    };
    let (ca, cb) = (chunks(a), chunks(b));
    if ca.is_none() && cb.is_none() {
        return None;
    }
    let (ca, cb) = (ca.unwrap_or_default(), cb.unwrap_or_default());
    let mut c = Counts::default();
    for (slot, h) in &ca {
        match cb.get(slot) {
            None => c.removed += 1,
            Some(h2) if h2 != h => c.changed += 1,
            _ => {}
        }
    }
    c.added = cb.keys().filter(|s| !ca.contains_key(s)).count() as u64;
    Some(c)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::{SnapshotHeader, Stats};
    use chrono::Utc;

    fn h(n: u8) -> ObjectId {
        ObjectId::of(&[n])
    }

    fn region(path: &str, chunks: &[(u16, u8)]) -> FileEntry {
        FileEntry::Region {
            path: path.into(),
            chunks: chunks
                .iter()
                .map(|&(slot, n)| ChunkEntry { slot, ts: n as u32, comp: 2, hash: h(n), verbatim: false })
                .collect(),
        }
    }

    fn blob(path: &str, n: u8) -> FileEntry {
        FileEntry::Blob { path: path.into(), size: 1, hash: h(n) }
    }

    fn snap(worlds: Vec<(&str, Vec<FileEntry>)>) -> Snapshot {
        Snapshot {
            header: SnapshotHeader {
                version: 2,
                created: Utc::now(),
                message: None,
                source: "/s".into(),
                stats: Stats::default(),
                world_names: worlds.iter().map(|w| w.0.to_string()).collect(),
            },
            worlds: worlds
                .into_iter()
                .map(|(name, files)| World {
                    name: name.into(),
                    path: name.into(),
                    stats: Stats::default(),
                    files,
                })
                .collect(),
            files: vec![],
        }
    }

    #[test]
    fn compares_chunks_files_and_worlds() {
        let a = snap(vec![
            ("world", vec![blob("level.dat", 1), region("region/r.0.0.mca", &[(0, 10), (1, 11), (2, 12)])]),
            ("old", vec![blob("level.dat", 5)]),
        ]);
        let b = snap(vec![
            (
                "world",
                vec![
                    blob("level.dat", 2),
                    region("region/r.0.0.mca", &[(0, 10), (1, 99), (3, 13)]),
                    region("DIM-1/region/r.1.1.mca", &[(0, 20)]),
                ],
            ),
            ("new", vec![blob("level.dat", 6)]),
        ]);
        let d = diff(&a, &b);
        assert_eq!(
            d.iter().map(|w| (w.name.as_str(), w.only_in)).collect::<Vec<_>>(),
            [("new", Some(Side::B)), ("old", Some(Side::A)), ("world", None)]
        );
        let w = &d[2];
        assert_eq!(w.dimensions["overworld"], Counts { changed: 1, added: 1, removed: 1 });
        assert_eq!(w.dimensions["the_nether"], Counts { changed: 0, added: 1, removed: 0 });
        assert_eq!(w.chunks(), Counts { changed: 1, added: 2, removed: 1 });
        assert_eq!(w.other_files(), 1);
        assert_eq!(w.files[0].path, "DIM-1/region/r.1.1.mca");
        assert_eq!(w.files[0].change, Change::Added);
    }

    #[test]
    fn same_content_is_no_change() {
        let a = snap(vec![("world", vec![region("region/r.0.0.mca", &[(0, 10)])])]);
        let mut b = a.clone();
        // Re-saved with a new timestamp only.
        if let FileEntry::Region { chunks, .. } = &mut b.worlds[0].files[0] {
            chunks[0].ts = 12345;
        }
        let d = diff(&a, &b);
        assert!(d[0].is_same(), "{d:?}");
    }
}
