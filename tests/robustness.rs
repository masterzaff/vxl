//! Parsers that read bytes from disk (region files, chunk payloads, NBT, packs) must
//! turn damage into errors, never panics or runaway allocations. A cheap stand-in for
//! `cargo fuzz` that runs on stable: valid inputs, deterministically mutated in many
//! ways (byte flips, overwrites with boundary values, truncation, splices).

use std::fs;

use vxl::anvil::{self, Compression, RawChunk};
use vxl::repo::{LockMode, Repo};
use vxl::{nbt, pack, synth};

struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 ^= self.0 << 13;
        self.0 ^= self.0 >> 7;
        self.0 ^= self.0 << 17;
        self.0
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n.max(1) as u64) as usize
    }
}

/// One mutated copy of `input`.
fn mutate(input: &[u8], rng: &mut Rng) -> Vec<u8> {
    let mut b = input.to_vec();
    for _ in 0..1 + rng.below(4) {
        if b.is_empty() {
            break;
        }
        let i = rng.below(b.len());
        match rng.below(6) {
            0 => b[i] ^= 1 << rng.below(8),
            1 => b[i] = [0x00, 0xFF, 0x7F, 0x80, 0x01][rng.below(5)],
            2 => {
                // A big-endian/little-endian "length" set to something extreme.
                let v: [u8; 4] =
                    [[0xFF; 4], [0x7F, 0xFF, 0xFF, 0xFF], [0, 0, 0, 0], [0x80, 0, 0, 0]][rng.below(4)];
                for (k, x) in v.iter().enumerate() {
                    if let Some(slot) = b.get_mut(i + k) {
                        *slot = *x;
                    }
                }
            }
            3 => b.truncate(i),
            4 => {
                let j = rng.below(b.len());
                let n = rng.below(64).min(b.len() - i.max(j));
                b.copy_within(j..j + n, i);
            }
            _ => {
                let extra: Vec<u8> = (0..rng.below(32)).map(|_| rng.next() as u8).collect();
                b.splice(i..i, extra);
            }
        }
    }
    b
}

const ROUNDS: usize = 1500;

fn sample_region() -> Vec<u8> {
    let chunks: Vec<RawChunk> = (0..6u16)
        .map(|slot| {
            let comp = [Compression::Gzip, Compression::Zlib, Compression::None, Compression::Lz4]
                [slot as usize % 4];
            RawChunk {
                slot: slot * 37,
                timestamp: 1_700_000_000 + slot as u32,
                compression: comp.to_byte(),
                external: false,
                payload: anvil::compress(comp, &synth::chunk_nbt(1, slot as i32, 0, 0)).unwrap(),
            }
        })
        .collect();
    anvil::build_region(&chunks, Some((0, 0))).unwrap().region
}

#[test]
fn region_files() {
    let region = sample_region();
    let mut rng = Rng(0x9E37_79B9_7F4A_7C15);
    for _ in 0..ROUNDS {
        let b = mutate(&region, &mut rng);
        let _ = anvil::check_layout(&b);
        if let Ok(p) = anvil::parse_region(&b, Some((0, 0)), |_| Err(std::io::ErrorKind::NotFound.into())) {
            for c in p.chunks {
                let _ = anvil::decompress(c.compression, &c.payload);
            }
        }
    }
}

#[test]
fn chunk_payloads() {
    let nbt = synth::chunk_nbt(2, 3, 4, 0);
    let mut rng = Rng(0xD1B5_4A32_D192_ED03);
    for comp in [Compression::Gzip, Compression::Zlib, Compression::None, Compression::Lz4] {
        let payload = anvil::compress(comp, &nbt).unwrap();
        for _ in 0..ROUNDS / 2 {
            let b = mutate(&payload, &mut rng);
            for byte in [comp.to_byte(), rng.next() as u8] {
                let _ = anvil::decompress(byte, &b);
            }
        }
    }
}

#[test]
fn nbt() {
    let bytes = synth::chunk_nbt(5, 6, 7, 1);
    let mut rng = Rng(0x2545_F491_4F6C_DD1D);
    for _ in 0..ROUNDS {
        let b = mutate(&bytes, &mut rng);
        for ext in [false, true] {
            let _ = nbt::parse(&b, ext);
        }
    }
    // Hostile nesting: lists of lists, far deeper than the parser allows.
    let mut deep = vec![10u8, 0, 0]; // root compound, empty name
    for _ in 0..100_000 {
        deep.extend_from_slice(&[9, 0, 1, b'a', 9, 0, 0, 0, 1]); // list "a" of 1 list…
    }
    assert!(nbt::parse(&deep, false).is_err());
}

#[test]
fn pack_files() {
    let dir = tempfile::tempdir().unwrap();
    let repo_dir = dir.path().join("repo");
    Repo::init(&repo_dir, 3, 64 << 10).unwrap();
    {
        let repo = Repo::open(&repo_dir, LockMode::Exclusive).unwrap();
        for i in 0..20u8 {
            repo.put(&synth::chunk_nbt(i as u64, 0, 0, 0)).unwrap();
        }
        repo.flush().unwrap();
    }
    let pack_path = fs::read_dir(repo_dir.join("packs")).unwrap().next().unwrap().unwrap().path();
    let original = fs::read(&pack_path).unwrap();
    let scratch = dir.path().join("x.pack");
    let mut rng = Rng(0x0123_4567_89AB_CDEF);
    for _ in 0..ROUNDS {
        fs::write(&scratch, mutate(&original, &mut rng)).unwrap();
        let _ = pack::read_index(&scratch);
    }
    // A damaged pack inside a repo is reported, not fatal, and other packs still open.
    let mut bad = original.clone();
    let n = bad.len();
    // Trailer: count (u32) | index offset (u64) | end magic. An offset near u64::MAX
    // must not overflow the bounds check.
    bad[n - 16..n - 8].copy_from_slice(&u64::MAX.to_le_bytes());
    fs::write(&scratch, &bad).unwrap();
    assert!(pack::read_index(&scratch).is_err());
    bad[n - 20..n - 16].copy_from_slice(&u32::MAX.to_le_bytes());
    fs::write(&scratch, &bad).unwrap();
    assert!(pack::read_index(&scratch).is_err());
    fs::write(&pack_path, bad).unwrap();
    let repo = Repo::open(&repo_dir, LockMode::Shared).unwrap();
    assert!(!repo.warnings.is_empty());
}
