//! COMPRESS: smaller JPEG, PNG and WebP files, with before/after sizes.

use super::error::{Error, Result};
use super::io;
use super::kind::{detect_head, SourceKind};
use super::raster::{self, EncodeOptions};
use crate::convert::Fmt;
use image::DynamicImage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
pub struct CompressOptions {
    /// 1..=100. JPEG only: PNG and WebP are always recompressed losslessly.
    pub jpeg_quality: u8,
}

impl Default for CompressOptions {
    fn default() -> Self {
        CompressOptions { jpeg_quality: 80 }
    }
}

#[derive(Debug)]
pub struct Compressed {
    pub fmt: Fmt,
    pub bytes: Vec<u8>,
    pub before: u64,
    pub after: u64,
}

/// Recompresses an image file's bytes in its own format. The result may be larger than the input; callers
/// that write files should use `compress_file`, which refuses to make a file bigger.
pub fn compress_bytes(input: &[u8], opts: &CompressOptions) -> Result<Compressed> {
    let fmt = match detect_head(None, input) {
        Some(SourceKind::Image(f @ (Fmt::Jpg | Fmt::Png | Fmt::Webp))) => f,
        Some(SourceKind::Image(f)) => {
            return Err(Error::Unsupported(format!("COMPRESS handles JPEG, PNG and WebP, not {}", f.label())))
        }
        _ => return Err(Error::Unsupported("COMPRESS needs an image file".into())),
    };
    let img = raster::decode(input)?;
    let bytes = match fmt {
        Fmt::Png => optimize_png(&img)?,
        other => raster::encode(&img, other, &EncodeOptions { jpeg_quality: opts.jpeg_quality })?,
    };
    Ok(Compressed { fmt, before: input.len() as u64, after: bytes.len() as u64, bytes })
}

/// Lossless PNG: indexed colour when the image has at most 256 colours, otherwise the narrowest
/// colour type that holds the pixels exactly, always with maximum deflate and adaptive filtering.
type PngPlan = (png::ColorType, png::BitDepth, Vec<u8>, Option<Vec<u8>>, Option<Vec<u8>>);

fn optimize_png(img: &DynamicImage) -> Result<Vec<u8>> {
    use png::{BitDepth, ColorType};

    let (w, h) = (img.width(), img.height());
    let (color, depth, data, palette, trns): PngPlan = match img {
        DynamicImage::ImageRgb16(i) => (ColorType::Rgb, BitDepth::Sixteen, be16(i.as_raw()), None, None),
        DynamicImage::ImageRgba16(i) => (ColorType::Rgba, BitDepth::Sixteen, be16(i.as_raw()), None, None),
        DynamicImage::ImageLuma16(i) => (ColorType::Grayscale, BitDepth::Sixteen, be16(i.as_raw()), None, None),
        DynamicImage::ImageLumaA16(i) => (ColorType::GrayscaleAlpha, BitDepth::Sixteen, be16(i.as_raw()), None, None),
        _ => {
            let rgba = img.to_rgba8();
            let raw = rgba.as_raw();
            match index_colours(raw) {
                Some((pal, indices)) => {
                    let n = pal.len();
                    let bits: u8 = match n {
                        0..=2 => 1,
                        3..=4 => 2,
                        5..=16 => 4,
                        _ => 8,
                    };
                    let rgb: Vec<u8> = pal.iter().flat_map(|c| [c[0], c[1], c[2]]).collect();
                    let last_alpha = pal.iter().rposition(|c| c[3] != 255);
                    let alpha = last_alpha.map(|l| pal[..=l].iter().map(|c| c[3]).collect::<Vec<u8>>());
                    let depth = match bits {
                        1 => BitDepth::One,
                        2 => BitDepth::Two,
                        4 => BitDepth::Four,
                        _ => BitDepth::Eight,
                    };
                    (ColorType::Indexed, depth, pack_rows(&indices, w as usize, bits), Some(rgb), alpha)
                }
                None => {
                    let opaque = raw.chunks_exact(4).all(|p| p[3] == 255);
                    let gray = raw.chunks_exact(4).all(|p| p[0] == p[1] && p[1] == p[2]);
                    match (gray, opaque) {
                        (true, true) => (
                            ColorType::Grayscale,
                            BitDepth::Eight,
                            raw.chunks_exact(4).map(|p| p[0]).collect(),
                            None,
                            None,
                        ),
                        (true, false) => (
                            ColorType::GrayscaleAlpha,
                            BitDepth::Eight,
                            raw.chunks_exact(4).flat_map(|p| [p[0], p[3]]).collect(),
                            None,
                            None,
                        ),
                        (false, true) => (
                            ColorType::Rgb,
                            BitDepth::Eight,
                            raw.chunks_exact(4).flat_map(|p| [p[0], p[1], p[2]]).collect(),
                            None,
                            None,
                        ),
                        (false, false) => (ColorType::Rgba, BitDepth::Eight, raw.clone(), None, None),
                    }
                }
            }
        }
    };
    let mut out = Vec::with_capacity(data.len() / 4 + 256);
    {
        let mut enc = png::Encoder::new(&mut out, w, h);
        enc.set_color(color);
        enc.set_depth(depth);
        // Maximum effort is worth it for icons and screenshots; on multi-megapixel photos it costs seconds for ~1%.
        enc.set_compression(if (w as u64) * (h as u64) > 2_000_000 {
            png::Compression::Balanced
        } else {
            png::Compression::High
        });
        if let Some(p) = palette {
            enc.set_palette(p);
        }
        if let Some(t) = trns {
            enc.set_trns(t);
        }
        let mut writer = enc.write_header().map_err(|e| Error::Corrupt(format!("PNG encode: {e}")))?;
        writer.write_image_data(&data).map_err(|e| Error::Corrupt(format!("PNG encode: {e}")))?;
        writer.finish().map_err(|e| Error::Corrupt(format!("PNG encode: {e}")))?;
    }
    Ok(out)
}

