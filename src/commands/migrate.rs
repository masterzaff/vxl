//! `vxl migrate`: convert a repository between formats 2 and 3.
//!
//! **To 3**: chunks get stored as group objects (`group.rs`). Snapshots, their ids and
//! their contents don't change (existing manifests keep their inline chunk lists; new
//! snapshots store them as tables).
//!
//! 1. The repository is marked format 3 (older vxl versions then refuse to open it).
//! 2. Every chunk the snapshots use gets a group copy next to its plain one, oldest
//!    snapshot first, so later versions of a chunk point at the sections of earlier ones.
//! 3. Every chunk is decoded from its group copy and hash-checked.
//! 4. Only then are the packs that were there before rewritten without the plain copies
//!    (and without objects no snapshot uses).
//!
//! An interruption at any point leaves a valid repository: until step 4 every plain copy
//! is still there, and reads try every copy. Running it again picks up where it stopped.
//!
//! **To 2** (the way back), the same steps in reverse: every chunk gets a plain copy, each
//! is checked, manifests with chunk tables are rewritten with their chunk lists inline
//! (those snapshots get new ids, since an id is the hash of the manifest; pins move
//! along), then packs are rewritten without group objects and tables, and only when none
//! are left is the repo marked format 2. Until that last step it's a valid format-3 repo,
//! and it can be rerun.

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use anyhow::{Context, Result, bail};
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::chunk_store::ChunkStore;
use crate::commands::list::list;
use crate::commands::prune::with_deps;
use crate::hooks::check_interrupted;
use crate::manifest::{FileEntry, Snapshot};
use crate::repo::{ObjectId, Repo};

#[derive(Debug, Default)]
pub struct MigrateReport {
    pub snapshots: usize,
    /// Distinct chunks the snapshots use.
    pub chunks: u64,
    /// Chunks given a group copy by this run.
    pub converted: u64,
    /// Chunks that stay plain (not chunk-shaped NBT), or already had a group copy.
    pub unchanged: u64,
    /// Total size of the packs before and after.
    pub bytes_before: u64,
    pub bytes_after: u64,
    /// To 2: snapshots rewritten with their chunk lists inline, (old id, new id).
    pub renamed: Vec<(ObjectId, ObjectId)>,
}

/// Convert to format `to` (2 or 3). Needs the exclusive lock.
pub fn migrate(repo: &mut Repo, to: u32, progress: &ProgressBar) -> Result<MigrateReport> {
    match to {
        3 => to_grouped(repo, progress),
        2 => to_plain(repo, progress),
        _ => bail!("no repository format {to}"),
    }
}

/// The distinct chunks the snapshots (oldest first) use, in first-use order, per snapshot.
fn chunk_batches(snaps: &[Snapshot]) -> (Vec<Vec<ObjectId>>, HashSet<ObjectId>) {
    let mut seen = HashSet::new();
    let batches = snaps
        .iter()
        .map(|snap| {
            let mut batch = Vec::new();
            for f in snap.worlds.iter().flat_map(|w| &w.files) {
                if let FileEntry::Region { chunks, .. } = f {
                    batch
                        .extend(chunks.iter().filter(|c| !c.verbatim && seen.insert(c.hash)).map(|c| c.hash));
                }
            }
            batch
        })
        .collect();
    (batches, seen)
}

fn load_all(repo: &Repo) -> Result<Vec<(ObjectId, Snapshot)>> {
    let mut snaps = list(repo)?;
    snaps.reverse(); // oldest first
    snaps
        .into_iter()
        .map(|(id, _)| Ok((id, repo.load_snapshot(&id).context("refusing to migrate")?)))
        .collect()
}

