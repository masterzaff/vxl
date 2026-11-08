//! Grouped chunk objects (repository format 3).
//!
//! A chunk is stored as its *skeleton* (the chunk NBT with every section replaced by a
//! short reference) together with the sections that weren't stored before, compressed as
//! one zstd frame. A reference names either a section stored in the same object or slot
//! `k` of an older group object, which the header lists once by id. Every section lives
//! in exactly one place, so a reference is never more than one step away, and the header
//! names every object this one needs.
//!
//! ```text
//! stored: "VXG\x01" | n:u8 | n × source id[32] | zstd frame (content size + checksum) of the body
//! body:   varint s | s × varint len | varint skeleton len | skeleton | the s sections
//! ref:    a ByteArray in the skeleton's section list: src:u8 | varint slot
//!         (src 0 = this object's own section, i = slot of sources[i - 1])
//! ```
//!
//! A group object is named like any chunk object: by the BLAKE3 of the chunk NBT it
//! decodes to, not of its stored bytes. Manifests and snapshot ids therefore don't
//! depend on how a chunk is stored, and the hash check after decoding covers the whole
//! reconstruction, references included. The stored bytes can't be mistaken for a plain
//! object: those always start with the zstd frame magic.
//!
//! Pure format code: which sections to reference is the caller's business
//! (see `chunk_store.rs`). [`encode`] checks its own output by decoding it.

use std::collections::HashMap;
use std::ops::Range;

use anyhow::{Context, Result, anyhow, bail, ensure};

use crate::anvil::MAX_CHUNK_BYTES;
use crate::nbt::{self, BYTE_ARRAY, COMPOUND, Root, Tag};
use crate::repo::ObjectId;
use crate::transform::sections_mut;

pub const MAGIC: &[u8; 4] = b"VXG\x01";
pub const MAX_SOURCES: usize = 255;
/// Enough bytes to hold any header (to read the sources without the body).
pub const MAX_HEADER_LEN: usize = MAGIC.len() + 1 + MAX_SOURCES * 32;

pub fn is_group(stored: &[u8]) -> bool {
    stored.starts_with(MAGIC)
}

/// Split stored bytes into the source list and the compressed body.
fn split_header(stored: &[u8]) -> Result<(Vec<ObjectId>, &[u8])> {
    let rest = stored.strip_prefix(MAGIC).ok_or_else(|| anyhow!("not a group object"))?;
    let (&n, rest) = rest.split_first().ok_or_else(|| anyhow!("truncated group header"))?;
    let (ids, frame) =
        rest.split_at_checked(n as usize * 32).ok_or_else(|| anyhow!("truncated group header"))?;
    let sources = ids.as_chunks::<32>().0.iter().map(|c| ObjectId(*c)).collect();
    Ok((sources, frame))
}

/// The objects a group object reads sections from, given at least its header
/// (a prefix of [`MAX_HEADER_LEN`] bytes is always enough).
pub fn sources(stored_prefix: &[u8]) -> Result<Vec<ObjectId>> {
    let rest = stored_prefix.strip_prefix(MAGIC).ok_or_else(|| anyhow!("not a group object"))?;
    let (&n, rest) = rest.split_first().ok_or_else(|| anyhow!("truncated group header"))?;
    ensure!(rest.len() >= n as usize * 32, "truncated group header");
    Ok(rest[..n as usize * 32].as_chunks::<32>().0.iter().map(|c| ObjectId(*c)).collect())
}

/// A decompressed group object.
pub struct Body {
    pub sources: Vec<ObjectId>,
    data: Vec<u8>,
    skeleton: Range<usize>,
    slots: Vec<Range<usize>>,
}

impl Body {
    /// Decompress and lay out the stored bytes of a group object. Checks the zstd
    /// checksum, so damage to the stored bytes is caught even before decoding a chunk.
    pub fn decode(stored: &[u8]) -> Result<Body> {
        let (sources, frame) = split_header(stored)?;
        let size = zstd::zstd_safe::get_frame_content_size(frame)
            .map_err(|_| anyhow!("group object is corrupt (bad zstd frame)"))?
            .ok_or_else(|| anyhow!("group object has no content size"))?;
        ensure!(size <= MAX_CHUNK_BYTES, "group object claims {size} bytes");
        let data = zstd::bulk::decompress(frame, size as usize)
            .context("group object is corrupt (bad zstd data)")?;
        ensure!(data.len() as u64 == size, "group object is corrupt (wrong size)");

        let mut r = Cursor { buf: &data, pos: 0 };
        let n = r.varint()? as usize;
        ensure!(n <= data.len(), "group object is corrupt (section count)");
        let lens = (0..n).map(|_| r.varint()).collect::<Result<Vec<_>>>()?;
        let skel_len = r.varint()? as usize;
        let skeleton = r.take(skel_len)?;
        let mut slots = Vec::with_capacity(n);
        for len in lens {
            slots.push(r.take(len as usize)?);
        }
        ensure!(r.pos == data.len(), "group object is corrupt (trailing bytes)");
        Ok(Body { sources, data, skeleton, slots })
    }

