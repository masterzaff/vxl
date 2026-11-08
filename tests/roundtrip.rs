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
    Repo::init(&repo_dir, 3, TEST_PACK_SIZE).unwrap();
    Env { _tmp: tmp, world, repo_dir, base }
}

fn repo(env: &Env) -> Repo {
    Repo::open(&env.repo_dir, LockMode::Exclusive).unwrap()
}

fn hidden() -> ProgressBar {
    ProgressBar::hidden()
}

fn do_backup(env: &Env, msg: &str) -> backup::BackupReport {
    backup::backup(&repo(env), &env.world, &[], Some(msg.into()), &hidden()).unwrap()
}

fn do_restore(env: &Env, id: &ObjectId, name: &str) -> std::path::PathBuf {
    let target = env.base.join(name);
    restore::restore(&repo(env), id, &target, None, restore::Mode::Fresh, &hidden()).unwrap();
    target
}

const REGIONS: &[(i32, i32)] = &[(0, 0), (-1, 0)];
/// Small packs so tests exercise pack rollover and repacking.
const TEST_PACK_SIZE: u64 = 256 << 10;

/// Flip one byte inside an object's compressed data, in its pack file.
fn damage(env: &Env, id: &ObjectId) {
    let (path, offset) = repo(env).locate(id).unwrap();
    let mut bytes = fs::read(&path).unwrap();
    bytes[offset as usize + 20] ^= 0xFF;
    fs::write(&path, bytes).unwrap();
}

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
    let objects_before = repo(&env).list_objects().len();

    let dry = prune::prune(&repo(&env), Some(2), true).unwrap();
    assert_eq!(dry.removed_snapshots.len(), 2);
    assert_eq!(repo(&env).list_objects().len(), objects_before);

    let packs_before = repo(&env).packs();
    let disk_before: u64 = packs_before.iter().map(|p| p.size).sum();
    let r = prune::prune(&repo(&env), Some(2), false).unwrap();
    assert_eq!(r.removed_snapshots, dry.removed_snapshots);
    assert!(r.deleted_objects > 0 && r.freed_bytes > 0);
    assert!(r.packs_repacked + r.packs_deleted > 0, "{r:?}");
    let disk_after: u64 = repo(&env).packs().iter().map(|p| p.size).sum();
    assert_eq!(disk_before - disk_after, r.freed_bytes);
    assert_eq!(repo(&env).list_objects().len() as u64, objects_before as u64 - r.deleted_objects);
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
    assert_eq!(again.packs_deleted, 0);
}

