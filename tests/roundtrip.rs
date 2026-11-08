//! End-to-end tests: build a synthetic world, back it up, change it, restore, prune.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;
use std::process::Command;

use indicatif::ProgressBar;
use tempfile::TempDir;
use vxl::anvil::{self, Compression, RawChunk};
use vxl::commands::{backup, list, partial, prune, restore, verify};
use vxl::repo::{LockMode, ObjectId, Repo};
use vxl::retention::Policy;
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

fn keep_last(n: u32) -> Policy {
    Policy { last: Some(n), ..Default::default() }
}

fn hidden() -> ProgressBar {
    ProgressBar::hidden()
}

fn do_backup(env: &Env, msg: &str) -> backup::BackupReport {
    backup::backup(
        &repo(env),
        &env.world,
        &backup::Options { message: Some(msg.into()), ..Default::default() },
        &hidden(),
    )
    .unwrap()
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

    let dry = prune::prune(&repo(&env), &keep_last(2), true).unwrap();
    assert_eq!(dry.removed_snapshots.len(), 2);
    assert_eq!(repo(&env).list_objects().len(), objects_before);

    let packs_before = repo(&env).packs();
    let disk_before: u64 = packs_before.iter().map(|p| p.size).sum();
    let r = prune::prune(&repo(&env), &keep_last(2), false).unwrap();
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
    let again = prune::prune(&repo(&env), &keep_last(2), false).unwrap();
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
    let err = backup::backup(&repo, &world, &backup::Options::default(), &hidden()).err().unwrap();
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

    let p = prune::prune(&repo(&env), &Policy::default(), false).unwrap();
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
    let r = backup::backup(
        &repo(&env),
        &s,
        &backup::Options { message: Some("all".into()), ..Default::default() },
        &hidden(),
    )
    .unwrap();
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
    let r2 = backup::backup(
        &repo(&env),
        &s,
        &backup::Options { worlds: some.to_vec(), ..Default::default() },
        &hidden(),
    )
    .unwrap();
    assert_eq!(repo(&env).load_header(&r2.id).unwrap().world_names, ["world", "world_nether"]);
    assert_eq!(r2.stats.new_chunks, 0);
}

#[test]
fn copied_worlds_are_stored_once() {
    let env = setup();
    let s = env.base.join("server");
    synth::write_world(&s.join("arena1"), 30, &[(0, 0)], 50, Compression::Zlib, |_, _| 0).unwrap();
    let one = backup::backup(&repo(&env), &s, &backup::Options::default(), &hidden()).unwrap();
    synth::write_world(&s.join("arena2"), 30, &[(0, 0)], 50, Compression::Zlib, |_, _| 0).unwrap();
    let two = backup::backup(&repo(&env), &s, &backup::Options::default(), &hidden()).unwrap();
    assert_eq!(two.stats.chunks, 100);
    assert_eq!(two.stats.new_chunks, 0, "arena2 is a copy of arena1");
    assert!(two.stats.stored_bytes < one.stats.stored_bytes / 20);
}