    pub fn slot(&self, k: u32) -> Option<&[u8]> {
        self.slots.get(k as usize).map(|r| &self.data[r.clone()])
    }

    /// Sections stored in this object.
    pub fn slots(&self) -> usize {
        self.slots.len()
    }

    /// Rebuild the chunk NBT. `source(i, k)` returns slot `k` of `sources[i]`.
    pub fn chunk(&self, mut source: impl FnMut(usize, u32) -> Result<Vec<u8>>) -> Result<Vec<u8>> {
        let mut root = nbt::parse(&self.data[self.skeleton.clone()], false).context("bad skeleton")?;
        let Some(Tag::List(elem, items)) = sections_mut(&mut root.tag, BYTE_ARRAY) else {
            bail!("skeleton has no section references");
        };
        for item in items.iter_mut() {
            let Tag::ByteArray(r) = item else { bail!("bad section reference") };
            let (src, k) = parse_ref(r)?;
            let bytes = if src == 0 {
                self.slot(k).ok_or_else(|| anyhow!("reference to missing section {k}"))?.to_vec()
            } else {
                ensure!(src as usize <= self.sources.len(), "reference to missing source {src}");
                source(src as usize - 1, k)?
            };
            *item = nbt::parse_compound_payload(&bytes, false).context("bad section")?;
        }
        *elem = COMPOUND;
        Ok(root.write())
    }
}

/// A chunk split into skeleton and sections, ready for [`encode`].
pub struct Split {
    root: Root,
    pub sections: Vec<Vec<u8>>,
    pub ids: Vec<ObjectId>,
}

/// Split a chunk; `None` if it isn't NBT that round-trips exactly or has no section list
/// (then it's stored whole).
pub fn split(nbt_bytes: &[u8]) -> Option<Split> {
    let mut root = nbt::parse(nbt_bytes, false).ok()?;
    if root.write() != nbt_bytes {
        return None;
    }
    let Some(Tag::List(_, items)) = sections_mut(&mut root.tag, COMPOUND) else { return None };
    let sections: Vec<Vec<u8>> = items
        .iter()
        .map(|t| {
            let mut b = Vec::new();
            t.write_payload(&mut b);
            b
        })
        .collect();
    let ids = sections.iter().map(|s| ObjectId::of(s)).collect();
    Some(Split { root, sections, ids })
}

/// Where a section the caller already has is stored: slot `k` of group object `X`.
pub type Location = (ObjectId, u32);

pub struct Encoded {
    /// The bytes to store.
    pub stored: Vec<u8>,
    /// Sections stored in this object, by slot.
    pub new: Vec<ObjectId>,
    /// Objects this one reads from.
    pub sources: Vec<ObjectId>,
}

