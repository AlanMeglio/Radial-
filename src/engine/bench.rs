//! Timing report for the heavy paths (not run by default).
//!
//! `cargo test --release bench_report -- --ignored --nocapture`

use super::kind::{SourceKind, Target};
use super::pipeline::{convert_bytes, ConvertOptions};
use super::raster::testutil::{jpeg, png};
use super::{background, compress, crop, meta, os, pdfwrite, redact};
use crate::convert::Fmt;
use image::DynamicImage;

fn img_kind(f: Fmt) -> SourceKind {
    SourceKind::Image(f)
}

/// Timing report for the heavy paths. Run: `cargo test --release bench_report -- --ignored --nocapture`
#[test]
#[ignore = "benchmark: run in release mode with --ignored --nocapture"]
fn bench_report() {
    use std::time::Instant;
    let mut rows: Vec<(String, f64)> = Vec::new();
    let mut t = |name: &str, f: &mut dyn FnMut()| {
        let start = Instant::now();
        f();
        rows.push((name.to_string(), start.elapsed().as_secs_f64() * 1000.0));
    };
    // 3000x2000 "photo": smooth gradients plus a little noise so nothing compresses to nothing.
    let mut seed = 7u32;
    let photo = image::RgbImage::from_fn(3000, 2000, |x, y| {
        seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
        let n = (seed >> 28) as u8;
        image::Rgb([((x / 12) as u8).wrapping_add(n), ((y / 8) as u8).wrapping_add(n), (((x + y) / 20) as u8)])
    });
    let img = DynamicImage::ImageRgb8(photo.clone());
    let png_bytes = png(&img);
    let jpg_bytes = jpeg(&photo);
    let o = ConvertOptions::default();
    for (name, to) in [
        ("PNG -> JPG", Target::Image(Fmt::Jpg)),
        ("PNG -> WEBP", Target::Image(Fmt::Webp)),
        ("PNG -> TIFF", Target::Image(Fmt::Tiff)),
        ("PNG -> BMP", Target::Image(Fmt::Bmp)),
        ("PNG -> PDF", Target::Pdf),
        ("PNG -> DOCX", Target::Docx),
        ("PNG -> HEIC (OS)", Target::Image(Fmt::Heic)),
    ] {
        t(&format!("3000x2000 {name}"), &mut || {
            let _ = convert_bytes(&png_bytes, img_kind(Fmt::Png), to, &o);
        });
    }
    t("3000x2000 JPG -> PDF (embed)", &mut || {
        convert_bytes(&jpg_bytes, img_kind(Fmt::Jpg), Target::Pdf, &o).unwrap();
    });
    t("3000x2000 compress PNG (lossless)", &mut || {
        compress::compress_bytes(&png_bytes, &Default::default()).unwrap();
    });
    t("3000x2000 compress JPG q80", &mut || {
        compress::compress_bytes(&jpg_bytes, &Default::default()).unwrap();
    });
    t("3000x2000 strip metadata (JPEG)", &mut || {
        meta::strip(&jpg_bytes).unwrap();
    });
    let rgba = photo_rgba(&photo);
    t("3000x2000 add background (pad 120, r40, shadow 60)", &mut || {
        let opts = background::BgOptions { padding: 120, corners: 40, shadow: 60, ..Default::default() };
        background::add_background(&rgba, &opts).unwrap();
    });
    t("3000x2000 crop 16:9", &mut || {
        crop::crop(&img, crop::largest_centered(3000, 2000, crop::Aspect::Ratio(16, 9))).unwrap();
    });
    t("3000x2000 redact 4 boxes (PNG round trip)", &mut || {
        let r = [crop::Rect::new(10, 10, 500, 300); 4];
        redact::redact_image(&png_bytes, &r).unwrap();
    });
    let text: String = (0..519)
        .map(|i| {
            format!(
                "Fila {i}: texto de prueba para medir el rendimiento del motor.
"
            )
        })
        .collect();
    let mut pdf10 = Vec::new();
    t("10-page text -> PDF", &mut || {
        pdf10 = pdfwrite::text_to_pdf(&text);
    });
    let pages = os::pdf_page_count(&pdf10).unwrap();
    for (name, to) in [
        ("PNG (all pages, 150 dpi)", Target::Image(Fmt::Png)),
        ("JPG (all pages, 150 dpi)", Target::Image(Fmt::Jpg)),
        ("TXT", Target::Txt),
        ("DOCX", Target::Docx),
    ] {
        t(&format!("{pages}-page PDF -> {name}"), &mut || {
            convert_bytes(&pdf10, SourceKind::Pdf, to, &o).unwrap();
        });
    }
    println!(
        "
--- engine timings (ms, single run) ---"
    );
    for (n, ms) in rows {
        println!("{ms:>9.1}  {n}");
    }
}

fn photo_rgba(p: &image::RgbImage) -> image::RgbaImage {
    DynamicImage::ImageRgb8(p.clone()).to_rgba8()
}
