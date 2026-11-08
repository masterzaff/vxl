//! Storage format 2 vs 3 on real copies of a world, through the real backup code.
//!
//!     cargo run --release --example format_bench -- WORK day1/world day2/world ...   # oldest first
//!
//! Each copy is hard-linked into `WORK/world` in turn (so every backup has the same
//! source, like nightly backups of one server) and backed up into a format-2 and a
//! format-3 repository under WORK. Then: the newest snapshot is restored from the
//! format-3 repo and compared with the original, both repos are verified, and a copy of
//! the format-2 repo is migrated. WORK must not exist; it's left behind for inspection.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Instant;

use indicatif::ProgressBar;
use vxl::commands::{backup, compare, migrate, restore, verify};
use vxl::repo::{LockMode, Repo};
use vxl::util::human_bytes;

fn link_tree(from: &Path, to: &Path) {
    for e in walkdir::WalkDir::new(from) {
        let e = e.unwrap();
        let dest = to.join(e.path().strip_prefix(from).unwrap());
        if e.file_type().is_dir() {
            fs::create_dir_all(&dest).unwrap();
        } else if e.file_type().is_file() {
            fs::hard_link(e.path(), &dest).unwrap();
        }
    }
}

fn dir_bytes(p: &Path) -> u64 {
    walkdir::WalkDir::new(p).into_iter().filter_map(|e| e.ok()?.metadata().ok()).map(|m| m.len()).sum()
}

fn copy_tree(from: &Path, to: &Path) {
    for e in walkdir::WalkDir::new(from) {
        let e = e.unwrap();
        let dest = to.join(e.path().strip_prefix(from).unwrap());
        if e.file_type().is_dir() {
            fs::create_dir_all(&dest).unwrap();
        } else {
            fs::copy(e.path(), &dest).unwrap();
        }
    }
}