#[test]
fn restoring_a_server_over_itself_touches_only_worlds() {
    let env = setup();
    let s = server(&env);
    let id = backup::backup(&repo(&env), &s, &backup::Options::default(), &hidden()).unwrap().id;
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
    let id = backup::backup(&repo(&env), &s, &backup::Options::default(), &hidden()).unwrap().id;

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

#[test]
fn cli_hooks() {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("server");
    let repo = tmp.path().join("repo");
    synth::write_world(&s.join("world"), 60, &[(0, 0)], 5, Compression::Zlib, |_, _| 0).unwrap();
    let vxl = |args: &[&str]| {
        Command::new(env!("CARGO_BIN_EXE_vxl"))
            .args(args)
            .env("VXL_REPO", &repo)
            .current_dir(&s)
            .output()
            .unwrap()
    };
    let log = || {
        let l = fs::read_to_string(s.join("hooks.log")).unwrap_or_default();
        fs::remove_file(s.join("hooks.log")).ok();
        l.lines().map(|l| l.trim().to_string()).collect::<Vec<_>>()
    };
    let src = s.to_str().unwrap();
    assert!(vxl(&["init", repo.to_str().unwrap()]).status.success());

    // Hooks run in vxl's working directory, around the backup.
    let out = vxl(&["backup", src, "--pre", "echo pre>> hooks.log", "--post", "echo post>> hooks.log"]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert_eq!(log(), ["pre", "post"]);
    assert_eq!(list::list(&Repo::open(&repo, LockMode::None).unwrap()).unwrap().len(), 1);

    // A failing pre hook: no snapshot, post still runs, non-zero exit.
    let out = vxl(&["backup", src, "--pre", "exit 4", "--post", "echo post>> hooks.log"]);
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("pre hook failed"));
    assert_eq!(log(), ["post"]);
    assert_eq!(list::list(&Repo::open(&repo, LockMode::None).unwrap()).unwrap().len(), 1);

    // A failing backup (no such world) still runs post.
    let out = vxl(&["backup", src, "-w", "nope", "--post", "echo post>> hooks.log"]);
    assert!(!out.status.success());
    assert_eq!(log(), ["post"]);

    // A failing post hook: the snapshot is kept, but the exit code says something's wrong.
    let out = vxl(&["backup", src, "--post", "exit 1"]);
    assert!(!out.status.success());
    let err = String::from_utf8_lossy(&out.stderr);
    assert!(err.contains("post hook failed") && err.contains("snapshot itself was saved"), "{err}");
    assert_eq!(list::list(&Repo::open(&repo, LockMode::None).unwrap()).unwrap().len(), 2);

    // The pre hook may create the source (like a ZFS snapshot); post gets the snapshot id.
    let frozen = tmp.path().join("frozen");
    let pre = if cfg!(windows) {
        format!("xcopy /E /I /Q \"{}\" \"{}\"", s.join("world").display(), frozen.display())
    } else {
        format!("cp -r '{}' '{}'", s.join("world").display(), frozen.display())
    };
    let post =
        if cfg!(windows) { "echo %VXL_SNAPSHOT%>> hooks.log" } else { "echo $VXL_SNAPSHOT >> hooks.log" };
    let out = vxl(&["backup", frozen.to_str().unwrap(), "--pre", &pre, "--post", post]);
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let id = log();
    assert_eq!(id.len(), 1);
    assert_eq!(id[0].len(), 64);
    assert!(String::from_utf8_lossy(&out.stdout).contains(&id[0][..6]));
}

#[test]
#[cfg(unix)]
fn cli_hooks_survive_sigint() {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("server");
    let repo = tmp.path().join("repo");
    synth::write_world(&s.join("world"), 61, &[(0, 0)], 5, Compression::Zlib, |_, _| 0).unwrap();
    Repo::init(&repo, 3, TEST_PACK_SIZE).unwrap();

    // The pre hook "hangs" (like a slow save-all); interrupt vxl while it does.
    // vxl gets its own process group, like a job a terminal starts.
    use std::os::unix::process::CommandExt;
    let mut child = Command::new(env!("CARGO_BIN_EXE_vxl"))
        .process_group(0)
        .current_dir(&s)
        .args(["backup", s.to_str().unwrap(), "--pre", "echo pre>> hooks.log; sleep 30"])
        .args(["--post", r#"echo "post $VXL_RESULT" >> hooks.log"#])
        .env("VXL_REPO", &repo)
        .spawn()
        .unwrap();
    let started = std::time::Instant::now();
    while !s.join("hooks.log").exists() {
        assert!(started.elapsed().as_secs() < 10, "pre hook never started");
        std::thread::sleep(std::time::Duration::from_millis(20));
    }
    // (vxl installs its handler before starting the pre hook, so it's in place now.)
    // Like a terminal's Ctrl-C: signal the whole group, vxl and its pre hook alike.
    assert!(
        Command::new("kill").args(["-INT", "--", &format!("-{}", child.id())]).status().unwrap().success()
    );
    let st = child.wait().unwrap();
    assert!(started.elapsed().as_secs() < 20, "vxl waited for the pre hook instead of stopping");
    assert_eq!(st.code(), Some(130));
    let log = fs::read_to_string(s.join("hooks.log")).unwrap();
    assert_eq!(log.lines().collect::<Vec<_>>(), ["pre", "post interrupted"]);
    assert!(list::list(&Repo::open(&repo, LockMode::None).unwrap()).unwrap().is_empty());
}

#[test]
fn delete_removes_exactly_those_snapshots() {
    let env = setup();
    let mut ids = vec![];
    for round in 0..3u32 {
        let edit = move |cx: i32, _cz: i32| if cx == round as i32 { round + 1 } else { 0 };
        synth::write_world(&env.world, 70, &[(0, 0)], 40, Compression::Zlib, edit).unwrap();
        ids.push(do_backup(&env, &format!("round {round}")).id);
    }
    let objects_before = repo(&env).list_objects().len();

    let dry = prune::delete(&repo(&env), &[ids[1]], true).unwrap();
    assert_eq!(dry.removed_snapshots, [ids[1]]);
    assert_eq!(repo(&env).snapshot_ids().unwrap().len(), 3);

    let r = prune::delete(&repo(&env), &[ids[1]], false).unwrap();
    assert_eq!(r.removed_snapshots, [ids[1]]);
    assert_eq!(r.kept_snapshots, 2);
    // Round 1's edited chunk was used only by that snapshot.
    assert!(r.deleted_objects >= 1, "{r:?}");
    let mut left = repo(&env).snapshot_ids().unwrap();
    left.sort();
    let mut want = vec![ids[0], ids[2]];
    want.sort();
    assert_eq!(left, want);
    assert!(verify::verify(&repo(&env), &want, &hidden()).unwrap().problems.is_empty());
    assert!(repo(&env).list_objects().len() < objects_before);
}

#[test]
fn cli_prune_policies_and_delete() {
    let tmp = tempfile::tempdir().unwrap();
    let world = tmp.path().join("world");
    let repo = tmp.path().join("repo");
    let vxl = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_vxl")).args(args).env("VXL_REPO", &repo).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    vxl(&["init", repo.to_str().unwrap()]);
    let mut ids = vec![];
    for round in 0..4u32 {
        let edit = move |cx: i32, _cz: i32| if cx == round as i32 { round + 1 } else { 0 };
        synth::write_world(&world, 71, &[(0, 0)], 10, Compression::Zlib, edit).unwrap();
        let out = vxl(&["backup", world.to_str().unwrap()]);
        ids.push(out["snapshot ".len()..][..6].to_string());
    }

    // All four were made today: keep-daily 1 keeps only the newest, keep-last 2 one more.
    let out = vxl(&["prune", "--keep-daily", "1", "--keep-last", "2", "--dry-run"]);
    assert!(out.contains(&format!("would keep   {} (last, daily)", ids[3])), "{out}");
    assert!(out.contains(&format!("would keep   {} (last)", ids[2])), "{out}");
    assert!(out.contains(&format!("would remove snapshot {}", ids[0])), "{out}");
    assert!(out.contains("would remove 2 snapshots, kept 2"), "{out}");

    let out = vxl(&["delete", &ids[0], &ids[1]]);
    assert!(out.contains("removed 2 snapshots, kept 2"), "{out}");
    let listing = vxl(&["list"]);
    assert!(!listing.contains(&ids[0]) && listing.contains(&ids[3]), "{listing}");

    // Without rules, prune deletes nothing.
    assert!(vxl(&["prune"]).contains("removed 0 snapshots, kept 2"));
    let bad = Command::new(env!("CARGO_BIN_EXE_vxl"))
        .args(["delete", "zzzz"])
        .env("VXL_REPO", &repo)
        .output()
        .unwrap();
    assert!(!bad.status.success());
}

#[test]
fn cli_config_file() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path();
    synth::write_world(&dir.join("server/world"), 80, &[(0, 0)], 10, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&dir.join("server/creative"), 81, &[(0, 0)], 10, Compression::Zlib, |_, _| 0).unwrap();
    // Relative paths are relative to the file.
    fs::write(
        dir.join("vxl.toml"),
        r#"
source = "server"
repo = "repo"
worlds = ["*", "!creative"]

[hooks]
post = "echo post>> hooks.log"

[keep]
last = 1
"#,
    )
    .unwrap();
    let vxl = |cwd: &Path, args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_vxl"))
            .args(args)
            .current_dir(cwd)
            .env_remove("VXL_REPO")
            .env_remove("VXL_CONFIG")
            .output()
            .unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };

    // ./vxl.toml is picked up: no arguments needed at all.
    vxl(dir, &["init"]);
    assert!(dir.join("repo/config.json").exists());
    let out = vxl(dir, &["backup"]);
    assert!(out.starts_with("worlds: world\n") && out.contains("| 10 chunks |"), "{out}");
    assert_eq!(fs::read_to_string(dir.join("hooks.log")).unwrap().trim(), "post");
    vxl(dir, &["backup", "-m", "second"]);

    // [keep] last = 1 applies…
    let out = vxl(dir, &["prune", "--dry-run"]);
    assert!(out.contains("would remove 1 snapshots, kept 1"), "{out}");
    // …unless the command line has rules of its own.
    let out = vxl(dir, &["prune", "--keep-last", "5", "--dry-run"]);
    assert!(out.contains("would remove 0 snapshots, kept 2"), "{out}");

    // Command-line options win over the file; --config works from anywhere.
    let cfg = dir.join("vxl.toml");
    let elsewhere = tmp.path().join("elsewhere");
    fs::create_dir_all(&elsewhere).unwrap();
    let out = vxl(&elsewhere, &["--config", cfg.to_str().unwrap(), "backup", "-w", "creative"]);
    assert!(out.starts_with("worlds: creative\n"), "{out}");
    let listing = vxl(&elsewhere, &["list", "-c", cfg.to_str().unwrap()]);
    assert_eq!(listing.lines().count(), 3, "{listing}");

    // A typo in the file is an error, not silently ignored.
    fs::write(dir.join("vxl.toml"), "[keep]\ndialy = 7\n").unwrap();
    let out = Command::new(env!("CARGO_BIN_EXE_vxl"))
        .args(["list", "--repo", dir.join("repo").to_str().unwrap()])
        .current_dir(dir)
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(
        String::from_utf8_lossy(&out.stderr).contains("dialy"),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );

    // Without a config, a missing repo is explained.
    let out = Command::new(env!("CARGO_BIN_EXE_vxl"))
        .args(["list"])
        .current_dir(&elsewhere)
        .env_remove("VXL_REPO")
        .env_remove("VXL_CONFIG")
        .output()
        .unwrap();
    assert!(String::from_utf8_lossy(&out.stderr).contains("no repository given"));
}

#[test]
fn cli_show_and_snapshot_by_time() {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("server");
    let repo = tmp.path().join("repo");
    synth::write_world(&s.join("world"), 90, &[(0, 0)], 12, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&s.join("worlds/lobby"), 91, &[(0, 0)], 3, Compression::Zlib, |_, _| 0).unwrap();
    let vxl = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_vxl")).args(args).env("VXL_REPO", &repo).output().unwrap();
        (out.status.success(), String::from_utf8(out.stdout).unwrap(), String::from_utf8(out.stderr).unwrap())
    };
    vxl(&["init", repo.to_str().unwrap()]);
    let (ok, out, err) = vxl(&["backup", s.to_str().unwrap(), "-m", "first"]);
    assert!(ok, "{err}");
    let first = out.lines().nth(1).unwrap()["snapshot ".len()..][..6].to_string();
    std::thread::sleep(std::time::Duration::from_millis(1100)); // distinct timestamps
    let (_, out, _) = vxl(&["backup", s.to_str().unwrap(), "-w", "lobby", "-m", "second"]);
    let second = out.lines().nth(1).unwrap()["snapshot ".len()..][..6].to_string();

    let (ok, out, err) = vxl(&["show", "latest"]);
    assert!(ok, "{err}");
    assert!(out.contains(&second) && out.contains("message   second"), "{out}");
    let (_, out, _) = vxl(&["show", &first]);
    let lobby = out.lines().find(|l| l.trim_start().starts_with("lobby")).unwrap();
    assert!(lobby.contains("worlds/lobby") && lobby.contains(" 3 "), "{out}");
    assert!(out.lines().any(|l| l.trim_start().starts_with("total") && l.contains(" 15 ")), "{out}");

    // A time picks the newest snapshot at or before it; a date far in the past finds nothing.
    let today = chrono::Local::now().format("%Y-%m-%d").to_string();
    let (_, out, _) = vxl(&["show", &today]);
    assert!(out.contains(&second), "{out}");
    let (ok, _, err) = vxl(&["show", "2001-01-01"]);
    assert!(!ok && err.contains("no snapshot at or before"), "{err}");
    let (ok, _, err) = vxl(&["show", "next-tuesday"]);
    assert!(!ok && err.contains("neither a snapshot id nor a time"), "{err}");

    let target = tmp.path().join("r");
    let (ok, _, err) = vxl(&["restore", "latest", target.to_str().unwrap(), "-w", "lobby"]);
    assert!(ok, "{err}");
    assert!(target.join("level.dat").exists());
}

