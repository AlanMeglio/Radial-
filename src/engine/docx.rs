//! DOCX (a zip of WordprocessingML) read as plain text and written from text or images.

use super::error::{Error, Result};
use super::zip::{ZipArchive, ZipWriter};

/// A picture to embed; `ext` is `png` or `jpeg` (Word reads both everywhere).
pub struct DocxImage {
    pub ext: &'static str,
    pub data: Vec<u8>,
    pub width_px: u32,
    pub height_px: u32,
}

pub enum Block {
    Paragraph(String),
    Image(DocxImage),
    PageBreak,
}

/// Largest `word/document.xml` we are willing to inflate.
const MAX_XML: usize = 64 * 1024 * 1024;

// A4 with 2 cm margins, in EMU (914400 per inch).
const MAX_W_EMU: u64 = 6_119_000;
const MAX_H_EMU: u64 = 9_250_000;
const EMU_PER_PX: u64 = 9525; // 96 dpi

fn escape_into(out: &mut String, text: &str) {
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            // XML 1.0 forbids most control characters even when escaped.
            c if (c as u32) < 0x20 && c != '\t' => {}
            c if matches!(c as u32, 0xFFFE | 0xFFFF) => {}
            c => out.push(c),
        }
    }
}

fn paragraph_xml(out: &mut String, text: &str) {
    if text.is_empty() {
        out.push_str("<w:p/>");
        return;
    }
    out.push_str("<w:p><w:r>");
    // Tabs become <w:tab/> so Word shows them as tabs; the text between stays preserved.
    for (i, part) in text.split('\t').enumerate() {
        if i > 0 {
            out.push_str("<w:tab/>");
        }
        if !part.is_empty() {
            out.push_str("<w:t xml:space=\"preserve\">");
            escape_into(out, part);
            out.push_str("</w:t>");
        }
    }
    out.push_str("</w:r></w:p>");
}

fn image_xml(out: &mut String, n: usize, img: &DocxImage) {
    let (w, h) = (img.width_px.max(1) as u64 * EMU_PER_PX, img.height_px.max(1) as u64 * EMU_PER_PX);
    let k = (MAX_W_EMU as f64 / w as f64).min(MAX_H_EMU as f64 / h as f64).min(1.0);
    let (cx, cy) = (((w as f64) * k) as u64, ((h as f64) * k) as u64);
    out.push_str(&format!(
        "<w:p><w:r><w:drawing><wp:inline distT=\"0\" distB=\"0\" distL=\"0\" distR=\"0\"><wp:extent cx=\"{cx}\" cy=\"{cy}\"/>\
<wp:docPr id=\"{n}\" name=\"Picture {n}\"/><a:graphic><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/picture\">\
<pic:pic><pic:nvPicPr><pic:cNvPr id=\"{n}\" name=\"image{n}.{ext}\"/><pic:cNvPicPr/></pic:nvPicPr>\
<pic:blipFill><a:blip r:embed=\"rId{n}\"/><a:stretch><a:fillRect/></a:stretch></pic:blipFill>\
<pic:spPr><a:xfrm><a:off x=\"0\" y=\"0\"/><a:ext cx=\"{cx}\" cy=\"{cy}\"/></a:xfrm><a:prstGeom prst=\"rect\"><a:avLst/></a:prstGeom></pic:spPr>\
</pic:pic></a:graphicData></a:graphic></wp:inline></w:drawing></w:r></w:p>",
        ext = img.ext
    ));
}

