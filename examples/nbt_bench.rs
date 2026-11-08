//! How much would chunk transforms save? Simulates a series of backups for
//! each storage method and reports what each would store.
//!
//!     cargo run --release --example nbt_bench                    # synthetic world + 5 "nights"
//!     cargo run --release --example nbt_bench -- day1 day2 day3  # copies of a real world, oldest first
//!
//! Methods (see src/transform.rs):
//!   whole            what vxl does today: one object per chunk
//!   unpack           bit-packed arrays expanded to bytes before zstd
//!   split            skeleton + one object per section (sub-chunk dedup)
//!   split+unpack     both
//!   ...+light        also expand light nibbles
//!
//! Every transformed chunk is verified to decode byte-identically; chunks that
//! don't are counted as fallbacks and stored whole.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering::Relaxed};
use std::time::{Duration, Instant};

use rayon::prelude::*;
use vxl::anvil;
use vxl::repo::ObjectId;
use vxl::transform::{self, Codec};
use vxl::util::human_bytes;
use vxl::world::{self, Kind};

const ZSTD_LEVEL: i32 = vxl::repo::DEFAULT_ZSTD_LEVEL;

// synthetic scenario
const REGIONS: &[(i32, i32)] = &[(0, 0), (-1, 0), (0, -1), (-1, -1)];
const NIGHTS: usize = 5;
const VISIT_ODDS: f64 = 0.30; // share of chunks loaded each night (timestamps change)
const EDIT_ODDS: f64 = 0.03; // share of chunks where blocks change each night
const BLOCKS_PER_EDIT: u32 = 40; // edit intensity (out of 4096 odds per near-surface block)

struct Method {
    name: &'static str,
    codec: Option<Codec>,
    seen: Mutex<HashSet<ObjectId>>,
    /// stored bytes per version
    per_version: Mutex<Vec<u64>>,
    current: AtomicU64,
    main_bytes: AtomicU64,
    part_bytes: AtomicU64,
    parts_total: AtomicU64,
    parts_new: AtomicU64,
    fallbacks: AtomicU64,
    time: Mutex<Duration>,
}

impl Method {
    fn new(name: &'static str, codec: Option<Codec>) -> Self {
        Method {
            name,
            codec,
            seen: Mutex::default(),
            per_version: Mutex::default(),
            current: AtomicU64::new(0),
            main_bytes: AtomicU64::new(0),
            part_bytes: AtomicU64::new(0),
            parts_total: AtomicU64::new(0),
            parts_new: AtomicU64::new(0),
            fallbacks: AtomicU64::new(0),
            time: Mutex::default(),
        }
    }

    /// Store an object unless seen before; returns compressed bytes added.
    fn store(&self, obj: &[u8]) -> u64 {
        if !self.seen.lock().unwrap().insert(ObjectId::of(obj)) {
            return 0;
        }
        let n = zstd::bulk::compress(obj, ZSTD_LEVEL).unwrap().len() as u64;
        self.current.fetch_add(n, Relaxed);
        n
    }

    fn process(&self, chunks: &[Vec<u8>]) {
        let t = Instant::now();
        chunks.par_iter().for_each(|nbt| {
            let Some(codec) = self.codec else {
                self.main_bytes.fetch_add(self.store(nbt), Relaxed);
                return;
            };
            match transform::encode(nbt, codec) {
                Ok(enc) => {
                    self.main_bytes.fetch_add(self.store(&enc.main), Relaxed);
                    for p in &enc.parts {
                        let n = self.store(p);
                        self.part_bytes.fetch_add(n, Relaxed);
                        self.parts_total.fetch_add(1, Relaxed);
                        self.parts_new.fetch_add((n > 0) as u64, Relaxed);
                    }
                }
                Err(_) => {
                    self.fallbacks.fetch_add(1, Relaxed);
                    self.main_bytes.fetch_add(self.store(nbt), Relaxed);
                }
            }
        });
        *self.time.lock().unwrap() += t.elapsed();
    }

    fn end_version(&self) {
        self.per_version.lock().unwrap().push(self.current.swap(0, Relaxed));
    }
}

fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn chance(a: u64, b: u64, odds: f64) -> bool {
    (mix(a ^ mix(b)) >> 11) as f64 / (1u64 << 53) as f64 <= odds
}

/// One version of the world, delivered one region at a time (keeps memory flat).
type Batches = Box<dyn FnMut(&mut dyn FnMut(Vec<Vec<u8>>))>;

fn synthetic_versions() -> Vec<(String, Batches)> {
    let coords: Vec<(i32, i32)> = REGIONS
        .iter()
        .flat_map(|&(rx, rz)| (0..1024).map(move |s| (rx * 32 + s % 32, rz * 32 + s / 32)))
        .collect();
    let mut out: Vec<(String, Batches)> = Vec::new();
    // state[i] = (edit level, tick) after each night, computed up front
    let mut state = vec![(0u32, 0u32); coords.len()];
    for night in 0..=NIGHTS {
        if night > 0 {
            for (i, st) in state.iter_mut().enumerate() {
                let edited = chance(night as u64, i as u64 * 2, EDIT_ODDS);
                if edited {
                    st.0 += BLOCKS_PER_EDIT;
                }
                if edited || chance(night as u64, i as u64 * 2 + 1, VISIT_ODDS) {
                    st.1 += 1;
                }
            }
        }
        let snapshot = state.clone();
        let coords = coords.clone();
        let name = if night == 0 { "initial".to_string() } else { format!("night {night}") };
        out.push((
            name,
            Box::new(move |sink: &mut dyn FnMut(Vec<Vec<u8>>)| {
                for (c, st) in coords.chunks(1024).zip(snapshot.chunks(1024)) {
                    let batch = c
                        .par_iter()
                        .zip(st)
                        .map(|(&(cx, cz), &(e, t))| vxl::synth::chunk_nbt_ex(42, cx, cz, e, t))
                        .collect();
                    sink(batch);
                }
            }),
        ));
    }
    out
}

