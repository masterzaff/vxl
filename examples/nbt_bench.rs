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
//!   grouped split    split for dedup, but a chunk's skeleton and its *new* sections
//!                    are compressed together as one object (keeps zstd's context)
//!   ...+compact      grouped, with section references in the skeleton shortened from
//!                    a 32-byte hash to "#n in this object" (sections stored alongside)
//!                    or a sequential id (sections stored earlier). Dedup still keys on
//!                    the full skeleton, so what counts as unchanged is the same.
//!
//! Every transformed chunk is verified to decode byte-identically; chunks that
//! don't are counted as fallbacks and stored whole.

use std::collections::{HashMap, HashSet};
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
    /// Compress skeleton + new sections as one object instead of separately.
    grouped: bool,
    /// (grouped only) store section references compactly instead of as hashes.
    compact: bool,
    seen: Mutex<HashSet<ObjectId>>,
    /// (compact only) sequential id of every stored section.
    section_ids: Mutex<HashMap<ObjectId, u32>>,
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
    fn new(name: &'static str, codec: Option<Codec>, grouped: bool) -> Self {
        Self::with(name, codec, grouped, false)
    }

    fn with(name: &'static str, codec: Option<Codec>, grouped: bool, compact: bool) -> Self {
        Method {
            name,
            codec,
            grouped,
            compact,
            seen: Mutex::default(),
            section_ids: Mutex::default(),
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

    /// Grouped: if the skeleton is new, store one object holding it plus every
    /// section not stored before. (An unchanged skeleton implies unchanged
    /// sections, since it contains their hashes.)
    fn store_grouped(&self, enc: &transform::Encoded) {
        if !self.seen.lock().unwrap().insert(ObjectId::of(&enc.main)) {
            return;
        }
        let mut sections = Vec::new();
        let mut local = 0u32; // sections stored in this object so far
        let mut refs: Vec<(ObjectId, Vec<u8>)> = Vec::new(); // hash → compact reference
        for p in &enc.parts {
            self.parts_total.fetch_add(1, Relaxed);
            let id = ObjectId::of(p);
            let mut seen = self.seen.lock().unwrap();
            if seen.insert(id) {
                if self.compact {
                    let mut ids = self.section_ids.lock().unwrap();
                    let next = ids.len() as u32;
                    ids.insert(id, next);
                    refs.push((id, compact_ref(0, local)));
                }
                sections.extend_from_slice(&(p.len() as u32).to_le_bytes());
                sections.extend_from_slice(p);
                local += 1;
                self.parts_new.fetch_add(1, Relaxed);
            } else if self.compact {
                let gid = self.section_ids.lock().unwrap()[&id];
                refs.push((id, compact_ref(1, gid)));
            }
        }
        let main = if self.compact { shorten_refs(&enc.main, &refs) } else { enc.main.clone() };
        let mut buf = main;
        buf.extend_from_slice(&sections);
        let n = zstd::bulk::compress(&buf, ZSTD_LEVEL).unwrap().len() as u64;
        self.current.fetch_add(n, Relaxed);
        self.main_bytes.fetch_add(n, Relaxed);
    }

    fn process(&self, chunks: &[Vec<u8>]) {
        let t = Instant::now();
        chunks.par_iter().for_each(|nbt| {
            let Some(codec) = self.codec else {
                self.main_bytes.fetch_add(self.store(nbt), Relaxed);
                return;
            };
            match transform::encode(nbt, codec) {
                Ok(enc) if self.grouped => self.store_grouped(&enc),
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

/// A compact section reference: kind (0 = n-th section in this object, 1 = section
/// with this sequential id, stored earlier) + varint.
fn compact_ref(kind: u8, mut n: u32) -> Vec<u8> {
    let mut out = vec![kind];
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return out;
        }
        out.push(b | 0x80);
    }
}

/// Replace each `ByteArray(32 bytes = hash)` section reference in a skeleton with the
/// compact one (length prefix included), as a real format would store it.
fn shorten_refs(main: &[u8], refs: &[(ObjectId, Vec<u8>)]) -> Vec<u8> {
    let mut out = main.to_vec();
    for (id, r) in refs {
        let mut pat = 32u32.to_be_bytes().to_vec();
        pat.extend_from_slice(&id.0);
        if let Some(pos) = out.windows(pat.len()).position(|w| w == pat) {
            let mut rep = (r.len() as u32).to_be_bytes().to_vec();
            rep.extend_from_slice(r);
            out.splice(pos..pos + pat.len(), rep);
        }
    }
    out
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
            // A copy is usually `<date>/world`: label it by the folder that tells copies apart.
            let leaf = |p: &std::path::Path| p.file_name().map(|n| n.to_string_lossy().into_owned());
            let name = match leaf(&dir) {
                Some(n) if n == "world" => dir.parent().and_then(leaf).unwrap_or(n),
                other => other.unwrap_or_else(|| dir.display().to_string()),
            };
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
        Method::new("whole (today)", None, false),
        Method::new("unpack", c(false, true, false), false),
        Method::new("unpack+light", c(false, true, true), false),
        Method::new("split", c(true, false, false), false),
        Method::new("split+unpack", c(true, true, false), false),
        Method::new("grouped split", c(true, false, false), true),
        Method::new("grouped split+unpack", c(true, true, false), true),
        Method::with("grouped+compact", c(true, false, false), true, true),
        Method::with("grouped+unpack+compact", c(true, true, false), true, true),
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
            "{:<22} {:>12} {:>6} {:>13} {:>6} {:>12} {:>6} {:>10.0} {:>9}",
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
            if m.grouped {
                println!(
                    "  {:<20} combined objects {:>9}            ({} of {} sections distinct)",
                    m.name,
                    human_bytes(main),
                    new,
                    total
                );
            } else if m.codec.is_some_and(|c| c.split) {
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
            print!(" {:>21}", m.name);
        }
        println!();
        for (i, (name, n)) in chunk_counts.iter().enumerate() {
            print!("  {:<14}", format!("{name} ({n})").chars().take(14).collect::<String>());
            for m in &methods {
                print!(" {:>21}", human_bytes(m.per_version.lock().unwrap()[i]));
            }
            println!();
        }
    }
}