#[test]
fn cli_diff() {
    let tmp = tempfile::tempdir().unwrap();
    let s = tmp.path().join("server");
    let repo = tmp.path().join("repo");
    synth::write_world(&s.join("world"), 95, &[(0, 0)], 20, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&s.join("world_nether"), 96, &[(0, 0)], 5, Compression::Zlib, |_, _| 0).unwrap();
    let vxl = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_vxl")).args(args).env("VXL_REPO", &repo).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    vxl(&["init", repo.to_str().unwrap()]);
    let first =
        vxl(&["backup", s.to_str().unwrap()]).lines().nth(1).unwrap()["snapshot ".len()..][..6].to_string();

    // Play: 3 chunks edited in the overworld, a new End region inside `world`; the nether untouched.
    let edit = |cx: i32, cz: i32| if cz == 0 && (0..3).contains(&cx) { 4 } else { 0 };
    synth::write_world(&s.join("world"), 95, &[(0, 0)], 20, Compression::Zlib, edit).unwrap();
    let end = [RawChunk {
        slot: 0,
        timestamp: 1,
        compression: 2,
        external: false,
        payload: anvil::compress(Compression::Zlib, &synth::chunk_nbt(97, 0, 0, 0)).unwrap(),
    }];
    write_region(&s.join("world/DIM1/region/r.0.0.mca"), (0, 0), &end);
    vxl(&["backup", s.to_str().unwrap()]);

    let out = vxl(&["diff", &first]);
    assert!(out.contains("world: chunks 3 changed, 1 added | 1 other file changed"), "{out}"); // playerdata
    assert!(out.contains("  overworld: 3 changed\n") && out.contains("  the_end: 1 added\n"), "{out}");
    assert!(out.contains("world_nether: unchanged"), "{out}");
    let out = vxl(&["diff", &first, "latest", "--files"]);
    assert!(out.contains("    M region/r.0.0.mca (3 changed)"), "{out}");
    assert!(out.contains("    A DIM1/region/r.0.0.mca (1 added)"), "{out}");
}

