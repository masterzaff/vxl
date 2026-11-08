//! Storage format 3 (chunks as group objects): exact restores, prune never breaking a
//! kept snapshot, leftovers of a crash, and `migrate`.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use indicatif::ProgressBar;
use tempfile::TempDir;
use vxl::anvil::{self, Compression};
use vxl::chunk_store::ChunkStore;
use vxl::commands::{backup, migrate, prune, restore, verify};
use vxl::repo::{LockMode, ObjectId, Repo};
use vxl::retention::Policy;
use vxl::synth;
use vxl::world::{self, Kind};

const REGIONS: &[(i32, i32)] = &[(0, 0), (-1, 0)];
const CHUNKS: usize = 48;
/// Small packs so tests exercise pack rollover and repacking.
const PACK: u64 = 128 << 10;

/// What a world means: blobs byte for byte, region files chunk by chunk.
type Contents = BTreeMap<String, Vec<(u16, u32, u8, Vec<u8>)>>;

fn contents(dir: &Path) -> Contents {
    let mut out = BTreeMap::new();
    for f in world::scan(dir).unwrap().files {
        if f.rel == "session.lock" {
            continue;
        }
        let data = fs::read(&f.abs).unwrap();
        let v = match f.kind {
            Kind::External => continue,
            Kind::Region { coords } => {
                let d = f.abs.parent().unwrap();
                let p = anvil::parse_region(&data, Some(coords), |n| fs::read(d.join(n))).unwrap();
                p.chunks
                    .into_iter()
                    .map(|c| {
                        (
                            c.slot,
                            c.timestamp,
                            c.compression,
                            anvil::decompress(c.compression, &c.payload).unwrap(),
                        )
                    })
                    .collect()
            }
            _ => vec![(0, 0, 0, data)],
        };
        out.insert(f.rel, v);
    }
    out
}

struct Env {
    _tmp: TempDir,
    base: PathBuf,
    world: PathBuf,
}

impl Env {
    fn new() -> Env {
        let tmp = tempfile::tempdir().unwrap();
        let base = tmp.path().to_path_buf();
        Env { world: base.join("world"), base, _tmp: tmp }
    }

    fn repo(&self, name: &str, format: u32) -> PathBuf {
        let p = self.base.join(name);
        Repo::init_format(&p, 3, PACK, format).unwrap();
        p
    }

