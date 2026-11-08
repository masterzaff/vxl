//! Byte-exact NBT parsing and writing.
//!
//! The goal is `write(parse(bytes)) == bytes` for anything Minecraft writes, so
//! vxl can take chunks apart and put them back together *identically*:
//! - compound entries keep their order,
//! - strings stay raw bytes (Java's "modified UTF-8" isn't quite UTF-8),
//! - floats/doubles are kept as raw bits (NaN payloads survive),
//! - list element types are kept even for empty lists.
//!
//! [`Tag::Unpacked`] is a vxl-only extension used by `transform.rs`; it never
//! appears in Minecraft data and is only accepted when parsing with `ext = true`.

use anyhow::{Result, bail};

pub const END: u8 = 0;
pub const BYTE: u8 = 1;
pub const SHORT: u8 = 2;
pub const INT: u8 = 3;
pub const LONG: u8 = 4;
pub const FLOAT: u8 = 5;
pub const DOUBLE: u8 = 6;
pub const BYTE_ARRAY: u8 = 7;
pub const STRING: u8 = 8;
pub const LIST: u8 = 9;
pub const COMPOUND: u8 = 10;
pub const INT_ARRAY: u8 = 11;
pub const LONG_ARRAY: u8 = 12;
/// vxl extension: a long/byte array expanded to one value per index.
pub const UNPACKED: u8 = 0x40;

/// Same limit Minecraft uses; stops stack overflows on hostile data.
const MAX_DEPTH: usize = 512;

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Tag {
    Byte(i8),
    Short(i16),
    Int(i32),
    Long(i64),
    /// Raw IEEE-754 bits.
    Float(u32),
    /// Raw IEEE-754 bits.
    Double(u64),
    ByteArray(Vec<u8>),
    /// Raw (modified UTF-8) bytes.
    String(Vec<u8>),
    /// (element type, items)
    List(u8, Vec<Tag>),
    Compound(Vec<(Vec<u8>, Tag)>),
    IntArray(Vec<i32>),
    LongArray(Vec<i64>),
    /// vxl extension. `orig` is the tag type it replaced (LONG_ARRAY or BYTE_ARRAY),
    /// `bits` the packed width, `values` one entry per packed value.
    Unpacked {
        orig: u8,
        bits: u8,
        values: Vec<u16>,
    },
}

/// A whole NBT document: root tag type, root name, root payload.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Root {
    pub name: Vec<u8>,
    pub tag: Tag,
}

impl Tag {
    pub fn id(&self) -> u8 {
        match self {
            Tag::Byte(_) => BYTE,
            Tag::Short(_) => SHORT,
            Tag::Int(_) => INT,
            Tag::Long(_) => LONG,
            Tag::Float(_) => FLOAT,
            Tag::Double(_) => DOUBLE,
            Tag::ByteArray(_) => BYTE_ARRAY,
            Tag::String(_) => STRING,
            Tag::List(..) => LIST,
            Tag::Compound(_) => COMPOUND,
            Tag::IntArray(_) => INT_ARRAY,
            Tag::LongArray(_) => LONG_ARRAY,
            Tag::Unpacked { .. } => UNPACKED,
        }
    }

    /// Look up a compound entry by name.
    pub fn get(&self, key: &str) -> Option<&Tag> {
        match self {
            Tag::Compound(entries) => entries.iter().find(|(k, _)| k == key.as_bytes()).map(|(_, v)| v),
            _ => None,
        }
    }

    pub fn get_mut(&mut self, key: &str) -> Option<&mut Tag> {
        match self {
            Tag::Compound(entries) => entries.iter_mut().find(|(k, _)| k == key.as_bytes()).map(|(_, v)| v),
            _ => None,
        }
    }

