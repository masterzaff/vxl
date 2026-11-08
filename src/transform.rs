//! Experimental, reversible chunk transforms (not used by the CLI yet).
//!
//! - **unpack**: bit-packed arrays (block states, biomes, heightmaps, and
//!   optionally light nibbles) are expanded to one value per byte (or u16), which
//!   lines data up on byte boundaries so zstd can find repeats.
//! - **split**: a chunk becomes a *skeleton* (everything except `sections`, with
//!   each section replaced by its hash) plus one object per section, so a chunk
//!   whose sections didn't change only costs a new skeleton.
//!
//! [`encode`] always checks itself: it decodes what it produced and fails unless
//! the result is byte-identical to the input. Callers fall back to storing the
//! chunk whole on failure, so these transforms can never change restored data.

use anyhow::{Context, Result, anyhow, bail};

use crate::nbt::{self, BYTE_ARRAY, COMPOUND, LONG_ARRAY, Root, Tag};
use crate::repo::ObjectId;

const FLAG_SPLIT: u8 = 1;
const FLAG_UNPACK: u8 = 2;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Codec {
    pub split: bool,
    pub unpack: bool,
    /// Also expand SkyLight/BlockLight nibbles (only with `unpack`).
    pub light: bool,
}

/// A transformed chunk: `main` (skeleton, or the whole chunk if not split)
/// plus the section objects it refers to by hash.
pub struct Encoded {
    pub main: Vec<u8>,
    pub parts: Vec<Vec<u8>>,
}

pub fn encode(nbt_bytes: &[u8], codec: Codec) -> Result<Encoded> {
    let mut root = nbt::parse(nbt_bytes, false).context("not parseable NBT")?;
    if root.write() != nbt_bytes {
        bail!("NBT doesn't round-trip byte-exactly");
    }
    let mut flags = 0;
    if codec.unpack && unpack(&mut root.tag, codec.light) > 0 {
        flags |= FLAG_UNPACK;
    }
    let mut parts = Vec::new();
    if codec.split
        && let Some(Tag::List(elem, items)) = sections_mut(&mut root.tag, COMPOUND)
    {
        for item in items.iter_mut() {
            let mut bytes = Vec::new();
            item.write_payload(&mut bytes);
            *item = Tag::ByteArray(ObjectId::of(&bytes).0.to_vec());
            parts.push(bytes);
        }
        *elem = BYTE_ARRAY;
        flags |= FLAG_SPLIT;
    }
    let mut main = vec![flags];
    main.extend_from_slice(&root.write());
    let enc = Encoded { main, parts };

    // Safety net: never hand out an encoding that doesn't decode to the input.
    let back = decode(&enc.main, |id| {
        enc.parts.iter().find(|p| ObjectId::of(p) == *id).cloned().ok_or_else(|| anyhow!("missing part"))
    })?;
    if back != nbt_bytes {
        bail!("transform is not reversible for this chunk");
    }
    Ok(enc)
}

pub fn decode(main: &[u8], part: impl Fn(&ObjectId) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
    let (&flags, body) = main.split_first().ok_or_else(|| anyhow!("empty encoded chunk"))?;
    let mut root: Root = nbt::parse(body, true)?;
    if flags & FLAG_SPLIT != 0 {
        let Some(Tag::List(elem, items)) = sections_mut(&mut root.tag, BYTE_ARRAY) else {
            bail!("split chunk without section references");
        };
        for item in items.iter_mut() {
            let Tag::ByteArray(h) = item else { bail!("bad section reference") };
            let id = ObjectId(h.as_slice().try_into().context("bad section reference")?);
            *item = nbt::parse_compound_payload(&part(&id)?, true)?;
        }
        *elem = COMPOUND;
    }
    if flags & FLAG_UNPACK != 0 {
        repack(&mut root.tag)?;
    }
    Ok(root.write())
}