#[test]
fn verify_catches_damage_and_restore_refuses_non_empty_target() {
    let env = setup();
    synth::write_world(&env.world, 5, &[(0, 0)], 10, Compression::Zlib, |_, _| 0).unwrap();
    let id = do_backup(&env, "x").id;

    let busy = env.base.join("busy");
    fs::create_dir_all(&busy).unwrap();
    fs::write(busy.join("keep.txt"), b"mine").unwrap();
    assert!(restore::restore(&repo(&env), &id, &busy, None, restore::Mode::Fresh, &hidden()).is_err());
    assert_eq!(fs::read(busy.join("keep.txt")).unwrap(), b"mine");

    let (victim, _) = repo(&env).list_objects().into_iter().max_by_key(|o| o.1).unwrap();
    damage(&env, &victim);
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
    Repo::init(&repo_dir, 3, TEST_PACK_SIZE).unwrap();
    let repo = Repo::open(&repo_dir, LockMode::Exclusive).unwrap();
    let err = backup::backup(&repo, &world, &[], None, &hidden()).err().unwrap();
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

/// A world that has moved on since the snapshot: edited chunks, a newer
/// level.dat, and a region + file the snapshot never had.
fn diverge(env: &Env) {
    let edited = |cx: i32, _cz: i32| if cx < 4 { 9 } else { 0 };
    synth::write_world(&env.world, 8, &[(0, 0), (1, 0)], 20, Compression::Zlib, edited).unwrap();
    fs::write(env.world.join("level.dat"), b"newer level.dat").unwrap();
    fs::write(env.world.join("notes.txt"), b"added after the snapshot").unwrap();
}

#[test]
fn force_replace_overwrites_but_keeps_extras() {
    let env = setup();
    synth::write_world(&env.world, 8, &[(0, 0)], 20, Compression::Zlib, |_, _| 0).unwrap();
    let mut snap = contents(&env.world);
    snap.remove("session.lock");
    let id = do_backup(&env, "x").id;
    diverge(&env);

    let r = restore::restore(&repo(&env), &id, &env.world, None, restore::Mode::Replace, &hidden()).unwrap();
    assert_eq!(r.replaced, 3); // level.dat, playerdata, region/r.0.0.mca
    let now = contents(&env.world);
    for (path, c) in &snap {
        assert_eq!(&now[path], c, "{path} should be back to the snapshot");
    }
    // Files the snapshot doesn't have are left alone.
    assert!(now.contains_key("notes.txt") && now.contains_key("region/r.1.0.mca"));
    assert!(env.world.join("session.lock").exists());
    assert_no_staging(&env.world);
}

#[test]
fn force_wipe_makes_target_exactly_the_snapshot() {
    let env = setup();
    synth::write_world(&env.world, 8, &[(0, 0)], 20, Compression::Zlib, |_, _| 0).unwrap();
    let mut snap = contents(&env.world);
    snap.remove("session.lock");
    let id = do_backup(&env, "x").id;
    diverge(&env);
    fs::create_dir_all(env.world.join("stats/deep")).unwrap();
    fs::write(env.world.join("stats/deep/x.json"), b"{}").unwrap();

    let r = restore::restore(&repo(&env), &id, &env.world, None, restore::Mode::Wipe, &hidden()).unwrap();
    assert!(r.wiped > 0);
    assert_eq!(contents(&env.world), snap);
    assert!(!env.world.join("stats").exists());
    assert_no_staging(&env.world);
}

#[test]
fn force_refuses_non_worlds_and_failed_restores_change_nothing() {
    let env = setup();
    synth::write_world(&env.world, 8, &[(0, 0)], 20, Compression::Zlib, |_, _| 0).unwrap();
    let id = do_backup(&env, "x").id;

    // Not a world → refused, untouched, for both modes.
    let home = env.base.join("not-a-world");
    fs::create_dir_all(&home).unwrap();
    fs::write(home.join("thesis.docx"), b"precious").unwrap();
    for mode in [restore::Mode::Replace, restore::Mode::Wipe] {
        let err = restore::restore(&repo(&env), &id, &home, None, mode, &hidden()).err().unwrap();
        assert!(err.to_string().contains("no level.dat"), "{err}");
        assert_eq!(fs::read(home.join("thesis.docx")).unwrap(), b"precious");
    }

    // Damage one object: the restore fails while staging, and the live world
    // must be exactly as it was (and no staging folder left behind).
    diverge(&env);
    let before = contents(&env.world);
    let (victim, _) = repo(&env).list_objects().into_iter().max_by_key(|o| o.1).unwrap();
    damage(&env, &victim);
    let err =
        restore::restore(&repo(&env), &id, &env.world, None, restore::Mode::Wipe, &hidden()).err().unwrap();
    assert!(format!("{err:#}").contains("was not changed"), "{err:#}");
    assert_eq!(contents(&env.world), before);
    assert_no_staging(&env.world);
}

#[test]
fn staging_leftovers_are_not_backed_up() {
    let env = setup();
    synth::write_world(&env.world, 8, &[], 0, Compression::Zlib, |_, _| 0).unwrap();
    let stale = env.world.join(".vxl-staging-123/region");
    fs::create_dir_all(&stale).unwrap();
    fs::write(stale.join("r.0.0.mca"), b"").unwrap();
    assert!(world::scan(&env.world).unwrap().files.iter().all(|f| !f.rel.contains("vxl-staging")));
}

fn assert_no_staging(dir: &Path) {
    let left: Vec<_> = fs::read_dir(dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter(|n| n.starts_with(".vxl-staging"))
        .collect();
    assert!(left.is_empty(), "staging left behind: {left:?}");
}

#[test]
fn packs_from_a_crashed_backup_are_collected() {
    let env = setup();
    synth::write_world(&env.world, 9, &[(0, 0)], 30, Compression::Zlib, |_, _| 0).unwrap();
    let id = do_backup(&env, "good").id;
    let good_packs = repo(&env).packs().len();

    // A backup that wrote packs and then died before its manifest.
    {
        let r = repo(&env);
        for i in 0..50u32 {
            r.put(&synth::chunk_nbt(9, 100 + i as i32, 0, 1)).unwrap();
        }
        r.flush().unwrap();
    }
    assert!(repo(&env).packs().len() > good_packs);

    let p = prune::prune(&repo(&env), None, false).unwrap();
    assert_eq!(p.deleted_objects, 50);
    assert_eq!(repo(&env).packs().len(), good_packs);
    let v = verify::verify(&repo(&env), &[id], &hidden()).unwrap();
    assert!(v.problems.is_empty());
}

#[test]
fn compare_confirms_restores_and_catches_differences() {
    use vxl::commands::compare::compare;
    let env = setup();
    synth::write_world(&env.world, 10, &[(0, 0), (-1, 2)], 60, Compression::Zlib, |_, _| 0).unwrap();
    let id = do_backup(&env, "x").id;
    let out = do_restore(&env, &id, "out");

    // A faithful restore: same world, readable layout (session.lock is ignored).
    let r = compare(&env.world, &out).unwrap();
    assert!(r.identical(), "{:?} {:?}", r.differences, r.layout_problems);
    assert_eq!((r.regions, r.chunks, r.same_chunks), (2, 120, 120));

    // Change one chunk's data and one file, drop a file: all three are reported.
    let edited = |cx: i32, cz: i32| if (cx, cz) == (3, 0) { 7 } else { 0 };
    synth::write_world(&out, 10, &[(0, 0)], 60, Compression::Zlib, edited).unwrap();
    fs::remove_file(out.join("region/r.-1.2.mca")).unwrap();
    fs::write(out.join("level.dat"), b"nope").unwrap();
    let r = compare(&env.world, &out).unwrap();
    assert!(
        r.differences.iter().any(|d| d.contains("region/r.0.0.mca: chunk (3,0): data, timestamp differ"))
    );
    assert!(r.differences.iter().any(|d| d.contains("r.-1.2.mca: missing")));
    assert!(r.differences.iter().any(|d| d.starts_with("level.dat")));
    assert_eq!(r.differences.len(), 3, "{:?}", r.differences);

    // Overlapping sectors are a layout problem even if the data reads fine.
    let path = out.join("region/r.0.0.mca");
    let mut bytes = fs::read(&path).unwrap();
    let first = bytes[0..4].to_vec();
    bytes[4..8].copy_from_slice(&first); // slot 1 now points at slot 0's sectors
    fs::write(&path, bytes).unwrap();
    let r = compare(&env.world, &out).unwrap();
    assert!(r.layout_problems.iter().any(|p| p.contains("share sectors")), "{:?}", r.layout_problems);
}

/// A Paper-style server folder: three worlds side by side, one under `worlds/`,
/// plus things that aren't worlds (including a plugin's world copy).
fn server(env: &Env) -> std::path::PathBuf {
    let s = env.base.join("server");
    synth::write_world(&s.join("world"), 20, &[(0, 0)], 30, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&s.join("world_nether"), 21, &[(0, 0)], 20, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&s.join("worlds/lobby"), 22, &[(-1, 0)], 10, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&s.join("plugins/Backup/world"), 23, &[(0, 0)], 40, Compression::Zlib, |_, _| 0)
        .unwrap();
    fs::create_dir_all(s.join("logs")).unwrap();
    fs::write(s.join("server.properties"), b"motd=hi").unwrap();
    s
}

fn without_lock(mut c: BTreeMap<String, Content>) -> BTreeMap<String, Content> {
    c.remove("session.lock");
    c
}

#[test]
fn server_folder_backs_up_every_world_as_one_snapshot() {
    let env = setup();
    let s = server(&env);
    let r = backup::backup(&repo(&env), &s, &[], Some("all".into()), &hidden()).unwrap();
    assert!(r.warnings.is_empty(), "{:?}", r.warnings);
    let names: Vec<_> = r.worlds.iter().map(|w| (w.name.as_str(), w.rel.as_str())).collect();
    assert_eq!(names, [("world", "world"), ("world_nether", "world_nether"), ("lobby", "worlds/lobby")]);
    assert_eq!(r.stats.chunks, 60, "the plugin's copy is not backed up");

    let h = repo(&env).load_header(&r.id).unwrap();
    assert_eq!(h.world_names, ["world", "world_nether", "lobby"]);

    // Everything: each world where it was in the server folder, and nothing else.
    let out = env.base.join("out");
    let rep = restore::restore(&repo(&env), &r.id, &out, None, restore::Mode::Fresh, &hidden()).unwrap();
    assert_eq!(rep.worlds.len(), 3);
    for rel in ["world", "world_nether", "worlds/lobby"] {
        assert_eq!(contents(&out.join(rel)), without_lock(contents(&s.join(rel))), "{rel}");
    }
    assert!(!out.join("server.properties").exists() && !out.join("plugins").exists());

    // One world, as a standalone world folder.
    let lobby = env.base.join("just-lobby");
    let rep =
        restore::restore(&repo(&env), &r.id, &lobby, Some("lobby"), restore::Mode::Fresh, &hidden()).unwrap();
    assert_eq!(rep.worlds, [("lobby".to_string(), lobby.clone())]);
    assert_eq!(contents(&lobby), without_lock(contents(&s.join("worlds/lobby"))));

    let err = restore::restore(
        &repo(&env),
        &r.id,
        &env.base.join("x"),
        Some("nope"),
        restore::Mode::Fresh,
        &hidden(),
    )
    .err()
    .unwrap();
    assert!(err.to_string().contains("has no world \"nope\""), "{err}");

    // Selecting fewer worlds.
    let some = ["*".to_string(), "!lobby".to_string()];
    let r2 = backup::backup(&repo(&env), &s, &some, None, &hidden()).unwrap();
    assert_eq!(repo(&env).load_header(&r2.id).unwrap().world_names, ["world", "world_nether"]);
    assert_eq!(r2.stats.new_chunks, 0);
}

#[test]
fn copied_worlds_are_stored_once() {
    let env = setup();
    let s = env.base.join("server");
    synth::write_world(&s.join("arena1"), 30, &[(0, 0)], 50, Compression::Zlib, |_, _| 0).unwrap();
    let one = backup::backup(&repo(&env), &s, &[], None, &hidden()).unwrap();
    synth::write_world(&s.join("arena2"), 30, &[(0, 0)], 50, Compression::Zlib, |_, _| 0).unwrap();
    let two = backup::backup(&repo(&env), &s, &[], None, &hidden()).unwrap();
    assert_eq!(two.stats.chunks, 100);
    assert_eq!(two.stats.new_chunks, 0, "arena2 is a copy of arena1");
    assert!(two.stats.stored_bytes < one.stats.stored_bytes / 20);
}

#[test]
fn restoring_a_server_over_itself_touches_only_worlds() {
    let env = setup();
    let s = server(&env);
    let id = backup::backup(&repo(&env), &s, &[], None, &hidden()).unwrap().id;
    let before: Vec<_> =
        ["world", "world_nether", "worlds/lobby"].map(|w| without_lock(contents(&s.join(w)))).into();

    // The server moves on: a new region in one world, a new plugin file.
    synth::write_world(&s.join("world_nether"), 21, &[(0, 0), (3, 3)], 20, Compression::Zlib, |_, _| 7)
        .unwrap();
    fs::write(s.join("plugins/new.jar"), b"jar").unwrap();

    // Roll the whole server back in place.
    let r = restore::restore(&repo(&env), &id, &s, None, restore::Mode::Wipe, &hidden()).unwrap();
    assert_eq!(r.worlds.len(), 3);
    let all: Vec<_> =
        ["world", "world_nether", "worlds/lobby"].map(|w| without_lock(contents(&s.join(w)))).into();
    assert_eq!(all, before);
    assert_eq!(fs::read(s.join("plugins/new.jar")).unwrap(), b"jar");
    assert_eq!(fs::read(s.join("server.properties")).unwrap(), b"motd=hi");
}

#[test]
fn a_failed_multi_world_restore_changes_nothing() {
    let env = setup();
    let s = env.base.join("server");
    synth::write_world(&s.join("a"), 40, &[(0, 0)], 20, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&s.join("b"), 41, &[(0, 0)], 20, Compression::Zlib, |_, _| 0).unwrap();
    let id = backup::backup(&repo(&env), &s, &[], None, &hidden()).unwrap().id;

    // Both worlds move on, then one object of `b` gets damaged.
    synth::write_world(&s.join("a"), 40, &[(0, 0)], 20, Compression::Zlib, |_, _| 3).unwrap();
    synth::write_world(&s.join("b"), 41, &[(0, 0)], 20, Compression::Zlib, |_, _| 3).unwrap();
    let before = (contents(&s.join("a")), contents(&s.join("b")));
    let snap = repo(&env).load_snapshot(&id).unwrap();
    let victim = snap.world("b").unwrap().files.iter().flat_map(|f| f.objects()).last().unwrap();
    damage(&env, &victim);

    let err = restore::restore(&repo(&env), &id, &s, None, restore::Mode::Wipe, &hidden()).err().unwrap();
    assert!(format!("{err:#}").contains("were not changed"), "{err:#}");
    assert_eq!((contents(&s.join("a")), contents(&s.join("b"))), before, "`a` must not be restored alone");
    assert_no_staging(&s.join("a"));
    assert_no_staging(&s.join("b"));
}

#[test]
fn cli_server_folder() {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("server");
    let repo = tmp.path().join("repo");
    synth::write_world(&s.join("world"), 50, &[(0, 0)], 10, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&s.join("world_the_end"), 51, &[(0, 0)], 5, Compression::Zlib, |_, _| 0).unwrap();
    let vxl = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_vxl")).args(args).env("VXL_REPO", &repo).output().unwrap();
        let stdout = String::from_utf8(out.stdout).unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        stdout
    };
    vxl(&["init", repo.to_str().unwrap()]);
    let out = vxl(&["backup", s.to_str().unwrap()]);
    assert!(out.starts_with("worlds: world, world_the_end\nsnapshot "), "{out}");
    assert!(out.contains("| 15 chunks |"), "{out}");
    let out = vxl(&["backup", s.to_str().unwrap(), "-w", "world_the_end"]);
    assert!(out.contains("| 5 chunks |"), "{out}");
    let listing = vxl(&["list"]);
    assert!(
        listing.contains("world, world_the_end")
            && listing.lines().next().unwrap().ends_with("world_the_end")
    );

    let id = &listing[..6];
    let end = tmp.path().join("end");
    let out = vxl(&["restore", id, end.to_str().unwrap(), "--world", "world_the_end"]);
    assert!(out.contains(&format!("into {} |", end.display())), "{out}");
    assert!(end.join("level.dat").exists() && end.join("region/r.0.0.mca").exists());
}
