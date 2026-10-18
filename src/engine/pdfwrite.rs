//! Minimal PDF writer: one raster image per page, or paginated text in the built-in Helvetica.

use super::error::Result;
use super::jpeg;
use image::DynamicImage;
use std::io::Write;

/// A page image ready to embed.
pub enum PdfImage {
    /// The original JPEG bytes, embedded untouched (DCTDecode).
    Jpeg { width: u32, height: u32, components: u8, data: Vec<u8> },
    /// Deflated 8-bit RGB with an optional deflated 8-bit alpha channel (soft mask).
    Raw { width: u32, height: u32, rgb: Vec<u8>, alpha: Option<Vec<u8>> },
}

fn deflate(data: &[u8]) -> Vec<u8> {
    let mut enc = flate2::write::ZlibEncoder::new(Vec::with_capacity(data.len() / 2), flate2::Compression::new(3));
    // Writing into a Vec cannot fail.
    let _ = enc.write_all(data);
    enc.finish().unwrap_or_default()
}

impl PdfImage {
    /// Builds a page image. `original_jpeg` is passed only when it is a baseline-compatible JPEG whose pixels
    /// are exactly `img` (same orientation), so it can be embedded without re-encoding.
    pub fn from_image(img: &DynamicImage, original_jpeg: Option<&[u8]>) -> Result<PdfImage> {
        let (width, height) = (img.width(), img.height());
        if let Some(data) = original_jpeg {
            if let Some(f) = jpeg::frame_info(data) {
                if f.precision == 8 && matches!(f.components, 1 | 3) && (f.width, f.height) == (width, height) {
                    return Ok(PdfImage::Jpeg { width, height, components: f.components, data: data.to_vec() });
                }
            }
        }
        let rgba = img.to_rgba8();
        let n = (width as usize) * (height as usize);
        let mut rgb = Vec::with_capacity(n * 3);
        let mut alpha = Vec::with_capacity(if img.color().has_alpha() { n } else { 0 });
        let mut any_transparent = false;
        for p in rgba.pixels() {
            rgb.extend_from_slice(&p.0[..3]);
            if img.color().has_alpha() {
                alpha.push(p[3]);
                any_transparent |= p[3] != 255;
            }
        }
        Ok(PdfImage::Raw { width, height, rgb: deflate(&rgb), alpha: any_transparent.then(|| deflate(&alpha)) })
    }

    fn size(&self) -> (u32, u32) {
        match self {
            PdfImage::Jpeg { width, height, .. } | PdfImage::Raw { width, height, .. } => (*width, *height),
        }
    }
}

/// Object-table based file builder.
struct Doc {
    objs: Vec<Vec<u8>>,
}

