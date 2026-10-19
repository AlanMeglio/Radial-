//! Source detection (extension plus magic bytes) and which conversions exist between kinds.

use crate::convert::Fmt;
use std::path::Path;

/// What a dropped file is, judged by content first and name second.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceKind {
    Image(Fmt),
    Pdf,
    Docx,
    /// Plain text: txt, md, log, json, xml, ...
    Text,
    /// An Excel workbook (.xlsx).
    Sheet,
    /// Comma, semicolon or tab separated values.
    Csv,
    /// A .zip archive.
    Archive,
    /// A directory.
    Folder,
    /// Anything the engine cannot convert. The menus still open for it, with the actions that work on any file.
    Other,
}

/// What a conversion can produce.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    Image(Fmt),
    Pdf,
    Docx,
    Txt,
    Csv,
    Xlsx,
}

impl Target {
    pub fn label(self) -> &'static str {
        match self {
            Target::Image(f) => f.label(),
            Target::Pdf => "PDF",
            Target::Docx => "DOCX",
            Target::Txt => "TXT",
            Target::Csv => "CSV",
            Target::Xlsx => "XLSX",
        }
    }

    #[cfg(test)]
    pub fn ext(self) -> &'static str {
        match self {
            Target::Image(f) => f.ext(),
            Target::Pdf => "pdf",
            Target::Docx => "docx",
            Target::Txt => "txt",
            Target::Csv => "csv",
            Target::Xlsx => "xlsx",
        }
    }
}

/// How many leading bytes `detect` needs.
pub const HEAD_LEN: usize = 1024;

const TEXT_EXTS: [&str; 12] = ["txt", "text", "md", "log", "rtf", "json", "xml", "yml", "yaml", "ini", "cfg", "html"];
const CSV_EXTS: [&str; 2] = ["csv", "tsv"];

/// Pure detection from the (optional) extension and the first bytes of the file.
pub fn detect_head(ext: Option<&str>, head: &[u8]) -> Option<SourceKind> {
    let ext = ext.map(str::to_ascii_lowercase);
    if let Some(f) = sniff_image(head) {
        return Some(SourceKind::Image(f));
    }
    if head.windows(5).take(HEAD_LEN).any(|w| w == b"%PDF-") && head.len() >= 5 {
        return Some(SourceKind::Pdf);
    }
    let zip = head.starts_with(b"PK\x03\x04");
    match ext.as_deref() {
        Some("docx") if zip => Some(SourceKind::Docx),
        Some("xlsx") if zip => Some(SourceKind::Sheet),
        Some("zip") if zip => Some(SourceKind::Archive),
        Some(e) if CSV_EXTS.contains(&e) && !head.contains(&0) => Some(SourceKind::Csv),
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

/// `detect`, but a directory is `Folder` and a file the engine does not convert is `Other` instead of nothing, so
/// every drag gets a menu.
pub fn detect_any(path: &Path) -> SourceKind {
    if path.is_dir() {
        return SourceKind::Folder;
    }
    detect(path).unwrap_or(SourceKind::Other)
}

/// Can the pipeline turn `kind` into `to`? Mirrors `convert_bytes`; a unit test keeps the two in step.
///
/// Documents (Word, text, spreadsheets) reach pictures through a PDF, and a picture reaches text through Windows OCR
/// (whether this machine has an OCR language is `pair_availability`'s business, not this table's).
pub fn supports(kind: SourceKind, to: Target) -> bool {
    use SourceKind as S;
    use Target as T;
    matches!(
        (kind, to),
        (S::Image(_), T::Image(_) | T::Pdf | T::Docx | T::Txt)
            | (S::Pdf, T::Image(_) | T::Txt | T::Docx)
            | (S::Docx, T::Image(_) | T::Txt | T::Pdf)
            | (S::Text, T::Image(_) | T::Pdf | T::Docx)
            | (S::Csv, T::Image(_) | T::Pdf | T::Xlsx)
            | (S::Sheet, T::Image(_) | T::Pdf | T::Txt | T::Csv)
    )
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
        assert_eq!(detect_head(Some("jar"), b"PK\x03\x04rest"), None);
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
    fn documents_and_pictures_reach_each_other_where_the_engine_can_go() {
        let png = SourceKind::Image(Fmt::Png);
        assert!(supports(png, Target::Txt), "a picture reads out as text with OCR");
        assert!(supports(png, Target::Docx) && supports(png, Target::Pdf));
        assert!(supports(SourceKind::Docx, Target::Image(Fmt::Png)), "through a PDF");
        assert!(supports(SourceKind::Text, Target::Image(Fmt::Jpg)));
        assert!(supports(SourceKind::Sheet, Target::Csv) && supports(SourceKind::Csv, Target::Xlsx));
        assert!(!supports(SourceKind::Sheet, Target::Xlsx), "not to itself");
        assert!(!supports(SourceKind::Pdf, Target::Pdf) && !supports(SourceKind::Text, Target::Txt));
        for k in [SourceKind::Archive, SourceKind::Folder, SourceKind::Other] {
            for t in [Target::Pdf, Target::Txt, Target::Docx, Target::Csv, Target::Xlsx, Target::Image(Fmt::Png)] {
                assert!(!supports(k, t), "{k:?} -> {t:?}");
            }
        }
    }

    #[test]
    fn spreadsheets_csv_and_archives_are_told_apart_from_word_and_text() {
        assert_eq!(detect_head(Some("xlsx"), b"PK\x03\x04rest"), Some(SourceKind::Sheet));
        assert_eq!(detect_head(Some("XLSX"), b"not a zip"), None);
        assert_eq!(detect_head(Some("zip"), b"PK\x03\x04rest"), Some(SourceKind::Archive));
        assert_eq!(detect_head(Some("csv"), b"a,b\n1,2\n"), Some(SourceKind::Csv));
        assert_eq!(detect_head(Some("tsv"), b"a\tb\n"), Some(SourceKind::Csv));
        assert_eq!(detect_head(Some("json"), b"{\"a\":1}"), Some(SourceKind::Text));
        assert_eq!(detect_head(Some("csv"), b"a,\0b"), None, "binary bytes are not text");
    }

    #[test]
    fn folders_and_unknown_files_still_get_a_kind() {
        let dir = std::env::temp_dir().join(format!("radial-kind-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ogg = dir.join("song.ogg");
        std::fs::write(&ogg, b"OggS\0\x02").unwrap();
        assert_eq!(detect_any(&dir), SourceKind::Folder);
        assert_eq!(detect_any(&ogg), SourceKind::Other);
        assert_eq!(detect_any(&dir.join("missing.bin")), SourceKind::Other);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn labels_and_extensions() {
        assert_eq!(Target::Pdf.label(), "PDF");
        assert_eq!(Target::Docx.ext(), "docx");
        assert_eq!(Target::Txt.ext(), "txt");
        assert_eq!((Target::Csv.label(), Target::Xlsx.ext()), ("CSV", "xlsx"));
        assert_eq!(Target::Image(Fmt::Jpg).label(), "JPG");
    }
}
