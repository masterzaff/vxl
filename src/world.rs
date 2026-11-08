//! Finding the worlds in a server folder, and walking a world folder to decide
//! how each file gets stored.

use std::fs;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};
use walkdir::WalkDir;

use crate::anvil::region_coords;
use crate::manifest::{is_valid_world_name, safe_rel_path};

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

/// A world folder found under a backup source.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FoundWorld {
    /// Its folder name: what it's stored and restored as.
    pub name: String,
    /// Where it is relative to the source, with `/` separators (`.` = the source itself).
    pub rel: String,
    /// Canonical path (symlinks resolved).
    pub abs: PathBuf,
}

/// Folders of a server that never hold its worlds, but may hold *copies* of worlds
/// (backup plugins, world-edit schematics…) that must not be mistaken for them.
const NOT_WORLDS: &[&str] =
    &["plugins", "mods", "config", "backups", "logs", "cache", "libraries", "versions", "crash-reports"];

pub fn is_world(dir: &Path) -> bool {
    dir.join("level.dat").is_file()
}

/// Find the worlds under `source`. If `source` is itself a world, that's the only
/// one. Otherwise: folders with a `level.dat` up to two levels down (`world/`,
/// `worlds/world1/`), not descending into a world once found (its dimensions belong
/// to it) and skipping hidden folders and [`NOT_WORLDS`]. Sorted by path.
pub fn find_worlds(source: &Path) -> Result<Vec<FoundWorld>> {
    let source = source.canonicalize().with_context(|| format!("opening {}", source.display()))?;
    if !source.is_dir() {
        bail!("{} is not a directory", source.display());
    }
    if is_world(&source) {
        let name = source
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|n| is_valid_world_name(n))
            .unwrap_or("world")
            .to_string();
        return Ok(vec![FoundWorld { name, rel: ".".into(), abs: source }]);
    }
    let mut out = Vec::new();
    for (name, dir) in candidate_dirs(&source)? {
        if is_world(&dir) {
            out.push(FoundWorld { rel: name.clone(), name, abs: dir.canonicalize()? });
            continue;
        }
        for (sub, subdir) in candidate_dirs(&dir)? {
            if is_world(&subdir) {
                out.push(FoundWorld { rel: format!("{name}/{sub}"), name: sub, abs: subdir.canonicalize()? });
            }
        }
    }
    Ok(out)
}

/// Sub-folders of `dir` that could be (or contain) a world, following symlinks, sorted.
fn candidate_dirs(dir: &Path) -> Result<Vec<(String, PathBuf)>> {
    let mut out = Vec::new();
    for e in fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let e = e?;
        let Some(name) = e.file_name().to_str().map(str::to_string) else { continue };
        let path = e.path();
        if name.starts_with('.') || NOT_WORLDS.contains(&name.to_ascii_lowercase().as_str()) || !path.is_dir()
        {
            continue;
        }
        out.push((name, path));
    }
    out.sort();
    Ok(out)
}

/// Pick worlds by `selectors` (none means `*`):
/// `*` = every found world, `name` = the found world with that folder name,
/// `some/path` = the world at that path under `source` (found or not),
/// `!name` / `!some/path` = leave that one out. Errors on unknown names, ambiguous
/// names, two selected worlds with the same name, or an empty result.
pub fn select_worlds(source: &Path, found: &[FoundWorld], selectors: &[String]) -> Result<Vec<FoundWorld>> {
    let source = source.canonicalize()?;
    let (neg, pos): (Vec<&str>, Vec<&str>) =
        selectors.iter().map(|s| s.trim()).partition(|s| s.starts_with('!'));
    let pos = if pos.is_empty() { vec!["*"] } else { pos };
    let found_names = || found.iter().map(|w| w.rel.as_str()).collect::<Vec<_>>().join(", ");

    let mut out: Vec<FoundWorld> = Vec::new();
    let mut add = |w: &FoundWorld| {
        if !out.iter().any(|o| o.abs == w.abs) {
            out.push(w.clone());
        }
    };
    for sel in pos {
        if sel == "*" {
            found.iter().for_each(&mut add);
        } else if is_path(sel) {
            add(&world_at(&source, sel)?);
        } else {
            match found.iter().filter(|w| w.name == sel).collect::<Vec<_>>().as_slice() {
                [w] => add(w),
                [] => bail!(
                    "no world named {sel:?} in {} (found: {})",
                    source.display(),
                    if found.is_empty() { "none".into() } else { found_names() }
                ),
                many => bail!(
                    "{sel:?} matches several worlds ({}); select one by its path",
                    many.iter().map(|w| w.rel.as_str()).collect::<Vec<_>>().join(", ")
                ),
            }
        }
    }
    for sel in neg {
        let sel = sel[1..].trim();
        let matches = |w: &FoundWorld| if is_path(sel) { w.rel == normalize(sel) } else { w.name == sel };
        let before = out.len();
        out.retain(|w| !matches(w));
        if out.len() == before {
            bail!("\"!{sel}\" doesn't match any selected world");
        }
    }

    for (i, a) in out.iter().enumerate() {
        if let Some(b) = out[i + 1..].iter().find(|b| b.name == a.name) {
            bail!(
                "two worlds are both named {:?} ({} and {}); select one of them by its path",
                a.name,
                a.rel,
                b.rel
            );
        }
    }
    for a in &out {
        if let Some(b) = out.iter().find(|b| b.abs != a.abs && b.abs.starts_with(&a.abs)) {
            bail!("{} is inside {}, which is selected too; select only one of them", b.rel, a.rel);
        }
    }
    if out.is_empty() {
        bail!(
            "no worlds to back up in {} (a world is a folder with a level.dat, at most two levels down)",
            source.display()
        );
    }
    Ok(out)
}

