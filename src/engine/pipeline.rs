//! The conversion matrix: every (source kind, target) pair the menus offer.

use super::docx::{self, Block, DocxImage};
use super::error::{Error, Result};
use super::io;
use super::kind::{detect_head, SourceKind, Target, HEAD_LEN};
use super::meta::exif_orientation;
use super::os::{self, Availability};
use super::pdfread;
use super::pdfwrite::{images_to_pdf, text_to_pdf, PdfImage};
use super::raster::{self, EncodeOptions, MAX_PIXELS};
use crate::convert::Fmt;
use image::DynamicImage;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug)]
pub struct ConvertOptions {
    /// 1..=100, for JPEG output.
    pub jpeg_quality: u8,
    /// Resolution when a PDF page is turned into a picture.
    pub pdf_dpi: u32,
    /// PDF to image: every page (`name-p1.png`, ...) or only the first.
    pub all_pages: bool,
}

impl Default for ConvertOptions {
    fn default() -> Self {
        ConvertOptions { jpeg_quality: 90, pdf_dpi: 150, all_pages: true }
    }
}

/// One output file, not yet written.
#[derive(Debug)]
pub struct Artifact {
    /// Appended to the source's file name stem (`""` or `-p2`).
    pub suffix: String,
    pub ext: &'static str,
    pub data: Vec<u8>,
}

/// Longest text file converted to PDF or DOCX.
const MAX_TEXT_BYTES: usize = 32 * 1024 * 1024;
/// Resolution of page pictures embedded in a DOCX when a PDF page has no text.
const DOCX_PAGE_DPI: u32 = 110;
/// Resolution used to read scanned pages with OCR.
const OCR_DPI: u32 = 200;

fn one(ext: &'static str, data: Vec<u8>) -> Result<Vec<Artifact>> {
    Ok(vec![Artifact { suffix: String::new(), ext, data }])
}

fn unsupported(kind: SourceKind, to: Target) -> Error {
    let from = match kind {
        SourceKind::Image(f) => f.label(),
        SourceKind::Pdf => "PDF",
        SourceKind::Docx => "DOCX",
        SourceKind::Text => "text",
    };
    Error::Unsupported(format!("cannot convert {from} to {}", to.label()))
}

/// Converts in memory. `kind` is what `kind::detect` said about `input`.
pub fn convert_bytes(input: &[u8], kind: SourceKind, to: Target, opts: &ConvertOptions) -> Result<Vec<Artifact>> {
    let enc = EncodeOptions { jpeg_quality: opts.jpeg_quality };
    match (kind, to) {
        (SourceKind::Image(_), Target::Image(dst)) => {
            let img = raster::decode(input)?;
            one(dst.ext(), raster::encode(&img, dst, &enc)?)
        }
        (SourceKind::Image(src), Target::Pdf) => {
            let img = raster::decode(input)?;
            // The JPEG can be embedded as is only when no EXIF rotation has to be applied to it.
            let original = (src == Fmt::Jpg && exif_orientation(input) == 1).then_some(input);
            one("pdf", images_to_pdf(&[PdfImage::from_image(&img, original)?]))
        }
        (SourceKind::Image(src), Target::Docx) => {
            let img = raster::decode(input)?;
            let (ext, data): (&'static str, Vec<u8>) = match src {
                Fmt::Jpg if exif_orientation(input) == 1 => ("jpeg", input.to_vec()),
                Fmt::Png if exif_orientation(input) == 1 => ("png", input.to_vec()),
                Fmt::Jpg => ("jpeg", raster::encode(&img, Fmt::Jpg, &EncodeOptions { jpeg_quality: 92 })?),
                _ => ("png", raster::encode(&img, Fmt::Png, &enc)?),
            };
            one(
                "docx",
                docx::build(&[Block::Image(DocxImage { ext, data, width_px: img.width(), height_px: img.height() })]),
            )
        }
        (SourceKind::Pdf, Target::Image(dst)) => pdf_to_images(input, dst, opts),
        (SourceKind::Pdf, Target::Txt) => {
            let join = |pages: Vec<String>| {
                pages.iter().map(|p| p.trim()).filter(|p| !p.is_empty()).collect::<Vec<_>>().join("\n\n")
            };
            let mut text = join(pdfread::extract_pages(input)?);
            if text.is_empty() {
                // No text layer: a scan, or fonts without a text map. Read the page pictures with Windows OCR.
                text = join(os::ocr_pdf_pages(input, OCR_DPI, MAX_PIXELS)?);
            }
            if text.is_empty() {
                return Err(Error::Unsupported("no text could be found in this PDF".into()));
            }
            one("txt", text.into_bytes())
        }
        (SourceKind::Pdf, Target::Docx) => pdf_to_docx(input),
        (SourceKind::Docx, Target::Txt) => one("txt", docx::docx_to_text(input)?.into_bytes()),
        (SourceKind::Docx, Target::Pdf) => one("pdf", text_to_pdf(&docx::docx_to_text(input)?)),
        (SourceKind::Text, Target::Pdf) => one("pdf", text_to_pdf(&decode_text(input)?)),
        (SourceKind::Text, Target::Docx) => one("docx", docx::text_to_docx(&decode_text(input)?)),
        _ => Err(unsupported(kind, to)),
    }
}

