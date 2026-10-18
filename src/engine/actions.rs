//! File-level wrappers for the image actions: read, apply, save next to the source without overwriting.

use super::annotate::{self, Annotation};
use super::background::{self, BgOptions};
use super::crop::{self, Rect};
use super::edit::{self, EditOp};
use super::error::{Error, Result};
use super::io;
use super::kind::{detect_head, SourceKind};
use super::raster::{self, EncodeOptions};
use super::redact::{self, PageRegion};
use crate::convert::Fmt;
use ab_glyph::FontArc;
use image::DynamicImage;
use std::path::{Path, PathBuf};

/// Quality used when an edited JPEG is written back.
const JPEG_QUALITY: u8 = 92;

fn dir_and_stem(src: &Path) -> (&Path, &str) {
    let dir = src.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    (dir, src.file_stem().and_then(|s| s.to_str()).unwrap_or("image"))
}

/// Saves `img` as `name (suffix).ext` next to `src`, in `fmt` or (when `None`) the source's own format.
/// Formats this machine cannot write fall back to PNG.
pub fn save_image(src: &Path, img: &DynamicImage, suffix: &str, fmt: Option<Fmt>) -> Result<PathBuf> {
    let wanted = fmt.or_else(|| src.extension().and_then(|e| e.to_str()).and_then(Fmt::from_ext)).unwrap_or(Fmt::Png);
    let opts = EncodeOptions { jpeg_quality: JPEG_QUALITY };
    let (fmt, bytes) = match raster::encode(img, wanted, &opts) {
        Ok(b) => (wanted, b),
        Err(Error::Unavailable { .. }) => (Fmt::Png, raster::encode(img, Fmt::Png, &opts)?),
        Err(e) => return Err(e),
    };
    let (dir, stem) = dir_and_stem(src);
    io::commit_unique(dir, &format!("{stem} ({suffix})"), fmt.ext(), &bytes)
}

/// Reads an image file: its own format (by content) and the decoded pixels.
fn load(src: &Path) -> Result<(Fmt, DynamicImage)> {
    let bytes = io::read_limited(src)?;
    let Some(SourceKind::Image(fmt)) = detect_head(None, &bytes) else {
        return Err(Error::Unsupported(format!("{}: not an image", src.display())));
    };
    Ok((fmt, raster::decode(&bytes)?))
}

pub fn crop_file(src: &Path, rect: Rect) -> Result<PathBuf> {
    let (fmt, img) = load(src)?;
    save_image(src, &crop::crop(&img, rect)?, "cropped", Some(fmt))
}

/// "Save with background": PNG unless `fmt` says otherwise (JPEG drops transparency).
pub fn background_file(src: &Path, opts: &BgOptions, fmt: Option<Fmt>) -> Result<PathBuf> {
    let (_, img) = load(src)?;
    let out = background::add_background(&img.to_rgba8(), opts)?;
    save_image(src, &DynamicImage::ImageRgba8(out), "background", Some(fmt.unwrap_or(Fmt::Png)))
}

pub fn annotate_file(src: &Path, annotations: &[Annotation], font: Option<&FontArc>) -> Result<PathBuf> {
    let (fmt, img) = load(src)?;
    let mut rgba = img.to_rgba8();
    annotate::render(&mut rgba, annotations, font)?;
    save_image(src, &DynamicImage::ImageRgba8(rgba), "annotated", Some(fmt))
}

pub fn edit_file(src: &Path, ops: &[EditOp]) -> Result<PathBuf> {
    let (fmt, img) = load(src)?;
    save_image(src, &edit::apply(img, ops)?, "edited", Some(fmt))
}

/// Redacts an image (`rects`) or a PDF (`regions`, in pixels of pages rendered at `dpi`) into `name (redacted).ext`.
pub fn redact_file(src: &Path, rects: &[Rect], regions: &[PageRegion], dpi: u32) -> Result<PathBuf> {
    let bytes = io::read_limited(src)?;
    let (out, ext) = match detect_head(src.extension().and_then(|e| e.to_str()), &bytes) {
        Some(SourceKind::Image(_)) => {
            let (data, fmt) = redact::redact_image(&bytes, rects)?;
            (data, fmt.ext())
        }
        Some(SourceKind::Pdf) => (redact::redact_pdf(&bytes, regions, dpi, 85)?, "pdf"),
        _ => return Err(Error::Unsupported("REDACT works on images and PDF files".into())),
    };
    let (dir, stem) = dir_and_stem(src);
    io::commit_unique(dir, &format!("{stem} (redacted)"), ext, &out)
}

#[cfg(test)]
mod tests {
    use super::super::background::{Background, Ratio};
    use super::super::error::Error;
    use super::super::raster::{self, testutil::*};
    use super::super::{os, pdfread, pdfwrite};
    use super::*;
    use image::{DynamicImage, GenericImageView};

