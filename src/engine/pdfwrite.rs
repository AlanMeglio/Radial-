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

// ---------------------------------------------------------------- tables

/// Courier advances every glyph by 0.6 em.
const COURIER_EM: f64 = 0.6;
const TABLE_MARGIN: f64 = 36.0;
/// Cells longer than this end with `...`.
const MAX_CELL_CHARS: usize = 40;
/// Characters between two columns.
const COL_GAP: usize = 2;

/// The cells of a table as WinAnsi bytes, with the width and alignment of each column.
struct Grid {
    cells: Vec<Vec<Vec<u8>>>,
    widths: Vec<usize>,
    numeric: Vec<bool>,
}

fn cell_bytes(s: &str) -> Vec<u8> {
    let mut v = Vec::new();
    for c in s.chars() {
        if v.len() > MAX_CELL_CHARS {
            break;
        }
        v.push(if c.is_control() { b' ' } else { winansi(c) });
    }
    if v.len() > MAX_CELL_CHARS {
        v.truncate(MAX_CELL_CHARS - 3);
        v.extend_from_slice(b"...");
    }
    v
}

fn is_number(cell: &[u8]) -> bool {
    let Ok(t) = std::str::from_utf8(cell) else { return false };
    let t = t.trim().trim_end_matches('%');
    t.bytes().any(|b| b.is_ascii_digit())
        && t.bytes().all(|b| b.is_ascii_digit() || matches!(b, b'.' | b',' | b'-' | b'+' | b'e' | b'E'))
        && t.replace(',', "").parse::<f64>().is_ok()
}

impl Grid {
    fn new(rows: &[Vec<String>]) -> Grid {
        let ncols = rows.iter().map(Vec::len).max().unwrap_or(0);
        let mut cells: Vec<Vec<Vec<u8>>> = rows
            .iter()
            .map(|r| {
                let mut c: Vec<Vec<u8>> = r.iter().map(|s| cell_bytes(s)).collect();
                c.resize(ncols, Vec::new());
                c
            })
            .collect();
        cells.shrink_to_fit();
        let widths = (0..ncols).map(|c| cells.iter().map(|r| r[c].len()).max().unwrap_or(0).max(1)).collect();
        let numeric = (0..ncols)
            .map(|c| {
                let body = || cells.iter().skip(1).map(|r| &r[c]).filter(|x| !x.is_empty());
                body().next().is_some() && body().all(|x| is_number(x))
            })
            .collect();
        Grid { cells, widths, numeric }
    }

    /// One row restricted to `cols`: columns padded to their width (numbers right-aligned), no trailing blanks.
    fn line(&self, row: usize, cols: &[usize]) -> Vec<u8> {
        let mut out = Vec::new();
        for (i, &c) in cols.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(&[b' '; COL_GAP]);
            }
            let cell = &self.cells[row][c];
            let pad = self.widths[c] - cell.len();
            if self.numeric[c] {
                out.extend(std::iter::repeat_n(b' ', pad));
                out.extend_from_slice(cell);
            } else {
                out.extend_from_slice(cell);
                out.extend(std::iter::repeat_n(b' ', pad));
            }
        }
        while out.last() == Some(&b' ') {
            out.pop();
        }
        out
    }

    fn span(&self, cols: &[usize]) -> usize {
        cols.iter().map(|&c| self.widths[c]).sum::<usize>() + COL_GAP * cols.len().saturating_sub(1)
    }
}

/// Characters of Courier at `size` pt that fit between the margins of a page `page_w` wide.
fn chars_per_line(page_w: f64, size: f64) -> usize {
    ((page_w - 2.0 * TABLE_MARGIN) / (size * COURIER_EM)).floor() as usize
}