fn main() {
    let mut args = std::env::args().skip(1).map(PathBuf::from);
    let work = args.next().expect("usage: format_bench WORK day1 day2 ...");
    let days: Vec<PathBuf> = args.collect();
    assert!(!days.is_empty(), "no world copies given");
    assert!(!work.exists(), "{} exists", work.display());
    fs::create_dir_all(&work).unwrap();
    let world = work.join("world");
    let repos = [work.join("v2"), work.join("v3")];
    for (r, v) in repos.iter().zip([2, 3]) {
        Repo::init_format(r, vxl::repo::DEFAULT_ZSTD_LEVEL, vxl::repo::DEFAULT_PACK_SIZE, v).unwrap();
    }
    let hidden = ProgressBar::hidden();

    println!(
        "{:<12} {:>8} {:>12} {:>8} {:>12} {:>8} {:>9}",
        "copy", "chunks", "v2 stored", "v2 s", "v3 stored", "v3 s", "v3/v2"
    );
    let mut totals = [0u64; 2];
    let mut ids = Vec::new();
    for day in &days {
        let _ = fs::remove_dir_all(&world);
        link_tree(day, &world);
        let label = match day.file_name().and_then(|n| n.to_str()) {
            Some("world") => day.parent().and_then(|p| p.file_name()).unwrap().to_string_lossy().into_owned(),
            _ => day.display().to_string(),
        };
        let mut row = Vec::new();
        for (i, r) in repos.iter().enumerate() {
            let repo = Repo::open(r, LockMode::Exclusive).unwrap();
            let t = Instant::now();
            let rep = backup::backup(&repo, &world, &backup::Options::default(), &hidden).unwrap();
            let secs = t.elapsed().as_secs_f64();
            for w in &rep.warnings {
                eprintln!("{label}: {w}");
            }
            // What the night really costs: new objects plus its manifest.
            let manifest =
                fs::metadata(r.join("snapshots").join(format!("{}.json.zst", rep.id.hex()))).unwrap().len();
            totals[i] += rep.stats.stored_bytes + manifest;
            row.push((rep.stats.chunks, rep.stats.stored_bytes + manifest, secs));
            if i == 1 {
                ids.push(rep.id);
            }
        }
        println!(
            "{:<12} {:>8} {:>12} {:>8.1} {:>12} {:>8.1} {:>8.0}%",
            label,
            row[0].0,
            human_bytes(row[0].1),
            row[0].2,
            human_bytes(row[1].1),
            row[1].2,
            100.0 * row[1].1 as f64 / row[0].1.max(1) as f64
        );
    }
    let sizes: Vec<u64> = repos.iter().map(|r| dir_bytes(r)).collect();
    println!(
        "{:<12} {:>8} {:>12} {:>8} {:>12} {:>8} {:>8.0}%   (on disk: {} vs {})",
        "total",
        "",
        human_bytes(totals[0]),
        "",
        human_bytes(totals[1]),
        "",
        100.0 * totals[1] as f64 / totals[0] as f64,
        human_bytes(sizes[0]),
        human_bytes(sizes[1])
    );

    // Restore the newest snapshot from format 3 and compare with the original.
    let t = Instant::now();
    let target = work.join("restored");
    let v3 = Repo::open(&repos[1], LockMode::Shared).unwrap();
    let ids_v3_last = ids.last().unwrap();
    restore::restore(&v3, ids_v3_last, &target, None, restore::Mode::Fresh, &hidden).unwrap();
    let restore_secs = t.elapsed().as_secs_f64();
    let cmp = compare::compare(days.last().unwrap(), &target).unwrap();
    println!(
        "\nrestore newest from v3: {restore_secs:.1}s; compare: {} of {} chunks identical, {} differences, {} layout problems",
        cmp.same_chunks,
        cmp.chunks,
        cmp.differences.len(),
        cmp.layout_problems.len()
    );
    for d in cmp.differences.iter().take(5) {
        println!("  differs: {d}");
    }
    fs::remove_dir_all(&target).unwrap();

    for (r, v) in repos.iter().zip([2, 3]) {
        let repo = Repo::open(r, LockMode::Shared).unwrap();
        let t = Instant::now();
        let all = repo.snapshot_ids().unwrap();
        let ids = &all;
        let rep = verify::verify(&repo, ids, &hidden).unwrap();
        println!(
            "verify v{v}: {} objects, {} problems, {:.1}s",
            rep.objects,
            rep.problems.len(),
            t.elapsed().as_secs_f64()
        );
    }

    // Migrate a copy of the format-2 repo.
    let m = work.join("v2-migrated");
    copy_tree(&repos[0], &m);
    let t = Instant::now();
    let rep = migrate::migrate(&mut Repo::open(&m, LockMode::Exclusive).unwrap(), 3, &hidden).unwrap();
    println!(
        "migrate v2 copy: {} chunks, {} converted, {} → {} in {:.1}s (on disk now {})",
        rep.chunks,
        rep.converted,
        human_bytes(rep.bytes_before),
        human_bytes(rep.bytes_after),
        t.elapsed().as_secs_f64(),
        human_bytes(dir_bytes(&m))
    );
    let repo = Repo::open(&m, LockMode::Shared).unwrap();
    let ids = repo.snapshot_ids().unwrap();
    let v = verify::verify(&repo, &ids, &hidden).unwrap();
    println!("verify migrated: {} objects, {} problems", v.objects, v.problems.len());
    drop(repo);
    fs::remove_dir_all(&m).unwrap();

    // And back: a copy of the format-3 repo to format 2.
    let back = work.join("v3-to-v2");
    copy_tree(&repos[1], &back);
    let t = Instant::now();
    let rep = migrate::migrate(&mut Repo::open(&back, LockMode::Exclusive).unwrap(), 2, &hidden).unwrap();
    println!(
        "migrate v3 copy to 2: {} chunks, {} given plain copies, {} → {} in {:.1}s, {} snapshots renamed",
        rep.chunks,
        rep.converted,
        human_bytes(rep.bytes_before),
        human_bytes(rep.bytes_after),
        t.elapsed().as_secs_f64(),
        rep.renamed.len()
    );
    let repo = Repo::open(&back, LockMode::Shared).unwrap();
    let ids = repo.snapshot_ids().unwrap();
    let v = verify::verify(&repo, &ids, &hidden).unwrap();
    println!("verify back in format 2: {} objects, {} problems", v.objects, v.problems.len());
    let newest = rep.renamed.iter().find(|(a, _)| a == ids_v3_last).map(|(_, b)| *b).unwrap();
    restore::restore(&repo, &newest, &target, None, restore::Mode::Fresh, &hidden).unwrap();
    let cmp = compare::compare(days.last().unwrap(), &target).unwrap();
    println!("restore newest from it: {} of {} chunks identical", cmp.same_chunks, cmp.chunks);
    fs::remove_dir_all(&target).unwrap();
}
