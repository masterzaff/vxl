//! Synthetic but structurally realistic worlds, for tests and benchmarks.
//!
//! Chunks are real NBT in the modern (1.18+) layout: 24 sections with paletted,
//! bit-packed block states and biomes, light arrays, heightmaps, block entities.
//! Terrain is noise-based (stone/deepslate, ores, caves, dirt, grass, water) so
//! it compresses roughly like a real world does. Not used by the CLI itself.

use std::fs;
use std::path::Path;

use anyhow::Result;

use crate::anvil::{self, Compression, RawChunk};

// ---------------------------------------------------------------------------
// minimal NBT writer

pub enum Tag {
    Byte(i8),
    Int(i32),
    Long(i64),
    ByteArray(Vec<u8>),
    Str(String),
    /// (element type id, elements)
    List(u8, Vec<Tag>),
    Compound(Vec<(String, Tag)>),
    LongArray(Vec<i64>),
}

impl Tag {
    fn id(&self) -> u8 {
        match self {
            Tag::Byte(_) => 1,
            Tag::Int(_) => 3,
            Tag::Long(_) => 4,
            Tag::ByteArray(_) => 7,
            Tag::Str(_) => 8,
            Tag::List(..) => 9,
            Tag::Compound(_) => 10,
            Tag::LongArray(_) => 12,
        }
    }

    fn write_payload(&self, out: &mut Vec<u8>) {
        match self {
            Tag::Byte(v) => out.push(*v as u8),
            Tag::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
            Tag::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
            Tag::ByteArray(v) => {
                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                out.extend_from_slice(v);
            }
            Tag::Str(s) => write_str(out, s),
            Tag::List(ty, items) => {
                out.push(if items.is_empty() { 0 } else { *ty });
                out.extend_from_slice(&(items.len() as i32).to_be_bytes());
                for i in items {
                    i.write_payload(out);
                }
            }
            Tag::Compound(fields) => {
                for (name, t) in fields {
                    out.push(t.id());
                    write_str(out, name);
                    t.write_payload(out);
                }
                out.push(0);
            }
            Tag::LongArray(v) => {
                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                for x in v {
                    out.extend_from_slice(&x.to_be_bytes());
                }
            }
        }
    }

    /// Serialise as a root compound (unnamed), the way chunk NBT is stored.
    pub fn to_root_bytes(&self) -> Vec<u8> {
        let mut out = vec![self.id(), 0, 0];
        self.write_payload(&mut out);
        out
    }
}

fn write_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&(s.len() as u16).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

fn c(fields: Vec<(&str, Tag)>) -> Tag {
    Tag::Compound(fields.into_iter().map(|(k, v)| (k.to_string(), v)).collect())
}

fn s(v: &str) -> Tag {
    Tag::Str(v.to_string())
}

// ---------------------------------------------------------------------------
// deterministic randomness