    /// Serialise just the payload (no type byte or name).
    pub fn write_payload(&self, out: &mut Vec<u8>) {
        match self {
            Tag::Byte(v) => out.push(*v as u8),
            Tag::Short(v) => out.extend_from_slice(&v.to_be_bytes()),
            Tag::Int(v) => out.extend_from_slice(&v.to_be_bytes()),
            Tag::Long(v) => out.extend_from_slice(&v.to_be_bytes()),
            Tag::Float(v) => out.extend_from_slice(&v.to_be_bytes()),
            Tag::Double(v) => out.extend_from_slice(&v.to_be_bytes()),
            Tag::ByteArray(v) => {
                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                out.extend_from_slice(v);
            }
            Tag::String(s) => {
                out.extend_from_slice(&(s.len() as u16).to_be_bytes());
                out.extend_from_slice(s);
            }
            Tag::List(ty, items) => {
                out.push(*ty);
                out.extend_from_slice(&(items.len() as i32).to_be_bytes());
                for i in items {
                    i.write_payload(out);
                }
            }
            Tag::Compound(entries) => {
                for (k, v) in entries {
                    out.push(v.id());
                    out.extend_from_slice(&(k.len() as u16).to_be_bytes());
                    out.extend_from_slice(k);
                    v.write_payload(out);
                }
                out.push(END);
            }
            Tag::IntArray(v) => {
                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                for x in v {
                    out.extend_from_slice(&x.to_be_bytes());
                }
            }
            Tag::LongArray(v) => {
                out.extend_from_slice(&(v.len() as i32).to_be_bytes());
                for x in v {
                    out.extend_from_slice(&x.to_be_bytes());
                }
            }
            Tag::Unpacked { orig, bits, values } => {
                out.push(*orig);
                out.push(*bits);
                out.extend_from_slice(&(values.len() as u32).to_be_bytes());
                if *bits <= 8 {
                    out.extend(values.iter().map(|&v| v as u8));
                } else {
                    for v in values {
                        out.extend_from_slice(&v.to_be_bytes());
                    }
                }
            }
        }
    }
}

impl Root {
    pub fn write(&self) -> Vec<u8> {
        let mut out = Vec::new();
        out.push(self.tag.id());
        out.extend_from_slice(&(self.name.len() as u16).to_be_bytes());
        out.extend_from_slice(&self.name);
        self.tag.write_payload(&mut out);
        out
    }
}

/// Parse a complete NBT document. Fails on trailing bytes, so a success means
/// `parse(b)?.write() == b` for Minecraft data. `ext` allows [`Tag::Unpacked`].
pub fn parse(bytes: &[u8], ext: bool) -> Result<Root> {
    let mut r = Reader { buf: bytes, pos: 0, ext };
    let ty = r.u8()?;
    if ty == END {
        bail!("empty NBT document");
    }
    let name = r.string()?;
    let tag = r.payload(ty, 0)?;
    if r.pos != bytes.len() {
        bail!("{} trailing bytes after NBT document", bytes.len() - r.pos);
    }
    Ok(Root { name, tag })
}

/// Parse a bare compound payload (as written by `Tag::write_payload`).
pub fn parse_compound_payload(bytes: &[u8], ext: bool) -> Result<Tag> {
    let mut r = Reader { buf: bytes, pos: 0, ext };
    let tag = r.payload(COMPOUND, 0)?;
    if r.pos != bytes.len() {
        bail!("trailing bytes after compound");
    }
    Ok(tag)
}

struct Reader<'a> {
    buf: &'a [u8],
    pos: usize,
    ext: bool,
}