    fn setup(name: &str, file: &str, bytes: &[u8]) -> (PathBuf, PathBuf) {
        let d = std::env::temp_dir().join(format!("radial-act-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        let p = d.join(file);
        std::fs::write(&p, bytes).unwrap();
        (d, p)
    }

    #[test]
    fn crop_file_saves_a_cropped_copy_in_the_same_format() {
        let (d, src) = setup("crop", "p.png", &png(&DynamicImage::ImageRgb8(gradient(40, 30))));
        let out = crop_file(&src, Rect::new(5, 5, 10, 8)).unwrap();
        assert_eq!(out, d.join("p (cropped).png"));
        assert_eq!(raster::decode(&std::fs::read(&out).unwrap()).unwrap().dimensions(), (10, 8));
        assert_eq!(crop_file(&src, Rect::new(5, 5, 10, 8)).unwrap(), d.join("p (cropped) (1).png"));
        assert!(matches!(crop_file(&src, Rect::new(500, 5, 10, 8)), Err(Error::InvalidArgument(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn background_file_defaults_to_png_and_honours_the_options() {
        let (d, src) = setup("bg", "shot.jpg", &jpeg(&gradient(40, 30)));
        let opts = BgOptions {
            background: Background::Solid([0, 0, 255, 255]),
            padding: 10,
            corners: 4,
            shadow: 6,
            ratio: Ratio::Square,
        };
        let out = background_file(&src, &opts, None).unwrap();
        assert_eq!(out, d.join("shot (background).png"));
        assert_eq!(raster::decode(&std::fs::read(&out).unwrap()).unwrap().dimensions(), (60, 60));
        let jpg = background_file(&src, &opts, Some(Fmt::Jpg)).unwrap();
        assert_eq!(jpg.extension().unwrap(), "jpg");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn annotate_and_edit_files() {
        let (d, src) = setup(
            "ann",
            "a.png",
            &png(&DynamicImage::ImageRgb8(image::RgbImage::from_pixel(30, 20, image::Rgb([255, 255, 255])))),
        );
        let a =
            Annotation::Rect { from: (0.0, 0.0), to: (30.0, 20.0), color: [255, 0, 0, 255], width: 1.0, fill: true };
        let out = annotate_file(&src, &[a], None).unwrap();
        assert_eq!(out, d.join("a (annotated).png"));
        assert_eq!(raster::decode(&std::fs::read(&out).unwrap()).unwrap().to_rgb8().get_pixel(5, 5).0, [255, 0, 0]);
        let edited = edit_file(&src, &[EditOp::Rotate90]).unwrap();
        assert_eq!(edited, d.join("a (edited).png"));
        assert_eq!(raster::decode(&std::fs::read(&edited).unwrap()).unwrap().dimensions(), (20, 30));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn redact_file_handles_images_and_pdfs() {
        let (d, src) = setup("red", "scan.png", &png(&DynamicImage::ImageRgb8(gradient(40, 30))));
        let out = redact_file(&src, &[Rect::new(0, 0, 20, 20)], &[], 150).unwrap();
        assert_eq!(out, d.join("scan (redacted).png"));
        assert_eq!(raster::decode(&std::fs::read(&out).unwrap()).unwrap().to_rgb8().get_pixel(3, 3).0, [0, 0, 0]);
        assert!(src.exists());

        let pdf = d.join("letter.pdf");
        std::fs::write(&pdf, pdfwrite::text_to_pdf("CONFIDENTIAL")).unwrap();
        let page = os::render_pdf_pages(&std::fs::read(&pdf).unwrap(), &[0], 72, 50_000_000).unwrap().remove(0);
        let region = PageRegion { page: 0, rect: Rect::new(0, 0, page.width(), page.height()) };
        let out = redact_file(&pdf, &[], &[region], 72).unwrap();
        assert_eq!(out, d.join("letter (redacted).pdf"));
        assert_eq!(pdfread::extract_pages(&std::fs::read(&out).unwrap()).unwrap(), vec![String::new()]);

        let txt = d.join("n.txt");
        std::fs::write(&txt, b"hello").unwrap();
        assert!(matches!(redact_file(&txt, &[Rect::new(0, 0, 1, 1)], &[], 72), Err(Error::Unsupported(_))));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn save_image_falls_back_to_png_when_the_format_cannot_be_written() {
        let (d, src) = setup("fb", "x.avif", b"placeholder, only the name matters here");
        let img = DynamicImage::ImageRgb8(gradient(8, 8));
        let out = save_image(&src, &img, "edited", Some(Fmt::Avif)).unwrap();
        let writable = super::super::os::can_encode(Fmt::Avif).is_available();
        assert_eq!(out, d.join(if writable { "x (edited).avif" } else { "x (edited).png" }));
        let _ = std::fs::remove_dir_all(&d);
    }
}
