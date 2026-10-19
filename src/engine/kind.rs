//! Source detection (extension plus magic bytes) and the target lists that drive the menus.

use crate::convert::Fmt;
use std::path::Path;

/// What a dropped file is, judged by content first and name second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Image(Fmt),
    Pdf,
    Docx,
    Text,
    /// Anything the engine does not read. The menus still open for it, with every sector greyed out and explained.
    Other,
}

/// What a conversion can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Image(Fmt),
    Pdf,
    Docx,
    Txt,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Target::Image(f) => f.label(),
            Target::Pdf => "PDF",
            Target::Docx => "DOCX",
            Target::Txt => "TXT",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Target::Image(f) => f.ext(),
            Target::Pdf => "pdf",
            Target::Docx => "docx",
            Target::Txt => "txt",
        }
    }
}

/// How many leading bytes `detect` needs.
pub const HEAD_LEN: usize = 1024;

const TEXT_EXTS: [&str; 6] = ["txt", "text", "md", "log", "csv", "rtf"];

/// Pure detection from the (optional) extension and the first bytes of the file.
pub fn detect_head(ext: Option<&str>, head: &[u8]) -> Option<SourceKind> {
    let ext = ext.map(str::to_ascii_lowercase);
    if let Some(f) = sniff_image(head) {
        return Some(SourceKind::Image(f));
    }
    if head.windows(5).take(HEAD_LEN).any(|w| w == b"%PDF-") && head.len() >= 5 {
        return Some(SourceKind::Pdf);
    }
    match ext.as_deref() {
        Some("docx") if head.starts_with(b"PK\x03\x04") => Some(SourceKind::Docx),
        Some(e) if TEXT_EXTS.contains(&e) && !head.contains(&0) => Some(SourceKind::Text),
        _ => None,
    }
}

fn sniff_image(h: &[u8]) -> Option<Fmt> {
    Some(match h {
        [0x89, b'P', b'N', b'G', ..] => Fmt::Png,
        [0xFF, 0xD8, 0xFF, ..] => Fmt::Jpg,
        [b'G', b'I', b'F', b'8', ..] => Fmt::Gif,
        [b'B', b'M', _, _, _, _, 0, 0, 0, 0, ..] => Fmt::Bmp, // reserved fields are zero; plain text "BMW" is not a BMP
        [b'I', b'I', 42, 0, ..] | [b'M', b'M', 0, 42, ..] => Fmt::Tiff,
        [b'R', b'I', b'F', b'F', _, _, _, _, b'W', b'E', b'B', b'P', ..] => Fmt::Webp,
        [_, _, _, _, b'f', b't', b'y', b'p', brand @ ..] if brand.len() >= 4 => match &brand[..4] {
            b"avif" | b"avis" => Fmt::Avif,
            b"heic" | b"heix" | b"heim" | b"heis" | b"hevc" | b"hevx" | b"mif1" | b"msf1" => Fmt::Heic,
            _ => return None,
        },
        _ => return None,
    })
}

/// Reads the head of `path` and classifies it. `None` when the file is unreadable or unknown.
pub fn detect(path: &Path) -> Option<SourceKind> {
    use std::io::Read;
    let mut head = Vec::with_capacity(HEAD_LEN);
    std::fs::File::open(path).ok()?.take(HEAD_LEN as u64).read_to_end(&mut head).ok()?;
    detect_head(path.extension().and_then(|e| e.to_str()), &head)
}

/// `detect`, but a file the engine does not read is `Other` instead of nothing, so every drag gets a menu.
pub fn detect_any(path: &Path) -> SourceKind {
    detect(path).unwrap_or(SourceKind::Other)
}

/// Can the pipeline turn `kind` into `to`? Mirrors `convert_bytes`; a unit test keeps the two in step.
pub fn supports(kind: SourceKind, to: Target) -> bool {
    use SourceKind as S;
    use Target as T;
    matches!(
        (kind, to),
        (S::Image(_), T::Image(_) | T::Pdf | T::Docx)
            | (S::Pdf, T::Image(_) | T::Txt | T::Docx)
            | (S::Docx, T::Txt | T::Pdf)
            | (S::Text, T::Pdf | T::Docx)
    )
}

/// Replacement for a source's own format in the 8-sector image menu.
const IMAGE_MENU: [Fmt; 6] = [Fmt::Png, Fmt::Webp, Fmt::Heic, Fmt::Tiff, Fmt::Avif, Fmt::Bmp];