fn be16(samples: &[u16]) -> Vec<u8> {
    samples.iter().flat_map(|s| s.to_be_bytes()).collect()
}

/// Palette (in order of first appearance) and per-pixel indices, or `None` above 256 colours.
fn index_colours(rgba: &[u8]) -> Option<(Vec<[u8; 4]>, Vec<u8>)> {
    let mut seen: HashMap<u32, u8> = HashMap::new();
    let mut palette: Vec<[u8; 4]> = Vec::new();
    let mut indices = Vec::with_capacity(rgba.len() / 4);
    for p in rgba.chunks_exact(4) {
        let key = u32::from_le_bytes([p[0], p[1], p[2], p[3]]);
        let idx = match seen.get(&key) {
            Some(&i) => i,
            None => {
                if palette.len() == 256 {
                    return None;
                }
                let i = palette.len() as u8;
                seen.insert(key, i);
                palette.push([p[0], p[1], p[2], p[3]]);
                i
            }
        };
        indices.push(idx);
    }
    Some((palette, indices))
}

/// Packs 8-bit indices into `bits`-per-pixel rows, each row padded to a byte boundary.
fn pack_rows(indices: &[u8], width: usize, bits: u8) -> Vec<u8> {
    if bits == 8 || width == 0 {
        return indices.to_vec();
    }
    let per_byte = 8 / bits as usize;
    let row_bytes = width.div_ceil(per_byte);
    let mut out = Vec::with_capacity(row_bytes * (indices.len() / width));
    for row in indices.chunks(width) {
        for chunk in row.chunks(per_byte) {
            let mut byte = 0u8;
            for (i, &v) in chunk.iter().enumerate() {
                byte |= v << (8 - bits as usize * (i + 1));
            }
            out.push(byte);
        }
    }
    out
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    Written {
        path: PathBuf,
        before: u64,
        after: u64,
    },
    /// Recompression could not beat the original, so no file was written.
    AlreadyOptimal {
        before: u64,
        best: u64,
    },
}

/// Writes `name (compressed).ext` next to the source when that is smaller than the source.
pub fn compress_file(src: &Path, opts: &CompressOptions) -> Result<Outcome> {
    let input = io::read_limited(src)?;
    let c = compress_bytes(&input, opts)?;
    if c.after >= c.before {
        return Ok(Outcome::AlreadyOptimal { before: c.before, best: c.after });
    }
    let dir = src.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let path = io::commit_unique(dir, &format!("{stem} (compressed)"), c.fmt.ext(), &c.bytes)?;
    Ok(Outcome::Written { path, before: c.before, after: c.after })
}

#[cfg(test)]
mod tests {
    use super::super::error::Error;
    use super::super::raster::{self, testutil::*, EncodeOptions};
    use super::*;
    use image::{DynamicImage, Rgb, RgbImage, Rgba, RgbaImage};

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    fn four_colour_blocks() -> DynamicImage {
        let colours = [[255, 0, 0], [0, 255, 0], [0, 0, 255], [250, 250, 20]];
        DynamicImage::ImageRgb8(RgbImage::from_fn(96, 96, |x, y| Rgb(colours[((x / 24) + (y / 48) * 2) as usize % 4])))
    }

    fn noise(w: u32, h: u32) -> RgbaImage {
        let mut s = 12345u32;
        RgbaImage::from_fn(w, h, |_, _| {
            s = s.wrapping_mul(1664525).wrapping_add(1013904223);
            Rgba([(s >> 24) as u8, (s >> 16) as u8, (s >> 8) as u8, 40 + (s as u8 % 200)])
        })
    }

    #[test]
    fn few_colours_become_a_palette_png_and_stay_pixel_exact() {
        let img = four_colour_blocks();
        let original = png(&img);
        let c = compress_bytes(&original, &CompressOptions::default()).unwrap();
        assert_eq!(c.fmt, Fmt::Png);
        assert!(has(&c.bytes, b"PLTE"), "indexed colour expected");
        assert!(c.after < c.before / 2, "{} vs {}", c.after, c.before);
        assert_eq!((c.before, c.after), (original.len() as u64, c.bytes.len() as u64));
        assert_eq!(raster::decode(&c.bytes).unwrap().to_rgba8(), img.to_rgba8());
    }

