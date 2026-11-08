use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use anyhow::{Context, Result, bail};
use chrono::Utc;
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::anvil;
use crate::chunk_store::ChunkStore;
use crate::commands::list::list;
use crate::hooks::check_interrupted;
use crate::manifest::{ChunkEntry, FORMAT_VERSION, FileEntry, Snapshot, SnapshotHeader, Stats, World};
use crate::repo::{ObjectId, Repo};
use crate::world::{self, FoundWorld, Kind, Scan, WorldFile};

pub struct BackupReport {
    pub id: ObjectId,
    /// Totals across all worlds.
    pub stats: Stats,
    /// The worlds that were backed up, in snapshot order.
    pub worlds: Vec<FoundWorld>,
    pub warnings: Vec<String>,
    /// Files that kept changing while being read; their stored copies may be torn.
    pub torn: u64,
}

#[derive(Clone, Debug, Default)]
pub struct Options {
    /// Which worlds (see [`world::select_worlds`]); empty = all.
    pub worlds: Vec<String>,
    /// Note to attach to the snapshot.
    pub message: Option<String>,
    /// Refuse to save a snapshot if any file kept changing while being read.
    pub strict: bool,
}

/// What backing up one file produced.
struct Done {
    entry: FileEntry,
    stats: Stats,
    warnings: Vec<String>,
    /// `.mcc` files read as part of this region (absolute paths).
    consumed: Vec<PathBuf>,
    /// The file never read the same twice: the stored copy may be torn.
    torn: bool,
}

/// Format 3: what's needed to store chunks as group objects.
struct Grouped<'a> {
    store: ChunkStore<'a>,
    /// (world, region file) → slot → the chunk's object in the latest snapshot of this source.
    prev: HashMap<(String, String), HashMap<u16, ObjectId>>,
}

impl Grouped<'_> {
    fn prev(&self, world: &str, path: &str, slot: u16) -> Option<&ObjectId> {
        self.prev.get(&(world.to_string(), path.to_string()))?.get(&slot)
    }
}

/// Read the latest snapshot of every source: its chunk objects can be reused unchecked,
/// and this source's tell each chunk where its previous version is.
fn grouped<'a>(repo: &'a Repo, source: &str, warnings: &mut Vec<String>) -> Result<Option<Grouped<'a>>> {
    if !repo.grouped() {
        return Ok(None);
    }
    let mut latest: HashMap<String, ObjectId> = HashMap::new();
    for (id, h) in list(repo)? {
        latest.entry(h.source).or_insert(id); // newest first
    }
    let mut trusted = HashSet::new();
    let mut prev = HashMap::new();
    for (src, id) in latest {
        let snap = match repo.load_snapshot(&id) {
            Ok(s) => s,
            Err(e) => {
                warnings.push(format!("can't read the latest snapshot of {src}: {e:#}"));
                continue;
            }
        };
        for w in snap.worlds {
            for f in w.files {
                let FileEntry::Region { path, chunks } = f else { continue };
                let live: Vec<&ChunkEntry> = chunks.iter().filter(|c| !c.verbatim).collect();
                trusted.extend(live.iter().map(|c| c.hash));
                if src == source {
                    prev.insert((w.name.clone(), path), live.iter().map(|c| (c.slot, c.hash)).collect());
                }
            }
        }
    }
    Ok(Some(Grouped { store: ChunkStore::new(repo, trusted), prev }))
}

/// Back up the worlds under `source` (or `source` itself, if it is a world) that
/// `opts.worlds` pick as one snapshot.
pub fn backup(repo: &Repo, source: &Path, opts: &Options, progress: &ProgressBar) -> Result<BackupReport> {
    let source_abs = source.canonicalize().with_context(|| format!("opening {}", source.display()))?;
    let found = world::find_worlds(&source_abs)?;
    let selected = world::select_worlds(&source_abs, &found, &opts.worlds)?;
    let repo_root = repo.root().canonicalize()?;
    for w in &selected {
        if repo_root.starts_with(&w.abs) {
            bail!("the repository is inside the world folder {}; put it somewhere else", w.abs.display());
        }
    }

    let scans: Vec<Scan> = selected.iter().map(|w| world::scan(&w.abs)).collect::<Result<_>>()?;
    progress.set_length(scans.iter().flat_map(|s| &s.files).map(|f| f.size).sum());

    let mut stats = Stats::default();
    let mut warnings = Vec::new();
    let g = grouped(repo, &source_abs.display().to_string(), &mut warnings)?;
    let mut torn = 0;
    let mut worlds = Vec::with_capacity(selected.len());
    for (w, scan) in selected.iter().zip(scans) {
        let (files, s) = backup_world(repo, g.as_ref(), scan, progress, &mut warnings, &mut torn, &w.name)?;
        add(&mut stats, &s);
        worlds.push(World { name: w.name.clone(), path: w.rel.clone(), stats: s, files });
    }

    // Objects first, durably; then the manifest. A crash (or interrupt) before this
    // point leaves only unreferenced objects, which `prune` cleans up.
    check_interrupted()?;
    if opts.strict && torn > 0 {
        bail!(
            "{torn} file(s) kept changing while being read, so this snapshot could be torn; \
             not saving it (--strict). Pause saving with a --pre hook"
        );
    }
    repo.flush()?;
    let snap = Snapshot {
        header: SnapshotHeader {
            version: FORMAT_VERSION,
            created: Utc::now(),
            message: opts.message.clone(),
            source: source_abs.display().to_string(),
            stats: stats.clone(),
            world_names: worlds.iter().map(|w| w.name.clone()).collect(),
        },
        worlds,
        files: vec![],
    };
    let id = repo.write_snapshot(&snap)?;
    Ok(BackupReport { id, stats, worlds: selected, warnings, torn })
}

