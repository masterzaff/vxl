//! Reading and writing Anvil region files (`r.<x>.<z>.mca`).
//!
//! Layout of a region file:
//! ```text
//! [0    .. 4096)  1024 × u32 BE "locations": (sector offset << 8) | sector count
//! [4096 .. 8192)  1024 × u32 BE last-modified timestamps (unix seconds)
//! [8192 ..     )  chunk payloads, each starting on a 4 KiB sector boundary:
//!                   u32 BE length (counts the compression byte + data)
//!                   u8     compression type (bit 0x80 = data lives in c.<x>.<z>.mcc)
//!                   [u8]   compressed NBT
//! ```
//! Chunk slot `i` holds local chunk `(i % 32, i / 32)`.
//!
//! This module knows nothing about the repo: it turns bytes into chunks and back.

use std::io::{Read, Write};

use anyhow::{Context, Result, anyhow, bail};
use flate2::{Compression as FlateLevel, read::GzDecoder, read::ZlibDecoder};
use serde::{Deserialize, Serialize};

pub const SECTOR: usize = 4096;
pub const HEADER_LEN: usize = 2 * SECTOR;
pub const SLOTS: usize = 1024;
/// The sector-count byte is a u8, so a chunk can span at most 255 sectors
/// (~1 MiB) before Minecraft moves it to an external `.mcc` file.
pub const MAX_INLINE_SECTORS: usize = 255;
/// Refuse to decompress anything bigger than this (protects against garbage/zip bombs).
pub const MAX_CHUNK_BYTES: u64 = 256 << 20;
const EXTERNAL_FLAG: u8 = 0x80;

/// Chunk compression types Minecraft understands.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Compression {
    Gzip,
    Zlib,
    None,
    /// Added in 1.20.5 (`region-file-compression=lz4`). Uses lz4-java's block stream format.
    Lz4,
}

impl Compression {
    pub fn from_byte(b: u8) -> Option<Self> {
        match b {
            1 => Some(Self::Gzip),
            2 => Some(Self::Zlib),
            3 => Some(Self::None),
            4 => Some(Self::Lz4),
            _ => None,
        }
    }

    pub fn to_byte(self) -> u8 {
        match self {
            Self::Gzip => 1,
            Self::Zlib => 2,
            Self::None => 3,
            Self::Lz4 => 4,
        }
    }
}

/// One chunk as stored on disk: still compressed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RawChunk {
    pub slot: u16,
    pub timestamp: u32,
    /// Compression byte with the external flag stripped.
    pub compression: u8,
    /// True if the payload came from (and should go back to) a `.mcc` file.
    pub external: bool,
    pub payload: Vec<u8>,
}

#[derive(Debug, Default)]
pub struct ParsedRegion {
    pub chunks: Vec<RawChunk>,
    /// Names of `.mcc` files that were read (so the caller doesn't also back them up as blobs).
    pub external_files: Vec<String>,
    /// Non-fatal problems: chunks that point outside the file etc.
    pub warnings: Vec<String>,
}

/// Parse `r.<x>.<z>.mca` → `(x, z)`.
pub fn region_coords(file_name: &str) -> Option<(i32, i32)> {
    let rest = file_name.strip_prefix("r.")?.strip_suffix(".mca")?;
    let (x, z) = rest.split_once('.')?;
    Some((x.parse().ok()?, z.parse().ok()?))
}

/// Name of the external file for a chunk slot of region `(rx, rz)`.
pub fn external_name(rx: i32, rz: i32, slot: u16) -> String {
    let cx = rx * 32 + (slot as i32 % 32);
    let cz = rz * 32 + (slot as i32 / 32);
    format!("c.{cx}.{cz}.mcc")
}

fn be_u32(b: &[u8]) -> u32 {
    u32::from_be_bytes([b[0], b[1], b[2], b[3]])
}

