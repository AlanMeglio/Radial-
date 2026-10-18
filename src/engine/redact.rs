//! Destructive redaction: pixels are replaced, so nothing underneath can be recovered from the output.

use super::crop::Rect;
use super::error::{Error, Result};
use super::kind::{detect_head, SourceKind};
use super::os;
use super::pdfwrite::{images_to_pdf_at, PdfImage};
use super::raster::{self, EncodeOptions, MAX_PIXELS};
use crate::convert::Fmt;
use image::{DynamicImage, RgbaImage};

/// Paints every rectangle solid black (alpha 255) and returns how many touched the image.
pub fn redact(img: &mut RgbaImage, rects: &[Rect]) -> usize {
    let (w, h) = img.dimensions();
    let stride = w as usize * 4;
    let raw: &mut [u8] = img.as_mut();
    let mut hit = 0;
    for r in rects {
        let Some(c) = r.clamp_to(w, h) else { continue };
        for y in c.y..c.y + c.h {
            let row = y as usize * stride;
            for px in raw[row + c.x as usize * 4..row + (c.x + c.w) as usize * 4].chunks_exact_mut(4) {
                px.copy_from_slice(&[0, 0, 0, 255]);
            }
        }
        hit += 1;
    }
    hit
}

/// Redacts an image file's bytes and re-encodes it in its own format (PNG when that format cannot be
/// written on this machine). Re-encoding also drops EXIF, which can carry a thumbnail of the original.
pub fn redact_image(bytes: &[u8], rects: &[Rect]) -> Result<(Vec<u8>, Fmt)> {
    let Some(SourceKind::Image(fmt)) = detect_head(None, bytes) else {
        return Err(Error::Unsupported("redaction needs an image file".into()));
    };
    let mut img = raster::decode(bytes)?.to_rgba8();
    if redact(&mut img, rects) == 0 {
        return Err(Error::InvalidArgument("no redaction rectangle overlaps the image".into()));
    }
    let img = DynamicImage::ImageRgba8(img);
    let opts = EncodeOptions { jpeg_quality: 92 };
    match raster::encode(&img, fmt, &opts) {
        Ok(out) => Ok((out, fmt)),
        Err(Error::Unavailable { .. }) => Ok((raster::encode(&img, Fmt::Png, &opts)?, Fmt::Png)),
        Err(e) => Err(e),
    }
}

/// A rectangle on a rendered PDF page, in pixels of the page rendered at the redaction `dpi`.
#[derive(Clone, Copy, Debug)]
pub struct PageRegion {
    /// 0-based page index.
    pub page: u32,
    pub rect: Rect,
}

/// Rasterizes the PDF at `dpi`, blacks out the regions and writes an image-only PDF: the text layer, fonts
/// and any hidden content of the original are gone.
pub fn redact_pdf(pdf: &[u8], regions: &[PageRegion], dpi: u32, jpeg_quality: u8) -> Result<Vec<u8>> {
    let count = os::pdf_page_count(pdf)?;
    if let Some(bad) = regions.iter().find(|r| r.page >= count) {
        return Err(Error::InvalidArgument(format!("page {} of {count}", bad.page + 1)));
    }
    let regions = regions.to_vec();
    let all: Vec<u32> = (0..count).collect();
    let pages = os::render_pdf_each(pdf, &all, dpi, MAX_PIXELS, move |i, mut bitmap| {
        let rects: Vec<Rect> = regions.iter().filter(|r| r.page == i).map(|r| r.rect).collect();
        redact(&mut bitmap, &rects);
        let img = DynamicImage::ImageRgba8(bitmap);
        let jpg = raster::encode(&img, Fmt::Jpg, &EncodeOptions { jpeg_quality })?;
        PdfImage::from_image(&img, Some(&jpg))
    })?;
    Ok(images_to_pdf_at(&pages, dpi as f64))
}

#[cfg(test)]
mod tests {
    use super::super::error::Error;
    use super::super::pdfread::extract_pages;
    use super::super::pdfwrite::text_to_pdf;
    use super::super::raster::{decode, testutil::*};
    use super::super::{os, raster};
    use super::*;

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn replaces_pixels_inside_and_leaves_the_rest() {
        let mut img = rgba(20, 20);
        let before = img.clone();
        let n = redact(&mut img, &[Rect::new(5, 5, 4, 3), Rect::new(18, 18, 10, 10), Rect::new(50, 50, 2, 2)]);
        assert_eq!(n, 2, "the rectangle fully outside does not count");
        for y in 0..20 {
            for x in 0..20 {
                let inside = (5..9).contains(&x) && (5..8).contains(&y) || x >= 18 && y >= 18;
                if inside {
                    assert_eq!(img.get_pixel(x, y).0, [0, 0, 0, 255], "{x},{y}");
                } else {
                    assert_eq!(img.get_pixel(x, y), before.get_pixel(x, y), "{x},{y}");
                }
            }
        }
    }