/// Renders a table as a PDF: A4 pages in monospace Courier (WinAnsi; other characters print `?`).
///
/// * Landscape when the table is wider than the portrait text area at 8 pt; when it is still wider than a
///   landscape page it shrinks to 6 pt, and when even that does not fit it is split into column groups printed
///   one after another, each repeating the first column.
/// * The first row is the header: bold, repeated on every page, with a rule under it. Columns are padded to
///   their widest cell (cells over 40 characters end with `...`) and numeric columns are right-aligned.
/// * `title` (usually the sheet name) is a larger line on the first page; every page has "n / N" in the footer.
///
/// An empty table gives a valid one-page PDF. There is no page limit: a very long table makes a long PDF.
pub fn table_to_pdf(title: &str, rows: &[Vec<String>]) -> Vec<u8> {
    let grid = Grid::new(rows);
    let ncols = grid.widths.len();
    let all: Vec<usize> = (0..ncols).collect();
    let span = grid.span(&all);

    // Orientation, font size and column groups.
    let (landscape, size) = if span <= chars_per_line(PAGE_W, 8.0) {
        (false, 8.0)
    } else if span <= chars_per_line(PAGE_H, 8.0) {
        (true, 8.0)
    } else {
        (true, 6.0)
    };
    let (pw, ph) = if landscape { (PAGE_H, PAGE_W) } else { (PAGE_W, PAGE_H) };
    let cap = chars_per_line(pw, size);
    let mut groups: Vec<Vec<usize>> = Vec::new();
    if span <= cap || ncols < 2 {
        groups.push(all);
    } else {
        let mut cur = vec![0usize];
        for c in 1..ncols {
            if cur.len() > 1 && grid.span(&cur) + COL_GAP + grid.widths[c] > cap {
                groups.push(std::mem::replace(&mut cur, vec![0]));
            }
            cur.push(c);
        }
        groups.push(cur);
    }

    // Page plan: (group, first body row, end body row, carries the title).
    let title_bytes: Vec<u8> = {
        let max = chars_per_line(pw, 12.0);
        title.chars().filter(|c| !c.is_control()).take(max).map(winansi).collect()
    };
    let title_block = if title_bytes.is_empty() { 0.0 } else { 26.0 };
    let leading = size * 1.25;
    let usable = ph - 2.0 * TABLE_MARGIN;
    let body_cap = |block: f64| (((usable - block) / leading).floor() as usize).saturating_sub(1).max(1);
    let nbody = rows.len().saturating_sub(1);
    let mut plan: Vec<(usize, usize, usize, bool)> = Vec::new();
    for g in 0..groups.len() {
        let mut start = 0;
        loop {
            let first = plan.is_empty();
            let end = (start + body_cap(if first { title_block } else { 0.0 })).min(nbody);
            plan.push((g, start, end, first));
            start = end;
            if start >= nbody {
                break;
            }
        }
    }

    let mut doc = Doc::new();
    let catalog = doc.alloc();
    let tree = doc.alloc();
    let regular = doc.alloc();
    let bold = doc.alloc();
    doc.set(regular, b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier /Encoding /WinAnsiEncoding >>".to_vec());
    doc.set(bold, b"<< /Type /Font /Subtype /Type1 /BaseFont /Courier-Bold /Encoding /WinAnsiEncoding >>".to_vec());
    let total = plan.len();
    let mut kids = String::new();
    for (n, &(g, start, end, first)) in plan.iter().enumerate() {
        let page_id = doc.alloc();
        let content = doc.alloc();
        kids.push_str(&format!("{page_id} 0 R "));
        doc.set(
            page_id,
            format!(
                "<< /Type /Page /Parent {tree} 0 R /MediaBox [0 0 {pw:.2} {ph:.2}] /Resources << /Font << /F1 {regular} 0 R /F2 {bold} 0 R >> >> /Contents {content} 0 R >>"
            )
            .into_bytes(),
        );
        let mut s: Vec<u8> = Vec::new();
        let mut y = ph - TABLE_MARGIN;
        if first && !title_bytes.is_empty() {
            y -= 12.0;
            s.extend_from_slice(format!("BT /F2 12 Tf {TABLE_MARGIN} {y:.2} Td (").as_bytes());
            s.extend_from_slice(&escape(&title_bytes));
            s.extend_from_slice(b") Tj ET\n");
            y -= title_block - 12.0;
        }
        let cols = &groups[g];
        if !grid.cells.is_empty() {
            y -= size;
            s.extend_from_slice(format!("BT /F2 {size} Tf {TABLE_MARGIN} {y:.2} Td (").as_bytes());
            s.extend_from_slice(&escape(&grid.line(0, cols)));
            s.extend_from_slice(b") Tj ET\n");
            let rule_y = y - size * 0.3;
            let rule_x2 = TABLE_MARGIN + grid.span(cols) as f64 * size * COURIER_EM;
            s.extend_from_slice(
                format!("0.5 w 0.4 G {TABLE_MARGIN} {rule_y:.2} m {rule_x2:.2} {rule_y:.2} l S 0 G\n").as_bytes(),
            );
            y -= leading;
            if end > start {
                s.extend_from_slice(format!("BT /F1 {size} Tf {leading} TL {TABLE_MARGIN} {y:.2} Td\n").as_bytes());
                for r in start..end {
                    s.push(b'(');
                    s.extend_from_slice(&escape(&grid.line(r + 1, cols)));
                    s.extend_from_slice(b") Tj T*\n");
                }
                s.extend_from_slice(b"ET\n");
            }
        }
        let footer = format!("{} / {total}", n + 1);
        let fx = (pw - footer.len() as f64 * 8.0 * COURIER_EM) / 2.0;
        s.extend_from_slice(format!("BT /F1 8 Tf {fx:.2} 18 Td ({footer}) Tj ET").as_bytes());
        doc.stream(content, "", &s);
    }
    doc.set(catalog, format!("<< /Type /Catalog /Pages {tree} 0 R >>").into_bytes());
    doc.set(tree, format!("<< /Type /Pages /Kids [{}] /Count {total} >>", kids.trim_end()).into_bytes());
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

    // ------------------------------------------------------------------ tables

    fn table(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|r| r.iter().map(|c| c.to_string()).collect()).collect()
    }

    /// Page size from the first `/MediaBox`.
    fn media_box(pdf: &[u8]) -> (f64, f64) {
        let s = String::from_utf8_lossy(pdf);
        let mb = s.split("/MediaBox [0 0 ").nth(1).unwrap();
        let mut it = mb.split([' ', ']']);
        (it.next().unwrap().parse().unwrap(), it.next().unwrap().parse().unwrap())
    }

    fn pages_text(pdf: &[u8]) -> Vec<String> {
        super::super::pdfread::extract_pages(pdf).unwrap()
    }

    fn many_rows(n: usize, cols: usize) -> Vec<Vec<String>> {
        let mut rows = vec![(0..cols).map(|c| format!("Col{c:02}")).collect::<Vec<_>>()];
        for r in 0..n {
            rows.push((0..cols).map(|c| format!("r{r}c{c}")).collect());
        }
        rows
    }

    #[test]
    fn a_table_becomes_a_valid_pdf_whose_text_reads_back() {
        let rows = table(&[&["Name", "Qty", "Note"], &["Ana", "12", "first"], &["Bob", "7", "ñandú (x)"]]);
        let pdf = table_to_pdf("Ventas", &rows);
        assert!(pdf.starts_with(b"%PDF-1.") && pdf.ends_with(b"%%EOF\n"));
        assert!(has(&pdf, b"/BaseFont /Courier") && has(&pdf, b"/WinAnsiEncoding"));
        let (w, h) = media_box(&pdf);
        assert_eq!((w, h), (PAGE_W, PAGE_H), "narrow tables stay portrait A4");
        let text = pages_text(&pdf).remove(0);
        for want in ["Ventas", "Name", "Qty", "Note", "Ana", "12", "first", "Bob", "ñandú (x)", "1 / 1"] {
            assert!(text.contains(want), "{want:?} missing from {text:?}");
        }
        assert_eq!(os::pdf_page_count(&pdf).unwrap(), 1);
    }

    #[test]
    fn an_empty_table_is_still_a_valid_one_page_pdf() {
        for pdf in [table_to_pdf("", &[]), table_to_pdf("Empty", &[]), table_to_pdf("", &[Vec::new()])] {
            assert!(pdf.starts_with(b"%PDF-1.") && pdf.ends_with(b"%%EOF\n"));
            assert_eq!(os::pdf_page_count(&pdf).unwrap(), 1);
            assert!(pages_text(&pdf)[0].contains("1 / 1"));
        }
        assert!(pages_text(&table_to_pdf("Empty", &[]))[0].contains("Empty"));
    }

    #[test]
    fn page_orientation_and_font_size_follow_the_table_width() {
        // 6 columns of 5 characters: 40 characters, fits portrait at 8 pt.
        let narrow = many_rows(2, 6);
        assert_eq!(media_box(&table_to_pdf("", &narrow)), (PAGE_W, PAGE_H));
        // 10 columns of 13 characters: 148 characters, over the 109 of portrait but under the 160 of landscape.
        let wide: Vec<Vec<String>> = (0..3).map(|r| (0..10).map(|c| format!("{r}-{c}-wide-cell")).collect()).collect();
        let pdf = table_to_pdf("", &wide);
        assert_eq!(media_box(&pdf), (PAGE_H, PAGE_W), "wider than portrait: landscape");
        assert!(has(&pdf, b"/F1 8 Tf"));
        // 9 columns of 20 characters: 196, over the 160 of landscape at 8 pt, so 6 pt (213).
        let wider: Vec<Vec<String>> =
            (0..3).map(|r| (0..9).map(|c| format!("{r}-{c}-widest-cell-text")).collect()).collect();
        let pdf = table_to_pdf("", &wider);
        assert_eq!(media_box(&pdf), (PAGE_H, PAGE_W));
        assert!(has(&pdf, b"/F1 6 Tf"));
        assert_eq!(os::pdf_page_count(&pdf).unwrap(), 1, "still one column group");
    }

    #[test]
    fn the_header_repeats_on_every_page_and_pages_are_numbered() {
        let pdf = table_to_pdf("Big", &many_rows(300, 4));
        let n = os::pdf_page_count(&pdf).unwrap() as usize;
        assert!(n >= 4, "{n}");
        let pages = pages_text(&pdf);
        assert_eq!(pages.len(), n);
        for (i, p) in pages.iter().enumerate() {
            assert!(p.contains("Col00") && p.contains("Col03"), "header missing on page {}", i + 1);
            assert!(p.contains(&format!("{} / {n}", i + 1)), "page number {}: {p}", i + 1);
        }
        assert!(pages[0].contains("Big") && !pages[1].contains("Big"), "title only on the first page");
        let all = pages.join("\n");
        for r in [0usize, 150, 299] {
            assert!(all.contains(&format!("r{r}c3")), "row {r} lost");
        }
        assert_eq!(all.matches("r42c0").count(), 1, "no row printed twice");
    }

    #[test]
    fn long_cells_end_with_an_ellipsis_at_40_characters() {
        let long = "abcdefghij".repeat(6);
        let g = Grid::new(&table(&[&["H"], &[&long], &["short"]]));
        assert_eq!(g.cells[1][0], format!("{}...", &long[..37]).into_bytes());
        assert_eq!(g.widths, [40]);
        let exactly = "x".repeat(40);
        assert_eq!(Grid::new(&table(&[&["H"], &[&exactly]])).cells[1][0], exactly.into_bytes());
        let pdf = table_to_pdf("", &table(&[&["H"], &[&long]]));
        let text = &pages_text(&pdf)[0];
        assert!(text.contains(&format!("{}...", &long[..37])) && !text.contains(&long[..38]), "{text}");
    }

    #[test]
    fn numeric_columns_are_right_aligned_and_text_columns_left_aligned() {
        let g = Grid::new(&table(&[
            &["Item", "Qty", "Price", "Note"],
            &["Apple", "5", "1.50", "7 days"],
            &["Fig", "120", "-0.25", ""],
            &["Kiwi", "", "10%", "x"],
        ]));
        assert_eq!(g.numeric, [false, true, true, false]);
        let cols = [0usize, 1, 2, 3];
        assert_eq!(String::from_utf8(g.line(0, &cols)).unwrap(), "Item   Qty  Price  Note");
        assert_eq!(String::from_utf8(g.line(1, &cols)).unwrap(), "Apple    5   1.50  7 days");
        assert_eq!(String::from_utf8(g.line(2, &cols)).unwrap(), "Fig    120  -0.25");
        assert_eq!(String::from_utf8(g.line(3, &[0, 2])).unwrap(), "Kiwi     10%");
        // A column with any non-number stays text.
        assert!(!Grid::new(&table(&[&["A"], &["1"], &["n/a"]])).numeric[0]);
        assert!(!Grid::new(&table(&[&["A"], &["nan"]])).numeric[0], "words that parse as floats are not numbers");
    }

    #[test]
    fn characters_outside_winansi_print_as_question_marks_and_controls_as_spaces() {
        let g = Grid::new(&table(&[&["日本 ñ€"], &["a\tb\nc"]]));
        assert_eq!(g.cells[0][0], vec![b'?', b'?', b' ', 0xF1, 0x80]);
        assert_eq!(g.cells[1][0], b"a b c");
        let pdf = table_to_pdf("(par)\\en", &table(&[&["f(x)"], &["a\\b"]]));
        assert!(has(&pdf, b"f\\(x\\)") && has(&pdf, b"a\\\\b") && has(&pdf, b"\\(par\\)\\\\en"));
        assert_eq!(os::pdf_page_count(&pdf).unwrap(), 1);
    }

    #[test]
    fn very_wide_tables_split_into_column_groups_that_repeat_the_first_column() {
        let rows: Vec<Vec<String>> = (0..3)
            .map(|r| {
                (0..30)
                    .map(|c| if r == 0 { format!("H{c:02}") } else { format!("{c:02}-{}", "x".repeat(17)) })
                    .collect()
            })
            .collect();
        let pdf = table_to_pdf("", &rows);
        let n = os::pdf_page_count(&pdf).unwrap() as usize;
        assert!(n >= 3, "{n}");
        let pages = pages_text(&pdf);
        for (i, p) in pages.iter().enumerate() {
            assert!(p.contains("H00"), "first column missing on group {}", i + 1);
            assert!(p.contains(&format!("{} / {n}", i + 1)));
        }
        let all = pages.join("\n");
        for c in 0..30 {
            assert!(all.contains(&format!("H{c:02}")), "column {c} lost");
        }
        assert_eq!(all.matches("H17").count(), 1, "a column is printed in exactly one group");
    }

    /// Writes sample table PDFs for opening in other viewers: `RADIAL_PDF_DIR=<dir> cargo test table_samples -- --ignored`.
    #[test]
    #[ignore]
    fn table_samples_for_other_viewers() {
        let Ok(dir) = std::env::var("RADIAL_PDF_DIR") else { return };
        let small = table(&[
            &["Name", "Qty", "Price", "Note"],
            &["Ana", "3", "1.50", "ñandú (x)"],
            &["Bob", "1000", "-2.25", "日本"],
        ]);
        std::fs::write(format!("{dir}/small.pdf"), table_to_pdf("Ventas", &small)).unwrap();
        std::fs::write(format!("{dir}/long.pdf"), table_to_pdf("Big", &many_rows(300, 4))).unwrap();
        let wide: Vec<Vec<String>> = (0..40)
            .map(|r| {
                (0..30)
                    .map(|c| if r == 0 { format!("H{c:02}") } else { format!("{c:02}-{}", "x".repeat(17)) })
                    .collect()
            })
            .collect();
        std::fs::write(format!("{dir}/wide.pdf"), table_to_pdf("Wide", &wide)).unwrap();
        std::fs::write(format!("{dir}/empty.pdf"), table_to_pdf("", &[])).unwrap();
    }

    #[test]
    fn ragged_rows_are_padded_and_the_table_renders_ink_on_windows() {
        let rows = table(&[&["a", "b", "c"], &["1"], &["1", "2", "3", "4"]]);
        let pdf = table_to_pdf("Ragged", &rows);
        let text = pages_text(&pdf).remove(0);
        assert!(text.contains('4') && text.contains("Ragged"));
        if let Ok(mut pages) = os::render_pdf_pages(&pdf, &[0], 96, 50_000_000) {
            let page = pages.remove(0);
            assert!(page.pixels().any(|p| p.0[0] < 100), "table text should leave dark pixels");
            // The grid rule under the header is a run of dark pixels wider than any glyph.
            let longest = (0..page.height())
                .map(|y| {
                    let (mut best, mut run) = (0, 0);
                    for x in 0..page.width() {
                        if page.get_pixel(x, y).0[0] < 160 {
                            run += 1;
                            best = best.max(run);
                        } else {
                            run = 0;
                        }
                    }
                    best
                })
                .max()
                .unwrap();
            assert!(longest > 40, "header rule missing ({longest})");
        }
    }
}
