use std::collections::HashSet;

use anyhow::Result;
use indicatif::ProgressBar;
use rayon::prelude::*;

use crate::repo::{ObjectId, Repo};

pub struct VerifyReport {
    pub snapshots: usize,
    pub objects: usize,
    /// Objects that are missing or corrupt, with the reason.
    pub problems: Vec<(ObjectId, String)>,
}

/// Re-read and re-hash every object the given snapshots use.
pub fn verify(repo: &Repo, ids: &[ObjectId], progress: &ProgressBar) -> Result<VerifyReport> {
    let mut objects = HashSet::new();
    for id in ids {
        objects.extend(repo.load_snapshot(id)?.objects());
    }
    progress.set_length(objects.len() as u64);
    let mut problems: Vec<(ObjectId, String)> = objects
        .par_iter()
        .filter_map(|id| {
            let r = repo.get(id).err().map(|e| (*id, format!("{e:#}")));
            progress.inc(1);
            r
        })
        .collect();
    problems.sort();
    Ok(VerifyReport { snapshots: ids.len(), objects: objects.len(), problems })
}
