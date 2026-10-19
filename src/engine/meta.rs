//! METADATA: list and remove EXIF, XMP, text chunks and other hidden data from JPEG, PNG, WebP and TIFF.
//!
//! Stripping JPEG, PNG and WebP edits the container only: the compressed pixels are not touched, so
//! the picture is bit-identical. TIFF stores its tags inside the image directory, so it is re-encoded
//! from pixels instead.

use super::error::{Error, Result};
use super::io;
use super::jpeg;
use super::kind::{detect_head, SourceKind};
use super::raster::{self, EncodeOptions};
use crate::convert::Fmt;
use std::collections::HashSet;
use std::path::{Path, PathBuf};

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct MetaEntry {
    /// Where it was found: `EXIF`, `GPS`, `TIFF`, `XMP`, `IPTC`, `PNG`, `JPEG`.
    pub group: &'static str,
    pub key: String,
    pub value: String,
}

#[derive(Clone, Debug)]
pub struct MetaReport {
    pub entries: Vec<MetaEntry>,
    /// True when location data is present.
    pub has_gps: bool,
    /// Bytes `strip` would drop (0 for TIFF, which is re-encoded).
    pub removable_bytes: u64,
}

fn bad(msg: &str) -> Error {
    Error::Corrupt(msg.to_string())
}

fn entry(group: &'static str, key: &str, value: impl Into<String>) -> MetaEntry {
    MetaEntry { group, key: key.to_string(), value: value.into() }
}

fn kind_of(bytes: &[u8]) -> Result<Fmt> {
    match detect_head(None, bytes) {
        Some(SourceKind::Image(f @ (Fmt::Jpg | Fmt::Png | Fmt::Webp | Fmt::Tiff))) => Ok(f),
        Some(SourceKind::Image(f)) => {
            Err(Error::Unsupported(format!("metadata tools handle JPEG, PNG, WebP and TIFF, not {}", f.label())))
        }
        _ => Err(Error::Unsupported("metadata tools need an image file".into())),
    }
}

// ------------------------------------------------------------------ TIFF / EXIF structures

struct Tiff<'a> {
    d: &'a [u8],
    le: bool,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Group {
    Tiff,
    Exif,
    Gps,
}

impl Group {
    fn name(self) -> &'static str {
        match self {
            Group::Tiff => "TIFF",
            Group::Exif => "EXIF",
            Group::Gps => "GPS",
        }
    }
}

struct Raw {
    tag: u16,
    typ: u16,
    count: u32,
    /// Byte range of the value inside the buffer.
    range: std::ops::Range<usize>,
    /// The inline 4-byte value field (used for pointers).
    field: u32,
}

