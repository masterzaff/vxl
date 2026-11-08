use std::collections::HashSet;

use anyhow::{Context, Result};
use rayon::prelude::*;

use crate::commands::list::list;
use crate::repo::{ObjectId, Repo};

#[derive(Debug, Default)]
pub struct PruneReport {
    pub removed_snapshots: Vec<ObjectId>,
    pub kept_snapshots: usize,
    pub deleted_objects: u64,
    pub freed_bytes: u64,
}

/// Drop all but the newest `keep_last` snapshots (all kept if `None`), then
/// delete every object no remaining snapshot references. Needs the exclusive lock.
pub fn prune(repo: &Repo, keep_last: Option<usize>, dry_run: bool) -> Result<PruneReport> {
    let snaps = list(repo)?; // newest first
    let keep = keep_last.unwrap_or(usize::MAX).min(snaps.len());
    let (kept, removed) = snaps.split_at(keep);

    // Mark. If any kept manifest can't be read we bail out *before* deleting
    // anything — otherwise we'd delete objects that snapshot still needs.
    let live: HashSet<ObjectId> = kept
        .par_iter()
        .map(|(id, _)| {
            let snap = repo.load_snapshot(id).context("refusing to garbage-collect")?;
            Ok(snap.objects().collect::<Vec<_>>())
        })
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();

    let mut report = PruneReport {
        removed_snapshots: removed.iter().map(|(id, _)| *id).collect(),
        kept_snapshots: kept.len(),
        ..Default::default()
    };
    if !dry_run {
        // Manifests go first: a crash between the two steps just leaves garbage
        // for the next prune, never a snapshot with missing objects.
        for id in &report.removed_snapshots {
            repo.delete_snapshot(id)?;
        }
    }

    // Sweep.
    let dead: Vec<(ObjectId, u64)> =
        repo.list_objects()?.into_iter().filter(|(id, _)| !live.contains(id)).collect();
    report.deleted_objects = dead.len() as u64;
    report.freed_bytes = dead.iter().map(|(_, n)| n).sum();
    if !dry_run {
        dead.par_iter().try_for_each(|(id, _)| repo.delete_object(id))?;
        repo.clean_tmp()?;
    }
    Ok(report)
}
