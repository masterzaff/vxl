//! Partial restores: put back one dimension, or one area of it, in an existing world
//! and leave everything else alone (other areas, `level.dat`, player data).
//!
//! A chunk lives in three region files with the same coordinates: blocks in
//! `region/`, mobs and item frames in `entities/` (since 1.17), points of interest
//! (beds, workstations, portals) in `poi/`. All three are restored together, or mobs
//! and villagers end up wrong. Inside the selection, the world is made exactly like
//! the snapshot: chunks that didn't exist yet are removed, so the game generates them
//! again.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::anvil::{self, RawChunk};
use crate::commands::restore::{self, Mode, STAGING_PREFIX, raw_chunk};
use crate::manifest::{ChunkEntry, FileEntry, safe_rel_path};
use crate::repo::{ObjectId, Repo};
use crate::world::{self, Kind, dimension};

/// A rectangle of chunks, inclusive.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Area {
    pub min: (i32, i32),
    pub max: (i32, i32),
}

impl Area {
    /// `x1,z1..x2,z2` in block coordinates (corners in any order). Every chunk the
    /// rectangle touches is selected.
    pub fn parse(s: &str) -> Result<Area> {
        let bad = || anyhow::anyhow!("{s:?} is not an area; use block coordinates like 100,-200..300,50");
        let (a, b) = s.split_once("..").ok_or_else(bad)?;
        let point = |p: &str| -> Result<(i32, i32)> {
            let (x, z) = p.split_once(',').ok_or_else(bad)?;
            Ok((x.trim().parse().map_err(|_| bad())?, z.trim().parse().map_err(|_| bad())?))
        };
        let ((x1, z1), (x2, z2)) = (point(a)?, point(b)?);
        let chunk = |b: i32| b.div_euclid(16);
        Ok(Area { min: (chunk(x1.min(x2)), chunk(z1.min(z2))), max: (chunk(x1.max(x2)), chunk(z1.max(z2))) })
    }

    fn contains(&self, (cx, cz): (i32, i32)) -> bool {
        (self.min.0..=self.max.0).contains(&cx) && (self.min.1..=self.max.1).contains(&cz)
    }

    fn overlaps_region(&self, (rx, rz): (i32, i32)) -> bool {
        let (x0, z0) = (rx * 32, rz * 32);
        x0 <= self.max.0 && x0 + 31 >= self.min.0 && z0 <= self.max.1 && z0 + 31 >= self.min.1
    }

    pub fn chunks(&self) -> u64 {
        (self.max.0 - self.min.0 + 1) as u64 * (self.max.1 - self.min.1 + 1) as u64
    }
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    /// Only this dimension ([`world::dimension`] names); `None` = all of them.
    pub dimension: Option<String>,
    /// Only these chunks; `None` = the whole dimension.
    pub area: Option<Area>,
}

impl Selection {
    fn region(&self, path: &str) -> Option<(i32, i32)> {
        let name = path.rsplit('/').next()?;
        let coords = anvil::region_coords(name)?;
        if self.dimension.as_ref().is_some_and(|d| *d != dimension(path)) {
            return None;
        }
        if self.area.is_some_and(|a| !a.overlaps_region(coords)) {
            return None;
        }
        Some(coords)
    }

    fn chunk(&self, (rx, rz): (i32, i32), slot: u16) -> bool {
        let c = (rx * 32 + (slot % 32) as i32, rz * 32 + (slot / 32) as i32);
        self.area.is_none_or(|a| a.contains(c))
    }
}

#[derive(Debug, Default)]
pub struct PartialReport {
    pub worlds: Vec<(String, PathBuf)>,
    /// Region files (region/, entities/, poi/) written or deleted.
    pub files_written: u64,
    pub files_deleted: u64,
    /// Chunks put back from the snapshot.
    pub chunks_restored: u64,
    /// Chunks removed because the snapshot didn't have them.
    pub chunks_removed: u64,
    pub warnings: Vec<String>,
}

/// What to do with one region file.
struct Job {
    dest: PathBuf,
    staging: PathBuf,
    rel: String,
    coords: (i32, i32),
    snapshot: Option<Vec<ChunkEntry>>,
}

/// Outcome of building one region in staging.
struct Built {
    job_index: usize,
    /// `None`: no chunks left, delete the file.
    region: Option<anvil::BuiltRegion>,
    restored: u64,
    removed: u64,
}