fn pdf_to_images(pdf: &[u8], dst: Fmt, opts: &ConvertOptions) -> Result<Vec<Artifact>> {
    let count = os::pdf_page_count(pdf)?;
    if count == 0 {
        return Err(Error::Corrupt("the PDF has no pages".into()));
    }
    let pages: Vec<u32> = if opts.all_pages { (0..count).collect() } else { vec![0] };
    let digits = count.to_string().len();
    let multi = pages.len() > 1;
    let enc = EncodeOptions { jpeg_quality: opts.jpeg_quality };
    os::render_pdf_each(pdf, &pages, opts.pdf_dpi.clamp(36, 600), MAX_PIXELS, move |i, bitmap| {
        let data = raster::encode(&DynamicImage::ImageRgba8(bitmap), dst, &enc)?;
        let suffix = if multi { format!("-p{:0digits$}", i + 1) } else { String::new() };
        Ok(Artifact { suffix, ext: dst.ext(), data })
    })
}

/// Text pages become paragraphs; pages without a text layer (scans) become embedded pictures.
fn pdf_to_docx(pdf: &[u8]) -> Result<Vec<Artifact>> {
    let count = os::pdf_page_count(pdf)?;
    let texts = pdfread::extract_pages(pdf).unwrap_or_default();
    let has_text = |i: u32| texts.get(i as usize).is_some_and(|t| !t.trim().is_empty());
    let scanned: Vec<u32> = (0..count).filter(|&i| !has_text(i)).collect();
    let mut pictures: HashMap<u32, DocxImage> = HashMap::new();
    if !scanned.is_empty() {
        let rendered = os::render_pdf_each(pdf, &scanned, DOCX_PAGE_DPI, MAX_PIXELS, |i, bitmap| {
            let (width_px, height_px) = bitmap.dimensions();
            let data =
                raster::encode(&DynamicImage::ImageRgba8(bitmap), Fmt::Jpg, &EncodeOptions { jpeg_quality: 82 })?;
            Ok((i, DocxImage { ext: "jpeg", data, width_px, height_px }))
        })?;
        pictures.extend(rendered);
    }
    let mut blocks = Vec::new();
    for i in 0..count {
        if i > 0 {
            blocks.push(Block::PageBreak);
        }
        match pictures.remove(&i) {
            Some(pic) => blocks.push(Block::Image(pic)),
            None => blocks.extend(texts[i as usize].trim().lines().map(|l| Block::Paragraph(l.trim_end().to_string()))),
        }
    }
    one("docx", docx::build(&blocks))
}

/// Converts `src` next to itself and returns the new paths. Never overwrites; on failure no partial output is left.
pub fn convert_file(src: &Path, to: Target, opts: &ConvertOptions) -> Result<Vec<PathBuf>> {
    let input = io::read_limited(src)?;
    let ext = src.extension().and_then(|e| e.to_str());
    let kind = detect_head(ext, &input[..input.len().min(HEAD_LEN)])
        .ok_or_else(|| Error::Unsupported(format!("{}: not a file type this app converts", src.display())))?;
    let artifacts = convert_bytes(&input, kind, to, opts)?;
    let dir = src.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("converted");
    let mut written: Vec<PathBuf> = Vec::with_capacity(artifacts.len());
    for a in &artifacts {
        match io::commit_unique(dir, &format!("{stem}{}", a.suffix), a.ext, &a.data) {
            Ok(p) => written.push(p),
            Err(e) => {
                written.iter().for_each(|p| {
                    let _ = std::fs::remove_file(p);
                });
                return Err(e);
            }
        }
    }
    Ok(written)
}