/// Encode a split chunk. `find(section id)` says where an identical section is already
/// stored; the caller must have checked that location (it's trusted here). Sections
/// that `find` doesn't know, or that would need more than [`MAX_SOURCES`] sources, are
/// stored in this object. With `base` (an older copy of this same object), its sections
/// keep their slots and new ones are appended, so references into it stay valid.
/// Fails unless the result decodes back to `nbt_bytes`.
pub fn encode(
    nbt_bytes: &[u8],
    split: Split,
    base: Option<&Body>,
    mut find: impl FnMut(&ObjectId) -> Option<Location>,
    level: i32,
) -> Result<Encoded> {
    let Split { mut root, sections, ids } = split;
    let mut new: Vec<ObjectId> = Vec::new();
    let mut own: HashMap<ObjectId, u32> = HashMap::new();
    let mut sources: Vec<ObjectId> = Vec::new();
    let mut src_index: HashMap<ObjectId, u8> = HashMap::new();
    let mut refs = Vec::with_capacity(ids.len());
    let mut new_bytes: Vec<&[u8]> = Vec::new();
    for k in 0..base.map_or(0, |b| b.slots() as u32) {
        let bytes = base.unwrap().slot(k).unwrap();
        let id = ObjectId::of(bytes);
        own.entry(id).or_insert(k);
        new.push(id);
        new_bytes.push(bytes);
    }
    for (id, bytes) in ids.iter().zip(&sections) {
        if let Some(&k) = own.get(id) {
            refs.push(make_ref(0, k));
            continue;
        }
        if let Some((obj, k)) = find(id) {
            let src = match src_index.get(&obj) {
                Some(&s) => Some(s),
                None if sources.len() < MAX_SOURCES => {
                    sources.push(obj);
                    let s = sources.len() as u8;
                    src_index.insert(obj, s);
                    Some(s)
                }
                None => None,
            };
            if let Some(s) = src {
                refs.push(make_ref(s, k));
                continue;
            }
        }
        let k = new.len() as u32;
        own.insert(*id, k);
        new.push(*id);
        new_bytes.push(bytes);
        refs.push(make_ref(0, k));
    }

    let Some(Tag::List(elem, items)) = sections_mut(&mut root.tag, COMPOUND) else {
        bail!("chunk has no section list");
    };
    for (item, r) in items.iter_mut().zip(refs) {
        *item = Tag::ByteArray(r);
    }
    *elem = BYTE_ARRAY;
    let skeleton = root.write();

    let mut body = Vec::with_capacity(skeleton.len() + new_bytes.iter().map(|b| b.len()).sum::<usize>() + 64);
    put_varint(&mut body, new_bytes.len() as u64);
    for b in &new_bytes {
        put_varint(&mut body, b.len() as u64);
    }
    put_varint(&mut body, skeleton.len() as u64);
    body.extend_from_slice(&skeleton);
    for b in &new_bytes {
        body.extend_from_slice(b);
    }
    let mut c = zstd::bulk::Compressor::new(level)?;
    c.set_parameter(zstd::zstd_safe::CParameter::ChecksumFlag(true))?;
    c.set_parameter(zstd::zstd_safe::CParameter::ContentSizeFlag(true))?;
    let frame = c.compress(&body)?;

    let mut stored = Vec::with_capacity(MAGIC.len() + 1 + sources.len() * 32 + frame.len());
    stored.extend_from_slice(MAGIC);
    stored.push(sources.len() as u8);
    for s in &sources {
        stored.extend_from_slice(&s.0);
    }
    stored.extend_from_slice(&frame);

    // Safety net: decode what we built. Sections from other objects are taken from the
    // chunk itself, by where `find` said they are.
    let mut at: HashMap<Location, usize> = HashMap::new();
    for (i, id) in ids.iter().enumerate() {
        if let Some(loc) = find(id) {
            at.entry(loc).or_insert(i);
        }
    }
    let back = Body::decode(&stored)?.chunk(|src, k| {
        let i = at.get(&(sources[src], k)).ok_or_else(|| anyhow!("unexpected reference"))?;
        Ok(sections[*i].clone())
    })?;
    ensure!(back == nbt_bytes, "grouped encoding is not reversible for this chunk");
    Ok(Encoded { stored, new, sources })
}

fn make_ref(src: u8, slot: u32) -> Vec<u8> {
    let mut r = vec![src];
    put_varint(&mut r, slot as u64);
    r
}

fn parse_ref(r: &[u8]) -> Result<(u8, u32)> {
    let (&src, rest) = r.split_first().ok_or_else(|| anyhow!("empty section reference"))?;
    let mut c = Cursor { buf: rest, pos: 0 };
    let k = c.varint()?;
    ensure!(c.pos == rest.len() && k <= u32::MAX as u64, "bad section reference");
    Ok((src, k as u32))
}