fn add(total: &mut Stats, s: &Stats) {
    total.files += s.files;
    total.chunks += s.chunks;
    total.new_chunks += s.new_chunks;
    total.new_objects += s.new_objects;
    total.stored_bytes += s.stored_bytes;
    total.world_bytes += s.world_bytes;
}

/// Store one world's files. Warnings are prefixed with `<world>/<path>`.
fn backup_world(
    repo: &Repo,
    g: Option<&Grouped>,
    scan: Scan,
    progress: &ProgressBar,
    warnings: &mut Vec<String>,
    torn: &mut u64,
    name: &str,
) -> Result<(Vec<FileEntry>, Stats)> {
    warnings.extend(scan.warnings.into_iter().map(|w| format!("{name}: {w}")));
    let (externals, files): (Vec<WorldFile>, Vec<WorldFile>) =
        scan.files.into_iter().partition(|f| f.kind == Kind::External);

    // Region files and blobs, in parallel (each region also parallelises its chunks).
    let mut done: Vec<Done> = files
        .par_iter()
        .map(|f| {
            check_interrupted()?;
            let r = match f.kind {
                Kind::Region { coords } => backup_region(repo, g, name, f, coords),
                _ => backup_blob(repo, f),
            };
            progress.inc(f.size);
            r.with_context(|| format!("backing up {name}/{}", f.rel))
        })
        .collect::<Result<_>>()?;

    // .mcc files nobody referenced (orphans) are kept as plain blobs, just in case.
    let consumed: HashSet<&PathBuf> = done.iter().flat_map(|d| &d.consumed).collect();
    let orphans: Vec<&WorldFile> = externals.iter().filter(|f| !consumed.contains(&f.abs)).collect();
    let orphan_done: Vec<Done> = orphans
        .par_iter()
        .map(|f| {
            check_interrupted()?;
            backup_blob(repo, f).with_context(|| format!("backing up {name}/{}", f.rel))
        })
        .collect::<Result<_>>()?;
    progress.inc(externals.iter().map(|f| f.size).sum());
    done.extend(orphan_done);

    let mut stats = Stats::default();
    let mut entries = Vec::with_capacity(done.len());
    for d in done {
        add(&mut stats, &d.stats);
        *torn += d.torn as u64;
        warnings.extend(d.warnings.into_iter().map(|w| format!("{name}/{}: {w}", d.entry.path())));
        entries.push(d.entry);
    }
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    Ok((entries, stats))
}

/// How hard to try for a consistent read of a file the server may be writing.
pub const READ_ATTEMPTS: u32 = 5;
/// Wait before the n-th retry: n × this.
const RETRY_DELAY: Duration = Duration::from_millis(100);

/// One read of a file.
pub struct Attempt<T> {
    pub value: T,
    /// Size and mtime were the same before and after the read.
    pub unchanged: bool,
    /// The content looks damaged (a chunk that won't decompress, a header pointing
    /// past the end…): either really damaged, or caught mid-write.
    pub suspect: bool,
    /// Hash of the raw bytes read, to tell "stably damaged" from "still changing".
    pub fingerprint: ObjectId,
}

