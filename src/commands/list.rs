use anyhow::Result;

use crate::manifest::SnapshotHeader;
use crate::repo::{ObjectId, Repo};

/// All snapshots, newest first.
pub fn list(repo: &Repo) -> Result<Vec<(ObjectId, SnapshotHeader)>> {
    let mut out = repo
        .snapshot_ids()?
        .into_iter()
        .map(|id| Ok((id, repo.load_header(&id)?)))
        .collect::<Result<Vec<_>>>()?;
    out.sort_by_key(|(_, h)| std::cmp::Reverse(h.created));
    Ok(out)
}