/// Chunks of one region file in a world: slot → decompressed NBT.
fn chunks_of(dir: &Path, rel: &str) -> BTreeMap<u16, Vec<u8>> {
    match contents(dir).remove(rel) {
        Some(Content::Region(cs)) => cs.into_iter().map(|(slot, _, _, nbt)| (slot, nbt)).collect(),
        None => BTreeMap::new(),
        Some(other) => panic!("{rel} is not a region: {other:?}"),
    }
}

fn entities(seed: u64, slots: &[u16]) -> Vec<RawChunk> {
    slots
        .iter()
        .map(|&slot| RawChunk {
            slot,
            timestamp: 1,
            compression: 2,
            external: false,
            payload: anvil::compress(Compression::Zlib, &synth::chunk_nbt(seed, slot as i32, 0, 0)).unwrap(),
        })
        .collect()
}

#[test]
fn partial_restore_puts_back_one_area_with_its_entities() {
    let env = setup();
    let w = &env.world;
    // 120 chunks per region = slots x 0..31, z 0..3.
    synth::write_world(w, 100, REGIONS, 120, Compression::Zlib, |_, _| 0).unwrap();
    write_region(&w.join("entities/r.0.0.mca"), (0, 0), &entities(1, &[0, 40]));
    write_region(&w.join("DIM-1/region/r.0.0.mca"), (0, 0), &entities(2, &[0]));
    let id = do_backup(&env, "before").id;
    let snap_region = chunks_of(w, "region/r.0.0.mca");
    let snap_entities = chunks_of(w, "entities/r.0.0.mca");

    // Grief: every chunk changes, more chunks get generated (z up to 6), mobs move,
    // the nether changes, players keep playing.
    synth::write_world(w, 100, REGIONS, 220, Compression::Zlib, |_, _| 9).unwrap();
    write_region(&w.join("entities/r.0.0.mca"), (0, 0), &entities(3, &[0, 40]));
    write_region(&w.join("DIM-1/region/r.0.0.mca"), (0, 0), &entities(4, &[0]));
    let griefed = contents(w);
    let griefed_region = chunks_of(w, "region/r.0.0.mca");

    let sel = |dim: Option<&str>, area: Option<&str>| partial::Selection {
        dimension: dim.map(String::from),
        area: area.map(|a| partial::Area::parse(a).unwrap()),
    };
    // Chunks x 0..=2, z 0..=6: 12 of them were in the snapshot, 9 are newer.
    let area = sel(Some("overworld"), Some("0,0..47,111"));
    let restore_area = |s: &partial::Selection, force: bool| {
        partial::restore_partial(&repo(&env), &id, w, None, s, force, &hidden())
    };

    // Safety: an existing world needs --force; --area needs a dimension here.
    assert!(restore_area(&area, false).unwrap_err().to_string().contains("--force"));
    let err = restore_area(&sel(None, Some("0,0..47,111")), true).unwrap_err().to_string();
    assert!(err.contains("--dimension") && err.contains("the_nether"), "{err}");
    assert!(
        restore_area(&sel(Some("the_end"), None), true).unwrap_err().to_string().contains("no dimension")
    );
    assert_eq!(contents(w), griefed);

    let r = restore_area(&area, true).unwrap();
    assert_eq!((r.chunks_restored, r.chunks_removed), (12 + 1, 9), "{r:?}"); // + the entity chunk
    assert_no_staging(w);

    let now = chunks_of(w, "region/r.0.0.mca");
    let in_area = |slot: u16| slot % 32 <= 2 && slot / 32 <= 6;
    for (slot, nbt) in &griefed_region {
        match (in_area(*slot), snap_region.get(slot)) {
            (true, Some(old)) => assert_eq!(&now[slot], old, "slot {slot} restored"),
            (true, None) => assert!(!now.contains_key(slot), "slot {slot} didn't exist then: removed"),
            (false, _) => assert_eq!(&now[slot], nbt, "slot {slot} outside the area: untouched"),
        }
    }
    // Entities: the one inside the area is back, the one at x=8 stays as it is now.
    let ents = chunks_of(w, "entities/r.0.0.mca");
    assert_eq!(ents[&0], snap_entities[&0]);
    assert_ne!(ents[&40], snap_entities[&40]);
    // Everything else is untouched: the other region, the nether, player data, level.dat.
    let mut after = contents(w);
    let mut before = griefed;
    for f in ["region/r.0.0.mca", "entities/r.0.0.mca"] {
        after.remove(f);
        before.remove(f);
    }
    assert_eq!(after, before);
}