fn mix(mut x: u64) -> u64 {
    // splitmix64
    x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    x = (x ^ (x >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    x ^ (x >> 31)
}

fn h(seed: u64, a: i64, b: i64, c: i64) -> u64 {
    mix(seed ^ mix(a as u64 ^ mix(b as u64 ^ mix(c as u64))))
}

/// Smooth 2D value noise in [0, 1).
fn noise(seed: u64, x: f64, z: f64) -> f64 {
    let (x0, z0) = (x.floor(), z.floor());
    let (fx, fz) = (x - x0, z - z0);
    let v = |dx: f64, dz: f64| {
        (h(seed, (x0 + dx) as i64, (z0 + dz) as i64, 0) >> 11) as f64 / (1u64 << 53) as f64
    };
    let sm = |t: f64| t * t * (3.0 - 2.0 * t);
    let (sx, sz) = (sm(fx), sm(fz));
    let a = v(0.0, 0.0) + (v(1.0, 0.0) - v(0.0, 0.0)) * sx;
    let b = v(0.0, 1.0) + (v(1.0, 1.0) - v(0.0, 1.0)) * sx;
    a + (b - a) * sz
}

// ---------------------------------------------------------------------------
// chunk generation

fn pack(indices: &[u16], bits: usize) -> Vec<i64> {
    let per = 64 / bits;
    indices
        .chunks(per)
        .map(|grp| grp.iter().enumerate().fold(0u64, |acc, (i, &v)| acc | (v as u64) << (i * bits)) as i64)
        .collect()
}

fn bits_for(n: usize, min: usize) -> usize {
    let mut b = 0;
    while (1 << b) < n {
        b += 1;
    }
    b.max(min)
}

/// Uncompressed NBT for chunk `(cx, cz)`. Changing `edit` changes a handful of
/// blocks and the timestamps, like a player passing through.
pub fn chunk_nbt(seed: u64, cx: i32, cz: i32, edit: u32) -> Vec<u8> {
    let height = |x: i32, z: i32| -> i32 {
        let (wx, wz) = ((cx * 16 + x) as f64, (cz * 16 + z) as f64);
        (50.0 + 30.0 * noise(seed, wx / 64.0, wz / 64.0) + 8.0 * noise(seed + 1, wx / 16.0, wz / 16.0)) as i32
    };
    let heights: Vec<i32> = (0..256).map(|i| height(i % 16, i / 16)).collect();

    let block = |x: i32, y: i32, z: i32| -> &'static str {
        let top = heights[(z * 16 + x) as usize];
        let r = h(seed, (cx * 16 + x) as i64, y as i64, (cz * 16 + z) as i64);
        if y == -64 {
            return "minecraft:bedrock";
        }
        if edit > 0 && (r % 4096) < edit as u64 && y > top - 3 && y <= top {
            return "minecraft:cobblestone"; // "player edits"
        }
        if y > top {
            return if y <= 62 {
                "minecraft:water"
            } else if y == top + 1 && r.is_multiple_of(23) {
                "minecraft:short_grass"
            } else {
                "minecraft:air"
            };
        }
        let cave = noise(seed + 7, (cx * 16 + x) as f64 / 12.0, (y as f64 + (cz * 16 + z) as f64) / 12.0);
        if y < top - 6 && cave > 0.78 {
            return if y < -54 { "minecraft:lava" } else { "minecraft:cave_air" };
        }
        if y == top {
            return if top < 63 { "minecraft:sand" } else { "minecraft:grass_block" };
        }
        if y > top - 4 {
            return if top < 63 { "minecraft:sand" } else { "minecraft:dirt" };
        }
        let deep = y < 0;
        match r % 200 {
            0..=2 => {
                if deep {
                    "minecraft:deepslate_iron_ore"
                } else {
                    "minecraft:iron_ore"
                }
            }
            3..=5 => {
                if deep {
                    "minecraft:deepslate_coal_ore"
                } else {
                    "minecraft:coal_ore"
                }
            }
            6 if y < 16 => {
                if deep {
                    "minecraft:deepslate_diamond_ore"
                } else {
                    "minecraft:diamond_ore"
                }
            }
            7..=9 => "minecraft:gravel",
            10..=13 => {
                if deep {
                    "minecraft:tuff"
                } else {
                    "minecraft:andesite"
                }
            }
            14..=16 => "minecraft:granite",
            _ => {
                if deep {
                    "minecraft:deepslate"
                } else {
                    "minecraft:stone"
                }
            }
        }
    };

    let mut sections = Vec::new();
    for sy in -4..20 {
        let mut palette: Vec<&str> = Vec::new();
        let mut idx = Vec::with_capacity(4096);
        for i in 0..4096 {
            let (x, z, y) = (i % 16, (i / 16) % 16, i / 256);
            let b = block(x, sy * 16 + y, z);
            let p = palette.iter().position(|q| *q == b).unwrap_or_else(|| {
                palette.push(b);
                palette.len() - 1
            });
            idx.push(p as u16);
        }
        let pal_tags: Vec<Tag> = palette
            .iter()
            .map(|n| {
                if n.ends_with("grass_block") {
                    c(vec![("Name", s(n)), ("Properties", c(vec![("snowy", s("false"))]))])
                } else if n.ends_with("water") || n.ends_with("lava") {
                    c(vec![("Name", s(n)), ("Properties", c(vec![("level", s("0"))]))])
                } else {
                    c(vec![("Name", s(n))])
                }
            })
            .collect();
        let mut block_states = vec![("palette", Tag::List(10, pal_tags))];
        if palette.len() > 1 {
            block_states.push(("data", Tag::LongArray(pack(&idx, bits_for(palette.len(), 4)))));
        }

        let biome = if heights[0] < 60 { "minecraft:river" } else { "minecraft:plains" };
        let mut biomes = vec![("palette", Tag::List(8, vec![s(biome)]))];
        if sy < 0 {
            biomes = vec![("palette", Tag::List(8, vec![s(biome), s("minecraft:dripstone_caves")]))];
            let bidx: Vec<u16> = (0..64).map(|i| (h(seed, cx as i64, cz as i64, i) % 2) as u16).collect();
            biomes.push(("data", Tag::LongArray(pack(&bidx, 1))));
        }

        let mut sec =
            vec![("Y", Tag::Byte(sy as i8)), ("block_states", c(block_states)), ("biomes", c(biomes))];
        // Sky light: full above ground, dark below, a gradient near the surface.
        let sky: Vec<u8> = (0..2048)
            .map(|i| {
                let y = sy * 16 + (i / 128) as i32;
                let top = heights[(i % 128) * 2];
                let l = |dy: i32| (15 - (top - dy).clamp(0, 15)).max(0) as u8;
                l(y) | l(y) << 4
            })
            .collect();
        if sy * 16 > heights.iter().copied().max().unwrap() {
            sec.push(("SkyLight", Tag::ByteArray(vec![0xFF; 2048])));
        } else if sy * 16 + 16 > heights.iter().copied().min().unwrap() - 16 {
            sec.push(("SkyLight", Tag::ByteArray(sky)));
        }
        if sy < 0 && sections.len() % 3 == 0 {
            let bl: Vec<u8> = (0..2048)
                .map(|i| if h(seed, cx as i64, sy as i64, i).is_multiple_of(50) { 0x7A } else { 0 })
                .collect();
            sec.push(("BlockLight", Tag::ByteArray(bl)));
        }
        sections.push(c(sec));
    }

    let hm = |off: i32| {
        let v: Vec<u16> = heights.iter().map(|t| (t + 65 + off) as u16).collect();
        let per = 7;
        Tag::LongArray(
            v.chunks(per)
                .map(|g| g.iter().enumerate().fold(0u64, |a, (i, &x)| a | (x as u64) << (i * 9)) as i64)
                .collect(),
        )
    };
    let mut block_entities = Vec::new();
    if h(seed, cx as i64, cz as i64, 99).is_multiple_of(6) {
        let items: Vec<Tag> = (0..(h(seed, cx as i64, cz as i64, 98) % 8) as i8)
            .map(|i| {
                c(vec![
                    ("Slot", Tag::Byte(i)),
                    ("id", s("minecraft:bread")),
                    ("count", Tag::Int(3 + i as i32)),
                ])
            })
            .collect();
        block_entities.push(c(vec![
            ("id", s("minecraft:chest")),
            ("x", Tag::Int(cx * 16 + 3)),
            ("y", Tag::Int(-20)),
            ("z", Tag::Int(cz * 16 + 9)),
            ("keepPacked", Tag::Byte(0)),
            ("Items", Tag::List(10, items)),
        ]));
    }

    let root = c(vec![
        ("DataVersion", Tag::Int(4189)),
        ("xPos", Tag::Int(cx)),
        ("zPos", Tag::Int(cz)),
        ("yPos", Tag::Int(-4)),
        ("Status", s("minecraft:full")),
        ("LastUpdate", Tag::Long(2_000_000 + edit as i64 * 1200)),
        ("InhabitedTime", Tag::Long(edit as i64 * 37)),
        ("isLightOn", Tag::Byte(1)),
        ("sections", Tag::List(10, sections)),
        ("block_entities", Tag::List(10, block_entities)),
        (
            "Heightmaps",
            c(vec![
                ("MOTION_BLOCKING", hm(1)),
                ("MOTION_BLOCKING_NO_LEAVES", hm(1)),
                ("OCEAN_FLOOR", hm(0)),
                ("WORLD_SURFACE", hm(1)),
            ]),
        ),
        ("fluid_ticks", Tag::List(10, vec![])),
        ("block_ticks", Tag::List(10, vec![])),
        ("PostProcessing", Tag::List(9, (0..24).map(|_| Tag::List(2, vec![])).collect())),
        ("structures", c(vec![("References", c(vec![])), ("starts", c(vec![]))])),
    ]);
    root.to_root_bytes()
}

/// Write a world folder. `edits(cx, cz)` picks each chunk's edit level (0 = pristine).
pub fn write_world(
    dir: &Path,
    seed: u64,
    regions: &[(i32, i32)],
    chunks_per_region: usize,
    compression: Compression,
    edits: impl Fn(i32, i32) -> u32,
) -> Result<()> {
    fs::create_dir_all(dir.join("region"))?;
    fs::create_dir_all(dir.join("playerdata"))?;
    fs::write(
        dir.join("level.dat"),
        anvil::compress(
            Compression::Gzip,
            &c(vec![("Data", c(vec![("LevelName", s("synth"))]))]).to_root_bytes(),
        )?,
    )?;
    fs::write(dir.join("session.lock"), "☃")?;
    fs::write(
        dir.join("playerdata/0000-player.dat"),
        anvil::compress(Compression::Gzip, &edits(0, 0).to_be_bytes())?,
    )?;
    for &(rx, rz) in regions {
        let chunks: Vec<RawChunk> = (0..chunks_per_region.min(1024))
            .map(|slot| {
                let (cx, cz) = (rx * 32 + (slot % 32) as i32, rz * 32 + (slot / 32) as i32);
                let e = edits(cx, cz);
                Ok(RawChunk {
                    slot: slot as u16,
                    timestamp: 1_700_000_000 + e,
                    compression: compression.to_byte(),
                    external: false,
                    payload: anvil::compress(compression, &chunk_nbt(seed, cx, cz, e))?,
                })
            })
            .collect::<Result<_>>()?;
        let built = anvil::build_region(&chunks, Some((rx, rz)))?;
        fs::write(dir.join(format!("region/r.{rx}.{rz}.mca")), built.region)?;
    }
    Ok(())
}