impl Doc {
    fn new() -> Self {
        Doc { objs: Vec::new() }
    }
    fn alloc(&mut self) -> usize {
        self.objs.push(Vec::new());
        self.objs.len()
    }
    fn set(&mut self, id: usize, body: Vec<u8>) {
        self.objs[id - 1] = body;
    }
    fn stream(&mut self, id: usize, dict: &str, data: &[u8]) {
        let mut b = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        b.extend_from_slice(data);
        b.extend_from_slice(b"\nendstream");
        self.set(id, b);
    }
    fn finish(self, root: usize) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n%\xE2\xE3\xCF\xD3\n".to_vec();
        let mut offsets = Vec::with_capacity(self.objs.len());
        for (i, body) in self.objs.iter().enumerate() {
            offsets.push(out.len());
            out.extend_from_slice(format!("{} 0 obj\n", i + 1).as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        let xref = out.len();
        out.extend_from_slice(format!("xref\n0 {}\n0000000000 65535 f \n", self.objs.len() + 1).as_bytes());
        for o in offsets {
            out.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        out.extend_from_slice(
            format!("trailer\n<< /Size {} /Root {root} 0 R >>\nstartxref\n{xref}\n%%EOF\n", self.objs.len() + 1)
                .as_bytes(),
        );
        out
    }
}

/// PDF page limit is 14400 pt per side; pixels map to points at `dpi`.
fn page_size(w: u32, h: u32, dpi: f64) -> (f64, f64) {
    let (mut pw, mut ph) = (w as f64 * 72.0 / dpi, h as f64 * 72.0 / dpi);
    let big = pw.max(ph);
    if big > 14_400.0 {
        let k = 14_400.0 / big;
        pw *= k;
        ph *= k;
    }
    (pw.max(1.0), ph.max(1.0))
}

/// One page per image, sized as if the pixels were printed at 96 dpi.
pub fn images_to_pdf(pages: &[PdfImage]) -> Vec<u8> {
    images_to_pdf_at(pages, 96.0)
}

/// Like `images_to_pdf`, with pixels mapped to page points at `dpi` (use the render dpi to keep the page size).
pub fn images_to_pdf_at(pages: &[PdfImage], dpi: f64) -> Vec<u8> {
    let dpi = if dpi.is_finite() && dpi >= 1.0 { dpi } else { 96.0 };
    let mut doc = Doc::new();
    let catalog = doc.alloc();
    let tree = doc.alloc();
    let mut kids = String::new();
    for page in pages {
        let (pw, ph) = page_size(page.size().0, page.size().1, dpi);
        let page_id = doc.alloc();
        let content = doc.alloc();
        let image = doc.alloc();
        kids.push_str(&format!("{page_id} 0 R "));
        doc.set(
            page_id,
            format!(
                "<< /Type /Page /Parent {tree} 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] /Resources << /XObject << /Im0 {image} 0 R >> >> /Contents {content} 0 R >>"
            )
            .into_bytes(),
        );
        doc.stream(content, "", format!("q {pw:.2} 0 0 {ph:.2} 0 0 cm /Im0 Do Q").as_bytes());
        match page {
            PdfImage::Jpeg { width, height, components, data } => {
                let cs = if *components == 1 { "/DeviceGray" } else { "/DeviceRGB" };
                doc.stream(
                    image,
                    &format!("/Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace {cs} /BitsPerComponent 8 /Filter /DCTDecode"),
                    data,
                );
            }
            PdfImage::Raw { width, height, rgb, alpha } => {
                let mask = alpha.as_ref().map(|a| {
                    let id = doc.alloc();
                    doc.stream(
                        id,
                        &format!("/Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceGray /BitsPerComponent 8 /Filter /FlateDecode"),
                        a,
                    );
                    id
                });
                let smask = mask.map(|id| format!(" /SMask {id} 0 R")).unwrap_or_default();
                doc.stream(
                    image,
                    &format!("/Type /XObject /Subtype /Image /Width {width} /Height {height} /ColorSpace /DeviceRGB /BitsPerComponent 8 /Filter /FlateDecode{smask}"),
                    rgb,
                );
            }
        }
    }
    doc.set(catalog, format!("<< /Type /Catalog /Pages {tree} 0 R >>").into_bytes());
    doc.set(tree, format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.trim_end(), pages.len()).into_bytes());
    doc.finish(catalog)
}

const PAGE_W: f64 = 595.28;
const PAGE_H: f64 = 841.89;
const MARGIN: f64 = 56.0;
const FONT: f64 = 11.0;
const LEADING: f64 = 14.0;

/// Helvetica advance widths (1/1000 em) for ASCII 32..=126.
const HELV: [u16; 95] = [
    278, 278, 355, 556, 556, 889, 667, 191, 333, 333, 389, 584, 278, 333, 278, 278, 556, 556, 556, 556, 556, 556, 556,
    556, 556, 556, 278, 278, 584, 584, 584, 556, 1015, 667, 667, 722, 722, 667, 611, 778, 722, 278, 500, 667, 556, 833,
    722, 778, 667, 778, 722, 667, 611, 722, 667, 944, 667, 667, 611, 278, 278, 278, 469, 556, 333, 556, 556, 500, 556,
    556, 278, 556, 556, 222, 222, 500, 222, 833, 556, 556, 556, 556, 333, 500, 278, 556, 500, 722, 500, 500, 500, 334,
    260, 334, 584,
];

fn winansi(c: char) -> u8 {
    match c {
        ' '..='~' | '\u{A0}'..='\u{FF}' => c as u32 as u8,
        '€' => 0x80,
        '…' => 0x85,
        '‘' => 0x91,
        '’' => 0x92,
        '“' => 0x93,
        '”' => 0x94,
        '•' => 0x95,
        '–' => 0x96,
        '—' => 0x97,
        '™' => 0x99,
        _ => b'?',
    }
}

fn glyph_width(b: u8) -> f64 {
    let units = if (32..=126).contains(&b) { HELV[(b - 32) as usize] } else { 556 };
    units as f64 * FONT / 1000.0
}

/// Wraps text into pages of WinAnsi-encoded lines.
fn layout(text: &str) -> Vec<Vec<Vec<u8>>> {
    let max_w = PAGE_W - 2.0 * MARGIN;
    let per_page = ((PAGE_H - 2.0 * MARGIN) / LEADING) as usize;
    let mut lines: Vec<Vec<u8>> = Vec::new();
    let normalized = text.replace("\r\n", "\n").replace('\r', "\n");
    for para in normalized.split('\n') {
        let bytes: Vec<u8> = para
            .chars()
            .flat_map(|c| {
                if c == '\t' {
                    vec![b' '; 4]
                } else if c.is_control() {
                    vec![]
                } else {
                    vec![winansi(c)]
                }
            })
            .collect();
        let mut line: Vec<u8> = Vec::new();
        let mut w = 0.0;
        let mut wrapped = false;
        for tok in bytes.split_inclusive(|&b| b == b' ') {
            // `tok` is a word with its trailing space (if any).
            let word_w: f64 = tok.iter().map(|&b| glyph_width(b)).sum();
            let visible_w: f64 = tok.iter().take_while(|&&b| b != b' ').map(|&b| glyph_width(b)).sum();
            if w + visible_w > max_w && !line.is_empty() {
                while line.last() == Some(&b' ') {
                    line.pop();
                }
                lines.push(std::mem::take(&mut line));
                w = 0.0;
                wrapped = true;
            }
            if wrapped && line.is_empty() && tok.iter().all(|&b| b == b' ') {
                continue; // no leading blanks on a wrapped continuation
            }
            if visible_w > max_w {
                // Unbreakable word: split by characters.
                for &b in tok {
                    let gw = glyph_width(b);
                    if w + gw > max_w {
                        lines.push(std::mem::take(&mut line));
                        w = 0.0;
                    }
                    line.push(b);
                    w += gw;
                }
            } else {
                line.extend_from_slice(tok);
                w += word_w;
            }
        }
        while line.last() == Some(&b' ') && wrapped {
            line.pop();
        }
        lines.push(line);
    }
    lines.chunks(per_page).map(<[Vec<u8>]>::to_vec).collect()
}

fn escape(line: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(line.len() + 2);
    for &b in line {
        if matches!(b, b'(' | b')' | b'\\') {
            out.push(b'\\');
        }
        out.push(b);
    }
    out
}

/// Lays out `text` (A4, 11 pt Helvetica) and returns the PDF.
///
/// The base-14 font only covers Windows-1252 (Western European) text; other scripts print as `?`.
pub fn text_to_pdf(text: &str) -> Vec<u8> {
    let mut pages = layout(text);
    if pages.is_empty() {
        pages.push(Vec::new());
    }
    let mut doc = Doc::new();
    let catalog = doc.alloc();
    let tree = doc.alloc();
    let font = doc.alloc();
    doc.set(font, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec());
    let mut kids = String::new();
    for lines in &pages {
        let page_id = doc.alloc();
        let content = doc.alloc();
        kids.push_str(&format!("{page_id} 0 R "));
        doc.set(
            page_id,
            format!(
                "<< /Type /Page /Parent {tree} 0 R /MediaBox [0 0 {PAGE_W} {PAGE_H}] /Resources << /Font << /F1 {font} 0 R >> >> /Contents {content} 0 R >>"
            )
            .into_bytes(),
        );
        let mut s = format!("BT /F1 {FONT} Tf {LEADING} TL {MARGIN} {:.2} Td\n", PAGE_H - MARGIN - FONT).into_bytes();
        for l in lines {
            s.push(b'(');
            s.extend_from_slice(&escape(l));
            s.extend_from_slice(b") Tj T*\n");
        }
        s.extend_from_slice(b"ET");
        doc.stream(content, "", &s);
    }
    doc.set(catalog, format!("<< /Type /Catalog /Pages {tree} 0 R >>").into_bytes());
    doc.set(tree, format!("<< /Type /Pages /Kids [{}] /Count {} >>", kids.trim_end(), pages.len()).into_bytes());
    doc.finish(catalog)
}

#[cfg(test)]
mod tests {
    use super::super::os;
    use super::super::raster::testutil::*;
    use super::*;
    use image::{Rgb, RgbImage};

