//! Format table and output naming for the radial menu. The conversion work itself lives in `engine`.

use std::path::{Path, PathBuf};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fmt {
    Png,
    Jpg,
    Webp,
    Gif,
    Avif,
    Tiff,
    Bmp,
    Heic,
}

impl Fmt {
    pub fn label(self) -> &'static str {
        match self {
            Fmt::Png => "PNG",
            Fmt::Jpg => "JPG",
            Fmt::Webp => "WEBP",
            Fmt::Gif => "GIF",
            Fmt::Avif => "AVIF",
            Fmt::Tiff => "TIFF",
            Fmt::Bmp => "BMP",
            Fmt::Heic => "HEIC",
        }
    }

    pub fn ext(self) -> &'static str {
        match self {
            Fmt::Png => "png",
            Fmt::Jpg => "jpg",
            Fmt::Webp => "webp",
            Fmt::Gif => "gif",
            Fmt::Avif => "avif",
            Fmt::Tiff => "tiff",
            Fmt::Bmp => "bmp",
            Fmt::Heic => "heic",
        }
    }

    pub fn from_ext(ext: &str) -> Option<Fmt> {
        Some(match ext.to_ascii_lowercase().as_str() {
            "png" => Fmt::Png,
            "jpg" | "jpeg" | "jpe" => Fmt::Jpg,
            "webp" => Fmt::Webp,
            "gif" => Fmt::Gif,
            "avif" => Fmt::Avif,
            "tif" | "tiff" => Fmt::Tiff,
            "bmp" => Fmt::Bmp,
            "heic" | "heif" => Fmt::Heic,
            _ => return None,
        })
    }
}

/// `dir/stem.ext`, or `dir/stem (1).ext`, `(2)`... when taken. Never returns an existing path.
pub fn unique_path(dir: &Path, stem: &str, ext: &str, exists: impl Fn(&Path) -> bool) -> PathBuf {
    let first = dir.join(format!("{stem}.{ext}"));
    if !exists(&first) {
        return first;
    }
    (1u32..).map(|n| dir.join(format!("{stem} ({n}).{ext}"))).find(|p| !exists(p)).expect("u32 range exhausted")
}

/// Binary-prefixed size with one decimal from KB up: `512 B`, `1.1 MB`.
pub fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["KB", "MB", "GB", "TB"];
    if bytes < 1024 {
        return format!("{bytes} B");
    }
    let mut v = bytes as f64 / 1024.0;
    let mut u = 0;
    while v >= 1024.0 && u < UNITS.len() - 1 {
        v /= 1024.0;
        u += 1;
    }
    format!("{v:.1} {}", UNITS[u])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extensions_map_case_insensitively_with_aliases() {
        assert_eq!(Fmt::from_ext("PNG"), Some(Fmt::Png));
        assert_eq!(Fmt::from_ext("jpeg"), Some(Fmt::Jpg));
        assert_eq!(Fmt::from_ext("JPG"), Some(Fmt::Jpg));
        assert_eq!(Fmt::from_ext("tif"), Some(Fmt::Tiff));
        assert_eq!(Fmt::from_ext("heif"), Some(Fmt::Heic));
        assert_eq!(Fmt::from_ext("txt"), None);
    }

    #[test]
    fn every_format_round_trips_through_its_extension() {
        for f in [Fmt::Png, Fmt::Jpg, Fmt::Webp, Fmt::Gif, Fmt::Avif, Fmt::Tiff, Fmt::Bmp, Fmt::Heic] {
            assert_eq!(Fmt::from_ext(f.ext()), Some(f));
            assert!(!f.label().is_empty());
        }
    }

    #[test]
    fn unique_path_never_overwrites() {
        let dir = Path::new("C:/pics");
        let none = |_: &Path| false;
        assert_eq!(unique_path(dir, "photo", "png", none), dir.join("photo.png"));
        let taken = |p: &Path| p == dir.join("photo.png") || p == dir.join("photo (1).png");
        assert_eq!(unique_path(dir, "photo", "png", taken), dir.join("photo (2).png"));
        let first = |p: &Path| p == dir.join("photo.png");
        assert_eq!(unique_path(dir, "photo", "png", first), dir.join("photo (1).png"));
    }

    #[test]
    fn sizes_are_human_readable() {
        assert_eq!(human_size(0), "0 B");
        assert_eq!(human_size(512), "512 B");
        assert_eq!(human_size(1024), "1.0 KB");
        assert_eq!(human_size(1_153_434), "1.1 MB");
        assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
    }
}