impl<'a> Tiff<'a> {
    fn new(d: &'a [u8]) -> Option<Tiff<'a>> {
        let le = match d.get(..4)? {
            [b'I', b'I', 42, 0] => true,
            [b'M', b'M', 0, 42] => false,
            _ => return None,
        };
        Some(Tiff { d, le })
    }
    fn u16(&self, at: usize) -> Option<u16> {
        let b: [u8; 2] = self.d.get(at..at.checked_add(2)?)?.try_into().ok()?;
        Some(if self.le { u16::from_le_bytes(b) } else { u16::from_be_bytes(b) })
    }
    fn u32(&self, at: usize) -> Option<u32> {
        let b: [u8; 4] = self.d.get(at..at.checked_add(4)?)?.try_into().ok()?;
        Some(if self.le { u32::from_le_bytes(b) } else { u32::from_be_bytes(b) })
    }
    fn first_ifd(&self) -> Option<usize> {
        Some(self.u32(4)? as usize)
    }
    fn entries(&self, off: usize) -> Vec<Raw> {
        let Some(n) = self.u16(off) else { return Vec::new() };
        let mut out = Vec::new();
        for i in 0..(n as usize).min(512) {
            let e = off + 2 + i * 12;
            let (Some(tag), Some(typ), Some(count), Some(field)) =
                (self.u16(e), self.u16(e + 2), self.u32(e + 4), self.u32(e + 8))
            else {
                break;
            };
            let unit: u64 = match typ {
                1 | 2 | 6 | 7 => 1,
                3 | 8 => 2,
                4 | 9 | 11 => 4,
                5 | 10 | 12 => 8,
                _ => continue,
            };
            let size = unit * count as u64;
            let start = if size <= 4 { e + 8 } else { field as usize };
            let Some(end) = start.checked_add(size as usize) else { continue };
            if end > self.d.len() {
                continue;
            }
            out.push(Raw { tag, typ, count, range: start..end, field });
        }
        out
    }

    fn rational(&self, at: usize, signed: bool) -> Option<(f64, f64)> {
        let (n, d) = (self.u32(at)?, self.u32(at + 4)?);
        Some(if signed { (n as i32 as f64, d as i32 as f64) } else { (n as f64, d as f64) })
    }

    fn format(&self, group: Group, r: &Raw) -> String {
        let bytes = &self.d[r.range.clone()];
        let nums = |f: &dyn Fn(usize) -> Option<String>| -> String {
            (0..(r.count as usize).min(8)).filter_map(f).collect::<Vec<_>>().join(", ")
        };
        match r.typ {
            2 => String::from_utf8_lossy(bytes).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string(),
            1 | 7 if r.tag == 0x9286 && bytes.len() > 8 => {
                String::from_utf8_lossy(&bytes[8..]).trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()
            }
            1 | 6 | 7 if bytes.len() <= 8 && r.tag != 0x927C => nums(&|i| Some(bytes[i].to_string())),
            1 | 6 | 7 => format!("{} bytes", bytes.len()),
            3 => nums(&|i| self.u16(r.range.start + i * 2).map(|v| v.to_string())),
            8 => nums(&|i| self.u16(r.range.start + i * 2).map(|v| (v as i16).to_string())),
            4 | 11 => nums(&|i| self.u32(r.range.start + i * 4).map(|v| v.to_string())),
            9 => nums(&|i| self.u32(r.range.start + i * 4).map(|v| (v as i32).to_string())),
            5 | 10 => {
                let signed = r.typ == 10;
                let at = |i: usize| self.rational(r.range.start + i * 8, signed);
                if group == Group::Gps && r.count == 3 && matches!(r.tag, 2 | 4) {
                    match (at(0), at(1), at(2)) {
                        (Some(d), Some(m), Some(s)) => {
                            format!("{}° {}′ {:.2}″", ratio(d), ratio(m), if s.1 == 0.0 { 0.0 } else { s.0 / s.1 })
                        }
                        _ => String::new(),
                    }
                } else if r.tag == 0x829A && r.count == 1 {
                    at(0)
                        .map(|(n, d)| if n == 1.0 && d > 1.0 { format!("1/{}", d) } else { ratio((n, d)) })
                        .unwrap_or_default()
                } else {
                    nums(&|i| at(i).map(ratio))
                }
            }
            _ => String::new(),
        }
    }
}

fn ratio((n, d): (f64, f64)) -> String {
    if d == 0.0 {
        return "?".into();
    }
    let v = n / d;
    if v.fract() == 0.0 {
        format!("{}", v as i64)
    } else {
        format!("{v:.4}").trim_end_matches('0').trim_end_matches('.').to_string()
    }
}

fn tag_name(g: Group, tag: u16) -> Option<&'static str> {
    Some(match (g, tag) {
        (Group::Tiff, 0x010E) => "ImageDescription",
        (Group::Tiff, 0x010F) => "Make",
        (Group::Tiff, 0x0110) => "Model",
        (Group::Tiff, 0x0112) => "Orientation",
        (Group::Tiff, 0x0131) => "Software",
        (Group::Tiff, 0x0132) => "DateTime",
        (Group::Tiff, 0x013B) => "Artist",
        (Group::Tiff, 0x013C) => "HostComputer",
        (Group::Tiff, 0x8298) => "Copyright",
        (Group::Exif, 0x829A) => "ExposureTime",
        (Group::Exif, 0x829D) => "FNumber",
        (Group::Exif, 0x8827) => "ISO",
        (Group::Exif, 0x9003) => "DateTimeOriginal",
        (Group::Exif, 0x9004) => "DateTimeDigitized",
        (Group::Exif, 0x920A) => "FocalLength",
        (Group::Exif, 0x9286) => "UserComment",
        (Group::Exif, 0x927C) => "MakerNote",
        (Group::Exif, 0xA420) => "ImageUniqueID",
        (Group::Exif, 0xA430) => "CameraOwnerName",
        (Group::Exif, 0xA431) => "BodySerialNumber",
        (Group::Exif, 0xA433) => "LensMake",
        (Group::Exif, 0xA434) => "LensModel",
        (Group::Exif, 0xA435) => "LensSerialNumber",
        (Group::Gps, 1) => "GPSLatitudeRef",
        (Group::Gps, 2) => "GPSLatitude",
        (Group::Gps, 3) => "GPSLongitudeRef",
        (Group::Gps, 4) => "GPSLongitude",
        (Group::Gps, 5) => "GPSAltitudeRef",
        (Group::Gps, 6) => "GPSAltitude",
        (Group::Gps, 7) => "GPSTimeStamp",
        (Group::Gps, 0x1D) => "GPSDateStamp",
        _ => return None,
    })
}

fn walk_ifd(
    t: &Tiff,
    off: usize,
    group: Group,
    depth: u8,
    seen: &mut HashSet<usize>,
    out: &mut Vec<MetaEntry>,
    gps: &mut bool,
) {
    if depth > 2 || !seen.insert(off) {
        return;
    }
    for r in t.entries(off) {
        match (group, r.tag) {
            (Group::Tiff, 0x8769) => walk_ifd(t, r.field as usize, Group::Exif, depth + 1, seen, out, gps),
            (Group::Tiff, 0x8825) => walk_ifd(t, r.field as usize, Group::Gps, depth + 1, seen, out, gps),
            _ => {
                if group == Group::Gps {
                    *gps = true;
                }
                if let Some(name) = tag_name(group, r.tag) {
                    out.push(entry(group.name(), name, t.format(group, &r)));
                }
            }
        }
    }
}

/// Reads every known tag of a TIFF-structured block (EXIF payload or a TIFF file) into `out`.
fn parse_tiff(tiff: &[u8], out: &mut Vec<MetaEntry>, gps: &mut bool) {
    let Some(t) = Tiff::new(tiff) else { return };
    if let Some(first) = t.first_ifd() {
        walk_ifd(&t, first, Group::Tiff, 0, &mut HashSet::new(), out, gps);
    }
}