/// Read until the result can be trusted: the file didn't change during the read and
/// either looks fine or reads identically twice in a row (then it's damaged on disk,
/// not torn, and is kept as it is). Returns the last read, how many reads it took, and
/// whether it settled; one that never settled may be torn.
pub fn settle<T>(
    mut read: impl FnMut() -> Result<Attempt<T>>,
    attempts: u32,
    delay: Duration,
) -> Result<(T, u32, bool)> {
    let mut prev: Option<Attempt<T>> = None;
    for n in 1..=attempts {
        let a = read()?;
        let same_as_before = prev.as_ref().is_some_and(|p| p.unchanged && p.fingerprint == a.fingerprint);
        if a.unchanged && (!a.suspect || same_as_before) {
            return Ok((a.value, n, true));
        }
        prev = Some(a);
        if n < attempts {
            check_interrupted()?;
            std::thread::sleep(delay * n);
        }
    }
    Ok((prev.expect("attempts ≥ 1").value, attempts, false))
}

/// Read a file, noting whether its size or mtime changed meanwhile.
fn read_once(path: &Path) -> Result<(Vec<u8>, bool)> {
    let stamp = |p: &Path| -> Result<(Option<SystemTime>, u64)> {
        let m = fs::metadata(p)?;
        Ok((m.modified().ok(), m.len()))
    };
    let before = stamp(path)?;
    let data = fs::read(path)?;
    let unchanged = stamp(path)? == before && data.len() as u64 == before.1;
    Ok((data, unchanged))
}

fn torn_warning(attempts: u32) -> String {
    format!(
        "kept changing while being read ({attempts} tries); this copy may be torn. \
         Is the server still saving? Pause saving with a --pre hook"
    )
}

fn backup_blob(repo: &Repo, f: &WorldFile) -> Result<Done> {
    let (data, tries, settled) = settle(
        || {
            let (data, unchanged) = read_once(&f.abs)?;
            let fingerprint = ObjectId::of(&data);
            Ok(Attempt { value: data, unchanged, suspect: false, fingerprint })
        },
        READ_ATTEMPTS,
        RETRY_DELAY,
    )?;
    let warnings = if settled { vec![] } else { vec![torn_warning(tries)] };
    let mut done = blob_done(repo, f, &data, warnings)?;
    done.torn = !settled;
    Ok(done)
}

fn blob_done(repo: &Repo, f: &WorldFile, data: &[u8], warnings: Vec<String>) -> Result<Done> {
    let put = repo.put(data)?;
    let stats = Stats {
        files: 1,
        new_objects: put.stored.is_some() as u64,
        stored_bytes: put.stored.unwrap_or(0),
        world_bytes: data.len() as u64,
        ..Default::default()
    };
    let entry = FileEntry::Blob { path: f.rel.clone(), size: data.len() as u64, hash: put.id };
    Ok(Done { entry, stats, warnings, consumed: vec![], torn: false })
}

/// One read of a region file, with its chunks already stored. If the read turns out
/// not to be trustworthy and is retried, what it stored is merely unreferenced (and
/// `prune` removes it); the stats are taken from the read that was kept.
enum ReadRegion {
    /// Not a valid region file: stored whole later.
    Invalid { data: Vec<u8>, error: String },
    Valid {
        data_len: u64,
        parsed_warnings: Vec<String>,
        external_files: Vec<String>,
        chunks: Vec<(ChunkEntry, Option<String>)>,
    },
}

struct RegionAt<'a> {
    world: &'a str,
    rel: &'a str,
    path: &'a Path,
}

fn read_region(
    repo: &Repo,
    g: Option<&Grouped>,
    at: &RegionAt,
    coords: (i32, i32),
    stored: &mut HashMap<ObjectId, u64>,
) -> Result<Attempt<ReadRegion>> {
    let (data, unchanged) = read_once(at.path)?;
    let fingerprint = ObjectId::of(&data);
    let dir = at.path.parent().unwrap();
    let parsed = match anvil::parse_region(&data, Some(coords), |name| fs::read(dir.join(name))) {
        Ok(p) => p,
        Err(e) => {
            // Empty region files are normal (Minecraft creates them eagerly).
            let suspect = !data.is_empty();
            let value = ReadRegion::Invalid { data, error: e.to_string() };
            return Ok(Attempt { value, unchanged, suspect, fingerprint });
        }
    };
    let data_len = data.len() as u64;
    drop(data);

    let results: Vec<(ChunkEntry, Option<u64>, Option<String>)> = parsed
        .chunks
        .into_par_iter()
        .map(|c| -> Result<_> {
            let (bytes, verbatim, warn) = match anvil::decompress(c.compression, &c.payload) {
                Ok(nbt) => (nbt, false, None),
                Err(e) => {
                    let (x, z) = (c.slot % 32, c.slot / 32);
                    (c.payload, true, Some(format!("chunk ({x},{z}): {e}; kept its raw bytes")))
                }
            };
            let put = match g {
                Some(g) if !verbatim => g.store.put(&bytes, g.prev(at.world, at.rel, c.slot))?,
                _ => repo.put(&bytes)?,
            };
            let entry =
                ChunkEntry { slot: c.slot, ts: c.timestamp, comp: c.compression, hash: put.id, verbatim };
            Ok((entry, put.stored, warn))
        })
        .collect::<Result<_>>()?;

    let suspect = !parsed.warnings.is_empty() || results.iter().any(|(e, _, _)| e.verbatim);
    let chunks = results
        .into_iter()
        .map(|(entry, new, warn)| {
            if let Some(n) = new {
                stored.insert(entry.hash, n);
            }
            (entry, warn)
        })
        .collect();
    let value = ReadRegion::Valid {
        data_len,
        parsed_warnings: parsed.warnings,
        external_files: parsed.external_files,
        chunks,
    };
    Ok(Attempt { value, unchanged, suspect, fingerprint })
}

