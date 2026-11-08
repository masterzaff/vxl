//! `vxl migrate`: convert a format-2 repository to format 3, so its chunks are stored as
//! group objects (`group.rs`). Snapshots, their ids and their contents don't change.
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

use std::collections::HashSet;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};

use anyhow::{Context, Result, bail};
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::chunk_store::ChunkStore;
use crate::commands::list::list;
use crate::commands::prune::with_deps;
use crate::hooks::check_interrupted;
use crate::manifest::FileEntry;
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
}

/// Needs the exclusive lock.
pub fn migrate(repo: &mut Repo, progress: &ProgressBar) -> Result<MigrateReport> {
    let mut report =
        MigrateReport { bytes_before: repo.packs().iter().map(|p| p.size).sum(), ..Default::default() };
    repo.upgrade_to_grouped()?;
    let old_packs = repo.packs();

    let mut snaps = list(repo)?;
    snaps.reverse(); // oldest first
    report.snapshots = snaps.len();

    // Every object the snapshots use, and the chunks in the order to convert them.
    let mut live = HashSet::new();
    let mut seen = HashSet::new();
    let mut batches: Vec<Vec<ObjectId>> = Vec::new();
    for (id, _) in &snaps {
        let snap = repo.load_snapshot(id).context("refusing to migrate")?;
        live.extend(snap.objects());
        let mut batch = Vec::new();
        for f in snap.worlds.iter().flat_map(|w| &w.files) {
            if let FileEntry::Region { chunks, .. } = f {
                batch.extend(chunks.iter().filter(|c| !c.verbatim && seen.insert(c.hash)).map(|c| c.hash));
            }
        }
        batches.push(batch);
    }
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
