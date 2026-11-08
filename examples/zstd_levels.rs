//! Find the zstd level sweet spot for chunk objects.
//!
//!     cargo run --release --example zstd_levels              # synthetic chunks
//!     cargo run --release --example zstd_levels -- ~/world   # your real world
//!
//! Each chunk is compressed on its own (exactly like the object store does),
//! single-threaded, and we report size and speed per level.

use std::time::{Duration, Instant};

use vxl::anvil::{self, Compression};
use vxl::world::{self, Kind};

const MAX_SAMPLES: usize = 3000;

fn load_world(path: &std::path::Path) -> Vec<Vec<u8>> {
    let scan = world::scan(path).expect("scan world");
    let regions: Vec<_> = scan.files.iter().filter(|f| matches!(f.kind, Kind::Region { .. })).collect();
    let mut all = Vec::new();
    for f in &regions {
        let Ok(data) = std::fs::read(&f.abs) else { continue };
        let dir = f.abs.parent().unwrap();
        let Kind::Region { coords } = f.kind else { unreachable!() };
        let Ok(parsed) = anvil::parse_region(&data, Some(coords), |n| std::fs::read(dir.join(n))) else {
            continue;
        };
        for c in parsed.chunks {
            if let Ok(nbt) = anvil::decompress(c.compression, &c.payload) {
                all.push(nbt);
            }
        }
    }
    // spread the sample across the whole world
    let step = all.len().div_ceil(MAX_SAMPLES).max(1);
    all.into_iter().step_by(step).collect()
}

fn synthetic() -> Vec<Vec<u8>> {
    let mut out = Vec::new();
    for cz in -20..20 {
        for cx in -20..20 {
            let edit = if (cx + cz) % 5 == 0 { 3 } else { 0 };
            out.push(vxl::synth::chunk_nbt(42, cx, cz, edit));
        }
    }
    out
}

fn mbps(bytes: usize, d: Duration) -> f64 {
    bytes as f64 / 1e6 / d.as_secs_f64()
}

fn main() {
    let samples = match std::env::args().nth(1) {
        Some(p) => load_world(p.as_ref()),
        None => synthetic(),
    };
    assert!(!samples.is_empty(), "no chunks found");
    let raw: usize = samples.iter().map(|s| s.len()).sum();
    let zlib: usize = samples.iter().map(|s| anvil::compress(Compression::Zlib, s).unwrap().len()).sum();
    println!("{} chunks, avg {:.1} KB raw", samples.len(), raw as f64 / samples.len() as f64 / 1e3);
    println!("zlib (Minecraft default): {:.2} MB, {:.2}x\n", zlib as f64 / 1e6, raw as f64 / zlib as f64);
    println!("level   size MB   ratio   vs zlib   compress MB/s   decompress MB/s");

    let mut rows = Vec::new();
    for level in [-3, -1, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 15, 17, 19] {
        let mut best_c = Duration::MAX;
        let mut best_d = Duration::MAX;
        let mut size = 0;
        for _ in 0..2 {
            let t = Instant::now();
            let packed: Vec<Vec<u8>> =
                samples.iter().map(|s| zstd::bulk::compress(s, level).unwrap()).collect();
            best_c = best_c.min(t.elapsed());
            size = packed.iter().map(|p| p.len()).sum::<usize>();
            let t = Instant::now();
            for p in &packed {
                std::hint::black_box(zstd::stream::decode_all(p.as_slice()).unwrap());
            }
            best_d = best_d.min(t.elapsed());
        }
        println!(
            "{level:>5}   {:>7.2}   {:>5.2}x   {:>6.1}%   {:>13.0}   {:>15.0}",
            size as f64 / 1e6,
            raw as f64 / size as f64,
            100.0 * size as f64 / zlib as f64,
            mbps(raw, best_c),
            mbps(raw, best_d)
        );
        rows.push((level, size, mbps(raw, best_c)));
    }

    // The knee: of the levels that still compress at >= 25% of level 1's speed,
    // take the smallest one that gets within 1% of the best size among them.
    let fast = rows.iter().find(|r| r.0 == 1).unwrap().2;
    let ok: Vec<_> = rows.iter().filter(|r| r.0 >= 1 && r.2 >= fast * 0.25).collect();
    let best = ok.iter().map(|r| r.1).min().unwrap();
    let pick = ok.iter().find(|r| r.1 as f64 <= best as f64 * 1.01).unwrap();
    println!("\nsweet spot: level {} ({:.0} MB/s single-threaded)", pick.0, pick.2);

    // How much would a trained dictionary add? (not used by vxl yet)
    let (train, test) = samples.split_at(samples.len() / 5);
    if let Ok(dict) = zstd::dict::from_samples(train, 110 * 1024) {
        let plain: usize = test.iter().map(|s| zstd::bulk::compress(s, pick.0).unwrap().len()).sum();
        let mut c = zstd::bulk::Compressor::with_dictionary(pick.0, &dict).unwrap();
        let with: usize = test.iter().map(|s| c.compress(s).unwrap().len()).sum();
        println!(
            "with a 110 KB trained dictionary: {:.1}% smaller at the same level",
            100.0 * (1.0 - with as f64 / plain as f64)
        );
    }
}