#[test]
fn partial_restore_of_a_dimension_and_into_a_new_folder() {
    let env = setup();
    let s = env.base.join("server");
    synth::write_world(&s.join("world"), 110, &[(0, 0)], 10, Compression::Zlib, |_, _| 0).unwrap();
    synth::write_world(&s.join("world_nether"), 111, &[], 0, Compression::Zlib, |_, _| 0).unwrap();
    write_region(&s.join("world_nether/DIM-1/region/r.0.0.mca"), (0, 0), &entities(5, &[0, 1]));
    let id = backup::backup(&repo(&env), &s, &backup::Options::default(), &hidden()).unwrap().id;
    let nether_then = contents(&s.join("world_nether"));

    // The nether gets wrecked, including a region the snapshot never had.
    write_region(&s.join("world_nether/DIM-1/region/r.0.0.mca"), (0, 0), &entities(6, &[0, 1, 2]));
    write_region(&s.join("world_nether/DIM-1/region/r.5.5.mca"), (5, 5), &entities(7, &[0]));
    synth::write_world(&s.join("world"), 110, &[(0, 0)], 10, Compression::Zlib, |_, _| 3).unwrap();
    let overworld_now = contents(&s.join("world"));

    // Paper layout: `--dimension nether` over the whole server finds it in world_nether.
    let sel = partial::Selection { dimension: Some("the_nether".into()), area: None };
    let r = partial::restore_partial(&repo(&env), &id, &s, None, &sel, true, &hidden()).unwrap();
    assert_eq!(r.files_deleted, 1, "r.5.5 didn't exist then");
    let mut nether_now = contents(&s.join("world_nether"));
    nether_now.remove("session.lock");
    let mut want = nether_then;
    want.remove("session.lock");
    assert_eq!(nether_now, want);
    assert_eq!(contents(&s.join("world")), overworld_now, "the overworld is untouched");

    // Into a new folder: just the selected region files, e.g. to hand someone an area.
    let out = env.base.join("export");
    let r = partial::restore_partial(&repo(&env), &id, &out, Some("world_nether"), &sel, false, &hidden())
        .unwrap();
    assert_eq!(r.files_written, 1);
    assert!(out.join("DIM-1/region/r.0.0.mca").exists() && !out.join("level.dat").exists());
}