fn tiff_orientation(tiff: &[u8]) -> Option<u16> {
    let t = Tiff::new(tiff)?;
    let r = t.entries(t.first_ifd()?).into_iter().find(|r| r.tag == 0x0112 && r.typ == 3 && r.count == 1)?;
    t.u16(r.range.start).filter(|v| (1..=8).contains(v))
}

/// A minimal EXIF APP1 payload holding only the orientation.
fn minimal_exif(orientation: u16) -> Vec<u8> {
    let mut v = b"Exif\0\0II*\0\x08\0\0\0".to_vec();
    v.extend_from_slice(&1u16.to_le_bytes());
    v.extend_from_slice(&0x0112u16.to_le_bytes());
    v.extend_from_slice(&3u16.to_le_bytes());
    v.extend_from_slice(&1u32.to_le_bytes());
    v.extend_from_slice(&orientation.to_le_bytes());
    v.extend_from_slice(&[0, 0]);
    v.extend_from_slice(&0u32.to_le_bytes());
    v
}

// ------------------------------------------------------------------ containers

/// Offset just past the EOI marker, walking inter-scan segments so payload bytes are never mistaken for markers.
fn find_eoi(b: &[u8], from: usize) -> Option<usize> {
    let mut i = from;
    while i + 1 < b.len() {
        if b[i] != 0xFF {
            i += 1;
            continue;
        }
        match b[i + 1] {
            0x00 | 0x01 | 0xD0..=0xD7 => i += 2,
            0xFF => i += 1,
            0xD9 => return Some(i + 2),
            _ => {
                let len = u16::from_be_bytes([*b.get(i + 2)?, *b.get(i + 3)?]) as usize;
                if len < 2 {
                    return None;
                }
                i += 2 + len;
            }
        }
    }
    None
}

const XMP_NS: &[u8] = b"http://ns.adobe.com/xap/1.0/\0";

fn is_removable_jpeg_segment(s: &jpeg::Segment) -> bool {
    match s.marker {
        0xE1 | 0xE3..=0xED | 0xEF | 0xFE => true,
        0xE2 => !s.data.starts_with(b"ICC_PROFILE\0"),
        _ => false,
    }
}

struct PngChunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
    start: usize,
    end: usize,
}

fn png_chunks(b: &[u8]) -> Result<Vec<PngChunk<'_>>> {
    if b.get(..8) != Some(&[0x89, b'P', b'N', b'G', 13, 10, 26, 10]) {
        return Err(bad("PNG: bad signature"));
    }
    let mut out = Vec::new();
    let mut i = 8;
    loop {
        let len = u32::from_be_bytes(b.get(i..i + 4).ok_or_else(|| bad("PNG: truncated"))?.try_into().expect("4 bytes"))
            as usize;
        let end = i
            .checked_add(12)
            .and_then(|v| v.checked_add(len))
            .filter(|&e| e <= b.len())
            .ok_or_else(|| bad("PNG: chunk past end of file"))?;
        let kind: [u8; 4] = b[i + 4..i + 8].try_into().expect("4 bytes");
        out.push(PngChunk { kind, data: &b[i + 8..i + 8 + len], start: i, end });
        i = end;
        if &kind == b"IEND" {
            return Ok(out);
        }
    }
}

fn png_removable(kind: &[u8; 4]) -> bool {
    matches!(kind, b"tEXt" | b"zTXt" | b"iTXt" | b"eXIf" | b"tIME")
}

struct WebpChunk<'a> {
    kind: [u8; 4],
    data: &'a [u8],
    start: usize,
    end: usize,
}

fn webp_chunks(b: &[u8]) -> Result<Vec<WebpChunk<'_>>> {
    if b.len() < 12 || &b[..4] != b"RIFF" || &b[8..12] != b"WEBP" {
        return Err(bad("WebP: bad header"));
    }
    let riff = u32::from_le_bytes(b[4..8].try_into().expect("4 bytes")) as usize;
    let total =
        riff.checked_add(8).filter(|&t| t <= b.len() && t >= 12).ok_or_else(|| bad("WebP: file is truncated"))?;
    let mut out = Vec::new();
    let mut i = 12;
    while i + 8 <= total {
        let kind: [u8; 4] = b[i..i + 4].try_into().expect("4 bytes");
        let len = u32::from_le_bytes(b[i + 4..i + 8].try_into().expect("4 bytes")) as usize;
        let end = i
            .checked_add(8)
            .and_then(|v| v.checked_add(len))
            .map(|v| v + (len & 1))
            .filter(|&e| e <= total + (len & 1))
            .ok_or_else(|| bad("WebP: chunk past end of file"))?;
        let data_end = (i + 8 + len).min(b.len());
        out.push(WebpChunk { kind, data: &b[i + 8..data_end], start: i, end: end.min(b.len()) });
        i = end;
    }
    Ok(out)
}

fn text(b: &[u8]) -> String {
    let s: String = String::from_utf8_lossy(b).chars().take(256).collect();
    s.trim_matches(|c: char| c == '\0' || c.is_whitespace()).to_string()
}

fn dimensions(bytes: &[u8]) -> Result<(u32, u32)> {
    Ok(image::ImageReader::new(std::io::Cursor::new(bytes)).with_guessed_format()?.into_dimensions()?)
}