/// Can this machine produce `to`? HEIC and AVIF depend on codecs Windows may not have.
pub fn target_availability(to: Target) -> Availability {
    match to {
        Target::Image(f @ (Fmt::Heic | Fmt::Avif)) => os::can_encode(f),
        _ => Availability::Available,
    }
}

/// Can this machine read `kind`?
pub fn source_availability(kind: SourceKind) -> Availability {
    match kind {
        SourceKind::Image(f @ (Fmt::Heic | Fmt::Avif)) => os::can_decode(f),
        _ => Availability::Available,
    }
}

/// Windows-1252 code points for bytes 0x80..=0x9F (undefined bytes keep their C1 control value).
const CP1252: [char; 32] = [
    '€', '\u{81}', '‚', 'ƒ', '„', '…', '†', '‡', 'ˆ', '‰', 'Š', '‹', 'Œ', '\u{8D}', 'Ž', '\u{8F}', '\u{90}', '‘', '’',
    '“', '”', '•', '–', '—', '˜', '™', 'š', '›', 'œ', '\u{9D}', 'ž', 'Ÿ',
];

fn latin(bytes: &[u8]) -> String {
    bytes.iter().map(|&b| if (0x80..=0x9F).contains(&b) { CP1252[(b - 0x80) as usize] } else { b as char }).collect()
}

