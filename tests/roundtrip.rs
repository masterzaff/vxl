//! End-to-end tests: build a synthetic world, back it up, change it, restore, prune.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use indicatif::ProgressBar;
use tempfile::TempDir;
use vxl::anvil::{self, Compression, RawChunk};
use vxl::commands::{backup, list, prune, restore, verify};
use vxl::repo::{LockMode, ObjectId, Repo};
use vxl::synth;
use vxl::world::{self, Kind};

/// What a world "means": blobs byte-for-byte, region files chunk-by-chunk
/// (slot, timestamp, compression, decompressed NBT).
#[derive(Debug, PartialEq)]
enum Content {
    Blob(Vec<u8>),
    Region(Vec<(u16, u32, u8, Vec<u8>)>),
}

fn contents(dir: &Path) -> BTreeMap<String, Content> {
    let mut out = BTreeMap::new();
    for f in world::scan(dir).unwrap().files {
        let data = fs::read(&f.abs).unwrap();
        let c = match f.kind {
            Kind::External => continue, // covered by its region
            Kind::Region { coords } => {
                let d = f.abs.parent().unwrap();
                let Ok(p) = anvil::parse_region(&data, Some(coords), |n| fs::read(d.join(n))) else {
                    out.insert(f.rel, Content::Blob(data));
                    continue;
                };
                Content::Region(
                    p.chunks
                        .into_iter()
                        .map(|c| {
                            let nbt = anvil::decompress(c.compression, &c.payload).unwrap_or(c.payload);
                            (c.slot, c.timestamp, c.compression, nbt)
                        })
                        .collect(),
                )
            }
            _ => Content::Blob(data),
        };
        out.insert(f.rel, c);
    }
    out
}

struct Env {
    _tmp: TempDir,
    world: std::path::PathBuf,
    repo_dir: std::path::PathBuf,
    base: std::path::PathBuf,
}

fn setup() -> Env {
    let tmp = tempfile::tempdir().unwrap();
    let base = tmp.path().to_path_buf();
    let world = base.join("world");
    let repo_dir = base.join("repo");
    Repo::init(&repo_dir, 3).unwrap();
    Env { _tmp: tmp, world, repo_dir, base }
}

fn repo(env: &Env) -> Repo {
    Repo::open(&env.repo_dir, LockMode::Exclusive).unwrap()
}

fn hidden() -> ProgressBar {
    ProgressBar::hidden()
}

fn do_backup(env: &Env, msg: &str) -> backup::BackupReport {
    backup::backup(&repo(env), &env.world, Some(msg.into()), &hidden()).unwrap()
}

fn do_restore(env: &Env, id: &ObjectId, name: &str) -> std::path::PathBuf {
    let target = env.base.join(name);
    restore::restore(&repo(env), id, &target, &hidden()).unwrap();
    target
}

const REGIONS: &[(i32, i32)] = &[(0, 0), (-1, 0)];

#[test]
fn backup_is_incremental_and_restore_is_lossless() {
    let env = setup();
    synth::write_world(&env.world, 1, REGIONS, 120, Compression::Zlib, |_, _| 0).unwrap();

    let first = do_backup(&env, "initial");
    assert!(first.warnings.is_empty(), "{:?}", first.warnings);
    assert_eq!(first.stats.chunks, 240);
    assert_eq!(first.stats.new_chunks, 240);
    assert!(first.stats.stored_bytes > 0);

    // Nothing changed → nothing stored.
    let second = do_backup(&env, "no-op");
    assert_eq!(second.stats.new_chunks, 0);
    assert_eq!(second.stats.stored_bytes, 0);
    assert_ne!(first.id, second.id, "every backup is its own snapshot");

    // "Play" in 3 chunks.
    let edited = |cx: i32, cz: i32| if cz == 0 && (0..3).contains(&cx) { 5 } else { 0 };
    let before = contents(&env.world);
    synth::write_world(&env.world, 1, REGIONS, 120, Compression::Zlib, edited).unwrap();
    let third = do_backup(&env, "edited");
    assert_eq!(third.stats.new_chunks, 3);
    assert!(third.stats.stored_bytes < first.stats.stored_bytes / 20);

    // Both versions restore exactly. session.lock is never backed up.
    let mut live = contents(&env.world);
    live.remove("session.lock");
    let mut old = before;
    old.remove("session.lock");
    assert_eq!(contents(&do_restore(&env, &third.id, "r3")), live);
    assert_eq!(contents(&do_restore(&env, &first.id, "r1")), old);
    assert!(!env.base.join("r3/session.lock").exists());

    let snaps = list::list(&repo(&env)).unwrap();
    let msgs: Vec<_> = snaps.iter().map(|(_, h)| h.message.clone().unwrap()).collect();
    assert_eq!(msgs, ["edited", "no-op", "initial"]);
}

