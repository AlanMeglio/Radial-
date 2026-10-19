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
    // The header sits at the very start (after at most a BOM or a few blank bytes); a `.pdf` may have more junk first,
    // but a source file that merely mentions "%PDF-" is not a PDF.
    let start = head.iter().position(|b| !b.is_ascii_whitespace() && !matches!(b, 0xEF | 0xBB | 0xBF)).unwrap_or(0);
    let pdf_at = head.windows(5).position(|w| w == b"%PDF-");
    if pdf_at.is_some_and(|at| at <= start || (ext.as_deref() == Some("pdf") && at < HEAD_LEN)) {
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

/// What a file probably is judging by its name alone, without opening it. Used for the bulk of a very large drag
/// and for files that live in the cloud (reading them would download them).
pub fn detect_by_name(path: &Path) -> SourceKind {
    if path.is_dir() {
        return SourceKind::Folder;
    }
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
    match ext.as_str() {
        "png" => SourceKind::Image(Fmt::Png),
        "jpg" | "jpeg" => SourceKind::Image(Fmt::Jpg),
        "webp" => SourceKind::Image(Fmt::Webp),
        "gif" => SourceKind::Image(Fmt::Gif),
        "bmp" => SourceKind::Image(Fmt::Bmp),
        "tif" | "tiff" => SourceKind::Image(Fmt::Tiff),
        "heic" | "heif" => SourceKind::Image(Fmt::Heic),
        "avif" => SourceKind::Image(Fmt::Avif),
        "pdf" => SourceKind::Pdf,
        "docx" => SourceKind::Docx,
        "xlsx" => SourceKind::Sheet,
        "zip" => SourceKind::Archive,
        e if CSV_EXTS.contains(&e) => SourceKind::Csv,
        e if TEXT_EXTS.contains(&e) => SourceKind::Text,
        _ => SourceKind::Other,
    }
}

/// True for a file whose content is not on this machine (OneDrive "online only" and the like): opening it to look at
/// its first bytes would download it.
fn is_cloud_placeholder(meta: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    // OFFLINE | RECALL_ON_OPEN | RECALL_ON_DATA_ACCESS
    meta.file_attributes() & (0x1000 | 0x4_0000 | 0x40_0000) != 0
}

/// `detect`, but a directory is `Folder` and a file the engine does not convert is `Other` instead of nothing, so
/// every drag gets a menu.
pub fn detect_any(path: &Path) -> SourceKind {
    match std::fs::metadata(path) {
        Ok(m) if m.is_dir() => SourceKind::Folder,
        Ok(m) if is_cloud_placeholder(&m) => detect_by_name(path),
        _ => detect(path).unwrap_or(SourceKind::Other),
    }
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
    fn a_source_file_that_mentions_the_pdf_header_is_not_a_pdf() {
        assert_eq!(detect_head(Some("rs"), b"// the header is %PDF-1.7 followed by...\nfn main() {}"), None);
        assert_eq!(
            detect_head(Some("txt"), b"see %PDF- inside the text here, long long line to push it out of range"),
            Some(SourceKind::Text)
        );
        let mut junk = vec![b'x'; 100];
        junk.extend_from_slice(b"%PDF-1.4");
        assert_eq!(detect_head(Some("pdf"), &junk), Some(SourceKind::Pdf), "a .pdf may start with junk");
        assert_eq!(detect_head(Some("bin"), &junk), None, "something else may not");
    }

    #[test]
    fn the_name_alone_is_enough_to_classify_a_big_drag() {
        for (name, want) in [
            ("a.PNG", SourceKind::Image(Fmt::Png)),
            ("a.jpeg", SourceKind::Image(Fmt::Jpg)),
            ("a.pdf", SourceKind::Pdf),
            ("a.docx", SourceKind::Docx),
            ("a.xlsx", SourceKind::Sheet),
            ("a.tsv", SourceKind::Csv),
            ("a.md", SourceKind::Text),
            ("a.zip", SourceKind::Archive),
            ("a.ogg", SourceKind::Other),
            ("noext", SourceKind::Other),
        ] {
            assert_eq!(detect_by_name(Path::new(name)), want, "{name}");
        }
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
