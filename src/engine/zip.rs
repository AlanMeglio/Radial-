//! Minimal zip reader/writer (stored + deflate, no zip64, no encryption). Enough for DOCX.

use super::error::{Error, Result};

pub struct ZipWriter {
    out: Vec<u8>,
    central: Vec<u8>,
    count: u16,
}

impl Default for ZipWriter {
    fn default() -> Self {
        Self::new()
    }
}

fn u16le(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn u32le(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

/// 1980-01-01 00:00: fixed so identical input gives identical bytes.
const DOS_TIME: u16 = 0;
const DOS_DATE: u16 = 0x21;

impl ZipWriter {
    pub fn new() -> Self {
        ZipWriter { out: Vec::new(), central: Vec::new(), count: 0 }
    }

    /// Adds a file; `deflate` picks compression (already-compressed media should pass `false`).
    pub fn add(&mut self, name: &str, data: &[u8], deflate: bool) {
        use std::io::Write;
        let mut method = 0u16;
        let mut body = std::borrow::Cow::Borrowed(data);
        if deflate && !data.is_empty() {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
            // Writing to a Vec cannot fail.
            let packed = enc.write_all(data).and_then(|()| enc.finish()).unwrap_or_default();
            if !packed.is_empty() && packed.len() < data.len() {
                method = 8;
                body = std::borrow::Cow::Owned(packed);
            }
        }
        let crc = crc32fast::hash(data);
        let flags: u16 = if name.is_ascii() { 0 } else { 1 << 11 }; // UTF-8 names
        let offset = self.out.len() as u32;
        let o = &mut self.out;
        u32le(o, 0x0403_4b50);
        u16le(o, 20);
        u16le(o, flags);
        u16le(o, method);
        u16le(o, DOS_TIME);
        u16le(o, DOS_DATE);
        u32le(o, crc);
        u32le(o, body.len() as u32);
        u32le(o, data.len() as u32);
        u16le(o, name.len() as u16);
        u16le(o, 0);
        o.extend_from_slice(name.as_bytes());
        o.extend_from_slice(&body);

        let c = &mut self.central;
        u32le(c, 0x0201_4b50);
        u16le(c, 20);
        u16le(c, 20);
        u16le(c, flags);
        u16le(c, method);
        u16le(c, DOS_TIME);
        u16le(c, DOS_DATE);
        u32le(c, crc);
        u32le(c, body.len() as u32);
        u32le(c, data.len() as u32);
        u16le(c, name.len() as u16);
        u16le(c, 0);
        u16le(c, 0);
        u16le(c, 0);
        u16le(c, 0);
        u32le(c, 0);
        u32le(c, offset);
        c.extend_from_slice(name.as_bytes());
        self.count += 1;
    }

    pub fn finish(mut self) -> Vec<u8> {
        let cd_offset = self.out.len() as u32;
        let cd_len = self.central.len() as u32;
        self.out.extend_from_slice(&self.central);
        let o = &mut self.out;
        u32le(o, 0x0605_4b50);
        u16le(o, 0);
        u16le(o, 0);
        u16le(o, self.count);
        u16le(o, self.count);
        u32le(o, cd_len);
        u32le(o, cd_offset);
        u16le(o, 0);
        self.out
    }
}

pub struct ZipArchive<'a> {
    data: &'a [u8],
    entries: Vec<Entry>,
}

struct Entry {
    name: String,
    method: u16,
    crc: u32,
    comp_size: usize,
    size: usize,
    offset: usize,
}

fn rd16(d: &[u8], at: usize) -> Option<u16> {
    Some(u16::from_le_bytes(d.get(at..at + 2)?.try_into().ok()?))
}
fn rd32(d: &[u8], at: usize) -> Option<u32> {
    Some(u32::from_le_bytes(d.get(at..at + 4)?.try_into().ok()?))
}

fn bad(msg: &str) -> Error {
    Error::Corrupt(format!("zip: {msg}"))
}

impl<'a> ZipArchive<'a> {
    pub fn new(data: &'a [u8]) -> Result<Self> {
        // The end-of-central-directory record sits in the last 22 + 65535 bytes.
        let from = data.len().saturating_sub(22 + 65_535);
        let eocd = (from..data.len().saturating_sub(21))
            .rev()
            .find(|&i| data[i..].starts_with(&[0x50, 0x4b, 0x05, 0x06]))
            .ok_or_else(|| bad("no end-of-directory record"))?;
        let count = rd16(data, eocd + 10).ok_or_else(|| bad("truncated"))? as usize;
        let cd_len = rd32(data, eocd + 12).ok_or_else(|| bad("truncated"))? as usize;
        let mut pos = rd32(data, eocd + 16).ok_or_else(|| bad("truncated"))? as usize;
        if pos.saturating_add(cd_len) > data.len() {
            return Err(bad("directory out of range"));
        }
        let mut entries = Vec::with_capacity(count.min(1024));
        for _ in 0..count {
            if rd32(data, pos) != Some(0x0201_4b50) {
                return Err(bad("bad directory entry"));
            }
            let flags = rd16(data, pos + 8).ok_or_else(|| bad("truncated"))?;
            let method = rd16(data, pos + 10).ok_or_else(|| bad("truncated"))?;
            let crc = rd32(data, pos + 16).ok_or_else(|| bad("truncated"))?;
            let comp = rd32(data, pos + 20).ok_or_else(|| bad("truncated"))?;
            let size = rd32(data, pos + 24).ok_or_else(|| bad("truncated"))?;
            let nlen = rd16(data, pos + 28).ok_or_else(|| bad("truncated"))? as usize;
            let xlen = rd16(data, pos + 30).ok_or_else(|| bad("truncated"))? as usize;
            let clen = rd16(data, pos + 32).ok_or_else(|| bad("truncated"))? as usize;
            let offset = rd32(data, pos + 42).ok_or_else(|| bad("truncated"))?;
            if flags & 1 != 0 {
                return Err(Error::Unsupported("encrypted zip entry".into()));
            }
            if comp == u32::MAX || size == u32::MAX || offset == u32::MAX {
                return Err(Error::Unsupported("zip64 archive".into()));
            }
            let name = data.get(pos + 46..pos + 46 + nlen).ok_or_else(|| bad("truncated name"))?;
            entries.push(Entry {
                name: String::from_utf8_lossy(name).into_owned(),
                method,
                crc,
                comp_size: comp as usize,
                size: size as usize,
                offset: offset as usize,
            });
            pos += 46 + nlen + xlen + clen;
        }
        Ok(ZipArchive { data, entries })
    }

    pub fn names(&self) -> impl Iterator<Item = &str> {
        self.entries.iter().map(|e| e.name.as_str())
    }

    /// Inflates one entry, refusing to produce more than `max` bytes.
    pub fn read(&self, name: &str, max: usize) -> Result<Option<Vec<u8>>> {
        use std::io::Read;
        let Some(e) = self.entries.iter().find(|e| e.name == name) else { return Ok(None) };
        if e.size > max {
            return Err(Error::TooLarge { width: 0, height: 0 });
        }
        let d = self.data;
        if rd32(d, e.offset) != Some(0x0403_4b50) {
            return Err(bad("bad local header"));
        }
        let nlen = rd16(d, e.offset + 26).ok_or_else(|| bad("truncated"))? as usize;
        let xlen = rd16(d, e.offset + 28).ok_or_else(|| bad("truncated"))? as usize;
        let start = e.offset + 30 + nlen + xlen;
        let raw = d.get(start..start + e.comp_size).ok_or_else(|| bad("truncated data"))?;
        let out = match e.method {
            0 => raw.to_vec(),
            8 => {
                // Trust the real stream, not the declared size: cap the output at `max` + 1.
                let mut buf = Vec::with_capacity(e.size.min(max));
                flate2::read::DeflateDecoder::new(raw)
                    .take(max as u64 + 1)
                    .read_to_end(&mut buf)
                    .map_err(|_| bad("bad deflate stream"))?;
                if buf.len() > max {
                    return Err(Error::TooLarge { width: 0, height: 0 });
                }
                buf
            }
            m => return Err(Error::Unsupported(format!("zip compression method {m}"))),
        };
        if out.len() != e.size || crc32fast::hash(&out) != e.crc {
            return Err(bad("checksum mismatch"));
        }
        Ok(Some(out))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<u8> {
        let mut w = ZipWriter::new();
        w.add("a.txt", b"hello hello hello hello hello hello", true);
        w.add("dir/b.bin", &[1, 2, 3, 4], false);
        w.add("ñ.txt", "acentuación".as_bytes(), true);
        w.finish()
    }

    #[test]
    fn round_trips_stored_and_deflated_entries() {
        let z = sample();
        let a = ZipArchive::new(&z).unwrap();
        assert_eq!(a.names().collect::<Vec<_>>(), ["a.txt", "dir/b.bin", "ñ.txt"]);
        assert_eq!(a.read("a.txt", 1 << 20).unwrap().unwrap(), b"hello hello hello hello hello hello");
        assert_eq!(a.read("dir/b.bin", 1 << 20).unwrap().unwrap(), [1, 2, 3, 4]);
        assert_eq!(a.read("ñ.txt", 1 << 20).unwrap().unwrap(), "acentuación".as_bytes());
        assert!(a.read("missing", 10).unwrap().is_none());
    }

    #[test]
    fn deflate_actually_shrinks_repetitive_data() {
        let mut w = ZipWriter::new();
        w.add("x", &vec![b'a'; 10_000], true);
        assert!(w.finish().len() < 500);
    }

    #[test]
    fn output_is_deterministic() {
        assert_eq!(sample(), sample());
    }

    #[test]
    fn corrupt_payload_fails_the_crc_check() {
        let mut z = sample();
        let pos = z.windows(4).position(|w| w == [1, 2, 3, 4]).unwrap();
        z[pos] ^= 0xFF;
        let a = ZipArchive::new(&z).unwrap();
        assert!(matches!(a.read("dir/b.bin", 100), Err(Error::Corrupt(_))));
    }

    #[test]
    fn garbage_and_truncated_input_are_typed_errors() {
        assert!(matches!(ZipArchive::new(b""), Err(Error::Corrupt(_))));
        assert!(matches!(ZipArchive::new(b"not a zip at all, just text"), Err(Error::Corrupt(_))));
        let z = sample();
        assert!(matches!(ZipArchive::new(&z[..z.len() - 10]), Err(Error::Corrupt(_))));
    }

    #[test]
    fn output_limit_stops_decompression_bombs() {
        let mut w = ZipWriter::new();
        w.add("big", &vec![0u8; 1_000_000], true);
        let z = w.finish();
        let a = ZipArchive::new(&z).unwrap();
        assert!(matches!(a.read("big", 1000), Err(Error::TooLarge { .. })));
        assert_eq!(a.read("big", 1_000_000).unwrap().unwrap().len(), 1_000_000);
    }

    #[test]
    fn reads_an_archive_made_by_another_implementation() {
        // Built with System.IO.Compression (Windows PowerShell), not with this module.
        let z = include_bytes!("../../tests/fixtures/sample.zip");
        let a = ZipArchive::new(z).unwrap();
        assert_eq!(a.read("hello.txt", 1 << 16).unwrap().unwrap(), b"hello from another zip writer\n");
    }
}