fn world_versions(dirs: Vec<PathBuf>) -> Vec<(String, Batches)> {
    dirs.into_iter()
        .map(|dir| {
            let name = dir.display().to_string();
            let f: Batches = Box::new(move |sink: &mut dyn FnMut(Vec<Vec<u8>>)| {
                let scan = world::scan(&dir).expect("scan world");
                for f in scan.files {
                    let Kind::Region { coords } = f.kind else { continue };
                    let Ok(data) = std::fs::read(&f.abs) else { continue };
                    let d = f.abs.parent().unwrap();
                    let Ok(p) = anvil::parse_region(&data, Some(coords), |n| std::fs::read(d.join(n))) else {
                        continue;
                    };
                    let batch: Vec<Vec<u8>> = p
                        .chunks
                        .par_iter()
                        .filter_map(|c| anvil::decompress(c.compression, &c.payload).ok())
                        .collect();
                    sink(batch);
                }
            });
            (name, f)
        })
        .collect()
}

fn main() {
    let args: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let synthetic = args.is_empty();
    let versions = if synthetic { synthetic_versions() } else { world_versions(args) };
    if synthetic {
        println!(
            "synthetic: {} chunks, {NIGHTS} nights, each night {:.0}% of chunks loaded, {:.0}% edited\n",
            REGIONS.len() * 1024,
            VISIT_ODDS * 100.0,
            EDIT_ODDS * 100.0
        );
    }

    let c = |split, unpack, light| Some(Codec { split, unpack, light });
    let methods = [
        Method::new("whole (today)", None),
        Method::new("unpack", c(false, true, false)),
        Method::new("unpack+light", c(false, true, true)),
        Method::new("split", c(true, false, false)),
        Method::new("split+unpack", c(true, true, false)),
        Method::new("split+unpack+light", c(true, true, true)),
    ];

    let mut raw_total = 0u64;
    let mut chunk_counts = Vec::new();
    let mut first = None;
    for (i, (name, mut batches)) in versions.into_iter().enumerate() {
        let mut chunks = 0u64;
        batches(&mut |batch| {
            chunks += batch.len() as u64;
            raw_total += batch.iter().map(|c| c.len() as u64).sum::<u64>();
            for m in &methods {
                m.process(&batch);
            }
        });
        for m in &methods {
            m.end_version();
        }
        if i == 0 {
            // remember the first-backup split between skeletons and sections
            first = Some(
                methods
                    .iter()
                    .map(|m| {
                        (
                            m.main_bytes.load(Relaxed),
                            m.part_bytes.load(Relaxed),
                            m.parts_total.load(Relaxed),
                            m.parts_new.load(Relaxed),
                        )
                    })
                    .collect::<Vec<_>>(),
            );
        }
        eprintln!("processed {name} ({chunks} chunks)");
        chunk_counts.push((name, chunks));
    }
    let n_versions = chunk_counts.len();

    let base: Vec<u64> = methods[0].per_version.lock().unwrap().clone();
    let pct = |v: u64, b: u64| {
        if b == 0 { "-".to_string() } else { format!("{:.0}%", 100.0 * v as f64 / b as f64) }
    };
    println!(
        "\n{:<20} {:>12} {:>6} {:>13} {:>6} {:>12} {:>6} {:>10} {:>9}",
        "method", "1st backup", "", "avg later", "", "total", "", "MB/s*", "fallback"
    );
    for m in &methods {
        let pv = m.per_version.lock().unwrap();
        let later: u64 = pv[1..].iter().sum();
        let base_later: u64 = base[1..].iter().sum();
        let avg = if n_versions > 1 { later / (n_versions as u64 - 1) } else { 0 };
        let base_avg = if n_versions > 1 { base_later / (n_versions as u64 - 1) } else { 0 };
        let total: u64 = pv.iter().sum();
        let secs = m.time.lock().unwrap().as_secs_f64();
        println!(
            "{:<20} {:>12} {:>6} {:>13} {:>6} {:>12} {:>6} {:>10.0} {:>9}",
            m.name,
            human_bytes(pv[0]),
            pct(pv[0], base[0]),
            human_bytes(avg),
            pct(avg, base_avg),
            human_bytes(total),
            pct(total, base.iter().sum()),
            raw_total as f64 / 1e6 / secs,
            m.fallbacks.load(Relaxed),
        );
    }
    println!("  * raw NBT MB/s, all cores, including the verify-by-decoding step");

    if let Some(first) = first {
        println!("\nfirst backup, split methods:");
        for (m, (main, part, total, new)) in methods.iter().zip(first) {
            if m.codec.is_some_and(|c| c.split) {
                println!(
                    "  {:<20} skeletons {:>9}, sections {:>9} ({} of {} sections distinct)",
                    m.name,
                    human_bytes(main),
                    human_bytes(part),
                    new,
                    total
                );
            }
        }
    }
    if n_versions > 1 {
        println!("\nper version (stored bytes):");
        print!("  {:<14}", "");
        for m in &methods {
            print!(" {:>19}", m.name);
        }
        println!();
        for (i, (name, n)) in chunk_counts.iter().enumerate() {
            print!("  {:<14}", format!("{name} ({n})").chars().take(14).collect::<String>());
            for m in &methods {
                print!(" {:>19}", human_bytes(m.per_version.lock().unwrap()[i]));
            }
            println!();
        }
    }
}
