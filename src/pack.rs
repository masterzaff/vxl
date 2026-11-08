//! Pack files: many objects in one file, with the pack's own index at the end.
//!
//! ```text
//! "VXLPACK\x01"                                   8-byte magic
//! object data                                      zstd frames, back to back
//! index      count × ( id[32] | offset u64 LE | len u32 LE )
//! trailer    count u32 LE | index offset u64 LE | "VXLPEND\x01"
//! ```
//!
//! A pack is written once, in full, then renamed into place — it's never
//! modified afterwards. Its file name is the BLAKE3 of its whole contents, so
//! the name doubles as a checksum. Pure format code: no repo knowledge.

use std::fs::File;
use std::io;
use std::path::Path;

use anyhow::{Context, Result, bail};

use crate::repo::ObjectId;

pub const MAGIC: &[u8; 8] = b"VXLPACK\x01";
pub const END_MAGIC: &[u8; 8] = b"VXLPEND\x01";
const ENTRY_LEN: usize = 32 + 8 + 4;
const TRAILER_LEN: usize = 4 + 8 + 8;

/// Where one object lives inside a pack.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Entry {
    pub id: ObjectId,
    pub offset: u64,
    /// Compressed length.
    pub len: u32,
}

/// Accumulates objects in memory until the pack is big enough to write.
pub struct Builder {
    buf: Vec<u8>,
    entries: Vec<Entry>,
}

impl Default for Builder {
    fn default() -> Self {
        Builder { buf: MAGIC.to_vec(), entries: Vec::new() }
    }
}

/// A finished pack, ready to be written to disk.
pub struct Finished {
    pub id: ObjectId,
    pub bytes: Vec<u8>,
    pub entries: Vec<Entry>,
}

impl Builder {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    /// Bytes of object data so far.
    pub fn data_len(&self) -> u64 {
        (self.buf.len() - MAGIC.len()) as u64
    }

    /// Append an already-compressed object.
    pub fn add(&mut self, id: ObjectId, compressed: &[u8]) -> Result<()> {
        let len = u32::try_from(compressed.len()).context("object larger than 4 GiB")?;
        self.entries.push(Entry { id, offset: self.buf.len() as u64, len });
        self.buf.extend_from_slice(compressed);
        Ok(())
    }

    pub fn finish(self) -> Finished {
        let Builder { mut buf, entries } = self;
        let index_offset = buf.len() as u64;
        for e in &entries {
            buf.extend_from_slice(&e.id.0);
            buf.extend_from_slice(&e.offset.to_le_bytes());
            buf.extend_from_slice(&e.len.to_le_bytes());
        }
        buf.extend_from_slice(&(entries.len() as u32).to_le_bytes());
        buf.extend_from_slice(&index_offset.to_le_bytes());
        buf.extend_from_slice(END_MAGIC);
        Finished { id: ObjectId::of(&buf), bytes: buf, entries }
    }
}

/// Read exactly `buf.len()` bytes at `offset` without moving a shared cursor,
/// so many threads can read one open pack at once.
pub fn read_at(f: &File, buf: &mut [u8], offset: u64) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::FileExt;
        f.read_exact_at(buf, offset)
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::FileExt;
        let mut done = 0;
        while done < buf.len() {
            match f.seek_read(&mut buf[done..], offset + done as u64)? {
                0 => return Err(io::ErrorKind::UnexpectedEof.into()),
                n => done += n,
            }
        }
        Ok(())
    }
}

/// Read a pack's index from the end of the file. Returns (file size, entries).
pub fn read_index(path: &Path) -> Result<(u64, Vec<Entry>)> {
    let f = File::open(path)?;
    let size = f.metadata()?.len();
    if size < (MAGIC.len() + TRAILER_LEN) as u64 {
        bail!("too short to be a pack");
    }
    let mut head = [0u8; 8];
    read_at(&f, &mut head, 0)?;
    let mut trailer = [0u8; TRAILER_LEN];
    read_at(&f, &mut trailer, size - TRAILER_LEN as u64)?;
    if &head != MAGIC || &trailer[12..] != END_MAGIC {
        bail!("not a vxl pack (bad magic)");
    }
    let count = u32::from_le_bytes(trailer[..4].try_into().unwrap()) as u64;
    let index_offset = u64::from_le_bytes(trailer[4..12].try_into().unwrap());
    // Checked: a damaged trailer can hold any value, and a wrapped sum could pass the
    // check and make us allocate `count` entries' worth of garbage.
    let end =
        index_offset.checked_add(count * ENTRY_LEN as u64).and_then(|e| e.checked_add(TRAILER_LEN as u64));
    if index_offset < MAGIC.len() as u64 || end != Some(size) {
        bail!("corrupt pack index");
    }
    let mut raw = vec![0u8; (count as usize) * ENTRY_LEN];
    read_at(&f, &mut raw, index_offset)?;
    let (entries, _) = raw.as_chunks::<ENTRY_LEN>();
    let entries = entries
        .iter()
        .map(|e| {
            let entry = Entry {
                id: ObjectId(e[..32].try_into().unwrap()),
                offset: u64::from_le_bytes(e[32..40].try_into().unwrap()),
                len: u32::from_le_bytes(e[40..44].try_into().unwrap()),
            };
            if entry.offset < MAGIC.len() as u64
                || entry.offset.checked_add(entry.len as u64).is_none_or(|end| end > index_offset)
            {
                bail!("corrupt pack index (entry out of bounds)");
            }
            Ok(entry)
        })
        .collect::<Result<_>>()?;
    Ok((size, entries))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_and_read_back() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = Builder::default();
        assert!(b.is_empty());
        b.add(ObjectId::of(b"a"), b"AAAA").unwrap();
        b.add(ObjectId::of(b"b"), b"").unwrap();
        b.add(ObjectId::of(b"c"), b"CC").unwrap();
        assert_eq!(b.data_len(), 6);
        let done = b.finish();
        assert_eq!(done.id, ObjectId::of(&done.bytes));
        let path = dir.path().join("p");
        std::fs::write(&path, &done.bytes).unwrap();

        let (size, entries) = read_index(&path).unwrap();
        assert_eq!(size, done.bytes.len() as u64);
        assert_eq!(entries, done.entries);
        let f = File::open(&path).unwrap();
        let mut buf = vec![0; 2];
        read_at(&f, &mut buf, entries[2].offset).unwrap();
        assert_eq!(buf, b"CC");
    }

    #[test]
    fn damaged_packs_are_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let mut b = Builder::default();
        b.add(ObjectId::of(b"a"), b"AAAA").unwrap();
        let bytes = b.finish().bytes;
        let path = dir.path().join("p");
        // truncated
        std::fs::write(&path, &bytes[..bytes.len() - 3]).unwrap();
        assert!(read_index(&path).is_err());
        // index offset pointing elsewhere
        let mut bad = bytes.clone();
        let n = bad.len();
        bad[n - 16] ^= 1;
        std::fs::write(&path, &bad).unwrap();
        assert!(read_index(&path).is_err());
        assert!(std::fs::write(&path, b"short").is_ok() && read_index(&path).is_err());
    }
}
