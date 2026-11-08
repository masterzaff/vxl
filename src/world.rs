//! Walking a world folder and deciding how each file gets stored.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

use crate::anvil::region_coords;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Kind {
    /// `r.X.Z.mca` — split into chunks. Covers region/, entities/ and poi/ in every dimension.
    Region { coords: (i32, i32) },
    /// `c.X.Z.mcc` — oversized chunk; normally consumed by its region file.
    External,
    /// Anything else: stored whole.
    Blob,
}

#[derive(Clone, Debug)]
pub struct WorldFile {
    /// Path relative to the world root, always with `/` separators.
    pub rel: String,
    pub abs: PathBuf,
    pub kind: Kind,
    pub size: u64,
}

/// Files that must never be backed up.
fn skipped(name: &str) -> bool {
    // The server holds this open; it's recreated on startup anyway.
    name == "session.lock"
}

pub struct Scan {
    pub files: Vec<WorldFile>,
    pub warnings: Vec<String>,
}

pub fn scan(world: &Path) -> Result<Scan> {
    if !world.is_dir() {
        bail!("{} is not a directory", world.display());
    }
    let mut files = Vec::new();
    let mut warnings = Vec::new();
    if !world.join("level.dat").exists() {
        warnings.push(format!("{} has no level.dat — is this really a world folder?", world.display()));
    }
    // Leftovers of an interrupted `restore --force…` are not part of the world.
    let not_staging = |e: &walkdir::DirEntry| {
        e.depth() != 1
            || !e.file_name().to_string_lossy().starts_with(crate::commands::restore::STAGING_PREFIX)
    };
    for entry in
        WalkDir::new(world).follow_links(false).sort_by_file_name().into_iter().filter_entry(not_staging)
    {
        let entry = entry.with_context(|| format!("reading {}", world.display()))?;
        let ft = entry.file_type();
        if ft.is_dir() {
            continue;
        }
        let rel_path = entry.path().strip_prefix(world)?;
        let Some(rel) = rel_path.to_str().map(|s| s.replace('\\', "/")) else {
            warnings.push(format!("skipping non-UTF-8 path {}", rel_path.display()));
            continue;
        };
        if !ft.is_file() {
            warnings.push(format!("skipping {rel} (symlink or special file)"));
            continue;
        }
        let name = entry.file_name().to_string_lossy();
        if skipped(&name) {
            continue;
        }
        let kind = if let Some(coords) = region_coords(&name) {
            Kind::Region { coords }
        } else if name.starts_with("c.") && name.ends_with(".mcc") {
            Kind::External
        } else {
            Kind::Blob
        };
        let size = entry.metadata()?.len();
        files.push(WorldFile { rel, abs: entry.path().to_path_buf(), kind, size });
    }
    Ok(Scan { files, warnings })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn classifies_files() {
        let dir = tempfile::tempdir().unwrap();
        let w = dir.path();
        for p in [
            "level.dat",
            "session.lock",
            "region/r.0.-1.mca",
            "region/c.3.4.mcc",
            "DIM-1/entities/r.1.1.mca",
            "playerdata/x.dat",
            "region/r.0.0.mca.bak",
        ] {
            let p = w.join(p);
            fs::create_dir_all(p.parent().unwrap()).unwrap();
            fs::write(p, b"x").unwrap();
        }
        let scan = scan(w).unwrap();
        assert!(scan.warnings.is_empty());
        let got: Vec<_> = scan.files.iter().map(|f| (f.rel.as_str(), f.kind.clone())).collect();
        assert_eq!(
            got,
            vec![
                ("DIM-1/entities/r.1.1.mca", Kind::Region { coords: (1, 1) }),
                ("level.dat", Kind::Blob),
                ("playerdata/x.dat", Kind::Blob),
                ("region/c.3.4.mcc", Kind::External),
                ("region/r.0.-1.mca", Kind::Region { coords: (0, -1) }),
                ("region/r.0.0.mca.bak", Kind::Blob),
            ]
        );
    }
}