/// Decodes a text file: UTF-8 (BOM optional), UTF-16 with BOM, or Windows-1252 as the last resort.
pub fn decode_text(bytes: &[u8]) -> Result<String> {
    if bytes.len() > MAX_TEXT_BYTES {
        return Err(Error::TooLarge { width: 0, height: 0 });
    }
    let utf16 = |rest: &[u8], le: bool| {
        let units: Vec<u16> = rest
            .chunks_exact(2)
            .map(|c| if le { u16::from_le_bytes([c[0], c[1]]) } else { u16::from_be_bytes([c[0], c[1]]) })
            .collect();
        String::from_utf16_lossy(&units)
    };
    Ok(match bytes {
        [0xFF, 0xFE, rest @ ..] => utf16(rest, true),
        [0xFE, 0xFF, rest @ ..] => utf16(rest, false),
        _ => {
            let body = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
            match std::str::from_utf8(body) {
                Ok(s) => s.to_string(),
                Err(_) => latin(body),
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::super::error::Error;
    use super::super::kind::{detect, targets_for};
    use super::super::raster::{self, testutil::*};
    use super::super::zip::ZipArchive;
    use super::super::{docx, os, pdfread, pdfwrite};
    use super::*;
    use crate::convert::Fmt;
    use image::{DynamicImage, GenericImageView};

    fn tmp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-pipe-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    fn run(bytes: &[u8], kind: SourceKind, to: Target) -> Result<Vec<Artifact>> {
        convert_bytes(bytes, kind, to, &ConvertOptions::default())
    }

    fn img_kind(f: Fmt) -> SourceKind {
        SourceKind::Image(f)
    }

    #[test]
    fn image_to_every_pure_format() {
        let src = png(&DynamicImage::ImageRgba8(rgba(30, 20)));
        for f in [Fmt::Jpg, Fmt::Webp, Fmt::Gif, Fmt::Tiff, Fmt::Bmp] {
            let out = run(&src, img_kind(Fmt::Png), Target::Image(f)).unwrap();
            assert_eq!((out.len(), out[0].suffix.as_str(), out[0].ext), (1, "", f.ext()));
            assert_eq!(raster::decode(&out[0].data).unwrap().dimensions(), (30, 20), "{f:?}");
        }
    }

    #[test]
    fn image_to_pdf_embeds_the_original_jpeg() {
        let jpg = jpeg(&gradient(64, 48));
        let out = run(&jpg, img_kind(Fmt::Jpg), Target::Pdf).unwrap();
        assert_eq!(out[0].ext, "pdf");
        assert!(has(&out[0].data, &jpg));
        assert_eq!(os::pdf_page_count(&out[0].data).unwrap(), 1);
        let rotated = jpeg_with_orientation(&gradient(64, 48), 6);
        let out = run(&rotated, img_kind(Fmt::Jpg), Target::Pdf).unwrap();
        assert!(!has(&out[0].data, &rotated), "a rotated JPEG must be re-encoded upright, not embedded as is");
        let page = os::render_pdf_pages(&out[0].data, &[0], 96, 50_000_000).unwrap().remove(0);
        assert!(page.height() > page.width(), "portrait after applying orientation");
    }

    #[test]
    fn image_to_pdf_from_png_keeps_transparency_as_a_soft_mask() {
        let out = run(&png(&DynamicImage::ImageRgba8(rgba(16, 16))), img_kind(Fmt::Png), Target::Pdf).unwrap();
        assert!(has(&out[0].data, b"/SMask"));
    }

    #[test]
    fn image_to_docx_embeds_the_picture() {
        let jpg = jpeg(&gradient(64, 48));
        let out = run(&jpg, img_kind(Fmt::Jpg), Target::Docx).unwrap();
        let z = ZipArchive::new(&out[0].data).unwrap();
        assert_eq!(z.read("word/media/image1.jpeg", 1 << 24).unwrap().unwrap(), jpg);
        let webp = raster::encode(&DynamicImage::ImageRgba8(rgba(20, 20)), Fmt::Webp, &Default::default()).unwrap();
        let out = run(&webp, img_kind(Fmt::Webp), Target::Docx).unwrap();
        let z = ZipArchive::new(&out[0].data).unwrap();
        assert!(z.read("word/media/image1.png", 1 << 24).unwrap().is_some(), "non-JPEG sources are embedded as PNG");
    }

    fn long_text_pdf() -> Vec<u8> {
        pdfwrite::text_to_pdf(&(0..150).map(|i| format!("fila {i}\n")).collect::<String>())
    }

    #[test]
    fn pdf_to_image_all_pages_or_first() {
        let pdf = long_text_pdf();
        let all = run(&pdf, SourceKind::Pdf, Target::Image(Fmt::Png)).unwrap();
        let suffixes: Vec<_> = all.iter().map(|a| a.suffix.as_str()).collect();
        assert_eq!(suffixes, ["-p1", "-p2", "-p3"]);
        assert!(all.iter().all(|a| a.ext == "png" && raster::decode(&a.data).is_ok()));
        let first = convert_bytes(
            &pdf,
            SourceKind::Pdf,
            Target::Image(Fmt::Jpg),
            &ConvertOptions { all_pages: false, ..Default::default() },
        )
        .unwrap();
        assert_eq!((first.len(), first[0].suffix.as_str(), first[0].ext), (1, "", "jpg"));
        let single = run(&pdfwrite::text_to_pdf("one page"), SourceKind::Pdf, Target::Image(Fmt::Png)).unwrap();
        assert_eq!(single[0].suffix, "", "a one-page PDF gets no page suffix");
    }

    #[test]
    fn pdf_dpi_controls_the_pixel_size() {
        let pdf = pdfwrite::text_to_pdf("x");
        let at = |dpi| {
            let o = ConvertOptions { pdf_dpi: dpi, ..Default::default() };
            raster::decode(&convert_bytes(&pdf, SourceKind::Pdf, Target::Image(Fmt::Png), &o).unwrap()[0].data)
                .unwrap()
                .width()
        };
        let (lo, hi) = (at(72), at(144));
        assert!(hi >= lo * 2 - 2 && hi <= lo * 2 + 2, "{lo} {hi}");
    }

    #[test]
    fn pdf_to_txt_and_docx_extract_the_text() {
        let text = "Informe anual\nresultados: 100% ñandú";
        let pdf = pdfwrite::text_to_pdf(text);
        let txt = run(&pdf, SourceKind::Pdf, Target::Txt).unwrap();
        assert_eq!(String::from_utf8(txt[0].data.clone()).unwrap().trim(), text);
        let dx = run(&pdf, SourceKind::Pdf, Target::Docx).unwrap();
        assert_eq!(docx::docx_to_text(&dx[0].data).unwrap().trim(), text);
    }

    #[test]
    fn scanned_pdf_falls_back_to_page_images_in_docx_and_refuses_txt() {
        let scan = pdfwrite::images_to_pdf(&[pdfwrite::PdfImage::from_image(
            &DynamicImage::ImageRgb8(gradient(200, 100)),
            None,
        )
        .unwrap()]);
        assert!(pdfread::extract_pages(&scan).unwrap()[0].is_empty());
        let dx = run(&scan, SourceKind::Pdf, Target::Docx).unwrap();
        let z = ZipArchive::new(&dx[0].data).unwrap();
        assert!(z.names().any(|n| n.starts_with("word/media/image")), "page picture embedded");
        // A picture with no letters: OCR (when installed) finds nothing, otherwise it is reported as unavailable.
        assert!(matches!(
            run(&scan, SourceKind::Pdf, Target::Txt),
            Err(Error::Unsupported(_) | Error::Unavailable { .. })
        ));
    }

    #[test]
    fn text_only_pdf_adds_no_pictures_to_the_docx() {
        let dx = run(&pdfwrite::text_to_pdf("solo texto"), SourceKind::Pdf, Target::Docx).unwrap();
        let z = ZipArchive::new(&dx[0].data).unwrap();
        assert!(!z.names().any(|n| n.starts_with("word/media/")));
    }

    #[test]
    fn text_and_docx_conversions_round_trip() {
        let text = "línea uno\n\nlínea tres con ñ y €";
        let pdf = run(text.as_bytes(), SourceKind::Text, Target::Pdf).unwrap();
        assert_eq!(pdf[0].ext, "pdf");
        assert_eq!(
            pdfread::extract_pages(&pdf[0].data).unwrap()[0].split_whitespace().collect::<Vec<_>>(),
            text.split_whitespace().collect::<Vec<_>>()
        );
        let dx = run(text.as_bytes(), SourceKind::Text, Target::Docx).unwrap();
        let back = run(&dx[0].data, SourceKind::Docx, Target::Txt).unwrap();
        assert_eq!(String::from_utf8(back[0].data.clone()).unwrap(), text);
        let docx_pdf = run(&dx[0].data, SourceKind::Docx, Target::Pdf).unwrap();
        assert_eq!(os::pdf_page_count(&docx_pdf[0].data).unwrap(), 1);
    }

    #[test]
    fn text_encodings() {
        assert_eq!(decode_text(b"\xEF\xBB\xBFhola").unwrap(), "hola");
        assert_eq!(decode_text("ñandú".as_bytes()).unwrap(), "ñandú");
        assert_eq!(decode_text(b"caf\xE9 \x80").unwrap(), "café €", "Windows-1252 fallback");
        let utf16: Vec<u8> =
            [0xFF, 0xFE].into_iter().chain("hi ñ".encode_utf16().flat_map(|u| u.to_le_bytes())).collect();
        assert_eq!(decode_text(&utf16).unwrap(), "hi ñ");
        let be: Vec<u8> = [0xFE, 0xFF].into_iter().chain("ok".encode_utf16().flat_map(|u| u.to_be_bytes())).collect();
        assert_eq!(decode_text(&be).unwrap(), "ok");
        assert!(matches!(decode_text(&vec![b'a'; 40 * 1024 * 1024]), Err(Error::TooLarge { .. })));
    }

    #[test]
    fn impossible_pairs_are_unsupported_errors() {
        for (kind, to) in [
            (SourceKind::Text, Target::Image(Fmt::Png)),
            (SourceKind::Pdf, Target::Pdf),
            (SourceKind::Docx, Target::Image(Fmt::Png)),
            (SourceKind::Text, Target::Txt),
        ] {
            assert!(matches!(run(b"x", kind, to), Err(Error::Unsupported(_))), "{kind:?} -> {to:?}");
        }
    }

    #[test]
    fn every_menu_entry_is_wired() {
        let samples: Vec<(SourceKind, Vec<u8>)> = vec![
            (SourceKind::Pdf, pdfwrite::text_to_pdf("menu test")),
            (img_kind(Fmt::Jpg), jpeg(&gradient(32, 24))),
            (img_kind(Fmt::Png), png(&DynamicImage::ImageRgba8(rgba(32, 24)))),
            (SourceKind::Docx, docx::text_to_docx("menu test")),
            (SourceKind::Text, b"menu test".to_vec()),
        ];
        for (kind, bytes) in samples {
            for t in targets_for(kind) {
                match run(&bytes, kind, t) {
                    Ok(out) => assert!(!out.is_empty() && !out[0].data.is_empty(), "{kind:?} -> {t:?}"),
                    Err(Error::Unavailable { .. }) => {
                        assert!(matches!(t, Target::Image(Fmt::Heic | Fmt::Avif)), "{kind:?} -> {t:?}")
                    }
                    Err(e) => panic!("{kind:?} -> {t:?}: {e}"),
                }
            }
        }
    }

    #[test]
    fn availability_is_reported_for_os_codecs_only() {
        assert!(target_availability(Target::Pdf).is_available());
        assert!(target_availability(Target::Image(Fmt::Png)).is_available());
        assert!(source_availability(SourceKind::Pdf).is_available());
        let heic = target_availability(Target::Image(Fmt::Heic));
        let out = run(&jpeg(&gradient(16, 16)), img_kind(Fmt::Jpg), Target::Image(Fmt::Heic));
        assert_eq!(
            heic.is_available(),
            out.is_ok(),
            "the probe must agree with reality: {heic:?} / {:?}",
            out.as_ref().err()
        );
        let avif = target_availability(Target::Image(Fmt::Avif));
        let out = run(&jpeg(&gradient(16, 16)), img_kind(Fmt::Jpg), Target::Image(Fmt::Avif));
        assert_eq!(avif.is_available(), out.is_ok(), "{avif:?}");
    }

    #[test]
    fn convert_file_names_outputs_next_to_the_source_and_never_overwrites() {
        let dir = tmp("files");
        let src = dir.join("photo.jpg");
        std::fs::write(&src, jpeg(&gradient(40, 30))).unwrap();
        let a = convert_file(&src, Target::Image(Fmt::Png), &Default::default()).unwrap();
        let b = convert_file(&src, Target::Image(Fmt::Png), &Default::default()).unwrap();
        assert_eq!(a, vec![dir.join("photo.png")]);
        assert_eq!(b, vec![dir.join("photo (1).png")]);
        let pdf = dir.join("doc.pdf");
        std::fs::write(&pdf, long_text_pdf()).unwrap();
        let pages = convert_file(&pdf, Target::Image(Fmt::Png), &Default::default()).unwrap();
        assert_eq!(pages, vec![dir.join("doc-p1.png"), dir.join("doc-p2.png"), dir.join("doc-p3.png")]);
        assert_eq!(detect(&pages[0]), Some(img_kind(Fmt::Png)));
        let leftovers = std::fs::read_dir(&dir)
            .unwrap()
            .filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn failures_leave_nothing_behind_and_are_typed() {
        let dir = tmp("fail");
        let bad = dir.join("broken.png");
        std::fs::write(&bad, b"\x89PNG\r\n\x1a\n garbage").unwrap();
        assert!(convert_file(&bad, Target::Image(Fmt::Jpg), &Default::default()).is_err());
        let unknown = dir.join("mystery.xyz");
        std::fs::write(&unknown, b"????").unwrap();
        assert!(matches!(convert_file(&unknown, Target::Pdf, &Default::default()), Err(Error::Unsupported(_))));
        assert!(matches!(convert_file(&dir.join("absent.png"), Target::Pdf, &Default::default()), Err(Error::Io(_))));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 2, "only the two inputs remain");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn scanned_text_is_read_back_with_windows_ocr_when_available() {
        let source = pdfwrite::text_to_pdf("Hello world\nRadial converts documents");
        let page = os::render_pdf_pages(&source, &[0], 200, 50_000_000).unwrap().remove(0);
        let scan = pdfwrite::images_to_pdf_at(
            &[pdfwrite::PdfImage::from_image(&DynamicImage::ImageRgba8(page), None).unwrap()],
            200.0,
        );
        assert!(pdfread::extract_pages(&scan).unwrap()[0].is_empty(), "the scan has no text layer");
        match run(&scan, SourceKind::Pdf, Target::Txt) {
            Ok(out) => {
                let text = String::from_utf8(out[0].data.clone()).unwrap().to_lowercase();
                assert!(text.contains("hello") && text.contains("radial"), "{text:?}");
            }
            Err(Error::Unavailable { feature, .. }) => assert_eq!(feature, "OCR"),
            Err(e) => panic!("{e}"),
        }
    }
}