pub fn build(blocks: &[Block]) -> Vec<u8> {
    let mut body = String::new();
    let mut rels = String::new();
    let mut media: Vec<(String, &[u8])> = Vec::new();
    let mut exts: Vec<&str> = Vec::new();
    for b in blocks {
        match b {
            Block::Paragraph(t) => paragraph_xml(&mut body, t),
            Block::PageBreak => body.push_str("<w:p><w:r><w:br w:type=\"page\"/></w:r></w:p>"),
            Block::Image(img) => {
                let n = media.len() + 1;
                image_xml(&mut body, n, img);
                rels.push_str(&format!(
                    "<Relationship Id=\"rId{n}\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/image\" Target=\"media/image{n}.{}\"/>",
                    img.ext
                ));
                media.push((format!("word/media/image{n}.{}", img.ext), &img.data));
                if !exts.contains(&img.ext) {
                    exts.push(img.ext);
                }
            }
        }
    }
    let document = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<w:document xmlns:w=\"http://schemas.openxmlformats.org/wordprocessingml/2006/main\" \
xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" \
xmlns:wp=\"http://schemas.openxmlformats.org/drawingml/2006/wordprocessingDrawing\" \
xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\" \
xmlns:pic=\"http://schemas.openxmlformats.org/drawingml/2006/picture\"><w:body>{body}\
<w:sectPr><w:pgSz w:w=\"11906\" w:h=\"16838\"/><w:pgMar w:top=\"1134\" w:right=\"1134\" w:bottom=\"1134\" w:left=\"1134\" w:header=\"708\" w:footer=\"708\" w:gutter=\"0\"/></w:sectPr>\
</w:body></w:document>"
    );
    let defaults: String =
        exts.iter().map(|e| format!("<Default Extension=\"{e}\" ContentType=\"image/{e}\"/>")).collect();
    let content_types = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\">\
<Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/>\
<Default Extension=\"xml\" ContentType=\"application/xml\"/>{defaults}\
<Override PartName=\"/word/document.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.wordprocessingml.document.main+xml\"/></Types>"
    );
    let root_rels = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">\
<Relationship Id=\"rId1\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/officeDocument\" Target=\"word/document.xml\"/></Relationships>";
    let doc_rels = format!(
        "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\
<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{rels}</Relationships>"
    );

    let mut zip = ZipWriter::new();
    zip.add("[Content_Types].xml", content_types.as_bytes(), true);
    zip.add("_rels/.rels", root_rels.as_bytes(), true);
    zip.add("word/document.xml", document.as_bytes(), true);
    zip.add("word/_rels/document.xml.rels", doc_rels.as_bytes(), true);
    for (name, data) in media {
        zip.add(&name, data, false); // PNG and JPEG are already compressed
    }
    zip.finish()
}

/// One paragraph per line of `text`.
pub fn text_to_docx(text: &str) -> Vec<u8> {
    let blocks: Vec<Block> =
        text.replace("\r\n", "\n").replace('\r', "\n").split('\n').map(|l| Block::Paragraph(l.to_string())).collect();
    build(&blocks)
}

pub fn docx_to_text(bytes: &[u8]) -> Result<String> {
    let zip = ZipArchive::new(bytes).map_err(|_| Error::Corrupt("not a DOCX file".into()))?;
    let xml = zip
        .read("word/document.xml", MAX_XML)?
        .ok_or_else(|| Error::Corrupt("not a Word document (word/document.xml is missing)".into()))?;
    Ok(extract_text(&String::from_utf8_lossy(&xml)))
}

fn decode_entity(ent: &str) -> Option<char> {
    match ent {
        "amp" => Some('&'),
        "lt" => Some('<'),
        "gt" => Some('>'),
        "quot" => Some('"'),
        "apos" => Some('\''),
        _ => {
            let num = ent.strip_prefix('#')?;
            let code = match num.strip_prefix(['x', 'X']) {
                Some(h) => u32::from_str_radix(h, 16).ok()?,
                None => num.parse().ok()?,
            };
            char::from_u32(code)
        }
    }
}