fn is_path(sel: &str) -> bool {
    sel.contains(['/', '\\'])
}

fn normalize(sel: &str) -> String {
    sel.replace('\\', "/").trim_matches('/').to_string()
}

fn world_at(source: &Path, sel: &str) -> Result<FoundWorld> {
    let rel = normalize(sel);
    let dir = source.join(safe_rel_path(&rel)?);
    if !is_world(&dir) {
        bail!("{} is not a world (no level.dat)", dir.display());
    }
    let name = rel.rsplit('/').next().unwrap_or_default().to_string();
    if !is_valid_world_name(&name) {
        bail!("can't use {sel:?} as a world name");
    }
    Ok(FoundWorld { name, rel, abs: dir.canonicalize()? })
}

/// The dimension a world-relative region path (`<dim>/region/r.x.z.mca`, or
/// `entities/`, `poi/`) belongs to: `overworld`, `the_nether`, `the_end`, or
/// `<namespace>:<name>` for other `dimensions/<namespace>/<name>/`.
///
/// Both layouts give the same names: the classic one (overworld at the world root,
/// `DIM-1`, `DIM1`) and the newer one where every dimension lives under
/// `dimensions/minecraft/<name>/`.
pub fn dimension(path: &str) -> String {
    let dir = path.rsplit_once('/').map_or("", |(d, _)| d);
    let dim = dir.rsplit_once('/').map_or("", |(d, _)| d);
    match dim {
        "" => "overworld".into(),
        "DIM-1" => "the_nether".into(),
        "DIM1" => "the_end".into(),
        d => match d.strip_prefix("dimensions/").and_then(|r| r.split_once('/')) {
            Some(("minecraft", name @ ("overworld" | "the_nether" | "the_end"))) => name.into(),
            Some((ns, name)) => format!("{ns}:{name}"),
            None => d.into(),
        },
    }
}