/// Restore `sel` of snapshot `id` (or of its world `only`) into `target`, placed like
/// [`restore::plan`]. A non-empty world folder is only changed with `force`
/// (`Mode::Replace`); it must be a world (`level.dat`). All affected region files are
/// built in a staging folder first; the world is only touched once all have succeeded.
pub fn restore_partial(
    repo: &Repo,
    id: &ObjectId,
    target: &Path,
    only: Option<&str>,
    sel: &Selection,
    force: bool,
    progress: &ProgressBar,
) -> Result<PartialReport> {
    let snap = repo.load_snapshot(id)?;
    let plan = restore::plan(&snap, target, only)?;
    let mut report = PartialReport {
        worlds: plan.iter().map(|(w, d)| (w.name.clone(), d.clone())).collect(),
        ..Default::default()
    };

    let dims: BTreeSet<String> = plan
        .iter()
        .flat_map(|(w, _)| &w.files)
        .filter(|f| matches!(f, FileEntry::Region { .. }))
        .map(|f| dimension(f.path()))
        .collect();
    let list = || dims.iter().cloned().collect::<Vec<_>>().join(", ");
    match &sel.dimension {
        Some(d) if !dims.contains(d) => bail!("this snapshot has no dimension {d:?} (it has: {})", list()),
        None if sel.area.is_some() && dims.len() > 1 => {
            bail!("--area needs --dimension, since the same coordinates exist in several: {}", list())
        }
        _ => {}
    }

    // Check every destination, then collect the region files to rebuild.
    let pid = std::process::id();
    let mut jobs = Vec::new();
    for (w, dest) in &plan {
        let exists = dest.exists() && fs::read_dir(dest)?.next().is_some();
        if exists {
            if !force {
                bail!(
                    "{} is not empty; a partial restore merges into an existing world only with --force",
                    dest.display()
                );
            }
            restore::check_overwrite_allowed(repo, dest, Mode::Replace)?;
        }
        // rel path → (region coords, the snapshot's chunks if it has the file)
        type Regions = BTreeMap<String, ((i32, i32), Option<Vec<ChunkEntry>>)>;
        let mut regions = Regions::new();
        for f in &w.files {
            let Some(coords) = sel.region(f.path()) else { continue };
            match f {
                FileEntry::Region { path, chunks } => {
                    regions.insert(path.clone(), (coords, Some(chunks.clone())));
                }
                FileEntry::Blob { path, .. } => report.warnings.push(format!(
                    "{}/{path}: was not a valid region file when backed up; left as it is",
                    w.name
                )),
            }
        }
        // Region files only the live world has (areas generated after the snapshot).
        if exists {
            for f in world::scan(dest)?.files {
                if let Kind::Region { coords } = f.kind
                    && sel.region(&f.rel).is_some()
                {
                    regions.entry(f.rel).or_insert((coords, None));
                }
            }
        }
        let staging = dest.join(format!("{STAGING_PREFIX}{pid}"));
        for (rel, (coords, snapshot)) in regions {
            jobs.push(Job { dest: dest.clone(), staging: staging.clone(), rel, coords, snapshot });
        }
    }

    // Build every region in staging. Nothing in the world has changed yet.
    progress.set_length(jobs.len() as u64);
    let built: Result<Vec<Built>> = jobs
        .par_iter()
        .enumerate()
        .map(|(i, job)| {
            let r = build(repo, job, sel, i).with_context(|| format!("rebuilding {}", job.rel));
            progress.inc(1);
            r
        })
        .collect();
    let stagings: BTreeSet<&PathBuf> = jobs.iter().map(|j| &j.staging).collect();
    let cleanup = || stagings.iter().for_each(|s| drop(fs::remove_dir_all(s)));
    let built = match built {
        Ok(b) => b,
        Err(e) => {
            cleanup();
            return Err(e.context("partial restore failed; nothing was changed"));
        }
    };

    // Commit: move each file into place.
    for b in &built {
        let job = &jobs[b.job_index];
        let rel = safe_rel_path(&job.rel)?;
        let (staged, live) = (job.staging.join(&rel), job.dest.join(&rel));
        let dir = live.parent().unwrap();
        let mut keep_external = BTreeSet::new();
        match &b.region {
            Some(region) => {
                fs::create_dir_all(dir)?;
                for (name, _) in &region.external {
                    fs::rename(staged.parent().unwrap().join(name), dir.join(name))?;
                    keep_external.insert(name.clone());
                }
                fs::rename(&staged, &live).with_context(|| format!("replacing {}", live.display()))?;
                report.files_written += 1;
            }
            None => {
                if live.exists() {
                    fs::remove_file(&live).with_context(|| format!("deleting {}", live.display()))?;
                    report.files_deleted += 1;
                }
            }
        }
        // Oversized-chunk files of selected chunks that no longer need one.
        for slot in (0..anvil::SLOTS as u16).filter(|s| sel.chunk(job.coords, *s)) {
            let name = anvil::external_name(job.coords.0, job.coords.1, slot);
            if !keep_external.contains(&name) && dir.join(&name).exists() {
                fs::remove_file(dir.join(&name))?;
            }
        }
        report.chunks_restored += b.restored;
        report.chunks_removed += b.removed;
    }
    cleanup();
    Ok(report)
}