/// The sections list: `sections` (1.18+) or `Level.Sections` (older worlds).
fn sections_mut(root: &mut Tag, elem: u8) -> Option<&mut Tag> {
    let is_list = |t: &Tag| matches!(t, Tag::List(e, _) if *e == elem);
    if root.get("sections").is_some_and(is_list) {
        return root.get_mut("sections");
    }
    let level = root.get_mut("Level")?;
    if level.get("Sections").is_some_and(is_list) { level.get_mut("Sections") } else { None }
}

// ---------------------------------------------------------------------------
// unpack / repack

fn ceil_log2(n: usize) -> u8 {
    let mut b = 0;
    while (1usize << b) < n {
        b += 1;
    }
    b
}

fn pack(values: &[u16], bits: u8) -> Vec<i64> {
    let per = 64 / bits as usize;
    let mut out = vec![0u64; values.len().div_ceil(per)];
    for (i, &v) in values.iter().enumerate() {
        out[i / per] |= (v as u64) << ((i % per) * bits as usize);
    }
    out.into_iter().map(|x| x as i64).collect()
}

/// Try to expand a packed long array holding `count` values of at least
/// `min_bits` bits. Only succeeds if packing again gives back the exact array
/// (so arrays with junk in their padding bits, or pre-1.16 spanning layouts, are left alone).
fn unpack_longs(arr: &[i64], count: usize, min_bits: u8) -> Option<Tag> {
    let bits = (min_bits.max(1)..=16).find(|&b| count.div_ceil(64 / b as usize) == arr.len())?;
    let per = 64 / bits as usize;
    let mask = (1u64 << bits) - 1;
    let values: Vec<u16> =
        (0..count).map(|i| (((arr[i / per] as u64) >> ((i % per) * bits as usize)) & mask) as u16).collect();
    (pack(&values, bits) == arr).then_some(Tag::Unpacked { orig: LONG_ARRAY, bits, values })
}

fn palette_len(entries: &[(Vec<u8>, Tag)], key: &[u8]) -> Option<(usize, u8)> {
    entries.iter().find(|(k, _)| k == key).and_then(|(_, t)| match t {
        Tag::List(elem, items) => Some((items.len(), *elem)),
        _ => None,
    })
}

/// Returns how many arrays were expanded.
fn unpack(tag: &mut Tag, light: bool) -> usize {
    let mut n = 0;
    match tag {
        Tag::Compound(entries) => {
            // Paletted containers: block_states {palette, data}, biomes {palette, data},
            // or pre-1.18 sections {Palette, BlockStates}.
            let containers: [(&[u8], &[u8]); 2] = [(b"palette", b"data"), (b"Palette", b"BlockStates")];
            for (pal_key, data_key) in containers {
                let Some((plen, pal_elem)) = palette_len(entries, pal_key) else { continue };
                // biome palettes are strings (64 entries/section), block palettes compounds (4096)
                let (count, min_bits) = if pal_elem == nbt::STRING {
                    (64, ceil_log2(plen))
                } else {
                    (4096, ceil_log2(plen).max(4))
                };
                if let Some((_, t)) = entries.iter_mut().find(|(k, _)| k == data_key)
                    && let Tag::LongArray(arr) = t
                    && let Some(u) = unpack_longs(arr, count, min_bits)
                {
                    *t = u;
                    n += 1;
                }
            }
            for (k, v) in entries.iter_mut() {
                if k == b"Heightmaps"
                    && let Tag::Compound(maps) = v
                {
                    for (_, m) in maps.iter_mut() {
                        if let Tag::LongArray(arr) = m
                            && let Some(u) = unpack_longs(arr, 256, 1)
                        {
                            *m = u;
                            n += 1;
                        }
                    }
                } else if light
                    && (k == b"SkyLight" || k == b"BlockLight")
                    && let Tag::ByteArray(b) = v
                    && b.len() == 2048
                {
                    let values = b.iter().flat_map(|&x| [(x & 0xF) as u16, (x >> 4) as u16]).collect();
                    *v = Tag::Unpacked { orig: BYTE_ARRAY, bits: 4, values };
                    n += 1;
                } else {
                    n += unpack(v, light);
                }
            }
        }
        Tag::List(_, items) => {
            for i in items {
                n += unpack(i, light);
            }
        }
        _ => {}
    }
    n
}