pub fn inspect(bytes: &[u8]) -> Result<MetaReport> {
    let fmt = kind_of(bytes)?;
    let mut entries = Vec::new();
    let mut has_gps = false;
    let mut removable = 0u64;
    match fmt {
        Fmt::Jpg => {
            let (segs, scan) = jpeg::segments(bytes)?;
            for s in &segs {
                if !is_removable_jpeg_segment(s) {
                    continue;
                }
                removable += (s.end - s.start) as u64;
                match s.marker {
                    0xE1 if s.data.starts_with(b"Exif\0\0") => parse_tiff(&s.data[6..], &mut entries, &mut has_gps),
                    0xE1 if s.data.starts_with(XMP_NS) => {
                        entries.push(entry("XMP", "XMP packet", format!("{} bytes", s.data.len() - XMP_NS.len())))
                    }
                    0xED if s.data.starts_with(b"Photoshop 3.0") => {
                        entries.push(entry("IPTC", "Photoshop / IPTC block", format!("{} bytes", s.data.len())))
                    }
                    0xFE => entries.push(entry("JPEG", "Comment", text(s.data))),
                    m => entries.push(entry("JPEG", &format!("APP{}", m & 0x0F), format!("{} bytes", s.data.len()))),
                }
            }
            if let Some(eoi) = find_eoi(bytes, scan) {
                if eoi < bytes.len() {
                    removable += (bytes.len() - eoi) as u64;
                    entries.push(entry(
                        "JPEG",
                        "Trailing data",
                        format!("{} bytes after the image", bytes.len() - eoi),
                    ));
                }
            }
        }
        Fmt::Png => {
            for c in png_chunks(bytes)? {
                if !png_removable(&c.kind) {
                    continue;
                }
                removable += (c.end - c.start) as u64;
                match &c.kind {
                    b"eXIf" => parse_tiff(c.data, &mut entries, &mut has_gps),
                    b"tEXt" => {
                        let (k, v) = c.data.split_at(c.data.iter().position(|&b| b == 0).unwrap_or(c.data.len()));
                        entries.push(entry("PNG", &text(k), text(v)));
                    }
                    b"zTXt" | b"iTXt" => {
                        let k = &c.data[..c.data.iter().position(|&b| b == 0).unwrap_or(c.data.len())];
                        entries.push(entry(
                            "PNG",
                            &text(k),
                            if &c.kind == b"zTXt" { "(compressed text)" } else { "(international text)" },
                        ));
                    }
                    b"tIME" if c.data.len() == 7 => {
                        let year = u16::from_be_bytes([c.data[0], c.data[1]]);
                        entries.push(entry(
                            "PNG",
                            "Modified",
                            format!(
                                "{year:04}-{:02}-{:02} {:02}:{:02}:{:02}",
                                c.data[2], c.data[3], c.data[4], c.data[5], c.data[6]
                            ),
                        ));
                    }
                    _ => {}
                }
            }
        }
        Fmt::Webp => {
            for c in webp_chunks(bytes)? {
                match &c.kind {
                    b"EXIF" => {
                        removable += (c.end - c.start) as u64;
                        parse_tiff(c.data.strip_prefix(b"Exif\0\0").unwrap_or(c.data), &mut entries, &mut has_gps);
                    }
                    b"XMP " => {
                        removable += (c.end - c.start) as u64;
                        entries.push(entry("XMP", "XMP packet", format!("{} bytes", c.data.len())));
                    }
                    _ => {}
                }
            }
        }
        Fmt::Tiff => parse_tiff(bytes, &mut entries, &mut has_gps),
        _ => unreachable!("kind_of only returns JPEG, PNG, WebP and TIFF"),
    }
    // A file that cannot be decoded has no metadata worth reporting.
    dimensions(bytes)?;
    Ok(MetaReport { entries, has_gps, removable_bytes: removable })
}

/// EXIF orientation 1..=8 (1 when absent or unreadable).
pub fn exif_orientation(bytes: &[u8]) -> u16 {
    let found = match detect_head(None, bytes) {
        Some(SourceKind::Image(Fmt::Jpg)) => jpeg::segments(bytes).ok().and_then(|(segs, _)| {
            segs.iter()
                .find(|s| s.marker == 0xE1 && s.data.starts_with(b"Exif\0\0"))
                .and_then(|s| tiff_orientation(&s.data[6..]))
        }),
        Some(SourceKind::Image(Fmt::Png)) => png_chunks(bytes)
            .ok()
            .and_then(|cs| cs.iter().find(|c| &c.kind == b"eXIf").and_then(|c| tiff_orientation(c.data))),
        Some(SourceKind::Image(Fmt::Webp)) => webp_chunks(bytes).ok().and_then(|cs| {
            cs.iter()
                .find(|c| &c.kind == b"EXIF")
                .and_then(|c| tiff_orientation(c.data.strip_prefix(b"Exif\0\0").unwrap_or(c.data)))
        }),
        _ => None,
    };
    found.unwrap_or(1)
}

/// Returns the file without metadata. The EXIF orientation of a JPEG is kept (as a minimal EXIF block)
/// so the picture does not turn sideways.
pub fn strip(bytes: &[u8]) -> Result<Vec<u8>> {
    match kind_of(bytes)? {
        Fmt::Jpg => strip_jpeg(bytes),
        Fmt::Png => {
            let chunks = png_chunks(bytes)?;
            let mut out = bytes[..8].to_vec();
            for c in chunks.iter().filter(|c| !png_removable(&c.kind)) {
                out.extend_from_slice(&bytes[c.start..c.end]);
            }
            Ok(out)
        }
        Fmt::Webp => strip_webp(bytes),
        Fmt::Tiff => raster::encode(&raster::decode(bytes)?, Fmt::Tiff, &EncodeOptions::default()),
        _ => unreachable!("kind_of only returns JPEG, PNG, WebP and TIFF"),
    }
}