    #[test]
    fn transparent_pixels_become_opaque_black() {
        let mut img = image::RgbaImage::from_pixel(4, 4, image::Rgba([9, 9, 9, 0]));
        redact(&mut img, &[Rect::new(0, 0, 4, 4)]);
        assert!(img.pixels().all(|p| p.0 == [0, 0, 0, 255]));
    }

    #[test]
    fn png_redaction_is_exact_and_keeps_the_format() {
        let src = png(&image::DynamicImage::ImageRgb8(gradient(40, 30)));
        let (out, fmt) = redact_image(&src, &[Rect::new(10, 10, 10, 10)]).unwrap();
        assert_eq!(fmt, Fmt::Png);
        let img = decode(&out).unwrap().to_rgba8();
        assert_eq!(img.get_pixel(12, 12).0, [0, 0, 0, 255]);
        assert_eq!(img.get_pixel(0, 0).0[..3], decode(&src).unwrap().to_rgba8().get_pixel(0, 0).0[..3]);
    }

    #[test]
    fn jpeg_redaction_removes_exif_and_blacks_the_area() {
        let src = jpeg_with_orientation(&gradient(64, 64), 1);
        assert!(has(&src, b"Exif"));
        let (out, fmt) = redact_image(&src, &[Rect::new(8, 8, 32, 32)]).unwrap();
        assert_eq!(fmt, Fmt::Jpg);
        assert!(!has(&out, b"Exif"), "metadata must not survive redaction");
        let img = decode(&out).unwrap().to_rgb8();
        let mean: u32 =
            (16..32).map(|x| img.get_pixel(x, 20).0.iter().map(|&c| c as u32).sum::<u32>()).sum::<u32>() / (16 * 3);
        assert!(mean < 12, "{mean}");
    }

    #[test]
    fn nothing_to_redact_or_garbage_is_reported() {
        let src = png(&image::DynamicImage::ImageRgb8(gradient(8, 8)));
        assert!(matches!(redact_image(&src, &[]), Err(Error::InvalidArgument(_))));
        assert!(matches!(redact_image(&src, &[Rect::new(100, 100, 5, 5)]), Err(Error::InvalidArgument(_))));
        assert!(redact_image(b"junk", &[Rect::new(0, 0, 1, 1)]).is_err());
    }

    #[test]
    fn pdf_redaction_removes_the_text_layer() {
        let pdf = text_to_pdf("TOP-SECRET-PAYLOAD 12345\nsecond line");
        assert!(extract_pages(&pdf).unwrap()[0].contains("TOP-SECRET-PAYLOAD"));
        let page = os::render_pdf_pages(&pdf, &[0], 72, 50_000_000).unwrap().remove(0);
        // Cover the whole upper half of the page.
        let region = PageRegion { page: 0, rect: Rect::new(0, 0, page.width(), page.height() / 2) };
        let out = redact_pdf(&pdf, &[region], 72, 85).unwrap();
        assert!(!has(&out, b"TOP-SECRET"), "text must not survive in the bytes");
        assert_eq!(os::pdf_page_count(&out).unwrap(), 1);
        assert_eq!(extract_pages(&out).unwrap(), vec![String::new()], "no extractable text remains");
        let back = os::render_pdf_pages(&out, &[0], 72, 50_000_000).unwrap().remove(0);
        let c = back.get_pixel(back.width() / 2, 40).0;
        assert!(c[0] < 40 && c[1] < 40 && c[2] < 40, "{c:?}");
    }

    #[test]
    fn pdf_regions_on_missing_pages_are_rejected() {
        let pdf = text_to_pdf("x");
        let bad = PageRegion { page: 3, rect: Rect::new(0, 0, 5, 5) };
        assert!(matches!(redact_pdf(&pdf, &[bad], 72, 85), Err(Error::InvalidArgument(_))));
        let _ = raster::MAX_PIXELS;
    }
}
