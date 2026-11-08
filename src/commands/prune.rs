use std::collections::HashSet;

use anyhow::{Context, Result};
use chrono::Local;
use rayon::prelude::*;

use crate::commands::list::list;
use crate::manifest::SnapshotHeader;
use crate::repo::{ObjectId, PackInfo, Repo};
use crate::retention::{self, Period, Policy};

/// Repack a pack once at least this fraction of its object data is garbage.
pub const REPACK_DEAD_FRACTION: f64 = 0.25;
/// Packs smaller than `pack_size / SMALL_DIVISOR` (the leftovers each
/// incremental backup produces) get merged once there are at least two.
const SMALL_DIVISOR: u64 = 4;

#[derive(Debug, Default)]
pub struct PruneReport {
    /// Newest first.
    pub removed_snapshots: Vec<ObjectId>,
    pub kept_snapshots: usize,
    /// For `prune`: each kept snapshot and the rules that keep it, newest first.
    pub kept_by: Vec<(ObjectId, Vec<Period>)>,
    /// Unreferenced objects removed from disk.
    pub deleted_objects: u64,
    /// Unreferenced objects left in packs that are still mostly in use
    /// (removed once a pack crosses the repack threshold).
    pub garbage_left: u64,
    /// Packs deleted outright (nothing in them was still needed).
    pub packs_deleted: u64,
    /// Packs whose live objects were copied into new packs.
    pub packs_repacked: u64,
    /// New packs written by repacking.
    pub packs_written: u64,
    /// Disk space given back (for a dry run: an upper-bound estimate).
    pub freed_bytes: u64,
}

/// Drop the snapshots `policy` doesn't keep (an empty policy keeps all), then collect
/// garbage: delete packs nothing uses, compact packs that are mostly garbage or tiny.
/// Periods are in local time. Needs the exclusive lock.
pub fn prune(repo: &Repo, policy: &Policy, dry_run: bool) -> Result<PruneReport> {
    let snaps = list(repo)?; // newest first
    let decisions = retention::apply(policy, &snaps, &Local);
    let remove: HashSet<ObjectId> = decisions.iter().filter(|d| d.kept_by.is_empty()).map(|d| d.id).collect();
    let mut report = collect(repo, &snaps, &remove, dry_run)?;
    report.kept_by =
        decisions.into_iter().filter(|d| !d.kept_by.is_empty()).map(|d| (d.id, d.kept_by)).collect();
    Ok(report)
}

/// Delete exactly these snapshots, then collect garbage like [`prune`].
pub fn delete(repo: &Repo, ids: &[ObjectId], dry_run: bool) -> Result<PruneReport> {
    let snaps = list(repo)?;
    collect(repo, &snaps, &ids.iter().copied().collect(), dry_run)
}

/// Add what group objects in `live` read sections from (format 3; one level is enough,
/// since a section is read straight out of the object that stores it). Bails rather
/// than guess if an object's header can't be read; an object that's missing altogether
/// has nothing left to protect.
pub fn with_deps(repo: &Repo, live: HashSet<ObjectId>) -> Result<HashSet<ObjectId>> {
    if !repo.grouped() {
        return Ok(live);
    }
    let deps: Vec<ObjectId> = live
        .par_iter()
        .filter(|id| repo.has(id))
        .map(|id| repo.deps(id).context("refusing to garbage-collect"))
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .flatten()
        .collect();
    let mut live = live;
    live.extend(deps);
    Ok(live)
}

/// Remove the snapshots in `remove` (of `snaps`, newest first) and collect garbage.
fn collect(
    repo: &Repo,
    snaps: &[(ObjectId, SnapshotHeader)],
    remove: &HashSet<ObjectId>,
    dry_run: bool,
) -> Result<PruneReport> {
    let (removed, kept): (Vec<_>, Vec<_>) = snaps.iter().partition(|(id, _)| remove.contains(id));

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
    let live = with_deps(repo, live)?;

    let mut report = PruneReport {
        removed_snapshots: removed.iter().map(|(id, _)| *id).collect(),
        kept_snapshots: kept.len(),
        ..Default::default()
    };

    // Plan.
    let packs = repo.packs();
    let mut delete: Vec<&PackInfo> = Vec::new();
    let mut repack: Vec<&PackInfo> = Vec::new();
    let mut small: Vec<&PackInfo> = Vec::new();
    for p in &packs {
        let data: u64 = p.entries.iter().map(|e| e.len as u64).sum();
        let dead: u64 = p.entries.iter().filter(|e| !live.contains(&e.id)).map(|e| e.len as u64).sum();
        if p.entries.iter().all(|e| !live.contains(&e.id)) {
            delete.push(p);
        } else if dead as f64 >= data as f64 * REPACK_DEAD_FRACTION {
            repack.push(p);
        } else if p.size < repo.config.pack_size / SMALL_DIVISOR {
            small.push(p);
        }
    }
    if small.len() >= 2 {
        repack.extend(small);
    }
    // Garbage only really goes away from packs we delete or rewrite; packs that
    // are mostly live keep theirs until they cross the threshold.
    let touched: HashSet<ObjectId> = delete.iter().chain(&repack).map(|p| p.id).collect();
    let dead =
        |p: &&PackInfo| p.entries.iter().filter(|e| !live.contains(&e.id)).map(|e| e.id).collect::<Vec<_>>();
    let left: HashSet<ObjectId> =
        packs.iter().filter(|p| !touched.contains(&p.id)).flat_map(|p| dead(&p)).collect();
    let removed_ids: HashSet<ObjectId> =
        delete.iter().chain(&repack).flat_map(dead).filter(|id| !left.contains(id)).collect();
    report.deleted_objects = removed_ids.len() as u64;
    report.garbage_left = left.len() as u64;
    report.packs_deleted = delete.len() as u64;
    report.packs_repacked = repack.len() as u64;

    let old_bytes: u64 = delete.iter().chain(&repack).map(|p| p.size).sum();
    if dry_run {
        let dead_in_repacked: u64 = repack
            .iter()
            .flat_map(|p| &p.entries)
            .filter(|e| !live.contains(&e.id))
            .map(|e| e.len as u64)
            .sum();
        report.freed_bytes = delete.iter().map(|p| p.size).sum::<u64>() + dead_in_repacked;
        return Ok(report);
    }

    // Manifests go first: a crash after this just leaves garbage for the next
    // prune, never a snapshot with missing objects.
    for id in &report.removed_snapshots {
        repo.delete_snapshot(id)?;
    }
    // Repack before deleting anything else: new packs are durable before old ones go.
    if !repack.is_empty() {
        let to_repack: Vec<PackInfo> = repack.iter().map(|p| (*p).clone()).collect();
        let (written, new_bytes) = repo.repack(&to_repack, &live)?;
        report.packs_written = written;
        report.freed_bytes = old_bytes.saturating_sub(new_bytes);
    } else {
        report.freed_bytes = old_bytes;
    }
    for p in delete {
        repo.delete_pack(&p.id)?;
    }
    repo.clean_tmp()?;
    Ok(report)
}