/// Split a region file into its chunks.
///
/// `coords` are the region coordinates (needed to find `.mcc` files) and
/// `read_external` loads one by file name. Returns `Err` only if the file isn't
/// a region file at all (e.g. too short); per-chunk problems become warnings.
pub fn parse_region(
    data: &[u8],
    coords: Option<(i32, i32)>,
    mut read_external: impl FnMut(&str) -> std::io::Result<Vec<u8>>,
) -> Result<ParsedRegion> {
    if data.len() < HEADER_LEN {
        bail!("file is {} bytes, smaller than the 8 KiB region header", data.len());
    }
    let mut out = ParsedRegion::default();
    for slot in 0..SLOTS {
        let loc = be_u32(&data[slot * 4..]);
        if loc == 0 {
            continue; // chunk not generated
        }
        let timestamp = be_u32(&data[SECTOR + slot * 4..]);
        let offset = (loc >> 8) as usize * SECTOR;
        let (cx, cz) = (slot % 32, slot / 32);
        let bad = |msg: &str| format!("chunk ({cx},{cz}) {msg}, skipped");

        if offset < HEADER_LEN || offset + 5 > data.len() {
            out.warnings.push(bad("points outside the file"));
            continue;
        }
        let len = be_u32(&data[offset..]) as usize;
        if len == 0 || offset + 4 + len > data.len() {
            out.warnings.push(bad(&format!("has invalid length {len}")));
            continue;
        }
        let comp_byte = data[offset + 4];
        let external = comp_byte & EXTERNAL_FLAG != 0;
        let payload = if external {
            let Some((rx, rz)) = coords else {
                out.warnings.push(bad("is external but the region name has no coordinates"));
                continue;
            };
            let name = external_name(rx, rz, slot as u16);
            match read_external(&name) {
                Ok(bytes) => {
                    out.external_files.push(name);
                    bytes
                }
                Err(e) => {
                    out.warnings.push(bad(&format!("needs missing {name} ({e})")));
                    continue;
                }
            }
        } else {
            data[offset + 5..offset + 4 + len].to_vec()
        };
        out.chunks.push(RawChunk {
            slot: slot as u16,
            timestamp,
            compression: comp_byte & !EXTERNAL_FLAG,
            external,
            payload,
        });
    }
    Ok(out)
}

/// A rebuilt region file plus any `.mcc` files it needs next to it.
pub struct BuiltRegion {
    pub region: Vec<u8>,
    pub external: Vec<(String, Vec<u8>)>,
}

/// Assemble a region file from chunks. Chunks too big for 255 sectors are
/// written to `.mcc` files (needs `coords`); `external` on the input is ignored —
/// placement is decided by size, the same way Minecraft does it.
pub fn build_region(chunks: &[RawChunk], coords: Option<(i32, i32)>) -> Result<BuiltRegion> {
    let mut header = vec![0u8; HEADER_LEN];
    let mut body: Vec<u8> = Vec::new();
    let mut external = Vec::new();
    let mut sorted: Vec<&RawChunk> = chunks.iter().collect();
    sorted.sort_by_key(|c| c.slot);

    for c in sorted {
        let slot = c.slot as usize;
        if slot >= SLOTS {
            bail!("chunk slot {slot} out of range");
        }
        let needed = (4 + 1 + c.payload.len()).div_ceil(SECTOR);
        let start_sector = 2 + body.len() / SECTOR;
        let sectors = if needed <= MAX_INLINE_SECTORS {
            body.extend_from_slice(&(1 + c.payload.len() as u32).to_be_bytes());
            body.push(c.compression);
            body.extend_from_slice(&c.payload);
            needed
        } else {
            let (rx, rz) = coords.ok_or_else(|| anyhow!("oversized chunk needs region coordinates"))?;
            external.push((external_name(rx, rz, c.slot), c.payload.clone()));
            body.extend_from_slice(&1u32.to_be_bytes());
            body.push(c.compression | EXTERNAL_FLAG);
            1
        };
        body.resize(body.len().next_multiple_of(SECTOR), 0);
        if start_sector >= 1 << 24 {
            bail!("region file too large");
        }
        let loc = ((start_sector as u32) << 8) | sectors as u32;
        header[slot * 4..slot * 4 + 4].copy_from_slice(&loc.to_be_bytes());
        header[SECTOR + slot * 4..SECTOR + slot * 4 + 4].copy_from_slice(&c.timestamp.to_be_bytes());
    }
    header.extend_from_slice(&body);
    Ok(BuiltRegion { region: header, external })
}

// ---------------------------------------------------------------------------
// chunk (de)compression

