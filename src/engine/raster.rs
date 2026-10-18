//! Safe image decode and encode: size caps, EXIF orientation, alpha flattening, OS codecs.

use super::error::{Error, Result};
use super::kind::{detect_head, SourceKind};
use super::os;
use crate::convert::Fmt;
use image::DynamicImage;

/// Most pixels the engine will decode (100 MP is roughly 400 MB as RGBA8).
pub const MAX_PIXELS: u64 = 100_000_000;
/// Longest side the engine will decode.
pub const MAX_SIDE: u32 = 30_000;

#[derive(Clone, Copy, Debug)]
pub struct EncodeOptions {
    /// 1..=100, used by JPEG and AVIF (the pure-Rust WebP encoder is lossless).
    pub jpeg_quality: u8,
}

impl Default for EncodeOptions {
    fn default() -> Self {
        EncodeOptions { jpeg_quality: 90 }
    }
}

/// Decodes `bytes` (format sniffed from content), applies the EXIF orientation and enforces the caps.
pub fn decode(bytes: &[u8]) -> Result<DynamicImage> {
    use image::{ImageDecoder, Limits};
    use std::io::Cursor;

    if let Some(SourceKind::Image(f @ (Fmt::Heic | Fmt::Avif))) = detect_head(None, bytes) {
        return os::decode_with_wic(bytes, f.label()).map(DynamicImage::ImageRgba8);
    }
    let reader = image::ImageReader::new(Cursor::new(bytes)).with_guessed_format()?;
    let mut decoder = reader.into_decoder()?;
    let (w, h) = decoder.dimensions();
    if w > MAX_SIDE || h > MAX_SIDE || (w as u64) * (h as u64) > MAX_PIXELS {
        return Err(Error::TooLarge { width: w, height: h });
    }
    let mut limits = Limits::default();
    limits.max_image_width = Some(MAX_SIDE);
    limits.max_image_height = Some(MAX_SIDE);
    limits.max_alloc = Some(MAX_PIXELS * 4 * 2);
    decoder.set_limits(limits)?;
    let orientation = decoder.orientation().unwrap_or(image::metadata::Orientation::NoTransforms);
    let mut img = DynamicImage::from_decoder(decoder)?;
    img.apply_orientation(orientation);
    Ok(img)
}

pub fn encode(img: &DynamicImage, fmt: Fmt, opts: &EncodeOptions) -> Result<Vec<u8>> {
    use image::ImageFormat;
    use std::io::Cursor;

    let mut out = Cursor::new(Vec::new());
    match fmt {
        Fmt::Jpg => {
            let rgb = flatten_on_white(img).to_rgb8();
            let q = opts.jpeg_quality.clamp(1, 100);
            image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, q).encode(
                rgb.as_raw(),
                rgb.width(),
                rgb.height(),
                image::ExtendedColorType::Rgb8,
            )?;
        }
        Fmt::Png => img.write_to(&mut out, ImageFormat::Png)?,
        Fmt::Bmp => img.write_to(&mut out, ImageFormat::Bmp)?,
        Fmt::Tiff => img.write_to(&mut out, ImageFormat::Tiff)?,
        Fmt::Gif => DynamicImage::ImageRgba8(img.to_rgba8()).write_to(&mut out, ImageFormat::Gif)?,
        Fmt::Webp => {
            let norm = if img.color().has_alpha() {
                DynamicImage::ImageRgba8(img.to_rgba8())
            } else {
                DynamicImage::ImageRgb8(img.to_rgb8())
            };
            norm.write_to(&mut out, ImageFormat::WebP)?
        }
        #[cfg(feature = "avif-encode")]
        Fmt::Avif => {
            let enc =
                image::codecs::avif::AvifEncoder::new_with_speed_quality(&mut out, 6, opts.jpeg_quality.clamp(1, 100));
            let rgba = DynamicImage::ImageRgba8(img.to_rgba8());
            rgba.write_with_encoder(enc)?;
        }
        #[cfg(not(feature = "avif-encode"))]
        Fmt::Avif => return os::encode_with_wic(&img.to_rgba8(), fmt),
        Fmt::Heic => return os::encode_with_wic(&img.to_rgba8(), fmt),
    }
    Ok(out.into_inner())
}