fn push_unescaped(out: &mut String, raw: &str) {
    let mut rest = raw;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        match rest.find(';').and_then(|j| decode_entity(&rest[1..j]).map(|c| (j, c))) {
            Some((j, c)) if j <= 10 => {
                out.push(c);
                rest = &rest[j + 1..];
            }
            _ => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
}

/// Plain text of WordprocessingML: paragraphs joined by `\n`, table cells by `\t`.
fn extract_text(xml: &str) -> String {
    let mut out = String::new();
    let mut cells = 0usize; // nesting depth of table cells
    let mut rest = xml;
    while let Some(lt) = rest.find('<') {
        rest = &rest[lt + 1..];
        let Some(gt) = rest.find('>') else { break };
        let tag = &rest[..gt];
        rest = &rest[gt + 1..];
        let (closing, body) = match tag.strip_prefix('/') {
            Some(b) => (true, b),
            None => (false, tag),
        };
        let self_closing = body.ends_with('/');
        let name = body.trim_end_matches('/').split_whitespace().next().unwrap_or("");
        match (closing, name) {
            (false, "w:t") if !self_closing => {
                let end = rest.find("</w:t>").unwrap_or(rest.len());
                push_unescaped(&mut out, &rest[..end]);
                rest = &rest[(end + 6).min(rest.len())..];
            }
            // A bare <w:tab/> is a tab character; tab *stops* carry attributes and live in pPr.
            (false, "w:tab") if self_closing && body.trim_end_matches('/').trim() == "w:tab" => out.push('\t'),
            (false, "w:br" | "w:cr") => out.push('\n'),
            (false, "w:tc") if !self_closing => cells += 1,
            (true, "w:tc") => {
                cells = cells.saturating_sub(1);
                out.push('\t');
            }
            (true, "w:tr") => {
                if out.ends_with('\t') {
                    out.pop();
                }
                out.push('\n');
            }
            (true, "w:p") if cells == 0 => out.push('\n'),
            (false, "w:p") if self_closing && cells == 0 => out.push('\n'),
            _ => {}
        }
    }
    if out.ends_with('\n') {
        out.pop();
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::error::Error;
    use super::super::raster::testutil::*;
    use super::super::zip::{ZipArchive, ZipWriter};
    use super::*;

    #[test]
    fn text_round_trips_with_special_characters_and_blank_lines() {
        let text = "Primera línea\n\n  con sangría & <tags> \"quotes\"\nÚltima: ñandú — €";
        let docx = text_to_docx(text);
        assert_eq!(docx_to_text(&docx).unwrap(), text);
    }

    #[test]
    fn package_has_the_required_parts() {
        let docx = text_to_docx("hola");
        let z = ZipArchive::new(&docx).unwrap();
        let names: Vec<_> = z.names().collect();
        assert_eq!(names[0], "[Content_Types].xml");
        for need in ["_rels/.rels", "word/document.xml"] {
            assert!(names.contains(&need), "{need} missing in {names:?}");
        }
        let doc = String::from_utf8(z.read("word/document.xml", 1 << 20).unwrap().unwrap()).unwrap();
        assert!(doc.starts_with("<?xml") && doc.contains("<w:t xml:space=\"preserve\">hola</w:t>"));
    }

    #[test]
    fn control_characters_never_reach_the_xml() {
        let docx = text_to_docx("a\u{1}b\u{0}c\td");
        let z = ZipArchive::new(&docx).unwrap();
        let doc = String::from_utf8(z.read("word/document.xml", 1 << 20).unwrap().unwrap()).unwrap();
        assert!(!doc.contains('\u{1}') && !doc.contains('\u{0}'));
        assert_eq!(docx_to_text(&docx).unwrap(), "abc\td");
    }

    #[test]
    fn images_are_embedded_verbatim_and_fit_the_page() {
        let png = png(&image::DynamicImage::ImageRgb8(gradient(4000, 3000)));
        let docx = build(&[Block::Image(DocxImage { ext: "png", data: png.clone(), width_px: 4000, height_px: 3000 })]);
        let z = ZipArchive::new(&docx).unwrap();
        assert_eq!(z.read("word/media/image1.png", 1 << 28).unwrap().unwrap(), png);
        let rels = String::from_utf8(z.read("word/_rels/document.xml.rels", 1 << 20).unwrap().unwrap()).unwrap();
        assert!(rels.contains("media/image1.png") && rels.contains("rId1"));
        let doc = String::from_utf8(z.read("word/document.xml", 1 << 20).unwrap().unwrap()).unwrap();
        assert!(doc.contains("r:embed=\"rId1\""));
        let cx: u64 = doc.split("<wp:extent cx=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        let cy: u64 = doc.split("cy=\"").nth(1).unwrap().split('"').next().unwrap().parse().unwrap();
        assert!(cx <= 6_200_000 && cy <= 9_300_000, "{cx}x{cy}");
        assert!((cx as f64 / cy as f64 - 4.0 / 3.0).abs() < 0.01);
        assert!(docx_to_text(&docx).unwrap().trim().is_empty());
    }

    #[test]
    fn small_images_keep_their_natural_size() {
        let docx = build(&[Block::Image(DocxImage { ext: "jpeg", data: vec![1, 2, 3], width_px: 96, height_px: 48 })]);
        let z = ZipArchive::new(&docx).unwrap();
        let doc = String::from_utf8(z.read("word/document.xml", 1 << 20).unwrap().unwrap()).unwrap();
        assert!(doc.contains("<wp:extent cx=\"914400\" cy=\"457200\"/>"), "96 px is one inch at 96 dpi");
    }

    /// A document shaped like Word's own output: split runs, tab stops, a table, tracked deletions.
    fn word_like() -> Vec<u8> {
        let body = concat!(
            "<?xml version=\"1.0\"?><w:document xmlns:w=\"x\"><w:body>",
            "<w:p><w:pPr><w:tabs><w:tab w:val=\"left\" w:pos=\"720\"/></w:tabs></w:pPr>",
            "<w:r><w:t>Hel</w:t></w:r><w:r><w:rPr><w:b/></w:rPr><w:t xml:space=\"preserve\">lo </w:t></w:r>",
            "<w:r><w:t>R&amp;D</w:t></w:r><w:r><w:tab/></w:r><w:r><w:t>x&#233;&#x41;</w:t></w:r>",
            "<w:r><w:br/></w:r><w:r><w:t>next</w:t></w:r></w:p>",
            "<w:p><w:r><w:delText>gone</w:delText></w:r><w:r><w:t>kept</w:t></w:r></w:p>",
            "<w:tbl><w:tr><w:tc><w:p><w:r><w:t>A</w:t></w:r></w:p></w:tc><w:tc><w:p><w:r><w:t>B</w:t></w:r></w:p></w:tc></w:tr></w:tbl>",
            "<w:p/></w:body></w:document>"
        );
        let mut w = ZipWriter::new();
        w.add("[Content_Types].xml", b"<Types/>", true);
        w.add("word/document.xml", body.as_bytes(), true);
        w.finish()
    }

    #[test]
    fn reads_word_style_markup() {
        let t = docx_to_text(&word_like()).unwrap();
        assert_eq!(t, "Hello R&D\txéA\nnext\nkept\nA\tB\n");
    }

    #[test]
    fn not_a_docx_is_a_typed_error() {
        assert!(matches!(docx_to_text(b"plain text"), Err(Error::Corrupt(_))));
        let mut w = ZipWriter::new();
        w.add("hello.txt", b"hi", true);
        assert!(matches!(docx_to_text(&w.finish()), Err(Error::Corrupt(_))));
    }

    #[test]
    fn truncated_xml_does_not_panic() {
        let mut w = ZipWriter::new();
        w.add("word/document.xml", b"<w:document><w:body><w:p><w:r><w:t>cut off<", true);
        let _ = docx_to_text(&w.finish());
        let mut w = ZipWriter::new();
        w.add("word/document.xml", b"<w:p><w:t>a&unknown;b&#xZZ;c&#99999999;</w:t></w:p>", true);
        assert_eq!(docx_to_text(&w.finish()).unwrap(), "a&unknown;b&#xZZ;c&#99999999;");
    }
}