/// What a user may type for a dimension: `nether`, `minecraft:the_nether`, `DIM-1`… →
/// the name [`dimension`] uses.
pub fn normalize_dimension(s: &str) -> String {
    let s = s.trim();
    let s = s.strip_prefix("minecraft:").unwrap_or(s);
    match s {
        "nether" | "DIM-1" => "the_nether".into(),
        "end" | "DIM1" => "the_end".into(),
        "world" => "overworld".into(),
        other => other.into(),
    }
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

    fn mk_world(dir: &Path) {
        fs::create_dir_all(dir.join("region")).unwrap();
        fs::write(dir.join("level.dat"), b"x").unwrap();
    }

    fn names(ws: &[FoundWorld]) -> Vec<(&str, &str)> {
        ws.iter().map(|w| (w.name.as_str(), w.rel.as_str())).collect()
    }

    fn sel(source: &Path, s: &[&str]) -> Result<Vec<FoundWorld>> {
        let found = find_worlds(source).unwrap();
        select_worlds(source, &found, &s.iter().map(|s| s.to_string()).collect::<Vec<_>>())
    }

    #[test]
    fn finds_worlds_in_a_server_folder() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        mk_world(&s.join("world"));
        fs::create_dir_all(s.join("world/DIM-1/region")).unwrap(); // vanilla nether: part of `world`
        mk_world(&s.join("world_nether"));
        mk_world(&s.join("worlds/lobby"));
        mk_world(&s.join("plugins/SomeBackup/world")); // a plugin's copy: not a world of the server
        mk_world(&s.join("backups/world"));
        mk_world(&s.join(".hidden"));
        mk_world(&s.join("a/b/too_deep"));
        fs::create_dir_all(s.join("logs")).unwrap();
        fs::write(s.join("server.properties"), b"").unwrap();

        let found = find_worlds(s).unwrap();
        assert_eq!(
            names(&found),
            [("world", "world"), ("world_nether", "world_nether"), ("lobby", "worlds/lobby")]
        );

        // A world given directly is the only one, named after its folder.
        let one = find_worlds(&s.join("world")).unwrap();
        assert_eq!(names(&one), [("world", ".")]);
    }

    #[test]
    #[cfg(unix)]
    fn follows_a_symlinked_world() {
        let dir = tempfile::tempdir().unwrap();
        let (s, elsewhere) = (dir.path().join("server"), dir.path().join("disk2/big"));
        fs::create_dir_all(&s).unwrap();
        mk_world(&elsewhere);
        std::os::unix::fs::symlink(&elsewhere, s.join("big")).unwrap();
        let found = find_worlds(&s).unwrap();
        assert_eq!(names(&found), [("big", "big")]);
        assert_eq!(found[0].abs, elsewhere.canonicalize().unwrap());
    }

    #[test]
    fn selectors() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        for w in ["world", "world_nether", "creative", "worlds/lobby"] {
            mk_world(&s.join(w));
        }
        let all = ["creative", "world", "world_nether", "lobby"];
        let got = |x: &[&str]| sel(s, x).unwrap().into_iter().map(|w| w.name).collect::<Vec<_>>();
        assert_eq!(got(&[]), all);
        assert_eq!(got(&["*"]), all);
        assert_eq!(got(&["world_nether", "world"]), ["world_nether", "world"]);
        assert_eq!(got(&["*", "!creative"]), ["world", "world_nether", "lobby"]);
        assert_eq!(got(&["!creative", "!worlds/lobby"]), ["world", "world_nether"]);
        assert_eq!(got(&["worlds/lobby"]), ["lobby"]);
        assert_eq!(got(&["world", "*"]), ["world", "creative", "world_nether", "lobby"]);

        assert!(sel(s, &["nope"]).unwrap_err().to_string().contains("no world named"));
        assert!(sel(s, &["!nope"]).unwrap_err().to_string().contains("doesn't match"));
        assert!(sel(s, &["logs/x"]).unwrap_err().to_string().contains("not a world"));
        assert!(sel(s, &["../world"]).is_err());
        mk_world(&s.join("world/inner"));
        assert!(sel(s, &["world", "world/inner"]).unwrap_err().to_string().contains("inside"));
        assert!(
            sel(s, &["*", "!creative", "!world", "!world_nether", "!lobby"])
                .unwrap_err()
                .to_string()
                .contains("no worlds")
        );
    }

    #[test]
    fn dimensions() {
        assert_eq!(dimension("region/r.0.0.mca"), "overworld");
        assert_eq!(dimension("entities/r.0.0.mca"), "overworld");
        assert_eq!(dimension("DIM-1/region/r.0.0.mca"), "the_nether");
        assert_eq!(dimension("DIM1/poi/r.0.0.mca"), "the_end");
        assert_eq!(dimension("dimensions/twilight/forest/region/r.0.0.mca"), "twilight:forest");
        // The newer layout, where every dimension lives under dimensions/minecraft/.
        assert_eq!(dimension("dimensions/minecraft/overworld/region/r.0.0.mca"), "overworld");
        assert_eq!(dimension("dimensions/minecraft/the_nether/entities/r.0.0.mca"), "the_nether");
        assert_eq!(dimension("dimensions/minecraft/the_end/poi/r.0.0.mca"), "the_end");
        assert_eq!(normalize_dimension("minecraft:the_nether"), "the_nether");
        assert_eq!(normalize_dimension("end"), "the_end");
        assert_eq!(normalize_dimension("twilight:forest"), "twilight:forest");
    }

    #[test]
    fn same_name_twice_is_an_error() {
        let dir = tempfile::tempdir().unwrap();
        let s = dir.path();
        mk_world(&s.join("a/world1"));
        mk_world(&s.join("b/world1"));
        let err = sel(s, &[]).unwrap_err().to_string();
        assert!(err.contains("a/world1") && err.contains("b/world1"), "{err}");
        assert!(sel(s, &["world1"]).unwrap_err().to_string().contains("several"));
        assert_eq!(names(&sel(s, &["b/world1"]).unwrap()), [("world1", "b/world1")]);
    }

    #[test]
    fn empty_server_folder() {
        let dir = tempfile::tempdir().unwrap();
        assert!(sel(dir.path(), &[]).unwrap_err().to_string().contains("no worlds"));
    }

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
