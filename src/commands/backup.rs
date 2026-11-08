use std::collections::HashSet;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::SystemTime;

use anyhow::{Context, Result, bail};
use chrono::Utc;
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::anvil;
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
}

/// What backing up one file produced.
struct Done {
    entry: FileEntry,
    stats: Stats,
    warnings: Vec<String>,
    /// `.mcc` files read as part of this region (absolute paths).
    consumed: Vec<PathBuf>,
}

/// Back up the worlds under `source` (or `source` itself, if it is a world) that
/// `selectors` pick (see [`world::select_worlds`]; empty = all) as one snapshot.
pub fn backup(
    repo: &Repo,
    source: &Path,
    selectors: &[String],
    message: Option<String>,
    progress: &ProgressBar,
) -> Result<BackupReport> {
    let source_abs = source.canonicalize().with_context(|| format!("opening {}", source.display()))?;
    let found = world::find_worlds(&source_abs)?;
    let selected = world::select_worlds(&source_abs, &found, selectors)?;
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
    let mut worlds = Vec::with_capacity(selected.len());
    for (w, scan) in selected.iter().zip(scans) {
        let (files, s) = backup_world(repo, scan, progress, &mut warnings, &w.name)?;
        add(&mut stats, &s);
        worlds.push(World { name: w.name.clone(), path: w.rel.clone(), stats: s, files });
    }

    // Objects first, durably; then the manifest. A crash before this point leaves
    // only unreferenced objects, which `prune` cleans up.
    repo.flush()?;
    let snap = Snapshot {
        header: SnapshotHeader {
            version: FORMAT_VERSION,
            created: Utc::now(),
            message,
            source: source_abs.display().to_string(),
            stats: stats.clone(),
            world_names: worlds.iter().map(|w| w.name.clone()).collect(),
        },
        worlds,
        files: vec![],
    };
    let id = repo.write_snapshot(&snap)?;
    Ok(BackupReport { id, stats, worlds: selected, warnings })
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
    scan: Scan,
    progress: &ProgressBar,
    warnings: &mut Vec<String>,
    name: &str,
) -> Result<(Vec<FileEntry>, Stats)> {
    warnings.extend(scan.warnings.into_iter().map(|w| format!("{name}: {w}")));
    let (externals, files): (Vec<WorldFile>, Vec<WorldFile>) =
        scan.files.into_iter().partition(|f| f.kind == Kind::External);

    // Region files and blobs, in parallel (each region also parallelises its chunks).
    let mut done: Vec<Done> = files
        .par_iter()
        .map(|f| {
            let r = match f.kind {
                Kind::Region { coords } => backup_region(repo, f, coords),
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
        .map(|f| backup_blob(repo, f).with_context(|| format!("backing up {name}/{}", f.rel)))
        .collect::<Result<_>>()?;
    progress.inc(externals.iter().map(|f| f.size).sum());
    done.extend(orphan_done);

    let mut stats = Stats::default();
    let mut entries = Vec::with_capacity(done.len());
    for d in done {
        add(&mut stats, &d.stats);
        warnings.extend(d.warnings.into_iter().map(|w| format!("{name}/{}: {w}", d.entry.path())));
        entries.push(d.entry);
    }
    entries.sort_by(|a, b| a.path().cmp(b.path()));
    Ok((entries, stats))
}

/// Read a file and report whether it changed while we were reading it
/// (a sign the server is still writing — torn data).
fn read_checked(path: &Path, warnings: &mut Vec<String>) -> Result<Vec<u8>> {
    let stamp = |p: &Path| -> Result<(Option<SystemTime>, u64)> {
        let m = fs::metadata(p)?;
        Ok((m.modified().ok(), m.len()))
    };
    let before = stamp(path)?;
    let data = fs::read(path)?;
    if stamp(path)? != before || data.len() as u64 != before.1 {
        warnings.push("changed while being read — is the server still saving? (run `save-off` first)".into());
    }
    Ok(data)
}

fn backup_blob(repo: &Repo, f: &WorldFile) -> Result<Done> {
    let mut warnings = Vec::new();
    let data = read_checked(&f.abs, &mut warnings)?;
    blob_done(repo, f, &data, warnings)
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
    Ok(Done { entry, stats, warnings, consumed: vec![] })
}

fn backup_region(repo: &Repo, f: &WorldFile, coords: (i32, i32)) -> Result<Done> {
    let mut warnings = Vec::new();
    let data = read_checked(&f.abs, &mut warnings)?;
    let dir = f.abs.parent().unwrap();

    let parsed = match anvil::parse_region(&data, Some(coords), |name| fs::read(dir.join(name))) {
        Ok(p) => p,
        Err(e) => {
            // Empty region files are normal (Minecraft creates them eagerly).
            if !data.is_empty() {
                warnings.push(format!("not a valid region file ({e}); stored as a plain file"));
            }
            return blob_done(repo, f, &data, warnings);
        }
    };
    warnings.extend(parsed.warnings);

    let results: Vec<(ChunkEntry, Option<u64>, Option<String>)> = parsed
        .chunks
        .par_iter()
        .map(|c| -> Result<_> {
            let (bytes, verbatim, warn) = match anvil::decompress(c.compression, &c.payload) {
                Ok(nbt) => (nbt, false, None),
                Err(e) => {
                    let (x, z) = (c.slot % 32, c.slot / 32);
                    (c.payload.clone(), true, Some(format!("chunk ({x},{z}): {e}; kept its raw bytes")))
                }
            };
            let put = repo.put(&bytes)?;
            let entry =
                ChunkEntry { slot: c.slot, ts: c.timestamp, comp: c.compression, hash: put.id, verbatim };
            Ok((entry, put.stored, warn))
        })
        .collect::<Result<_>>()?;

    let mut stats = Stats { files: 1, world_bytes: data.len() as u64, ..Default::default() };
    let mut chunks = Vec::with_capacity(results.len());
    for (entry, stored, warn) in results {
        stats.chunks += 1;
        if let Some(n) = stored {
            stats.new_chunks += 1;
            stats.new_objects += 1;
            stats.stored_bytes += n;
        }
        warnings.extend(warn);
        chunks.push(entry);
    }
    let consumed: Vec<PathBuf> = parsed.external_files.iter().map(|n| dir.join(n)).collect();
    for p in &consumed {
        stats.world_bytes += fs::metadata(p).map(|m| m.len()).unwrap_or(0);
    }
    Ok(Done { entry: FileEntry::Region { path: f.rel.clone(), chunks }, stats, warnings, consumed })
}
