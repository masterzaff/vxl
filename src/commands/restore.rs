use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::anvil::{self, Compression, RawChunk};
use crate::manifest::{FileEntry, safe_rel_path};
use crate::repo::{ObjectId, Repo};

/// What to do when the target folder already has something in it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Mode {
    /// Target must not exist or be empty.
    #[default]
    Fresh,
    /// Overwrite files the snapshot contains; leave every other file alone.
    Replace,
    /// Delete everything in the target, then restore: the folder ends up
    /// exactly like the snapshot.
    Wipe,
}

pub struct RestoreReport {
    pub id: ObjectId,
    pub files: u64,
    pub chunks: u64,
    /// Existing files that were overwritten (`Replace`).
    pub replaced: u64,
    /// Existing top-level entries that were deleted (`Wipe`).
    pub wiped: u64,
}

/// Staging folders live inside the target (same filesystem → renames are cheap
/// and atomic) and are skipped by `world::scan`.
pub const STAGING_PREFIX: &str = ".vxl-staging-";

/// Rebuild snapshot `id` into `target`.
///
/// For a non-empty target (`Replace`/`Wipe`) the whole snapshot is first rebuilt
/// in a staging folder; the target is only touched once that has fully succeeded.
pub fn restore(
    repo: &Repo,
    id: &ObjectId,
    target: &Path,
    mode: Mode,
    progress: &ProgressBar,
) -> Result<RestoreReport> {
    let snap = repo.load_snapshot(id)?;
    let mut report =
        RestoreReport { id: *id, files: snap.files.len() as u64, chunks: 0, replaced: 0, wiped: 0 };

    let non_empty = target.exists() && {
        if !target.is_dir() {
            bail!("{} exists and is not a folder", target.display());
        }
        fs::read_dir(target)?.next().is_some()
    };
    if !non_empty {
        fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
        report.chunks = write_all(repo, &snap.files, target, progress)?;
        return Ok(report);
    }

    check_overwrite_allowed(repo, target, mode)?;
    let staging = target.join(format!("{STAGING_PREFIX}{}", std::process::id()));
    fs::create_dir(&staging).with_context(|| format!("creating {}", staging.display()))?;
    match write_all(repo, &snap.files, &staging, progress) {
        Ok(n) => report.chunks = n,
        Err(e) => {
            let _ = fs::remove_dir_all(&staging);
            return Err(e.context(format!("restore failed; {} was not changed", target.display())));
        }
    }
    let commit = match mode {
        Mode::Wipe => commit_wipe(target, &staging, &mut report),
        _ => commit_replace(&staging, target, &mut report),
    };
    commit.with_context(|| {
        format!("moving files into place failed; the complete restored world is in {}", staging.display())
    })?;
    fs::remove_dir_all(&staging)?;
    Ok(report)
}

fn check_overwrite_allowed(repo: &Repo, target: &Path, mode: Mode) -> Result<()> {
    if mode == Mode::Fresh {
        bail!(
            "{} is not empty. Restore into a new folder, or use --force (overwrite files) \
             or --force-wipe (replace the folder's contents)",
            target.display()
        );
    }
    if !target.join("level.dat").exists() {
        bail!(
            "{} doesn't look like a Minecraft world (no level.dat); refusing to overwrite it",
            target.display()
        );
    }
    let (t, r) = (target.canonicalize()?, repo.root().canonicalize()?);
    if r.starts_with(&t) {
        bail!("the repository is inside {}; refusing to overwrite it", target.display());
    }
    Ok(())
}

fn write_all(repo: &Repo, files: &[FileEntry], dir: &Path, progress: &ProgressBar) -> Result<u64> {
    progress.set_length(files.len() as u64);
    files
        .par_iter()
        .map(|f| {
            let n = restore_file(repo, f, dir).with_context(|| format!("restoring {}", f.path()));
            progress.inc(1);
            n
        })
        .sum()
}

/// Delete everything in `target` except the staging folder, then move the
/// staged world up into `target`.
fn commit_wipe(target: &Path, staging: &Path, report: &mut RestoreReport) -> Result<()> {
    for e in fs::read_dir(target)? {
        let path = e?.path();
        if path == staging {
            continue;
        }
        if path.is_dir() && !path.is_symlink() { fs::remove_dir_all(&path) } else { fs::remove_file(&path) }
            .with_context(|| format!("deleting {}", path.display()))?;
        report.wiped += 1;
    }
    for e in fs::read_dir(staging)? {
        let e = e?;
        fs::rename(e.path(), target.join(e.file_name()))?;
    }
    Ok(())
}

/// Move every staged file over its counterpart in `target`, one by one.
fn commit_replace(staging: &Path, target: &Path, report: &mut RestoreReport) -> Result<()> {
    for e in walkdir::WalkDir::new(staging).min_depth(1) {
        let e = e?;
        if e.file_type().is_dir() {
            continue;
        }
        let dest = target.join(e.path().strip_prefix(staging)?);
        fs::create_dir_all(dest.parent().unwrap())?;
        if dest.is_dir() && !dest.is_symlink() {
            fs::remove_dir_all(&dest)?; // a folder where the snapshot has a file
        }
        if dest.exists() {
            report.replaced += 1;
        }
        fs::rename(e.path(), &dest).with_context(|| format!("replacing {}", dest.display()))?;
    }
    Ok(())
}

/// Returns how many chunks were written.
fn restore_file(repo: &Repo, f: &FileEntry, target: &Path) -> Result<u64> {
    let dest = target.join(safe_rel_path(f.path())?);
    let dir = dest.parent().unwrap();
    fs::create_dir_all(dir)?;
    match f {
        FileEntry::Blob { size, hash, .. } => {
            let data = repo.get(hash)?;
            if data.len() as u64 != *size {
                bail!("object {hash} has the wrong size");
            }
            fs::write(&dest, data)?;
            Ok(0)
        }
        FileEntry::Region { chunks, .. } => {
            let raw: Vec<RawChunk> = chunks
                .par_iter()
                .map(|c| {
                    let data = repo.get(&c.hash)?;
                    let payload = if c.verbatim {
                        data
                    } else {
                        let comp = Compression::from_byte(c.comp)
                            .ok_or_else(|| anyhow!("unknown compression {} in manifest", c.comp))?;
                        anvil::compress(comp, &data)?
                    };
                    Ok(RawChunk {
                        slot: c.slot,
                        timestamp: c.ts,
                        compression: c.comp,
                        external: false,
                        payload,
                    })
                })
                .collect::<Result<_>>()?;
            let name = dest.file_name().and_then(|n| n.to_str()).unwrap_or_default();
            let built = anvil::build_region(&raw, anvil::region_coords(name))?;
            fs::write(&dest, built.region)?;
            for (name, bytes) in built.external {
                fs::write(dir.join(name), bytes)?;
            }
            Ok(chunks.len() as u64)
        }
    }
}