fn strip_jpeg(bytes: &[u8]) -> Result<Vec<u8>> {
    let (segs, scan) = jpeg::segments(bytes)?;
    let orientation = exif_orientation(bytes);
    let mut out = vec![0xFF, 0xD8];
    let mut exif_pending = orientation != 1;
    let put_exif = |out: &mut Vec<u8>| {
        let payload = minimal_exif(orientation);
        out.extend_from_slice(&[0xFF, 0xE1]);
        out.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&payload);
    };
    // EXIF belongs right after SOI, or right after a leading JFIF block.
    if exif_pending && !segs.first().is_some_and(|s| s.marker == 0xE0) {
        put_exif(&mut out);
        exif_pending = false;
    }
    for (i, s) in segs.iter().enumerate() {
        if !is_removable_jpeg_segment(s) {
            out.extend_from_slice(&bytes[s.start..s.end]);
        }
        if i == 0 && exif_pending {
            put_exif(&mut out);
            exif_pending = false;
        }
    }
    // Everything after EOI (embedded previews, vendor trailers) is dropped; if the stream cannot be walked, keep it all.
    let tail_end = find_eoi(bytes, scan).unwrap_or(bytes.len());
    out.extend_from_slice(&bytes[scan..tail_end]);
    Ok(out)
}

fn strip_webp(bytes: &[u8]) -> Result<Vec<u8>> {
    let chunks = webp_chunks(bytes)?;
    if !chunks.iter().any(|c| matches!(&c.kind, b"EXIF" | b"XMP ")) {
        return Ok(bytes.to_vec());
    }
    let mut body = b"WEBP".to_vec();
    for c in &chunks {
        if matches!(&c.kind, b"EXIF" | b"XMP ") {
            continue;
        }
        let mut raw = bytes[c.start..c.end].to_vec();
        if &c.kind == b"VP8X" && raw.len() > 8 {
            raw[8] &= !0x0C; // EXIF and XMP presence flags
        }
        body.extend_from_slice(&raw);
    }
    let mut out = b"RIFF".to_vec();
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(&body);
    Ok(out)
}

/// Writes `name (clean).ext` next to the source and returns its path.
pub fn strip_file(src: &Path) -> Result<PathBuf> {
    let input = io::read_limited(src)?;
    let fmt = kind_of(&input)?;
    let out = strip(&input)?;
    let dir = src.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    io::commit_unique(dir, &format!("{stem} (clean)"), fmt.ext(), &out)
}

#[cfg(test)]
mod tests {
    use super::super::error::Error;
    use super::super::jpeg;
    use super::super::raster::{self, testutil::*};
    use super::*;
    use image::{DynamicImage, GenericImageView};

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    struct Tag {
        id: u16,
        typ: u16,
        count: u32,
        data: Vec<u8>,
    }
    fn ascii(id: u16, s: &str) -> Tag {
        let mut d = s.as_bytes().to_vec();
        d.push(0);
        Tag { id, typ: 2, count: d.len() as u32, data: d }
    }
    fn short(id: u16, v: u16) -> Tag {
        Tag { id, typ: 3, count: 1, data: v.to_le_bytes().to_vec() }
    }
    fn rationals(id: u16, vals: &[(u32, u32)]) -> Tag {
        let data = vals.iter().flat_map(|(n, d)| [n.to_le_bytes(), d.to_le_bytes()].concat()).collect();
        Tag { id, typ: 5, count: vals.len() as u32, data }
    }

    /// Little-endian TIFF with IFD0, an EXIF sub-IFD and a GPS sub-IFD (either may be empty).
    fn build_tiff(ifd0: Vec<Tag>, exif: Vec<Tag>, gps: Vec<Tag>) -> Vec<u8> {
        let mut ifd0 = ifd0;
        let ifd_size = |n: usize| 2 + n * 12 + 4;
        let extra = (!exif.is_empty()) as usize + (!gps.is_empty()) as usize;
        let ifd0_off = 8usize;
        let exif_off = ifd0_off + ifd_size(ifd0.len() + extra);
        let gps_off = exif_off + if exif.is_empty() { 0 } else { ifd_size(exif.len()) };
        let data_off = gps_off + if gps.is_empty() { 0 } else { ifd_size(gps.len()) };
        if !exif.is_empty() {
            ifd0.push(Tag { id: 0x8769, typ: 4, count: 1, data: (exif_off as u32).to_le_bytes().to_vec() });
        }
        if !gps.is_empty() {
            ifd0.push(Tag { id: 0x8825, typ: 4, count: 1, data: (gps_off as u32).to_le_bytes().to_vec() });
        }
        ifd0.sort_by_key(|t| t.id);
        let mut blob: Vec<u8> = Vec::new();
        let mut out = b"II*\0".to_vec();
        out.extend_from_slice(&8u32.to_le_bytes());
        let write_ifd = |tags: &[Tag], out: &mut Vec<u8>, blob: &mut Vec<u8>| {
            out.extend_from_slice(&(tags.len() as u16).to_le_bytes());
            for t in tags {
                out.extend_from_slice(&t.id.to_le_bytes());
                out.extend_from_slice(&t.typ.to_le_bytes());
                out.extend_from_slice(&t.count.to_le_bytes());
                if t.data.len() <= 4 {
                    let mut v = t.data.clone();
                    v.resize(4, 0);
                    out.extend_from_slice(&v);
                } else {
                    out.extend_from_slice(&((data_off + blob.len()) as u32).to_le_bytes());
                    blob.extend_from_slice(&t.data);
                    if blob.len() % 2 == 1 {
                        blob.push(0);
                    }
                }
            }
            out.extend_from_slice(&0u32.to_le_bytes());
        };
        write_ifd(&ifd0, &mut out, &mut blob);
        if !exif.is_empty() {
            write_ifd(&exif, &mut out, &mut blob);
        }
        if !gps.is_empty() {
            write_ifd(&gps, &mut out, &mut blob);
        }
        out.extend_from_slice(&blob);
        out
    }