/// Flattens transparency onto white (JPEG and friends have no alpha).
pub fn flatten_on_white(img: &DynamicImage) -> DynamicImage {
    if !img.color().has_alpha() {
        return img.clone();
    }
    let mut rgba = img.to_rgba8();
    for p in rgba.pixels_mut() {
        let a = p[3] as u32;
        for c in &mut p.0[..3] {
            *c = ((*c as u32 * a + 255 * (255 - a)) / 255) as u8;
        }
        p[3] = 255;
    }
    DynamicImage::ImageRgb8(DynamicImage::ImageRgba8(rgba).to_rgb8())
}

#[cfg(test)]
pub(crate) mod testutil {
    use image::{DynamicImage, RgbImage, RgbaImage};

    pub fn gradient(w: u32, h: u32) -> RgbImage {
        RgbImage::from_fn(w, h, |x, y| image::Rgb([(x * 255 / w.max(1)) as u8, (y * 255 / h.max(1)) as u8, 128]))
    }
    pub fn rgba(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_fn(w, h, |x, y| {
            image::Rgba([(x * 7) as u8, (y * 11) as u8, 90, if x < w / 2 { 255 } else { 128 }])
        })
    }
    pub fn png(img: &DynamicImage) -> Vec<u8> {
        let mut out = std::io::Cursor::new(Vec::new());
        img.write_to(&mut out, image::ImageFormat::Png).unwrap();
        out.into_inner()
    }
    pub fn jpeg(img: &RgbImage) -> Vec<u8> {
        let mut out = Vec::new();
        image::codecs::jpeg::JpegEncoder::new_with_quality(&mut out, 85).encode_image(img).unwrap();
        out
    }
    /// A JPEG with `tiff` (a TIFF-structured EXIF payload) in an APP1 segment right after SOI.
    pub fn jpeg_with_exif(img: &RgbImage, tiff: &[u8]) -> Vec<u8> {
        let base = jpeg(img);
        let mut app1 = b"Exif\x00\x00".to_vec();
        app1.extend_from_slice(tiff);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE1];
        out.extend_from_slice(&((app1.len() + 2) as u16).to_be_bytes());
        out.extend_from_slice(&app1);
        out.extend_from_slice(&base[2..]);
        out
    }

    /// A JPEG carrying an EXIF block whose only tag is Orientation = `orientation`.
    pub fn jpeg_with_orientation(img: &RgbImage, orientation: u16) -> Vec<u8> {
        let mut tiff = b"II*\x00\x08\x00\x00\x00".to_vec();
        tiff.extend_from_slice(&1u16.to_le_bytes());
        tiff.extend_from_slice(&0x0112u16.to_le_bytes());
        tiff.extend_from_slice(&3u16.to_le_bytes());
        tiff.extend_from_slice(&1u32.to_le_bytes());
        tiff.extend_from_slice(&orientation.to_le_bytes());
        tiff.extend_from_slice(&[0, 0]);
        tiff.extend_from_slice(&0u32.to_le_bytes());
        jpeg_with_exif(img, &tiff)
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use image::GenericImageView;

    #[test]
    fn every_pure_rust_format_round_trips_dimensions() {
        let img = DynamicImage::ImageRgba8(rgba(24, 16));
        for f in [Fmt::Png, Fmt::Jpg, Fmt::Webp, Fmt::Gif, Fmt::Tiff, Fmt::Bmp] {
            let bytes = encode(&img, f, &EncodeOptions::default()).unwrap();
            let back = decode(&bytes).unwrap_or_else(|e| panic!("{f:?}: {e}"));
            assert_eq!(back.dimensions(), (24, 16), "{f:?}");
        }
    }

    #[test]
    fn png_round_trip_is_pixel_exact() {
        let img = DynamicImage::ImageRgba8(rgba(9, 7));
        let back = decode(&encode(&img, Fmt::Png, &EncodeOptions::default()).unwrap()).unwrap();
        assert_eq!(back.to_rgba8(), img.to_rgba8());
    }

    #[test]
    fn jpeg_output_is_flattened_on_white_not_black() {
        let img = DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(8, 8, image::Rgba([0, 0, 0, 0])));
        let back = decode(&encode(&img, Fmt::Jpg, &EncodeOptions::default()).unwrap()).unwrap().to_rgb8();
        assert!(back.pixels().all(|p| p.0.iter().all(|&c| c > 240)));
    }

    #[test]
    fn jpeg_quality_changes_the_size() {
        let img = DynamicImage::ImageRgb8(gradient(128, 128));
        let hi = encode(&img, Fmt::Jpg, &EncodeOptions { jpeg_quality: 95 }).unwrap().len();
        let lo = encode(&img, Fmt::Jpg, &EncodeOptions { jpeg_quality: 20 }).unwrap().len();
        assert!(lo < hi, "{lo} !< {hi}");
    }

    #[test]
    fn exif_orientation_is_applied_on_decode() {
        let src = gradient(4, 2);
        assert_eq!(decode(&jpeg_with_orientation(&src, 1)).unwrap().dimensions(), (4, 2));
        assert_eq!(decode(&jpeg_with_orientation(&src, 6)).unwrap().dimensions(), (2, 4));
    }

    #[test]
    fn corrupt_and_truncated_input_never_panics() {
        let good = png(&DynamicImage::ImageRgb8(gradient(32, 32)));
        for cut in [0, 1, 8, 20, good.len() / 2, good.len() - 20] {
            assert!(decode(&good[..cut]).is_err(), "cut {cut}");
        }
        let mut flipped = good.clone();
        for i in (40..flipped.len()).step_by(7) {
            flipped[i] ^= 0x5A;
        }
        let _ = decode(&flipped); // may or may not error, must not panic
        assert!(decode(b"definitely not an image").is_err());
        assert!(decode(&[0xFF, 0xD8, 0xFF, 0xE0, 0, 4, 0, 0]).is_err());
    }

    #[test]
    fn oversized_dimensions_are_refused_before_allocating() {
        // Valid PNG signature + IHDR claiming 60000x60000 (3.6 gigapixels), nothing else.
        let mut b = vec![0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13];
        let mut ihdr = b"IHDR".to_vec();
        ihdr.extend_from_slice(&60_000u32.to_be_bytes());
        ihdr.extend_from_slice(&60_000u32.to_be_bytes());
        ihdr.extend_from_slice(&[8, 2, 0, 0, 0]);
        b.extend_from_slice(&ihdr);
        b.extend_from_slice(&crc32fast::hash(&ihdr).to_be_bytes());
        // Empty IDAT so the decoder reaches the point where it reports dimensions.
        b.extend_from_slice(&0u32.to_be_bytes());
        b.extend_from_slice(b"IDAT");
        b.extend_from_slice(&crc32fast::hash(b"IDAT").to_be_bytes());
        assert!(matches!(decode(&b), Err(Error::TooLarge { .. })), "{:?}", decode(&b));
    }

    #[test]
    fn heic_and_avif_outputs_are_typed_when_missing() {
        let img = DynamicImage::ImageRgba8(rgba(16, 16));
        for f in [Fmt::Heic, Fmt::Avif] {
            match encode(&img, f, &EncodeOptions::default()) {
                Ok(bytes) => assert!(bytes.len() > 16, "{f:?}"),
                Err(Error::Unavailable { feature, .. }) => assert_eq!(feature, f.label()),
                Err(e) => panic!("{f:?}: unexpected {e}"),
            }
        }
    }

    #[cfg(feature = "avif-encode")]
    #[test]
    fn avif_is_written_by_the_builtin_encoder() {
        let img = DynamicImage::ImageRgba8(rgba(32, 24));
        let bytes = encode(&img, Fmt::Avif, &EncodeOptions { jpeg_quality: 70 }).unwrap();
        assert_eq!(super::super::kind::detect_head(None, &bytes), Some(SourceKind::Image(Fmt::Avif)));
        if os::can_decode(Fmt::Avif).is_available() {
            assert_eq!(decode(&bytes).unwrap().dimensions(), (32, 24));
        }
    }
}