/// The sectors of the radial menu for a source, clockwise from the top.
pub fn targets_for(kind: SourceKind) -> Vec<Target> {
    match kind {
        SourceKind::Pdf => vec![Target::Docx, Target::Image(Fmt::Jpg), Target::Image(Fmt::Png), Target::Txt],
        SourceKind::Image(src) => {
            // The source's own format is swapped for JPG so the menu always has 8 sectors.
            let mut t: Vec<Target> =
                IMAGE_MENU.iter().map(|&f| Target::Image(if f == src { Fmt::Jpg } else { f })).collect();
            t.extend([Target::Pdf, Target::Docx]);
            t
        }
        SourceKind::Docx => vec![Target::Pdf, Target::Txt],
        SourceKind::Text => vec![Target::Pdf, Target::Docx],
        SourceKind::Other => vec![],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const PNG: &[u8] = &[0x89, b'P', b'N', b'G', 13, 10, 26, 10, 0, 0, 0, 13];

    #[test]
    fn magic_bytes_identify_images() {
        assert_eq!(detect_head(None, PNG), Some(SourceKind::Image(Fmt::Png)));
        assert_eq!(detect_head(None, &[0xFF, 0xD8, 0xFF, 0xE0, 0, 0]), Some(SourceKind::Image(Fmt::Jpg)));
        assert_eq!(detect_head(None, b"GIF89a.."), Some(SourceKind::Image(Fmt::Gif)));
        assert_eq!(
            detect_head(None, b"BM6\x00\x00\x00\x00\x00\x00\x006\x00\x00\x00"),
            Some(SourceKind::Image(Fmt::Bmp))
        );
        assert_eq!(detect_head(None, b"BMW is a car maker"), None);
        assert_eq!(detect_head(None, b"II*\0\x08\0\0\0"), Some(SourceKind::Image(Fmt::Tiff)));
        assert_eq!(detect_head(None, b"MM\0*\0\0\0\x08"), Some(SourceKind::Image(Fmt::Tiff)));
        assert_eq!(detect_head(None, b"RIFF\x10\0\0\0WEBPVP8 "), Some(SourceKind::Image(Fmt::Webp)));
    }

    #[test]
    fn isobmff_brands_split_avif_from_heic() {
        assert_eq!(detect_head(None, b"\0\0\0\x1cftypavif\0\0\0\0"), Some(SourceKind::Image(Fmt::Avif)));
        assert_eq!(detect_head(None, b"\0\0\0\x18ftypheic\0\0\0\0"), Some(SourceKind::Image(Fmt::Heic)));
        assert_eq!(detect_head(None, b"\0\0\0\x18ftypmif1\0\0\0\0"), Some(SourceKind::Image(Fmt::Heic)));
        assert_eq!(detect_head(None, b"\0\0\0\x18ftypmp42\0\0\0\0"), None);
    }

    #[test]
    fn content_beats_a_lying_extension() {
        assert_eq!(detect_head(Some("jpg"), PNG), Some(SourceKind::Image(Fmt::Png)));
        assert_eq!(detect_head(Some("txt"), PNG), Some(SourceKind::Image(Fmt::Png)));
    }

    #[test]
    fn pdf_is_found_even_after_leading_junk() {
        assert_eq!(detect_head(Some("pdf"), b"%PDF-1.7\n"), Some(SourceKind::Pdf));
        assert_eq!(detect_head(None, b"\xEF\xBB\xBF\n%PDF-1.4"), Some(SourceKind::Pdf));
    }

    #[test]
    fn zip_is_docx_only_with_the_docx_extension() {
        assert_eq!(detect_head(Some("DOCX"), b"PK\x03\x04rest"), Some(SourceKind::Docx));
        assert_eq!(detect_head(Some("zip"), b"PK\x03\x04rest"), None);
        assert_eq!(detect_head(None, b"PK\x03\x04rest"), None);
    }

    #[test]
    fn text_needs_a_text_extension_and_no_binary_bytes() {
        assert_eq!(detect_head(Some("txt"), b"hola mundo\r\n"), Some(SourceKind::Text));
        assert_eq!(detect_head(Some("md"), b"# title"), Some(SourceKind::Text));
        assert_eq!(detect_head(Some("txt"), b"ab\0cd"), None);
        assert_eq!(detect_head(Some("exe"), b"MZ\x90\0"), None);
        assert_eq!(detect_head(None, b"plain"), None);
    }

    #[test]
    fn pdf_menu_matches_the_reference() {
        let t = targets_for(SourceKind::Pdf);
        assert_eq!(t, vec![Target::Docx, Target::Image(Fmt::Jpg), Target::Image(Fmt::Png), Target::Txt]);
    }

    #[test]
    fn image_menu_has_eight_distinct_sectors_without_the_source_format() {
        let jpg = targets_for(SourceKind::Image(Fmt::Jpg));
        let want = [Fmt::Png, Fmt::Webp, Fmt::Heic, Fmt::Tiff, Fmt::Avif, Fmt::Bmp];
        let mut expect: Vec<Target> = want.iter().map(|f| Target::Image(*f)).collect();
        expect.extend([Target::Pdf, Target::Docx]);
        assert_eq!(jpg, expect);
        for f in [Fmt::Png, Fmt::Webp, Fmt::Gif, Fmt::Heic, Fmt::Tiff, Fmt::Avif, Fmt::Bmp, Fmt::Jpg] {
            let t = targets_for(SourceKind::Image(f));
            assert_eq!(t.len(), 8, "{f:?}");
            assert!(!t.contains(&Target::Image(f)), "{f:?} must not convert to itself");
            let mut d = t.clone();
            d.dedup();
            assert_eq!(d.len(), 8);
        }
        assert!(targets_for(SourceKind::Image(Fmt::Png)).contains(&Target::Image(Fmt::Jpg)));
    }

    #[test]
    fn document_menus() {
        assert_eq!(targets_for(SourceKind::Docx), vec![Target::Pdf, Target::Txt]);
        assert_eq!(targets_for(SourceKind::Text), vec![Target::Pdf, Target::Docx]);
    }

    #[test]
    fn labels_and_extensions() {
        assert_eq!(Target::Pdf.label(), "PDF");
        assert_eq!(Target::Docx.ext(), "docx");
        assert_eq!(Target::Txt.ext(), "txt");
        assert_eq!(Target::Image(Fmt::Jpg).label(), "JPG");
    }
}