fn to_grouped(repo: &mut Repo, progress: &ProgressBar) -> Result<MigrateReport> {
    let mut report =
        MigrateReport { bytes_before: repo.packs().iter().map(|p| p.size).sum(), ..Default::default() };
    repo.set_format(3)?;
    let old_packs = repo.packs();

    let snaps: Vec<Snapshot> = load_all(repo)?.into_iter().map(|(_, s)| s).collect();
    report.snapshots = snaps.len();
    // Every object the snapshots use, and the chunks in the order to convert them.
    let live: HashSet<ObjectId> = snaps.iter().flat_map(|s| s.objects()).collect();
    let (batches, seen) = chunk_batches(&snaps);
    drop(snaps);
    report.chunks = seen.len() as u64;
    progress.set_length(report.chunks);

    let converted = AtomicU64::new(0);
    {
        let repo = &*repo;
        let store = ChunkStore::new(repo, HashSet::new());
        for batch in &batches {
            batch.par_iter().try_for_each(|id| -> Result<()> {
                check_interrupted()?;
                let nbt = repo.get(id).with_context(|| format!("reading chunk {id}"))?;
                if store.convert(&nbt)?.stored.is_some() {
                    converted.fetch_add(1, Relaxed);
                }
                progress.inc(1);
                Ok(())
            })?;
        }
    }
    repo.flush()?;
    report.converted = converted.into_inner();
    report.unchanged = report.chunks - report.converted;

    // Nothing has been removed yet. Check every chunk decodes from its group copy (or,
    // for the few that stay plain, from its plain one) before anything is.
    progress.set_position(0);
    let bad: Vec<String> = seen
        .par_iter()
        .filter_map(|id| {
            progress.inc(1);
            let r = repo.get_group(id).or_else(|_| repo.get(id));
            r.err().map(|e| format!("{e:#}"))
        })
        .collect();
    if let Some(e) = bad.first() {
        bail!("{} chunks don't decode after converting, nothing removed (first: {e})", bad.len());
    }

    check_interrupted()?;
    let live = with_deps(repo, live)?;
    repo.repack(&old_packs, &live)?;
    repo.clean_tmp()?;
    repo.reload_index()?;
    report.bytes_after = repo.packs().iter().map(|p| p.size).sum();
    Ok(report)
}

fn to_plain(repo: &mut Repo, progress: &ProgressBar) -> Result<MigrateReport> {
    let mut report =
        MigrateReport { bytes_before: repo.packs().iter().map(|p| p.size).sum(), ..Default::default() };
    if !repo.grouped() {
        report.bytes_after = report.bytes_before;
        return Ok(report);
    }
    let snaps = load_all(repo)?;
    report.snapshots = snaps.len();
    let (_, chunks) = chunk_batches(&snaps.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>());
    report.chunks = chunks.len() as u64;
    progress.set_length(report.chunks);

    // 1. A plain copy of every chunk that has none.
    let converted = AtomicU64::new(0);
    chunks.par_iter().try_for_each(|id| -> Result<()> {
        check_interrupted()?;
        if repo.get_plain(id).is_err() {
            let nbt = repo.get(id).with_context(|| format!("reading chunk {id}"))?;
            let packed = zstd::bulk::compress(&nbt, repo.config.zstd_level)?;
            if repo.put_stored(*id, &packed, true)?.stored.is_some() {
                converted.fetch_add(1, Relaxed);
            }
        }
        progress.inc(1);
        Ok(())
    })?;
    repo.flush()?;
    report.converted = converted.into_inner();
    report.unchanged = report.chunks - report.converted;

    // 2. Check every one before anything goes.
    progress.set_position(0);
    let bad: Vec<String> = chunks
        .par_iter()
        .filter_map(|id| {
            progress.inc(1);
            repo.get_plain(id).err().map(|e| format!("{e:#}"))
        })
        .collect();
    if let Some(e) = bad.first() {
        bail!("{} chunks have no good plain copy, nothing removed (first: {e})", bad.len());
    }

    // 3. Manifests with chunk tables get their chunk lists back inline.
    check_interrupted()?;
    let mut live = HashSet::new();
    let pins = repo.pins()?;
    for (id, mut snap) in snaps {
        let tabled = snap
            .worlds
            .iter()
            .flat_map(|w| &w.files)
            .any(|f| matches!(f, FileEntry::Region { table: Some(_), .. }));
        if tabled || snap.header.version > 2 {
            for f in snap.worlds.iter_mut().flat_map(|w| &mut w.files) {
                if let FileEntry::Region { table, .. } = f {
                    *table = None;
                }
            }
            snap.header.version = 2;
            let new = repo.write_snapshot(&snap)?;
            if new != id {
                // The pin moves with the snapshot (before the old one, and its pin, go).
                if let Some(note) = pins.get(&id) {
                    repo.pin(&new, note)?;
                }
                repo.delete_snapshot(&id)?;
                report.renamed.push((id, new));
            }
        }
        live.extend(snap.objects());
    }

    // 4. Packs without group objects or tables; 5. format 2 once none are left.
    check_interrupted()?;
    let packs = repo.packs();
    repo.repack_with(&packs, &live, true)?;
    repo.clean_tmp()?;
    repo.reload_index()?;
    if repo.has_group_objects()? {
        bail!("some group objects are still there, so the repository stays format 3; run it again");
    }
    repo.set_format(2)?;
    report.bytes_after = repo.packs().iter().map(|p| p.size).sum();
    Ok(report)
}