/// Decompress a chunk payload into raw NBT bytes.
pub fn decompress(compression: u8, payload: &[u8]) -> Result<Vec<u8>> {
    let mut out = Vec::new();
    match Compression::from_byte(compression) {
        Some(Compression::Gzip) => {
            GzDecoder::new(payload).take(MAX_CHUNK_BYTES).read_to_end(&mut out)?;
        }
        Some(Compression::Zlib) => {
            ZlibDecoder::new(payload).take(MAX_CHUNK_BYTES).read_to_end(&mut out)?;
        }
        Some(Compression::None) => out.extend_from_slice(payload),
        Some(Compression::Lz4) => out = lz4java::decode(payload)?,
        None => bail!("unknown compression type {compression}"),
    }
    if out.len() as u64 >= MAX_CHUNK_BYTES {
        bail!("chunk decompresses to more than {MAX_CHUNK_BYTES} bytes");
    }
    Ok(out)
}

/// Compress raw NBT bytes the way Minecraft would for `compression`.
pub fn compress(compression: Compression, data: &[u8]) -> Result<Vec<u8>> {
    Ok(match compression {
        Compression::Gzip => {
            let mut e = flate2::write::GzEncoder::new(Vec::new(), FlateLevel::default());
            e.write_all(data)?;
            e.finish()?
        }
        Compression::Zlib => {
            let mut e = flate2::write::ZlibEncoder::new(Vec::new(), FlateLevel::default());
            e.write_all(data)?;
            e.finish()?
        }
        Compression::None => data.to_vec(),
        Compression::Lz4 => lz4java::encode(data),
    })
}

/// lz4-java's `LZ4BlockOutputStream` framing, which is what Minecraft writes for
/// compression type 4. Each block:
/// `"LZ4Block" | token u8 | compressed len LE | original len LE | xxh32 LE`
/// followed by an empty terminating block.
pub mod lz4java {
    use super::*;
    use xxhash_rust::xxh32::xxh32;

    const MAGIC: &[u8; 8] = b"LZ4Block";
    const HEADER: usize = 8 + 1 + 4 + 4 + 4;
    const METHOD_RAW: u8 = 0x10;
    const METHOD_LZ4: u8 = 0x20;
    const LEVEL_BASE: u32 = 10;
    const SEED: u32 = 0x9747_b28c;
    /// lz4-java's default block size (64 KiB) → level nibble 6.
    const BLOCK_SIZE: usize = 1 << 16;
    const LEVEL: u8 = 6;

    fn checksum(data: &[u8]) -> u32 {
        xxh32(data, SEED) & 0x0FFF_FFFF
    }

    fn le(b: &[u8]) -> u32 {
        u32::from_le_bytes([b[0], b[1], b[2], b[3]])
    }

    pub fn decode(mut input: &[u8]) -> Result<Vec<u8>> {
        let mut out = Vec::new();
        loop {
            if input.len() < HEADER || &input[..8] != MAGIC {
                bail!("corrupt LZ4 block stream");
            }
            let token = input[8];
            let method = token & 0xF0;
            let max_len = 1usize << (LEVEL_BASE + (token & 0x0F) as u32);
            let clen = le(&input[9..]) as usize;
            let olen = le(&input[13..]) as usize;
            let check = le(&input[17..]);
            input = &input[HEADER..];
            if olen == 0 && clen == 0 {
                return Ok(out); // end marker
            }
            if olen > max_len || clen > input.len() || (method == METHOD_RAW && clen != olen) {
                bail!("corrupt LZ4 block header");
            }
            if (out.len() + olen) as u64 > MAX_CHUNK_BYTES {
                bail!("LZ4 chunk too large");
            }
            let block = &input[..clen];
            let start = out.len();
            match method {
                METHOD_RAW => out.extend_from_slice(block),
                METHOD_LZ4 => {
                    let d = lz4_flex::block::decompress(block, olen).context("corrupt LZ4 block data")?;
                    if d.len() != olen {
                        bail!("LZ4 block length mismatch");
                    }
                    out.extend_from_slice(&d);
                }
                _ => bail!("unknown LZ4 block method {method:#x}"),
            }
            if checksum(&out[start..]) != check {
                bail!("LZ4 block checksum mismatch");
            }
            input = &input[clen..];
        }
    }