    fn solid(w: u32, h: u32, c: [u8; 3]) -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_pixel(w, h, Rgb(c)))
    }

    fn render_first(pdf: &[u8]) -> image::RgbaImage {
        os::render_pdf_pages(pdf, &[0], 96, 50_000_000).unwrap().remove(0)
    }

    fn has(hay: &[u8], needle: &[u8]) -> bool {
        hay.windows(needle.len()).any(|w| w == needle)
    }

    #[test]
    fn raw_image_page_renders_with_the_right_aspect_and_colour() {
        let pdf = images_to_pdf(&[PdfImage::from_image(&solid(200, 100, [220, 30, 30]), None).unwrap()]);
        assert!(pdf.starts_with(b"%PDF-1.") && pdf.ends_with(b"%%EOF\n"));
        assert_eq!(os::pdf_page_count(&pdf).unwrap(), 1);
        let page = render_first(&pdf);
        let ratio = page.width() as f32 / page.height() as f32;
        assert!((ratio - 2.0).abs() < 0.05, "{ratio}");
        let c = page.get_pixel(page.width() / 2, page.height() / 2).0;
        assert!(c[0] > 200 && c[1] < 60 && c[2] < 60, "{c:?}");
    }

    #[test]
    fn jpeg_is_embedded_verbatim_and_renders() {
        let src = RgbImage::from_pixel(64, 48, Rgb([20, 40, 220]));
        let jpg = jpeg(&src);
        let page_img = PdfImage::from_image(&DynamicImage::ImageRgb8(src), Some(&jpg)).unwrap();
        assert!(matches!(page_img, PdfImage::Jpeg { components: 3, .. }));
        let pdf = images_to_pdf(&[page_img]);
        assert!(has(&pdf, &jpg), "original JPEG bytes must be inside the PDF");
        assert!(has(&pdf, b"/DCTDecode"));
        let page = render_first(&pdf);
        let c = page.get_pixel(page.width() / 2, page.height() / 2).0;
        assert!(c[2] > 180 && c[0] < 80, "{c:?}");
    }

    #[test]
    fn cmyk_or_garbage_jpeg_falls_back_to_pixels() {
        let img = solid(8, 8, [1, 2, 3]);
        let p = PdfImage::from_image(&img, Some(b"\xFF\xD8 not really")).unwrap();
        assert!(matches!(p, PdfImage::Raw { .. }));
    }

    #[test]
    fn alpha_becomes_a_soft_mask() {
        let rgba = DynamicImage::ImageRgba8(rgba(16, 16));
        let pdf = images_to_pdf(&[PdfImage::from_image(&rgba, None).unwrap()]);
        assert!(has(&pdf, b"/SMask"));
        assert_eq!(os::pdf_page_count(&pdf).unwrap(), 1);
        let opaque = images_to_pdf(&[PdfImage::from_image(&solid(4, 4, [0; 3]), None).unwrap()]);
        assert!(!has(&opaque, b"/SMask"));
    }

    #[test]
    fn several_images_make_several_pages() {
        let imgs: Vec<_> = (0..3).map(|i| PdfImage::from_image(&solid(30 + i, 20, [9; 3]), None).unwrap()).collect();
        assert_eq!(os::pdf_page_count(&images_to_pdf(&imgs)).unwrap(), 3);
    }

    #[test]
    fn huge_pixel_dimensions_still_give_a_valid_page_size() {
        let pdf = images_to_pdf(&[PdfImage::from_image(&solid(40_000, 2, [1; 3]), None).unwrap()]);
        let s = String::from_utf8_lossy(&pdf);
        let mb = s.split("/MediaBox [0 0 ").nth(1).unwrap();
        let w: f64 = mb.split(' ').next().unwrap().parse().unwrap();
        assert!(w <= 14_400.0, "{w}");
    }

    #[test]
    fn text_pdf_renders_ink_and_paginates() {
        let short = text_to_pdf("Hola mundo\nsegunda línea");
        assert_eq!(os::pdf_page_count(&short).unwrap(), 1);
        let page = render_first(&short);
        assert!(page.pixels().any(|p| p.0[0] < 100), "text should leave dark pixels");
        let long: String = (0..200).map(|i| format!("line number {i}\n")).collect();
        assert!(os::pdf_page_count(&text_to_pdf(&long)).unwrap() >= 3);
        assert_eq!(os::pdf_page_count(&text_to_pdf("")).unwrap(), 1);
    }

    #[test]
    fn layout_wraps_within_the_margins_and_keeps_words_whole() {
        let words = "palabra ".repeat(80);
        let pages = layout(&words);
        for line in pages.iter().flatten() {
            assert!(line.len() < 100, "{}", line.len());
            assert!(!line.starts_with(b" "));
        }
        let long_word = "x".repeat(500);
        assert!(layout(&long_word)[0].len() > 1, "an unbreakable word is split by characters");
    }

    #[test]
    fn layout_maps_latin1_and_replaces_unrepresentable_characters() {
        let pages = layout("áéñ€ 漢");
        assert_eq!(pages[0][0], vec![0xE1, 0xE9, 0xF1, 0x80, b' ', b'?']);
    }

    #[test]
    fn special_characters_are_escaped_in_the_content_stream() {
        let pdf = text_to_pdf("f(x) = a\\b");
        assert!(has(&pdf, b"f\\(x\\) = a\\\\b"));
        assert_eq!(os::pdf_page_count(&pdf).unwrap(), 1);
    }
}
