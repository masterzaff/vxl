//! `cache/packs.idx`: every pack's index in one file, so opening a repo reads one
//! file instead of seeking to the end of each of thousands of packs.
//!
//! Packs are immutable and named by the hash of their content, so a cached entry is
//! valid exactly as long as a pack with that id and size exists. The repo checks that
//! against a directory listing on every open, reads only packs the cache doesn't know,
//! and drops entries for packs that are gone. The file ends with a BLAKE3 of its body;
//! a damaged or unreadable cache is ignored (and rewritten), never trusted.
//!
//! ```text
//! "VXLIDX\0\1" | u32 packs | per pack: id[32] size:u64 n:u32 n × (id[32] offset:u64 len:u32) | blake3[32]
//! ```
//! (integers little-endian)

use std::collections::HashMap;
use std::fs;
use std::path::Path;

use anyhow::Result;

use crate::pack::Entry;
use crate::repo::ObjectId;
use crate::util::atomic_write;

const MAGIC: &[u8; 8] = b"VXLIDX\0\x01";
const ENTRY_LEN: usize = 32 + 8 + 4;

/// pack id → (pack file size, entries)
pub type Cached = HashMap<ObjectId, (u64, Vec<Entry>)>;

/// Read the cache; `None` if it's missing, damaged or from another format.
pub fn read(path: &Path) -> Option<Cached> {
    decode(&fs::read(path).ok()?)
}

pub fn decode(b: &[u8]) -> Option<Cached> {
    let body = b.strip_prefix(MAGIC)?;
    let (body, sum) = body.split_at_checked(body.len().checked_sub(32)?)?;
    if blake3::hash(body).as_bytes() != sum {
        return None;
    }
    let mut r = Reader(body);
    let packs = r.u32()?;
    let mut out = Cached::new();
    for _ in 0..packs {
        let id = r.id()?;
        let size = r.u64()?;
        let n = r.u32()? as usize;
        if n.checked_mul(ENTRY_LEN)? > r.0.len() {
            return None;
        }
        let mut entries = Vec::with_capacity(n);
        for _ in 0..n {
            entries.push(Entry { id: r.id()?, offset: r.u64()?, len: r.u32()? });
        }
        out.insert(id, (size, entries));
    }
    r.0.is_empty().then_some(out)
}

pub fn encode<'a>(packs: impl ExactSizeIterator<Item = (&'a ObjectId, u64, &'a [Entry])>) -> Vec<u8> {
    let mut b = Vec::new();
    b.extend_from_slice(&(packs.len() as u32).to_le_bytes());
    for (id, size, entries) in packs {
        b.extend_from_slice(&id.0);
        b.extend_from_slice(&size.to_le_bytes());
        b.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        for e in entries {
            b.extend_from_slice(&e.id.0);
            b.extend_from_slice(&e.offset.to_le_bytes());
            b.extend_from_slice(&e.len.to_le_bytes());
        }
    }
    let sum = *blake3::hash(&b).as_bytes();
    let mut out = MAGIC.to_vec();
    out.extend_from_slice(&b);
    out.extend_from_slice(&sum);
    out
}

pub fn write<'a>(
    path: &Path,
    tmp_dir: &Path,
    packs: impl ExactSizeIterator<Item = (&'a ObjectId, u64, &'a [Entry])>,
) -> Result<()> {
    fs::create_dir_all(path.parent().unwrap())?;
    atomic_write(path, tmp_dir, &encode(packs))
}

struct Reader<'a>(&'a [u8]);

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Option<&[u8]> {
        let (a, rest) = self.0.split_at_checked(n)?;
        self.0 = rest;
        Some(a)
    }
    fn u32(&mut self) -> Option<u32> {
        Some(u32::from_le_bytes(self.take(4)?.try_into().ok()?))
    }
    fn u64(&mut self) -> Option<u64> {
        Some(u64::from_le_bytes(self.take(8)?.try_into().ok()?))
    }
    fn id(&mut self) -> Option<ObjectId> {
        Some(ObjectId(self.take(32)?.try_into().ok()?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_and_damage() {
        let (a, b) = (ObjectId::of(b"a"), ObjectId::of(b"b"));
        let ea = vec![Entry { id: ObjectId::of(b"x"), offset: 8, len: 100 }];
        let eb = vec![];
        let bytes = encode([(&a, 1000, ea.as_slice()), (&b, 64, eb.as_slice())].into_iter());
        let got = decode(&bytes).unwrap();
        assert_eq!(got.len(), 2);
        assert_eq!(got[&a].0, 1000);
        assert_eq!(got[&a].1[0].len, 100);
        assert!(got[&b].1.is_empty());
        // Any flipped byte or truncation is rejected.
        for i in 0..bytes.len() {
            let mut d = bytes.clone();
            d[i] ^= 1;
            assert!(decode(&d).is_none(), "flip at {i}");
        }
        assert!(decode(&bytes[..bytes.len() - 1]).is_none());
        assert!(decode(b"").is_none());
    }
}