#[test]
fn cli_partial_restore() {
    let tmp = tempfile::tempdir().unwrap();
    let world = tmp.path().join("world");
    let repo = tmp.path().join("repo");
    synth::write_world(&world, 120, &[(0, 0)], 64, Compression::Zlib, |_, _| 0).unwrap();
    let vxl = |args: &[&str]| {
        let out = Command::new(env!("CARGO_BIN_EXE_vxl")).args(args).env("VXL_REPO", &repo).output().unwrap();
        assert!(out.status.success(), "{args:?}: {}", String::from_utf8_lossy(&out.stderr));
        String::from_utf8(out.stdout).unwrap()
    };
    vxl(&["init", repo.to_str().unwrap()]);
    vxl(&["backup", world.to_str().unwrap()]);
    synth::write_world(&world, 120, &[(0, 0)], 64, Compression::Zlib, |_, _| 2).unwrap();
    let out = vxl(&["restore", "latest", world.to_str().unwrap(), "--force", "--area", "-16,-16..31,15"]);
    // Chunks x -1..=1, z -1..=0; the region holds x 0..31, z 0..1, so x 0..=1, z 0 exist.
    assert!(
        out.contains("restored 6 chunks from") && out.contains("| 2 chunks put back, 0 removed |"),
        "{out}"
    );
}