fn put_varint(out: &mut Vec<u8>, mut n: u64) {
    loop {
        let b = (n & 0x7F) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

struct Cursor<'a> {
    buf: &'a [u8],
    pos: usize,
}

impl Cursor<'_> {
    fn varint(&mut self) -> Result<u64> {
        let mut n = 0u64;
        for shift in (0..64).step_by(7) {
            let &b = self.buf.get(self.pos).ok_or_else(|| anyhow!("truncated varint"))?;
            self.pos += 1;
            n |= ((b & 0x7F) as u64) << shift;
            if b & 0x80 == 0 {
                return Ok(n);
            }
        }
        bail!("varint too long")
    }

    fn take(&mut self, n: usize) -> Result<Range<usize>> {
        let end =
            self.pos.checked_add(n).filter(|&e| e <= self.buf.len()).ok_or_else(|| anyhow!("truncated"))?;
        let r = self.pos..end;
        self.pos = end;
        Ok(r)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn decode_with(stored: &[u8], others: &HashMap<ObjectId, Vec<u8>>) -> Result<Vec<u8>> {
        let body = Body::decode(stored)?;
        body.chunk(|i, k| {
            let src = Body::decode(&others[&body.sources[i]])?;
            Ok(src.slot(k).ok_or_else(|| anyhow!("no slot"))?.to_vec())
        })
    }

    #[test]
    fn standalone_and_with_sources() {
        let a = crate::synth::chunk_nbt(3, 4, 4, 0);
        let ea = encode(&a, split(&a).unwrap(), None, |_| None, 3).unwrap();
        assert!(ea.sources.is_empty());
        assert_eq!(ea.new.len(), 24);
        assert_eq!(decode_with(&ea.stored, &HashMap::new()).unwrap(), a);

        // A later version: unchanged sections come from `a`'s object.
        let id_a = ObjectId::of(&a);
        let slot_of: HashMap<ObjectId, u32> =
            ea.new.iter().enumerate().map(|(k, s)| (*s, k as u32)).collect();
        let b = crate::synth::chunk_nbt(3, 4, 4, 40);
        let eb = encode(&b, split(&b).unwrap(), None, |s| slot_of.get(s).map(|&k| (id_a, k)), 3).unwrap();
        assert_eq!(eb.sources, [id_a]);
        assert!(eb.new.len() < ea.new.len());
        assert!(eb.stored.len() < ea.stored.len() / 2, "{} vs {}", eb.stored.len(), ea.stored.len());
        let others = HashMap::from([(id_a, ea.stored.clone())]);
        assert_eq!(decode_with(&eb.stored, &others).unwrap(), b);
        assert_eq!(sources(&eb.stored[..MAX_HEADER_LEN.min(eb.stored.len())]).unwrap(), [id_a]);
    }

    #[test]
    fn a_wrong_location_is_refused() {
        // `find` claims every section is slot 0 of some object: decoding through that
        // claim gives the wrong chunk, which `encode` must notice... unless it isn't wrong.
        let a = crate::synth::chunk_nbt(3, 1, 1, 7);
        let x = ObjectId::of(b"x");
        let sp = split(&a).unwrap();
        let distinct: std::collections::HashSet<_> = sp.ids.iter().collect();
        assert!(distinct.len() > 1);
        assert!(encode(&a, sp, None, |_| Some((x, 0)), 3).is_err());
    }

    #[test]
    fn a_repair_keeps_the_old_slots() {
        let a = crate::synth::chunk_nbt(5, 2, 2, 0);
        let old = encode(&a, split(&a).unwrap(), None, |_| None, 3).unwrap();
        let old_body = Body::decode(&old.stored).unwrap();
        let b = crate::synth::chunk_nbt(5, 2, 2, 60);
        let fixed = encode(&b, split(&b).unwrap(), Some(&old_body), |_| None, 3).unwrap();
        let body = Body::decode(&fixed.stored).unwrap();
        assert!(body.slots() > old_body.slots());
        for k in 0..old_body.slots() as u32 {
            assert_eq!(body.slot(k), old_body.slot(k));
        }
        assert_eq!(decode_with(&fixed.stored, &HashMap::new()).unwrap(), b);
    }

    #[test]
    fn not_chunks_are_not_split() {
        assert!(split(b"junk").is_none());
        let root = Root { name: vec![], tag: Tag::Compound(vec![(b"x".to_vec(), Tag::Int(1))]) };
        assert!(split(&root.write()).is_none());
    }

    #[test]
    fn damage_never_decodes_wrong() {
        let a = crate::synth::chunk_nbt(9, 0, 0, 3);
        let e = encode(&a, split(&a).unwrap(), None, |_| None, 3).unwrap();
        for i in 0..e.stored.len() {
            let mut d = e.stored.clone();
            d[i] ^= 0x10;
            // Either refused, or (a bit zstd ignores) still exactly right: never wrong.
            if let Ok(back) = decode_with(&d, &HashMap::new()) {
                assert_eq!(back, a, "flip at {i} decoded to something else");
            }
        }
    }
}