/// Merge one region: the live file's chunks outside the selection, the snapshot's
/// inside it. Writes the result into the job's staging folder.
fn build(repo: &Repo, job: &Job, sel: &Selection, job_index: usize) -> Result<Built> {
    let rel = safe_rel_path(&job.rel)?;
    let live = job.dest.join(&rel);
    let mut chunks: Vec<RawChunk> = Vec::new();
    let mut live_selected = BTreeSet::new();
    if live.exists() {
        let data = fs::read(&live)?;
        if !data.is_empty() {
            let dir = live.parent().unwrap();
            let parsed = anvil::parse_region(&data, Some(job.coords), |n| fs::read(dir.join(n)))
                .with_context(|| format!("{} is damaged; restore the whole world instead", live.display()))?;
            for c in parsed.chunks {
                if sel.chunk(job.coords, c.slot) {
                    live_selected.insert(c.slot);
                } else {
                    chunks.push(c);
                }
            }
        }
    }
    let from_snapshot: Vec<RawChunk> = job
        .snapshot
        .iter()
        .flatten()
        .filter(|c| sel.chunk(job.coords, c.slot))
        .map(|c| raw_chunk(repo, c))
        .collect::<Result<_>>()?;
    let restored = from_snapshot.len() as u64;
    let replaced: BTreeSet<u16> = from_snapshot.iter().map(|c| c.slot).collect();
    // Selected chunks the live file had and the snapshot didn't: gone, to be regenerated.
    let removed = live_selected.difference(&replaced).count() as u64;
    chunks.extend(from_snapshot);

    if chunks.is_empty() {
        return Ok(Built { job_index, region: None, restored, removed });
    }
    let region = anvil::build_region(&chunks, Some(job.coords))?;
    let staged = job.staging.join(&rel);
    let dir = staged.parent().unwrap();
    fs::create_dir_all(dir)?;
    fs::write(&staged, &region.region)?;
    for (name, bytes) in &region.external {
        fs::write(dir.join(name), bytes)?;
    }
    Ok(Built { job_index, region: Some(region), restored, removed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn areas() {
        let a = Area::parse("100,-200..-20,50").unwrap();
        assert_eq!(a, Area { min: (-2, -13), max: (6, 3) });
        assert_eq!(a.chunks(), 9 * 17);
        assert!(a.contains((-2, 3)) && !a.contains((7, 0)));
        assert!(a.overlaps_region((0, 0)) && a.overlaps_region((-1, -1)));
        assert!(!a.overlaps_region((1, 0)));
        assert_eq!(Area::parse("0,0..15,15").unwrap(), Area { min: (0, 0), max: (0, 0) });
        assert!(Area::parse("1,2,3,4").is_err());
        assert!(Area::parse("a,b..c,d").is_err());
    }

    #[test]
    fn selection() {
        let s = Selection {
            dimension: Some("the_nether".into()),
            area: Some(Area::parse("0,0..40,40").unwrap()),
        };
        assert_eq!(s.region("DIM-1/region/r.0.0.mca"), Some((0, 0)));
        assert_eq!(s.region("DIM-1/entities/r.0.0.mca"), Some((0, 0)));
        assert_eq!(s.region("region/r.0.0.mca"), None);
        assert_eq!(s.region("DIM-1/region/r.1.0.mca"), None);
        assert_eq!(s.region("DIM-1/level.dat"), None);
        // Chunks (0..=2, 0..=2) of region (0, 0).
        assert!(s.chunk((0, 0), 0) && s.chunk((0, 0), 2 + 2 * 32) && !s.chunk((0, 0), 3));
    }
}