#[test]
fn partial_restore_with_the_newer_dimensions_layout() {
    // Newer versions keep every dimension under dimensions/minecraft/<name>/.
    let env = setup();
    let w = &env.world;
    synth::write_world(w, 130, &[], 0, Compression::Zlib, |_, _| 0).unwrap();
    let over = "dimensions/minecraft/overworld/region/r.0.0.mca";
    let nether = "dimensions/minecraft/the_nether/region/r.0.0.mca";
    write_region(&w.join(over), (0, 0), &entities(10, &[0, 1, 40]));
    write_region(&w.join(nether), (0, 0), &entities(11, &[0]));
    let id = do_backup(&env, "x").id;
    let (over_then, nether_then) = (chunks_of(w, over), chunks_of(w, nether));

    write_region(&w.join(over), (0, 0), &entities(12, &[0, 1, 40]));
    write_region(&w.join(nether), (0, 0), &entities(13, &[0]));
    let nether_now = chunks_of(w, nether);
    // `overworld` means dimensions/minecraft/overworld here; chunks x 0..=1, z 0.
    let sel = partial::Selection {
        dimension: Some(vxl::world::normalize_dimension("overworld")),
        area: Some(partial::Area::parse("0,0..31,15").unwrap()),
    };
    let r = partial::restore_partial(&repo(&env), &id, w, None, &sel, true, &hidden()).unwrap();
    assert_eq!(r.chunks_restored, 2);
    let o = chunks_of(w, over);
    assert_eq!((&o[&0], &o[&1]), (&over_then[&0], &over_then[&1]));
    assert_ne!(o[&40], over_then[&40], "outside the area");
    assert_eq!(chunks_of(w, nether), nether_now, "other dimension untouched");
    assert_ne!(nether_now, nether_then);
}
