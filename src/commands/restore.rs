use std::fs;
use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::anvil::{self, Compression, RawChunk};
use crate::manifest::{FileEntry, safe_rel_path};
use crate::repo::{ObjectId, Repo};

pub struct RestoreReport {
    pub id: ObjectId,
    pub files: u64,
    pub chunks: u64,
}

/// Rebuild snapshot `id` into `target`, which must not exist or be empty.
pub fn restore(repo: &Repo, id: &ObjectId, target: &Path, progress: &ProgressBar) -> Result<RestoreReport> {
    if target.exists() {
        if !target.is_dir() || fs::read_dir(target)?.next().is_some() {
            bail!("{} already exists and is not empty; restore into a new folder", target.display());
        }
    } else {
        fs::create_dir_all(target).with_context(|| format!("creating {}", target.display()))?;
    }

    let snap = repo.load_snapshot(id)?;
    progress.set_length(snap.files.len() as u64);
    let chunks = snap
        .files
        .par_iter()
        .map(|f| {
            let n = restore_file(repo, f, target).with_context(|| format!("restoring {}", f.path()));
            progress.inc(1);
            n
        })
        .sum::<Result<u64>>()?;
    Ok(RestoreReport { id: *id, files: snap.files.len() as u64, chunks })
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
