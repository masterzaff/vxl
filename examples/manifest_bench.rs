//! What a snapshot's chunk list costs per night, stored different ways.
//!
//!     cargo run --release --example manifest_bench -- day1/world day2/world ...   # oldest first
//!
//! - inline: every chunk entry in the manifest (format 2), zstd 9 like the manifest
//! - per region: each region file's chunk list is its own object; unchanged lists dedupe,
//!   and the manifest only names one object per region file
//! - per N chunks: the same, with each region's list cut into pieces of N slots

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use rayon::prelude::*;
use vxl::anvil;
use vxl::repo::ObjectId;
use vxl::util::human_bytes;
use vxl::world::{self, Kind};

/// (file, [(slot, ts, comp, hash)])
type Day = Vec<(String, Vec<(u16, u32, u8, ObjectId)>)>;

fn read(dir: &Path) -> Day {
    let scan = world::scan(dir).unwrap();
    let mut out: Day = scan
        .files
        .par_iter()
        .filter_map(|f| {
            let Kind::Region { coords } = f.kind else { return None };
            let data = std::fs::read(&f.abs).ok()?;
            let d = f.abs.parent().unwrap();
            let p = anvil::parse_region(&data, Some(coords), |n| std::fs::read(d.join(n))).ok()?;
            let chunks = p
                .chunks
                .iter()
                .map(|c| {
                    let h = anvil::decompress(c.compression, &c.payload).map(|n| ObjectId::of(&n));
                    (c.slot, c.timestamp, c.compression, h.unwrap_or_else(|_| ObjectId::of(&c.payload)))
                })
                .collect();
            Some((f.rel.clone(), chunks))
        })
        .collect();
    out.sort_by(|a, b| a.0.cmp(&b.0));
    out
}

fn table(entries: &[(u16, u32, u8, ObjectId)]) -> Vec<u8> {
    let mut b = Vec::new();
    for (slot, ts, comp, h) in entries {
        b.extend_from_slice(&slot.to_le_bytes());
        b.extend_from_slice(&ts.to_le_bytes());
        b.push(*comp);
        b.extend_from_slice(&h.0);
    }
    b
}

fn main() {
    let days: Vec<PathBuf> = std::env::args().skip(1).map(PathBuf::from).collect();
    let pieces = [0usize, 1024, 256, 64, 32];
    let mut seen: Vec<HashSet<ObjectId>> = pieces.iter().map(|_| HashSet::new()).collect();
    print!("{:<12} {:>10}", "copy", "inline");
    for p in &pieces[1..] {
        print!(" {:>14}", if *p == 1024 { "per region".to_string() } else { format!("per {p}") });
    }
    println!();
    for dir in &days {
        let day = read(dir);
        // inline: hex hashes in JSON, like the real manifest
        let json: Vec<u8> = serde_json::to_vec(
            &day.iter()
                .map(|(f, c)| (f, c.iter().map(|(s, t, k, h)| (s, t, k, h.hex())).collect::<Vec<_>>()))
                .collect::<Vec<_>>(),
        )
        .unwrap();
        let inline = zstd::bulk::compress(&json, 9).unwrap().len() as u64;
        let label = dir.parent().and_then(|p| p.file_name()).unwrap().to_string_lossy().into_owned();
        print!("{label:<12} {:>10}", human_bytes(inline));
        for (i, &n) in pieces.iter().enumerate().skip(1) {
            let mut new_bytes = 0u64;
            let mut ids = Vec::new();
            for (_, chunks) in &day {
                // cut by slot range so a piece always covers the same chunks
                let mut start = 0;
                while start < chunks.len() {
                    let lo = chunks[start].0 as usize / n;
                    let mut end = start;
                    while end < chunks.len() && chunks[end].0 as usize / n == lo {
                        end += 1;
                    }
                    let t = table(&chunks[start..end]);
                    let id = ObjectId::of(&t);
                    if seen[i].insert(id) {
                        new_bytes += zstd::bulk::compress(&t, 6).unwrap().len() as u64;
                    }
                    ids.push(id.hex());
                    start = end;
                }
            }
            let list = zstd::bulk::compress(&serde_json::to_vec(&ids).unwrap(), 9).unwrap().len() as u64;
            print!(" {:>14}", format!("{} ({})", human_bytes(new_bytes + list), ids.len()));
        }
        println!();
    }
}