    #[test]
    fn palette_png_keeps_transparency() {
        let img =
            RgbaImage::from_fn(16, 16, |x, _| if x < 8 { Rgba([10, 20, 30, 0]) } else { Rgba([200, 100, 50, 128]) });
        let c = compress_bytes(&png(&DynamicImage::ImageRgba8(img.clone())), &CompressOptions::default()).unwrap();
        assert_eq!(raster::decode(&c.bytes).unwrap().to_rgba8(), img);
    }

    #[test]
    fn photographic_and_alpha_images_stay_exact() {
        let img = noise(40, 40);
        let src = png(&DynamicImage::ImageRgba8(img.clone()));
        let c = compress_bytes(&src, &CompressOptions::default()).unwrap();
        assert_eq!(raster::decode(&c.bytes).unwrap().to_rgba8(), img);
        assert!(c.after <= c.before + 64);
        let opaque = DynamicImage::ImageRgb8(gradient(64, 48));
        let c = compress_bytes(&png(&opaque), &CompressOptions::default()).unwrap();
        assert_eq!(raster::decode(&c.bytes).unwrap().to_rgb8(), opaque.to_rgb8());
        assert!(!has(&c.bytes, b"PLTE"));
    }

    #[test]
    fn sixteen_bit_pngs_keep_their_depth() {
        let img = DynamicImage::ImageRgb16(image::ImageBuffer::from_fn(8, 8, |x, y| {
            image::Rgb([x as u16 * 8000, y as u16 * 8000, 777])
        }));
        let c = compress_bytes(&png(&img), &CompressOptions::default()).unwrap();
        let back = raster::decode(&c.bytes).unwrap();
        assert!(matches!(back, DynamicImage::ImageRgb16(_)), "{:?}", back.color());
        assert_eq!(back.to_rgb16(), img.to_rgb16());
    }

    #[test]
    fn jpeg_quality_trades_size_for_fidelity() {
        let img = DynamicImage::ImageRgb8(gradient(160, 120));
        let src = raster::encode(&img, Fmt::Jpg, &EncodeOptions { jpeg_quality: 98 }).unwrap();
        let c = compress_bytes(&src, &CompressOptions { jpeg_quality: 50 }).unwrap();
        assert_eq!(c.fmt, Fmt::Jpg);
        assert!(c.after < c.before, "{} !< {}", c.after, c.before);
        assert_eq!(raster::decode(&c.bytes).unwrap().to_rgb8().dimensions(), (160, 120));
    }

    #[test]
    fn webp_is_recompressed_losslessly() {
        let img = four_colour_blocks();
        let src = raster::encode(&img, Fmt::Webp, &EncodeOptions::default()).unwrap();
        let c = compress_bytes(&src, &CompressOptions::default()).unwrap();
        assert_eq!(c.fmt, Fmt::Webp);
        assert_eq!(raster::decode(&c.bytes).unwrap().to_rgba8(), img.to_rgba8());
    }

    #[test]
    fn formats_without_a_compressor_and_bad_input_are_typed_errors() {
        let img = four_colour_blocks();
        for f in [Fmt::Bmp, Fmt::Gif, Fmt::Tiff] {
            let src = raster::encode(&img, f, &EncodeOptions::default()).unwrap();
            assert!(matches!(compress_bytes(&src, &CompressOptions::default()), Err(Error::Unsupported(_))), "{f:?}");
        }
        assert!(compress_bytes(b"", &CompressOptions::default()).is_err());
        assert!(compress_bytes(b"not an image", &CompressOptions::default()).is_err());
        let good = png(&img);
        assert!(compress_bytes(&good[..good.len() / 2], &CompressOptions::default()).is_err());
    }

    #[test]
    fn compress_file_writes_next_to_the_source_without_overwriting() {
        let dir = std::env::temp_dir().join(format!("radial-compress-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("art.png");
        std::fs::write(&src, png(&four_colour_blocks())).unwrap();
        let first = compress_file(&src, &CompressOptions::default()).unwrap();
        let Outcome::Written { path, before, after } = first else { panic!("expected a file") };
        assert_eq!(path, dir.join("art (compressed).png"));
        assert!(after < before);
        assert_eq!(std::fs::metadata(&path).unwrap().len(), after);
        let Outcome::Written { path: second, .. } = compress_file(&src, &CompressOptions::default()).unwrap() else {
            panic!()
        };
        assert_eq!(second, dir.join("art (compressed) (1).png"));
        assert!(std::fs::metadata(&src).unwrap().len() == before, "the original is untouched");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_file_that_cannot_shrink_is_reported_and_nothing_is_written() {
        let dir = std::env::temp_dir().join(format!("radial-compress2-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let src = dir.join("tiny.png");
        let once = compress_bytes(&png(&four_colour_blocks()), &CompressOptions::default()).unwrap();
        std::fs::write(&src, &once.bytes).unwrap();
        let out = compress_file(&src, &CompressOptions::default()).unwrap();
        assert!(matches!(out, Outcome::AlreadyOptimal { .. }), "{out:?}");
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