impl Reader<'_> {
    fn take(&mut self, n: usize) -> Result<&[u8]> {
        if self.buf.len() - self.pos < n {
            bail!("unexpected end of NBT data");
        }
        let s = &self.buf[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }

    fn remaining(&self) -> usize {
        self.buf.len() - self.pos
    }

    fn u8(&mut self) -> Result<u8> {
        Ok(self.take(1)?[0])
    }

    fn arr<const N: usize>(&mut self) -> Result<[u8; N]> {
        Ok(self.take(N)?.try_into().unwrap())
    }

    /// A non-negative i32 length, at most `remaining / elem_size`.
    fn len(&mut self, elem_size: usize) -> Result<usize> {
        let n = i32::from_be_bytes(self.arr()?);
        if n < 0 || (n as usize).saturating_mul(elem_size.max(1)) > self.remaining() {
            bail!("invalid NBT length {n}");
        }
        Ok(n as usize)
    }

    fn string(&mut self) -> Result<Vec<u8>> {
        let n = u16::from_be_bytes(self.arr()?) as usize;
        Ok(self.take(n)?.to_vec())
    }

    fn payload(&mut self, ty: u8, depth: usize) -> Result<Tag> {
        if depth > MAX_DEPTH {
            bail!("NBT nested too deeply");
        }
        Ok(match ty {
            BYTE => Tag::Byte(self.u8()? as i8),
            SHORT => Tag::Short(i16::from_be_bytes(self.arr()?)),
            INT => Tag::Int(i32::from_be_bytes(self.arr()?)),
            LONG => Tag::Long(i64::from_be_bytes(self.arr()?)),
            FLOAT => Tag::Float(u32::from_be_bytes(self.arr()?)),
            DOUBLE => Tag::Double(u64::from_be_bytes(self.arr()?)),
            BYTE_ARRAY => {
                let n = self.len(1)?;
                Tag::ByteArray(self.take(n)?.to_vec())
            }
            STRING => Tag::String(self.string()?),
            LIST => {
                let elem = self.u8()?;
                // every non-End element is at least 1 byte, so `len(1)` bounds the allocation
                let n = self.len(1)?;
                if elem == END && n > 0 {
                    bail!("list of End tags with {n} items");
                }
                let mut items = Vec::with_capacity(n);
                for _ in 0..n {
                    items.push(self.payload(elem, depth + 1)?);
                }
                Tag::List(elem, items)
            }
            COMPOUND => {
                let mut entries = Vec::new();
                loop {
                    let t = self.u8()?;
                    if t == END {
                        break;
                    }
                    let k = self.string()?;
                    entries.push((k, self.payload(t, depth + 1)?));
                }
                Tag::Compound(entries)
            }
            INT_ARRAY => {
                let n = self.len(4)?;
                Tag::IntArray((0..n).map(|_| Ok(i32::from_be_bytes(self.arr()?))).collect::<Result<_>>()?)
            }
            LONG_ARRAY => {
                let n = self.len(8)?;
                Tag::LongArray((0..n).map(|_| Ok(i64::from_be_bytes(self.arr()?))).collect::<Result<_>>()?)
            }
            UNPACKED if self.ext => {
                let orig = self.u8()?;
                let bits = self.u8()?;
                let n = u32::from_be_bytes(self.arr()?) as usize;
                let width = if bits <= 8 { 1 } else { 2 };
                if n.saturating_mul(width) > self.remaining() {
                    bail!("invalid unpacked length");
                }
                let values = if width == 1 {
                    self.take(n)?.iter().map(|&b| b as u16).collect()
                } else {
                    (0..n).map(|_| Ok(u16::from_be_bytes(self.arr()?))).collect::<Result<_>>()?
                };
                Tag::Unpacked { orig, bits, values }
            }
            other => bail!("unknown NBT tag type {other}"),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn synthetic_chunks_roundtrip_exactly() {
        for (cx, cz, e) in [(0, 0, 0), (-5, 9, 3), (31, -31, 1)] {
            let bytes = crate::synth::chunk_nbt(1, cx, cz, e);
            let root = parse(&bytes, false).unwrap();
            assert_eq!(root.write(), bytes);
            assert!(root.tag.get("sections").is_some());
        }
    }

    #[test]
    fn odd_but_valid_values_survive() {
        let nan_payload = 0x7FC0_1234u32;
        let root = Root {
            name: b"root".to_vec(),
            tag: Tag::Compound(vec![
                (b"f".to_vec(), Tag::Float(nan_payload)),
                (b"d".to_vec(), Tag::Double(0x8000_0000_0000_0000)), // -0.0
                // modified UTF-8 null (0xC0 0x80) is not valid UTF-8
                (b"s".to_vec(), Tag::String(vec![b'a', 0xC0, 0x80, b'b'])),
                (b"empty_typed_list".to_vec(), Tag::List(COMPOUND, vec![])),
                (b"empty_end_list".to_vec(), Tag::List(END, vec![])),
                (b"ints".to_vec(), Tag::IntArray(vec![-1, 0, i32::MAX])),
                (b"dup".to_vec(), Tag::Byte(1)),
                (b"dup".to_vec(), Tag::Byte(2)), // duplicate keys are kept as-is
            ]),
        };
        let bytes = root.write();
        let back = parse(&bytes, false).unwrap();
        assert_eq!(back, root);
        assert_eq!(back.write(), bytes);
    }

    #[test]
    fn rejects_garbage() {
        assert!(parse(&[], false).is_err());
        assert!(parse(&[10, 0, 0], false).is_err()); // compound never closed
        assert!(parse(&[10, 0, 0, 0, 99], false).is_err()); // trailing byte
        assert!(parse(&[7, 0, 0, 0x7F, 0xFF, 0xFF, 0xFF], false).is_err()); // huge length
        assert!(parse(&[9, 0, 0, 0, 0, 0, 0, 5], false).is_err()); // 5 End items
        let mut deep = vec![10, 0, 0];
        for _ in 0..600 {
            deep.extend_from_slice(&[10, 0, 1, b'x']);
        }
        assert!(parse(&deep, false).is_err());
        // the extension tag only parses when allowed
        let ext = Root {
            name: vec![],
            tag: Tag::Compound(vec![(
                b"u".to_vec(),
                Tag::Unpacked { orig: 12, bits: 4, values: vec![1, 2] },
            )]),
        };
        assert!(parse(&ext.write(), false).is_err());
        assert_eq!(parse(&ext.write(), true).unwrap(), ext);
    }
}