fn backup_region(
    repo: &Repo,
    g: Option<&Grouped>,
    world: &str,
    f: &WorldFile,
    coords: (i32, i32),
) -> Result<Done> {
    // Objects this file's reads added to the repo (any attempt), with their stored size.
    let mut stored = HashMap::new();
    let at = RegionAt { world, rel: &f.rel, path: &f.abs };
    let (read, tries, settled) =
        settle(|| read_region(repo, g, &at, coords, &mut stored), READ_ATTEMPTS, RETRY_DELAY)?;
    let mut warnings = Vec::new();
    if !settled {
        warnings.push(torn_warning(tries));
    }
    let (data_len, parsed_warnings, external_files, results) = match read {
        ReadRegion::Invalid { data, error } => {
            if !data.is_empty() {
                warnings.push(format!("not a valid region file ({error}); stored as a plain file"));
            }
            let mut done = blob_done(repo, f, &data, warnings)?;
            done.torn = !settled;
            return Ok(done);
        }
        ReadRegion::Valid { data_len, parsed_warnings, external_files, chunks } => {
            (data_len, parsed_warnings, external_files, chunks)
        }
    };
    warnings.extend(parsed_warnings);

    let mut stats = Stats { files: 1, world_bytes: data_len, ..Default::default() };
    let mut chunks = Vec::with_capacity(results.len());
    for (entry, warn) in results {
        stats.chunks += 1;
        // `remove`: two identical chunks in one region are one new object, counted once.
        if let Some(n) = stored.remove(&entry.hash) {
            stats.new_chunks += 1;
            stats.new_objects += 1;
            stats.stored_bytes += n;
        }
        warnings.extend(warn);
        chunks.push(entry);
    }
    let dir = f.abs.parent().unwrap();
    let consumed: Vec<PathBuf> = external_files.iter().map(|n| dir.join(n)).collect();
    for p in &consumed {
        stats.world_bytes += fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    }
    Ok(Done {
        entry: FileEntry::Region { path: f.rel.clone(), chunks },
        stats,
        warnings,
        consumed,
        torn: !settled,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Feed `settle` a scripted sequence of reads: (unchanged, suspect, fingerprint byte).
    fn run(script: &[(bool, bool, u8)]) -> (usize, u32, bool) {
        let mut i = 0;
        let (value, tries, settled) = settle(
            || {
                let (unchanged, suspect, fp) = script[i.min(script.len() - 1)];
                i += 1;
                Ok(Attempt { value: i, unchanged, suspect, fingerprint: ObjectId::of(&[fp]) })
            },
            READ_ATTEMPTS,
            Duration::ZERO,
        )
        .unwrap();
        (value, tries, settled)
    }

    #[test]
    fn clean_read_settles_at_once() {
        assert_eq!(run(&[(true, false, 1)]), (1, 1, true));
    }

    #[test]
    fn retries_while_the_file_changes() {
        assert_eq!(run(&[(false, false, 1), (false, false, 2), (true, false, 3)]), (3, 3, true));
    }

    #[test]
    fn damaged_but_stable_settles_on_the_second_identical_read() {
        assert_eq!(run(&[(true, true, 7), (true, true, 7)]), (2, 2, true));
    }

    #[test]
    fn damage_that_goes_away_was_a_torn_read() {
        assert_eq!(run(&[(true, true, 7), (true, false, 8)]), (2, 2, true));
    }

    #[test]
    fn damage_that_keeps_changing_never_settles() {
        let script: Vec<_> = (0..10).map(|n| (true, true, n)).collect();
        assert_eq!(run(&script), (5, 5, false));
    }

    #[test]
    fn a_file_that_never_stops_changing_never_settles() {
        assert_eq!(run(&[(false, false, 1)]), (5, 5, false));
        // A changed read never counts as the "same" read for a damaged file either.
        assert_eq!(run(&[(false, true, 1), (true, true, 1), (true, true, 1)]), (3, 3, true));
    }
}