#[test]
fn every_compression_type_restores_as_itself() {
    for comp in [Compression::Gzip, Compression::Zlib, Compression::None, Compression::Lz4] {
        let env = setup();
        synth::write_world(&env.world, 2, &[(3, -2)], 40, comp, |_, _| 0).unwrap();
        let r = do_backup(&env, "x");
        let out = do_restore(&env, &r.id, "out");
        let mut live = contents(&env.world);
        live.remove("session.lock");
        assert_eq!(contents(&out), live, "{comp:?}");
        let Content::Region(chunks) = &contents(&out)["region/r.3.-2.mca"] else { panic!() };
        assert!(chunks.iter().all(|c| c.2 == comp.to_byte()));
    }
}

fn write_region(path: &Path, coords: (i32, i32), chunks: &[RawChunk]) {
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let built = anvil::build_region(chunks, Some(coords)).unwrap();
    fs::write(path, built.region).unwrap();
    for (name, bytes) in built.external {
        fs::write(path.parent().unwrap().join(name), bytes).unwrap();
    }
}

#[test]
fn odd_files_survive() {
    let env = setup();
    synth::write_world(&env.world, 3, &[], 0, Compression::Zlib, |_, _| 0).unwrap();

    // A chunk with garbage zlib data, plus a normal one.
    let good = anvil::compress(Compression::Zlib, &synth::chunk_nbt(3, 0, 0, 0)).unwrap();
    let chunks = [
        RawChunk { slot: 0, timestamp: 5, compression: 2, external: false, payload: good },
        RawChunk { slot: 1, timestamp: 6, compression: 2, external: false, payload: b"not zlib".to_vec() },
    ];
    write_region(&env.world.join("region/r.0.0.mca"), (0, 0), &chunks);

    // A >1 MiB chunk that must live in an external .mcc file.
    let mut x = 9u64;
    let noise: Vec<u8> = (0..1_500_000)
        .map(|_| {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            x as u8
        })
        .collect();
    let big = [RawChunk { slot: 33, timestamp: 7, compression: 3, external: false, payload: noise }];
    write_region(&env.world.join("DIM1/region/r.-1.-1.mca"), (-1, -1), &big);
    assert!(env.world.join("DIM1/region/c.-31.-31.mcc").exists());

    // An empty region file (Minecraft makes these) and a truncated one.
    fs::write(env.world.join("region/r.5.5.mca"), b"").unwrap();
    fs::write(env.world.join("region/r.6.6.mca"), b"short").unwrap();

    let r = do_backup(&env, "odd");
    assert_eq!(r.warnings.len(), 2, "{:?}", r.warnings);
    assert!(r.warnings.iter().any(|w| w.contains("r.0.0.mca") && w.contains("raw bytes")));
    assert!(r.warnings.iter().any(|w| w.contains("r.6.6.mca") && w.contains("plain file")));

    let out = do_restore(&env, &r.id, "out");
    let mut live = contents(&env.world);
    live.remove("session.lock");
    assert_eq!(contents(&out), live);
    // the broken chunk comes back byte-identical
    let data = fs::read(out.join("region/r.0.0.mca")).unwrap();
    let parsed = anvil::parse_region(&data, Some((0, 0)), |_| unreachable!()).unwrap();
    assert_eq!(parsed.chunks[1].payload, b"not zlib");
    assert!(out.join("DIM1/region/c.-31.-31.mcc").exists());
}