fn repack(tag: &mut Tag) -> Result<()> {
    match tag {
        Tag::Unpacked { orig, bits, values } => {
            *tag = match (*orig, *bits) {
                (LONG_ARRAY, b) if (1..=16).contains(&b) => Tag::LongArray(pack(values, b)),
                (BYTE_ARRAY, 4) => Tag::ByteArray(
                    values.chunks(2).map(|p| (p[0] | p.get(1).unwrap_or(&0) << 4) as u8).collect(),
                ),
                _ => bail!("bad unpacked array"),
            };
        }
        Tag::Compound(entries) => {
            for (_, v) in entries {
                repack(v)?;
            }
        }
        Tag::List(_, items) => {
            for i in items {
                repack(i)?;
            }
        }
        _ => {}
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ALL: [Codec; 5] = [
        Codec { split: false, unpack: true, light: false },
        Codec { split: false, unpack: true, light: true },
        Codec { split: true, unpack: false, light: false },
        Codec { split: true, unpack: true, light: false },
        Codec { split: true, unpack: true, light: true },
    ];

    fn roundtrip(nbt: &[u8], codec: Codec) -> Encoded {
        let enc = encode(nbt, codec).unwrap();
        let back = decode(&enc.main, |id| {
            enc.parts.iter().find(|p| ObjectId::of(p) == *id).cloned().ok_or_else(|| anyhow!("missing"))
        })
        .unwrap();
        assert_eq!(back, nbt);
        enc
    }

    #[test]
    fn every_codec_is_exact_on_synthetic_chunks() {
        for (cx, cz, e) in [(0, 0, 0), (7, -3, 2), (-20, 11, 5)] {
            let nbt = crate::synth::chunk_nbt(3, cx, cz, e);
            for c in ALL {
                let enc = roundtrip(&nbt, c);
                assert_eq!(enc.parts.len(), if c.split { 24 } else { 0 }, "{c:?}");
                assert_eq!(enc.main[0] & FLAG_UNPACK != 0, c.unpack);
            }
        }
    }

    #[test]
    fn unchanged_sections_hash_the_same_across_visits() {
        // Same blocks, different LastUpdate/InhabitedTime → same section objects.
        let a = encode(&crate::synth::chunk_nbt_ex(3, 4, 4, 0, 1), ALL[2]).unwrap();
        let b = encode(&crate::synth::chunk_nbt_ex(3, 4, 4, 0, 9), ALL[2]).unwrap();
        assert_ne!(a.main, b.main);
        assert_eq!(a.parts, b.parts);
    }

    #[test]
    fn packing_is_exact_and_rejects_junk_padding() {
        // 4-bit values, 4096 entries → 256 longs exactly; 5-bit → 342 longs with 4 bits padding each
        let vals: Vec<u16> = (0..4096).map(|i| (i * 7 % 13) as u16).collect();
        let packed = pack(&vals, 5);
        assert_eq!(packed.len(), 342);
        assert!(matches!(unpack_longs(&packed, 4096, 4), Some(Tag::Unpacked { bits: 5, .. })));
        let mut junk = packed.clone();
        junk[0] |= 1 << 62; // set a padding bit
        assert!(unpack_longs(&junk, 4096, 4).is_none());
    }

    #[test]
    fn non_chunks_are_refused_not_mangled() {
        assert!(encode(b"definitely not nbt", ALL[3]).is_err());
        // valid NBT without sections: unpack/split are no-ops, still exact
        let root = Root {
            name: vec![],
            tag: Tag::Compound(vec![(b"Entities".to_vec(), Tag::List(COMPOUND, vec![]))]),
        };
        let enc = roundtrip(&root.write(), ALL[4]);
        assert!(enc.parts.is_empty() && enc.main[0] == 0);
    }
}