    fn rich_tiff(orientation: u16) -> Vec<u8> {
        build_tiff(
            vec![
                ascii(0x010F, "TestCam"),
                ascii(0x0110, "X-100"),
                short(0x0112, orientation),
                ascii(0x0131, "radial-tests 1.0"),
            ],
            vec![ascii(0x9003, "2024:05:01 10:20:30"), short(0x8827, 200)],
            vec![
                ascii(1, "N"),
                rationals(2, &[(40, 1), (26, 1), (4630, 100)]),
                ascii(3, "W"),
                rationals(4, &[(3, 1), (42, 1), (30, 1)]),
            ],
        )
    }

    fn value<'a>(r: &'a MetaReport, key: &str) -> Option<&'a str> {
        r.entries.iter().find(|e| e.key == key).map(|e| e.value.as_str())
    }

    fn photo() -> image::RgbImage {
        gradient(48, 32)
    }

    #[test]
    fn inspects_exif_and_location_of_a_jpeg() {
        let r = inspect(&jpeg_with_exif(&photo(), &rich_tiff(1))).unwrap();
        assert_eq!(value(&r, "Make"), Some("TestCam"));
        assert_eq!(value(&r, "Model"), Some("X-100"));
        assert_eq!(value(&r, "DateTimeOriginal"), Some("2024:05:01 10:20:30"));
        assert_eq!(value(&r, "ISO"), Some("200"));
        assert_eq!(value(&r, "GPSLatitude"), Some("40° 26′ 46.30″"));
        assert_eq!(value(&r, "GPSLatitudeRef"), Some("N"));
        assert!(r.has_gps);
        assert!(r.entries.iter().any(|e| e.group == "GPS") && r.entries.iter().any(|e| e.group == "EXIF"));
        assert!(r.removable_bytes > 100);
    }

    #[test]
    fn a_clean_jpeg_reports_nothing() {
        let r = inspect(&jpeg(&photo())).unwrap();
        assert!(r.entries.is_empty() && !r.has_gps && r.removable_bytes == 0);
    }

    #[test]
    fn stripping_a_jpeg_keeps_the_scan_data_and_removes_every_tag() {
        let src = jpeg_with_exif(&photo(), &rich_tiff(1));
        let out = strip(&src).unwrap();
        assert!(!has(&out, b"TestCam") && !has(&out, b"Exif"));
        let r = inspect(&out).unwrap();
        assert!(r.entries.is_empty() && !r.has_gps);
        // Same compressed pixels, byte for byte.
        let (_, scan_src) = jpeg::segments(&src).unwrap();
        let (_, scan_out) = jpeg::segments(&out).unwrap();
        assert_eq!(src[scan_src..], out[scan_out..]);
        assert_eq!(raster::decode(&out).unwrap().to_rgb8(), raster::decode(&src).unwrap().to_rgb8());
        assert!(out.len() < src.len());
    }

    #[test]
    fn stripping_keeps_the_orientation_so_photos_do_not_turn_sideways() {
        let src = jpeg_with_exif(&photo(), &rich_tiff(6));
        assert_eq!(exif_orientation(&src), 6);
        let out = strip(&src).unwrap();
        assert_eq!(exif_orientation(&out), 6);
        assert!(!has(&out, b"TestCam"));
        let r = inspect(&out).unwrap();
        assert_eq!(r.entries.len(), 1, "{:?}", r.entries);
        assert_eq!(value(&r, "Orientation"), Some("6"));
        assert_eq!(raster::decode(&out).unwrap().dimensions(), (32, 48));
        assert_eq!(exif_orientation(&jpeg(&photo())), 1);
    }

    #[test]
    fn xmp_comments_and_trailing_data_go_but_icc_and_jfif_stay() {
        let base = jpeg(&photo());
        let (segs, scan) = jpeg::segments(&base).unwrap();
        let seg = |marker: u8, payload: &[u8]| {
            let mut v = vec![0xFF, marker];
            v.extend_from_slice(&((payload.len() + 2) as u16).to_be_bytes());
            v.extend_from_slice(payload);
            v
        };
        let mut src = vec![0xFF, 0xD8];
        src.extend(seg(0xE0, b"JFIF\0\x01\x01\0\0\x01\0\x01\0\0"));
        src.extend(seg(0xE1, b"http://ns.adobe.com/xap/1.0/\0<x:xmpmeta>PRIVATE-XMP</x:xmpmeta>"));
        src.extend(seg(0xE2, b"ICC_PROFILE\0\x01\x01profile-bytes"));
        src.extend(seg(0xED, b"Photoshop 3.0\08BIM-IPTC-DATA"));
        src.extend(seg(0xFE, b"camera comment"));
        let _ = segs;
        src.extend_from_slice(&base[2..]);
        assert!(scan > 0);
        src.extend_from_slice(b"SECRET-TRAILER");
        let before = inspect(&src).unwrap();
        assert!(before.entries.iter().any(|e| e.group == "XMP") && before.entries.iter().any(|e| e.group == "IPTC"));
        assert!(before.entries.iter().any(|e| e.key == "Comment" && e.value == "camera comment"));
        let out = strip(&src).unwrap();
        for gone in [&b"PRIVATE-XMP"[..], b"IPTC", b"camera comment", b"SECRET-TRAILER"] {
            assert!(!has(&out, gone), "{}", String::from_utf8_lossy(gone));
        }
        assert!(has(&out, b"JFIF") && has(&out, b"ICC_PROFILE") && has(&out, b"profile-bytes"));
        assert_eq!(raster::decode(&out).unwrap().to_rgb8(), raster::decode(&base).unwrap().to_rgb8());
        assert!(out.ends_with(&[0xFF, 0xD9]));
    }

    fn chunk(kind: &[u8; 4], data: &[u8]) -> Vec<u8> {
        let mut v = (data.len() as u32).to_be_bytes().to_vec();
        let mut body = kind.to_vec();
        body.extend_from_slice(data);
        v.extend_from_slice(&body);
        v.extend_from_slice(&crc32fast::hash(&body).to_be_bytes());
        v
    }

    /// `base` PNG with extra chunks inserted before IDAT.
    fn png_with(extra: &[Vec<u8>]) -> Vec<u8> {
        let base = png(&DynamicImage::ImageRgb8(photo()));
        let idat = base.windows(4).position(|w| w == b"IDAT").unwrap() - 4;
        let mut out = base[..idat].to_vec();
        for c in extra {
            out.extend_from_slice(c);
        }
        out.extend_from_slice(&base[idat..]);
        out
    }

    #[test]
    fn png_text_exif_and_time_are_listed_and_removed() {
        let src = png_with(&[
            chunk(b"tEXt", b"Author\0Jane Doe"),
            chunk(b"eXIf", &rich_tiff(1)),
            chunk(b"tIME", &[0x07, 0xE8, 5, 1, 10, 20, 30]),
            chunk(b"pHYs", &[0, 0, 0x0B, 0x13, 0, 0, 0x0B, 0x13, 1]),
        ]);
        let r = inspect(&src).unwrap();
        assert_eq!(value(&r, "Author"), Some("Jane Doe"));
        assert_eq!(value(&r, "Make"), Some("TestCam"));
        assert_eq!(value(&r, "Modified"), Some("2024-05-01 10:20:30"));
        assert!(r.has_gps && r.removable_bytes > 0);
        let out = strip(&src).unwrap();
        assert!(!has(&out, b"Jane Doe") && !has(&out, b"eXIf") && !has(&out, b"tIME") && !has(&out, b"TestCam"));
        assert!(has(&out, b"pHYs"), "harmless rendering hints stay");
        assert!(inspect(&out).unwrap().entries.is_empty());
        assert_eq!(raster::decode(&out).unwrap().to_rgb8(), photo());
        let ihdr_src = &src[8..8 + 25];
        assert_eq!(&out[8..8 + 25], ihdr_src);
    }

    /// Lossless WebP image wrapped in an extended (VP8X) container with EXIF and XMP chunks.
    fn webp_with_metadata() -> Vec<u8> {
        let plain =
            raster::encode(&DynamicImage::ImageRgb8(photo()), Fmt::Webp, &raster::EncodeOptions::default()).unwrap();
        let image_chunks = &plain[12..];
        let mut payload = b"WEBP".to_vec();
        let mut vp8x = vec![0x08 | 0x04, 0, 0, 0];
        vp8x.extend_from_slice(&47u32.to_le_bytes()[..3]);
        vp8x.extend_from_slice(&31u32.to_le_bytes()[..3]);
        let riff_chunk = |kind: &[u8; 4], data: &[u8]| {
            let mut v = kind.to_vec();
            v.extend_from_slice(&(data.len() as u32).to_le_bytes());
            v.extend_from_slice(data);
            if data.len() % 2 == 1 {
                v.push(0);
            }
            v
        };
        payload.extend(riff_chunk(b"VP8X", &vp8x));
        payload.extend_from_slice(image_chunks);
        payload.extend(riff_chunk(b"EXIF", &rich_tiff(1)));
        payload.extend(riff_chunk(b"XMP ", b"<x:xmpmeta>WEBP-XMP</x:xmpmeta>"));
        let mut out = b"RIFF".to_vec();
        out.extend_from_slice(&(payload.len() as u32).to_le_bytes());
        out.extend(payload);
        out
    }

    #[test]
    fn webp_exif_and_xmp_are_removed_and_the_container_stays_valid() {
        let src = webp_with_metadata();
        assert_eq!(raster::decode(&src).unwrap().to_rgb8(), photo(), "fixture must decode");
        let r = inspect(&src).unwrap();
        assert_eq!(value(&r, "Make"), Some("TestCam"));
        assert!(r.entries.iter().any(|e| e.group == "XMP"));
        let out = strip(&src).unwrap();
        assert!(!has(&out, b"EXIF") && !has(&out, b"XMP ") && !has(&out, b"TestCam") && !has(&out, b"WEBP-XMP"));
        let flags_at = out.windows(4).position(|w| w == b"VP8X").unwrap() + 8;
        assert_eq!(out[flags_at] & 0x0C, 0, "EXIF and XMP flags cleared");
        assert_eq!(u32::from_le_bytes(out[4..8].try_into().unwrap()) as usize, out.len() - 8);
        assert_eq!(raster::decode(&out).unwrap().to_rgb8(), photo());
        assert!(inspect(&out).unwrap().entries.is_empty());
    }

    #[test]
    fn a_plain_webp_is_returned_unchanged() {
        let plain =
            raster::encode(&DynamicImage::ImageRgb8(photo()), Fmt::Webp, &raster::EncodeOptions::default()).unwrap();
        assert_eq!(strip(&plain).unwrap(), plain);
    }

    #[test]
    fn tiff_files_are_inspected_and_stripped_by_reencoding() {
        let src =
            raster::encode(&DynamicImage::ImageRgb8(photo()), Fmt::Tiff, &raster::EncodeOptions::default()).unwrap();
        let r = inspect(&src).unwrap();
        assert_eq!(r.removable_bytes, 0);
        let out = strip(&src).unwrap();
        assert_eq!(raster::decode(&out).unwrap().to_rgb8(), photo());
    }

    #[test]
    fn bare_tiff_structures_parse_like_exif() {
        let tiff = rich_tiff(8);
        let mut entries = Vec::new();
        let mut gps = false;
        super::parse_tiff(&tiff, &mut entries, &mut gps);
        assert!(gps && entries.iter().any(|e| e.key == "Software" && e.value == "radial-tests 1.0"));
        assert!(entries.iter().any(|e| e.key == "Orientation" && e.value == "8"));
    }

    #[test]
    fn hostile_exif_cannot_crash_or_hang() {
        // IFD that points back to itself, a count that overflows, and offsets beyond the buffer.
        let mut looped = b"II*\0\x08\0\0\0".to_vec();
        looped.extend_from_slice(&2u16.to_le_bytes());
        for (tag, count, off) in [(0x010Fu16, 0xFFFF_FFF0u32, 0x0FFF_FFFFu32), (0x8769, 1, 8)] {
            looped.extend_from_slice(&tag.to_le_bytes());
            looped.extend_from_slice(&(if tag == 0x8769 { 4u16 } else { 2u16 }).to_le_bytes());
            looped.extend_from_slice(&count.to_le_bytes());
            looped.extend_from_slice(&off.to_le_bytes());
        }
        looped.extend_from_slice(&8u32.to_le_bytes()); // next IFD = itself
        let j = jpeg_with_exif(&photo(), &looped);
        let _ = inspect(&j).unwrap();
        assert_eq!(exif_orientation(&j), 1);
        assert!(strip(&j).is_ok());
        let mut tiny = Vec::new();
        super::parse_tiff(&[0x49, 0x49], &mut tiny, &mut false);
        super::parse_tiff(b"", &mut tiny, &mut false);
        assert!(tiny.is_empty());
    }

    #[test]
    fn damaged_containers_are_typed_errors() {
        let j = jpeg_with_exif(&photo(), &rich_tiff(1));
        assert!(matches!(strip(&j[..30]), Err(Error::Corrupt(_))));
        assert!(inspect(&j[..30]).is_err());
        let p = png_with(&[chunk(b"tEXt", b"A\0B")]);
        assert!(matches!(strip(&p[..p.len() / 2]), Err(Error::Corrupt(_))));
        let mut p2 = p.clone();
        p2[8..12].copy_from_slice(&u32::MAX.to_be_bytes());
        assert!(matches!(strip(&p2), Err(Error::Corrupt(_))));
        let w = webp_with_metadata();
        assert!(strip(&w[..w.len() - 9]).is_err());
        assert!(strip(b"").is_err());
        assert!(strip(b"nothing").is_err());
    }

    #[test]
    fn formats_without_metadata_support_say_so() {
        let img = DynamicImage::ImageRgb8(photo());
        for f in [Fmt::Bmp, Fmt::Gif] {
            let b = raster::encode(&img, f, &raster::EncodeOptions::default()).unwrap();
            assert!(matches!(strip(&b), Err(Error::Unsupported(_))), "{f:?}");
            assert!(matches!(inspect(&b), Err(Error::Unsupported(_))), "{f:?}");
        }
    }

    #[test]
    fn strip_file_writes_a_clean_copy_without_touching_the_original() {
        let dir = std::env::temp_dir().join(format!("radial-meta-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("pic.jpg");
        let original = jpeg_with_exif(&photo(), &rich_tiff(1));
        std::fs::write(&src, &original).unwrap();
        let out = strip_file(&src).unwrap();
        assert_eq!(out, dir.join("pic (clean).jpg"));
        assert_eq!(std::fs::read(&src).unwrap(), original);
        assert!(!has(&std::fs::read(&out).unwrap(), b"TestCam"));
        assert_eq!(strip_file(&src).unwrap(), dir.join("pic (clean) (1).jpg"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