#[test]
fn prune_keeps_what_is_needed() {
    let env = setup();
    let mut ids = vec![];
    for round in 0..4u32 {
        let edit = move |cx: i32, _cz: i32| if cx == round as i32 { round + 1 } else { 0 };
        synth::write_world(&env.world, 4, &[(0, 0)], 50, Compression::Zlib, edit).unwrap();
        ids.push(do_backup(&env, &format!("round {round}")).id);
    }
    let objects_before = repo(&env).list_objects().unwrap().len();

    let dry = prune::prune(&repo(&env), Some(2), true).unwrap();
    assert_eq!(dry.removed_snapshots.len(), 2);
    assert_eq!(repo(&env).list_objects().unwrap().len(), objects_before);

    let r = prune::prune(&repo(&env), Some(2), false).unwrap();
    assert_eq!(r.removed_snapshots, dry.removed_snapshots);
    assert!(r.deleted_objects > 0 && r.freed_bytes > 0);
    assert_eq!(repo(&env).snapshot_ids().unwrap().len(), 2);

    // Newest two still verify and restore; the old ones are gone.
    let v = verify::verify(&repo(&env), &[ids[2], ids[3]], &hidden()).unwrap();
    assert!(v.problems.is_empty());
    let mut live = contents(&env.world);
    live.remove("session.lock");
    assert_eq!(contents(&do_restore(&env, &ids[3], "out")), live);
    assert!(repo(&env).resolve(&ids[0].hex()[..8]).is_err());

    // A second prune finds nothing.
    let again = prune::prune(&repo(&env), Some(2), false).unwrap();
    assert_eq!(again.deleted_objects, 0);
}

#[test]
fn verify_catches_damage_and_restore_refuses_non_empty_target() {
    let env = setup();
    synth::write_world(&env.world, 5, &[(0, 0)], 10, Compression::Zlib, |_, _| 0).unwrap();
    let id = do_backup(&env, "x").id;

    let busy = env.base.join("busy");
    fs::create_dir_all(&busy).unwrap();
    fs::write(busy.join("keep.txt"), b"mine").unwrap();
    assert!(restore::restore(&repo(&env), &id, &busy, &hidden()).is_err());
    assert_eq!(fs::read(busy.join("keep.txt")).unwrap(), b"mine");

    let (victim, _) = repo(&env).list_objects().unwrap()[0];
    let hex = victim.hex();
    let path = env.repo_dir.join("objects").join(&hex[..2]).join(&hex[2..]);
    fs::write(&path, zstd::bulk::compress(b"bitrot", 3).unwrap()).unwrap();
    let v = verify::verify(&repo(&env), &[id], &hidden()).unwrap();
    assert_eq!(v.problems.len(), 1);
    assert_eq!(v.problems[0].0, victim);
}

#[test]
fn refuses_repo_inside_world() {
    let tmp = tempfile::tempdir().unwrap();
    let world = tmp.path().join("world");
    synth::write_world(&world, 6, &[], 0, Compression::Zlib, |_, _| 0).unwrap();
    let repo_dir = world.join("backups");
    Repo::init(&repo_dir, 3).unwrap();
    let repo = Repo::open(&repo_dir, LockMode::Exclusive).unwrap();
    let err = backup::backup(&repo, &world, None, &hidden()).err().unwrap();
    assert!(err.to_string().contains("inside the world"));
}

#[test]
fn cli_smoke() {
    let tmp = tempfile::tempdir().unwrap();
    let world = tmp.path().join("world");
    let repo = tmp.path().join("repo");
    synth::write_world(&world, 7, &[(0, 0)], 30, Compression::Zlib, |_, _| 0).unwrap();

    let vxl = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_vxl")).args(args).env("VXL_REPO", &repo).output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        stdout
    };
    vxl(&["init", repo.to_str().unwrap()]);
    let out = vxl(&["backup", world.to_str().unwrap(), "-m", "before nether update"]);
    assert!(out.starts_with("snapshot ") && out.contains("| 30 chunks | 30 new | +"), "{out}");
    let id = &out["snapshot ".len()..][..6];

    let listing = vxl(&["list"]);
    assert!(listing.starts_with(id) && listing.contains("before nether update"), "{listing}");

    let target = tmp.path().join("restored");
    vxl(&["restore", &id[..4], target.to_str().unwrap()]);
    assert!(target.join("region/r.0.0.mca").exists());
    assert!(vxl(&["verify"]).contains("all OK"));
    assert!(vxl(&["prune", "--keep-last", "1"]).contains("removed 0 snapshots"));
}
