//! Just enough JPEG container parsing to embed, inspect and strip files without re-encoding.

use super::error::{Error, Result};

pub struct Segment<'a> {
    pub marker: u8,
    /// Payload after the 2-byte length field.
    pub data: &'a [u8],
    /// Byte range of the whole segment (marker, length and payload) in the file.
    pub start: usize,
    pub end: usize,
}

/// Header segments up to and including SOS. Entropy-coded data after SOS is not walked.
/// Returns the segments and the offset where the scan data begins.
pub fn segments(bytes: &[u8]) -> Result<(Vec<Segment<'_>>, usize)> {
    let bad = || Error::Corrupt("JPEG: malformed segment".into());
    if !bytes.starts_with(&[0xFF, 0xD8]) {
        return Err(Error::Corrupt("JPEG: missing SOI".into()));
    }
    let mut out = Vec::new();
    let mut i = 2;
    loop {
        // Skip fill bytes: any run of 0xFF before the marker byte.
        if bytes.get(i) != Some(&0xFF) {
            return Err(bad());
        }
        while bytes.get(i + 1) == Some(&0xFF) {
            i += 1;
        }
        let marker = *bytes.get(i + 1).ok_or_else(bad)?;
        match marker {
            0xD9 => return Ok((out, i + 2)),
            0x01 | 0xD0..=0xD7 => {
                i += 2;
                continue;
            }
            _ => {}
        }
        let len = u16::from_be_bytes([*bytes.get(i + 2).ok_or_else(bad)?, *bytes.get(i + 3).ok_or_else(bad)?]) as usize;
        if len < 2 {
            return Err(bad());
        }
        let end = i + 2 + len;
        let data = bytes.get(i + 4..end).ok_or_else(bad)?;
        out.push(Segment { marker, data, start: i, end });
        i = end;
        if marker == 0xDA {
            return Ok((out, i));
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct FrameInfo {
    pub width: u32,
    pub height: u32,
    pub components: u8,
    pub precision: u8,
    /// Adobe APP14 colour transform flag, when present (0 means CMYK/RGB without YCC).
    pub adobe_transform: Option<u8>,
}

pub fn frame_info(bytes: &[u8]) -> Option<FrameInfo> {
    let (segs, _) = segments(bytes).ok()?;
    let sof = segs.iter().find(|s| matches!(s.marker, 0xC0..=0xCF) && !matches!(s.marker, 0xC4 | 0xC8 | 0xCC))?;
    let d = sof.data;
    if d.len() < 6 {
        return None;
    }
    let adobe =
        segs.iter().find(|s| s.marker == 0xEE && s.data.starts_with(b"Adobe")).and_then(|s| s.data.get(11).copied());
    Some(FrameInfo {
        precision: d[0],
        height: u16::from_be_bytes([d[1], d[2]]) as u32,
        width: u16::from_be_bytes([d[3], d[4]]) as u32,
        components: d[5],
        adobe_transform: adobe,
    })
}

#[cfg(test)]
mod tests {
    use super::super::raster::testutil::*;
    use super::*;

    #[test]
    fn reads_frame_info_of_a_real_jpeg() {
        let j = jpeg(&gradient(40, 24));
        let f = frame_info(&j).unwrap();
        assert_eq!((f.width, f.height, f.components, f.precision), (40, 24, 3, 8));
        assert_eq!(f.adobe_transform, None);
    }

    #[test]
    fn lists_exif_segment_and_finds_scan_start() {
        let j = jpeg_with_orientation(&gradient(8, 8), 6);
        let (segs, scan) = segments(&j).unwrap();
        assert!(segs.iter().any(|s| s.marker == 0xE1 && s.data.starts_with(b"Exif\0\0")));
        assert_eq!(segs.last().unwrap().marker, 0xDA);
        assert_eq!(scan, segs.last().unwrap().end);
        assert!(scan < j.len());
    }

    #[test]
    fn malformed_input_is_an_error_not_a_panic() {
        assert!(segments(b"").is_err());
        assert!(segments(b"\xFF\xD8").is_err());
        assert!(segments(b"\xFF\xD8\xFF\xE1\x00\x01").is_err());
        assert!(segments(b"\xFF\xD8\xFF\xE1\xFF\xFFabc").is_err());
        assert_eq!(frame_info(b"not a jpeg"), None);
    }
}