    /// Night `n`: each chunk at one of a few edit levels, so chunks change, and also go
    /// back to contents an older snapshot had.
    fn write_night(&self, seed: u64, n: u64) {
        synth::write_world(&self.world, 7, REGIONS, CHUNKS, Compression::Zlib, |cx, cz| {
            let h = mix(seed ^ mix(n) ^ mix((cx as u64) << 32 | cz as u32 as u64));
            if n == 0 || h % 10 >= 3 { 0 } else { [40, 80, 120][(h >> 8) as usize % 3] }
        })
        .unwrap();
    }
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn open(p: &Path) -> Repo {
    Repo::open(p, LockMode::Exclusive).unwrap()
}

fn backup(repo: &Path, world: &Path) -> backup::BackupReport {
    backup::backup(&open(repo), world, &backup::Options::default(), &ProgressBar::hidden()).unwrap()
}

fn restored(env: &Env, repo: &Path, id: &ObjectId) -> Contents {
    let target = env.base.join(format!("restore-{}", &id.hex()[..12]));
    let _ = fs::remove_dir_all(&target);
    restore::restore(&open(repo), id, &target, None, restore::Mode::Fresh, &ProgressBar::hidden()).unwrap();
    let c = contents(&target);
    fs::remove_dir_all(&target).unwrap();
    c
}

fn verify_all(repo: &Path) {
    let r = open(repo);
    let ids = r.snapshot_ids().unwrap();
    let v = verify::verify(&r, &ids, &ProgressBar::hidden()).unwrap();
    assert!(v.problems.is_empty(), "{:?}", v.problems);
}

fn pack_bytes(repo: &Path) -> u64 {
    open(repo).packs().iter().map(|p| p.size).sum()
}

#[test]
fn grouped_backups_restore_exactly_and_store_less() {
    let env = Env::new();
    let (v2, v3) = (env.repo("v2", 2), env.repo("v3", 3));
    let mut versions = Vec::new();
    let mut stored = [Vec::new(), Vec::new()];
    for n in 0..5 {
        env.write_night(1, n);
        let a = backup(&v2, &env.world);
        let b = backup(&v3, &env.world);
        assert!(a.warnings.is_empty() && b.warnings.is_empty());
        assert_eq!(a.stats.new_chunks, b.stats.new_chunks, "the same chunks count as new");
        stored[0].push(a.stats.stored_bytes);
        stored[1].push(b.stats.stored_bytes);
        versions.push((b.id, contents(&env.world)));
    }
    for (id, want) in &versions {
        assert_eq!(&restored(&env, &v3, id), want);
    }
    verify_all(&v3);
    let later = |v: &Vec<u64>| v[1..].iter().sum::<u64>();
    assert!(later(&stored[1]) * 2 < later(&stored[0]), "nightly: {stored:?}");
    assert!(pack_bytes(&v3) < pack_bytes(&v2));
}

/// Random histories of backups, deletes and prunes: after every deletion, every
/// snapshot still there restores exactly what was backed up. One test per seed, so they
/// run in parallel.
#[test]
fn prune_never_breaks_a_kept_snapshot_0() {
    prune_history(0);
}
#[test]
fn prune_never_breaks_a_kept_snapshot_1() {
    prune_history(1);
}
#[test]
fn prune_never_breaks_a_kept_snapshot_2() {
    prune_history(2);
}
#[test]
fn prune_never_breaks_a_kept_snapshot_3() {
    prune_history(3);
}

fn prune_history(seed: u64) {
    let env = Env::new();
    let repo = env.repo("repo", 3);
    let mut want: HashMap<ObjectId, Contents> = HashMap::new();
    for n in 0..10 {
        env.write_night(seed, n);
        let r = backup(&repo, &env.world);
        want.insert(r.id, contents(&env.world));

        let ids = open(&repo).snapshot_ids().unwrap();
        let removed = match mix(seed * 100 + n) % 4 {
            0 if ids.len() > 1 => {
                // Delete a random snapshot, not necessarily the oldest.
                let victim = ids[(mix(n + seed) % ids.len() as u64) as usize];
                prune::delete(&open(&repo), &[victim], false).unwrap();
                want.remove(&victim);
                true
            }
            1 => {
                let keep = Policy { last: Some(2), ..Default::default() };
                let rep = prune::prune(&open(&repo), &keep, false).unwrap();
                for id in &rep.removed_snapshots {
                    want.remove(id);
                }
                !rep.removed_snapshots.is_empty()
            }
            _ => false,
        };
        // After anything was deleted, and at the end: every snapshot left is intact.
        if !removed && n < 9 {
            continue;
        }
        for (id, c) in &want {
            assert_eq!(&restored(&env, &repo, id), c, "seed {seed}, night {n}: snapshot {id}");
        }
    }
    verify_all(&repo);
    // Everything gone: nothing is left behind.
    let ids = open(&repo).snapshot_ids().unwrap();
    prune::delete(&open(&repo), &ids, false).unwrap();
    assert!(open(&repo).list_objects().is_empty(), "seed {seed}");
}

/// A group object whose source is gone (a crash, or garbage whose sources a prune
/// removed) is never reused as is: it's stored again, and reads find the good copy.
#[test]
fn an_object_whose_source_is_gone_is_stored_again() {
    let env = Env::new();
    let dir = env.repo("repo", 3);
    let a = synth::chunk_nbt(3, 0, 0, 0);
    let b = synth::chunk_nbt(3, 0, 0, 40);
    let (ia, ib) = (ObjectId::of(&a), ObjectId::of(&b));
    {
        let repo = open(&dir);
        let store = ChunkStore::new(&repo, HashSet::new());
        store.put(&a, None).unwrap();
        repo.flush().unwrap();
        store.put(&b, Some(&ia)).unwrap();
        repo.flush().unwrap();
        assert_eq!(repo.deps(&ib).unwrap(), [ia], "b reads its unchanged sections from a");
        let pack_a = repo.packs().into_iter().find(|p| p.entries.iter().any(|e| e.id == ia)).unwrap();
        assert!(!pack_a.entries.iter().any(|e| e.id == ib));
        repo.delete_pack(&pack_a.id).unwrap();
        assert!(repo.get(&ib).is_err());
    }
    let repo = open(&dir);
    let store = ChunkStore::new(&repo, HashSet::new());
    assert!(store.put(&b, None).unwrap().stored.is_some(), "stored again");
    repo.flush().unwrap();
    drop(repo);
    // Two copies of b on disk now; whichever the index lists first, reads work.
    let repo = open(&dir);
    assert_eq!(repo.get(&ib).unwrap(), b);
    assert!(repo.deps(&ib).unwrap().contains(&ia), "the old copy still names a");
}

#[test]
fn migrate_keeps_every_snapshot_and_shrinks_the_repo() {
    let env = Env::new();
    let dir = env.repo("repo", 2);
    let mut want = Vec::new();
    for n in 0..5 {
        env.write_night(9, n);
        let r = backup(&dir, &env.world);
        want.push((r.id, contents(&env.world)));
    }
    let before = pack_bytes(&dir);
    let r = migrate::migrate(&mut open(&dir), &ProgressBar::hidden()).unwrap();
    assert_eq!(r.snapshots, 5);
    assert_eq!(r.converted, r.chunks);
    assert_eq!(open(&dir).config.version, 3);
    let mut ids = open(&dir).snapshot_ids().unwrap();
    ids.sort();
    let mut old: Vec<ObjectId> = want.iter().map(|(id, _)| *id).collect();
    old.sort();
    assert_eq!(ids, old, "snapshot ids don't change");
    for (id, c) in &want {
        assert_eq!(&restored(&env, &dir, id), c);
    }
    verify_all(&dir);
    assert!(pack_bytes(&dir) < before, "{} → {}", before, pack_bytes(&dir));
    // No plain copies of chunks are left.
    let repo = open(&dir);
    for (id, _) in repo.list_objects() {
        let raw = repo.read_raw(&id).unwrap();
        let is_blob = zstd::stream::decode_all(&raw[..]).is_ok_and(|d| ObjectId::of(&d) == id);
        assert!(vxl::group::is_group(&raw) || is_blob);
    }
    drop(repo);

    // Again: nothing left to do. And backups carry on in format 3.
    let again = migrate::migrate(&mut open(&dir), &ProgressBar::hidden()).unwrap();
    assert_eq!(again.converted, 0);
    env.write_night(9, 5);
    let r = backup(&dir, &env.world);
    assert_eq!(restored(&env, &dir, &r.id), contents(&env.world));
    verify_all(&dir);
}

#[test]
fn an_interrupted_migration_leaves_a_working_repo() {
    let env = Env::new();
    let dir = env.repo("repo", 2);
    let mut want = Vec::new();
    for n in 0..3 {
        env.write_night(4, n);
        let r = backup(&dir, &env.world);
        want.push((r.id, contents(&env.world)));
    }
    // Stop after converting some chunks: format 3, plain and group copies side by side.
    {
        let mut repo = open(&dir);
        repo.upgrade_to_grouped().unwrap();
        let store = ChunkStore::new(&repo, HashSet::new());
        let ids: Vec<ObjectId> = repo.list_objects().into_iter().map(|(id, _)| id).collect();
        for id in ids.iter().step_by(2) {
            if let Ok(nbt) = repo.get(id) {
                store.convert(&nbt).unwrap();
            }
        }
        repo.flush().unwrap();
    }
    for (id, c) in &want {
        assert_eq!(&restored(&env, &dir, id), c);
    }
    verify_all(&dir);
    // A prune in this state keeps one good copy of everything.
    prune::prune(&open(&dir), &Policy::default(), false).unwrap();
    for (id, c) in &want {
        assert_eq!(&restored(&env, &dir, id), c);
    }
    migrate::migrate(&mut open(&dir), &ProgressBar::hidden()).unwrap();
    for (id, c) in &want {
        assert_eq!(&restored(&env, &dir, id), c);
    }
    verify_all(&dir);
}

#[test]
fn formats() {
    let env = Env::new();
    let p = env.base.join("bad");
    assert!(Repo::init_format(&p, 3, PACK, 4).is_err());
    let p = env.repo("future", 3);
    let cfg = fs::read_to_string(p.join("config.json")).unwrap().replace("\"version\": 3", "\"version\": 4");
    fs::write(p.join("config.json"), cfg).unwrap();
    let err = Repo::open(&p, LockMode::Shared).err().unwrap().to_string();
    assert!(err.contains("newer"), "{err}");
}