    pub fn encode(data: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(data.len() / 2 + 64);
        for block in data.chunks(BLOCK_SIZE) {
            let compressed = lz4_flex::block::compress(block);
            let (method, body) = if compressed.len() >= block.len() {
                (METHOD_RAW, block)
            } else {
                (METHOD_LZ4, compressed.as_slice())
            };
            out.extend_from_slice(MAGIC);
            out.push(method | LEVEL);
            out.extend_from_slice(&(body.len() as u32).to_le_bytes());
            out.extend_from_slice(&(block.len() as u32).to_le_bytes());
            out.extend_from_slice(&checksum(block).to_le_bytes());
            out.extend_from_slice(body);
        }
        out.extend_from_slice(MAGIC);
        out.push(METHOD_RAW | LEVEL);
        out.extend_from_slice(&[0u8; 12]);
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn chunk(slot: u16, comp: Compression, data: &[u8]) -> RawChunk {
        RawChunk {
            slot,
            timestamp: 1_700_000_000 + slot as u32,
            compression: comp.to_byte(),
            external: false,
            payload: compress(comp, data).unwrap(),
        }
    }

    #[test]
    fn region_name_parsing() {
        assert_eq!(region_coords("r.0.0.mca"), Some((0, 0)));
        assert_eq!(region_coords("r.-3.12.mca"), Some((-3, 12)));
        assert_eq!(region_coords("r.a.0.mca"), None);
        assert_eq!(region_coords("level.dat"), None);
        assert_eq!(external_name(-1, 2, 33), "c.-31.65.mcc");
    }

    #[test]
    fn every_compression_roundtrips() {
        let data: Vec<u8> = (0..300_000u32).map(|i| (i % 251) as u8 ^ (i / 997) as u8).collect();
        for c in [Compression::Gzip, Compression::Zlib, Compression::None, Compression::Lz4] {
            let packed = compress(c, &data).unwrap();
            assert_eq!(decompress(c.to_byte(), &packed).unwrap(), data, "{c:?}");
        }
    }

    #[test]
    fn lz4_rejects_corruption() {
        let mut packed = lz4java::encode(&vec![7u8; 10_000]);
        let n = packed.len();
        packed[n - 30] ^= 0xFF;
        assert!(lz4java::decode(&packed).is_err());
    }

    #[test]
    fn region_roundtrip() {
        let chunks = vec![
            chunk(0, Compression::Zlib, b"hello"),
            chunk(5, Compression::Gzip, &[1u8; 9000]),
            chunk(1023, Compression::Lz4, b"lz4 chunk"),
        ];
        let built = build_region(&chunks, Some((0, 0))).unwrap();
        assert!(built.external.is_empty());
        assert_eq!(built.region.len() % SECTOR, 0);
        let parsed = parse_region(&built.region, Some((0, 0)), |_| unreachable!()).unwrap();
        assert!(parsed.warnings.is_empty());
        assert_eq!(parsed.chunks, chunks);
    }

    #[test]
    fn oversized_chunks_go_external() {
        // incompressible data > 255 sectors
        let mut x: u64 = 1;
        let big: Vec<u8> = (0..1_200_000)
            .map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                x as u8
            })
            .collect();
        let chunks = vec![chunk(40, Compression::None, &big), chunk(41, Compression::Zlib, b"small")];
        let built = build_region(&chunks, Some((1, -1))).unwrap();
        assert_eq!(built.external.len(), 1);
        assert_eq!(built.external[0].0, "c.40.-31.mcc");
        let ext = built.external.clone();
        let parsed = parse_region(&built.region, Some((1, -1)), |name| {
            Ok(ext.iter().find(|(n, _)| n == name).unwrap().1.clone())
        })
        .unwrap();
        assert_eq!(parsed.external_files, vec!["c.40.-31.mcc".to_string()]);
        assert!(parsed.chunks[0].external);
        assert_eq!(parsed.chunks[0].payload, chunks[0].payload);
        assert_eq!(parsed.chunks[1], chunks[1]);
    }

    #[test]
    fn bad_offsets_become_warnings() {
        let mut data = vec![0u8; HEADER_LEN];
        data[0..4].copy_from_slice(&((50u32 << 8) | 1).to_be_bytes()); // past EOF
        let parsed = parse_region(&data, None, |_| unreachable!()).unwrap();
        assert!(parsed.chunks.is_empty());
        assert_eq!(parsed.warnings.len(), 1);
        assert!(parse_region(&[0u8; 100], None, |_| unreachable!()).is_err());
    }
}
