//! `vxl compare <a> <b>`: do two world folders hold the same world?
//!
//! Region files are compared chunk by chunk *by content* (decompressed NBT,
//! timestamp, compression type), not byte for byte, since a restore rebuilds
//! region files and zlib output can differ while the data is identical. Other
//! files must match byte for byte. Region files in `b` are also checked for a
//! layout Minecraft can read, and every chunk in `b` must decompress to valid NBT.
//!
//! Use it after a restore (`compare original restored` should report no
//! differences), or between two snapshots to see what changed.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::Path;

use anyhow::Result;
use rayon::prelude::*;

use crate::anvil::{self, RawChunk};
use crate::nbt;
use crate::world::{self, Kind, WorldFile};

#[derive(Debug, Default)]
pub struct CompareReport {
    pub files: u64,
    pub regions: u64,
    pub chunks: u64,
    pub same_chunks: u64,
    /// Content differences between the two worlds.
    pub differences: Vec<String>,
    /// Files or chunks in `b` that Minecraft would have trouble reading.
    pub layout_problems: Vec<String>,
}

impl CompareReport {
    pub fn identical(&self) -> bool {
        self.differences.is_empty() && self.layout_problems.is_empty()
    }
}

#[derive(Default)]
struct FileResult {
    region: bool,
    chunks: u64,
    same: u64,
    differences: Vec<String>,
    layout: Vec<String>,
}

pub fn compare(a: &Path, b: &Path) -> Result<CompareReport> {
    let index = |dir: &Path| -> Result<BTreeMap<String, WorldFile>> {
        Ok(world::scan(dir)?
            .files
            .into_iter()
            .filter(|f| f.kind != Kind::External) // compared as part of their region
            .map(|f| (f.rel.clone(), f))
            .collect())
    };
    let (fa, fb) = (index(a)?, index(b)?);
    let mut report = CompareReport::default();
    for p in fa.keys().filter(|p| !fb.contains_key(*p)) {
        report.differences.push(format!("{p}: missing from {}", b.display()));
    }
    for p in fb.keys().filter(|p| !fa.contains_key(*p)) {
        report.differences.push(format!("{p}: only in {}", b.display()));
    }

    let common: Vec<(&WorldFile, &WorldFile)> =
        fa.iter().filter_map(|(p, x)| fb.get(p).map(|y| (x, y))).collect();
    let results: Vec<(String, Result<FileResult>)> =
        common.par_iter().map(|(x, y)| (x.rel.clone(), compare_file(x, y))).collect();
    for (rel, r) in results {
        report.files += 1;
        match r {
            Ok(r) => {
                report.regions += r.region as u64;
                report.chunks += r.chunks;
                report.same_chunks += r.same;
                report.differences.extend(r.differences.into_iter().map(|d| format!("{rel}: {d}")));
                report.layout_problems.extend(r.layout.into_iter().map(|d| format!("{rel}: {d}")));
            }
            Err(e) => report.differences.push(format!("{rel}: couldn't compare ({e:#})")),
        }
    }
    report.differences.sort();
    report.layout_problems.sort();
    Ok(report)
}

fn read_region(f: &WorldFile, data: &[u8]) -> Option<BTreeMap<u16, RawChunk>> {
    let Kind::Region { coords } = f.kind else { return None };
    let dir = f.abs.parent()?;
    let parsed = anvil::parse_region(data, Some(coords), |n| fs::read(dir.join(n))).ok()?;
    Some(parsed.chunks.into_iter().map(|c| (c.slot, c)).collect())
}

fn compare_file(a: &WorldFile, b: &WorldFile) -> Result<FileResult> {
    let (da, db) = (fs::read(&a.abs)?, fs::read(&b.abs)?);
    let mut r = FileResult::default();
    let (Some(ca), Some(cb)) = (read_region(a, &da), read_region(b, &db)) else {
        // Not both valid region files: compare as plain files.
        if da != db {
            r.differences.push(format!("contents differ ({} vs {} bytes)", da.len(), db.len()));
        }
        return Ok(r);
    };
    r.region = true;
    r.layout = anvil::check_layout(&db);

    let slots: BTreeSet<u16> = ca.keys().chain(cb.keys()).copied().collect();
    for slot in slots {
        let at = format!("chunk ({},{})", slot % 32, slot / 32);
        r.chunks += 1;
        let (x, y) = match (ca.get(&slot), cb.get(&slot)) {
            (Some(x), Some(y)) => (x, y),
            (Some(_), None) => {
                r.differences.push(format!("{at} missing"));
                continue;
            }
            (None, Some(_)) => {
                r.differences.push(format!("{at} extra"));
                continue;
            }
            (None, None) => unreachable!(),
        };
        let ny = anvil::decompress(y.compression, &y.payload);
        if let Ok(bytes) = &ny
            && nbt::parse(bytes, false).is_err()
        {
            r.layout.push(format!("{at} isn't valid NBT"));
        }
        let same = match (anvil::decompress(x.compression, &x.payload), ny) {
            (Ok(p), Ok(q)) => p == q,
            // Chunks that were already unreadable must come back byte-identical.
            (Err(_), _) | (_, Err(_)) => x.payload == y.payload,
        };
        let mut why = Vec::new();
        if !same {
            why.push("data");
        }
        if x.timestamp != y.timestamp {
            why.push("timestamp");
        }
        if x.compression != y.compression {
            why.push("compression type");
        }
        if why.is_empty() {
            r.same += 1;
        } else {
            r.differences.push(format!("{at}: {} differ", why.join(", ")));
        }
    }
    Ok(r)
}
