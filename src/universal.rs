//! The actions that work on any file: zip, unzip, info, checksum, copy path and show in Explorer. They run on a
//! worker thread (the drop has already returned) and report once through the tray notification path.

use std::path::{Path, PathBuf};

use crate::config::{tr, Config, Lang};
use crate::convert::human_size;
use crate::engine::fileops;
use crate::jobs::{fill, notice, percent_change, place};
use crate::ring::ActionKind;
use crate::shell;

/// Runs `action` on `files` and posts the outcome.
pub fn run(action: ActionKind, files: Vec<PathBuf>, cfg: Config) {
    let l = cfg.lang;
    let result = match action {
        ActionKind::Zip => zip(&files, &cfg),
        ActionKind::Unzip => unzip(&files, &cfg),
        ActionKind::Info => info(&files, l),
        ActionKind::Checksum => checksum(&files, l),
        ActionKind::CopyPath => copy_path(&files, l),
        ActionKind::Reveal => reveal(&files, l),
        // The picture tools are not universal; nothing to do.
        _ => return,
    };
    match result {
        Ok(Some(text)) => notice(text, false),
        Ok(None) => {}
        Err(e) => notice(fill(tr(l, "failed"), &[&e]), true),
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// `a1b2c3d4...e5f6a7b8`: enough of a hash to recognise it in a balloon.
pub fn short_hash(h: &str) -> String {
    if h.len() <= 20 {
        h.to_string()
    } else {
        format!("{}...{}", &h[..8], &h[h.len() - 8..])
    }
}

/// The first of `base.ext`, `base (1).ext`, ... that does not exist in `dir` (`ext` is `None` for a folder).
pub fn free_name(dir: &Path, base: &str, ext: Option<&str>) -> PathBuf {
    let make = |n: u32| {
        let stem = if n == 0 { base.to_string() } else { format!("{base} ({n})") };
        match ext {
            Some(e) => dir.join(format!("{stem}.{e}")),
            None => dir.join(stem),
        }
    };
    (0..).map(make).find(|p| !p.exists()).unwrap_or_else(|| make(0))
}

/// What to call the archive for these files: the file's own name, or the folder's name for several.
pub fn zip_base(files: &[PathBuf]) -> String {
    match files {
        [one] => one.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Archive".into()),
        many => many
            .first()
            .and_then(|f| f.parent())
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "Archive".into()),
    }
}

fn zip(files: &[PathBuf], cfg: &Config) -> Result<Option<String>, String> {
    let first = files.first().ok_or("nothing to zip")?;
    let dir = first.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let dest = free_name(dir, &zip_base(files), Some("zip"));
    let report = fileops::zip_paths(files, &dest).map_err(|e| e.to_string())?;
    let kept = place(first, dest, cfg)?.unwrap_or_else(|| first.clone());
    let l = cfg.lang;
    Ok(Some(fill(
        tr(l, "zip-done"),
        &[
            &name_of(&kept),
            &report.files.to_string(),
            &human_size(report.bytes_in),
            &human_size(report.bytes_out),
            &percent_change(report.bytes_in, report.bytes_out),
        ],
    )))
}

fn unzip(files: &[PathBuf], cfg: &Config) -> Result<Option<String>, String> {
    let l = cfg.lang;
    let (mut total, mut last) = (0u32, PathBuf::new());
    for f in files {
        let parent = cfg.out_path().unwrap_or_else(|| f.parent().map(Path::to_path_buf).unwrap_or_default());
        let stem = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Extracted".into());
        let dest = free_name(&parent, &stem, None);
        let report = fileops::unzip_to(f, &dest).map_err(|e| format!("{}: {e}", name_of(f)))?;
        total += report.files;
        last = dest;
    }
    Ok(Some(match files {
        [_] => fill(tr(l, "unzip-done"), &[&total.to_string(), &name_of(&last)]),
        _ => fill(tr(l, "unzip-many"), &[&total.to_string(), &files.len().to_string()]),
    }))
}

/// "Name: report.pdf" lines for one file, using the localized label of each property.
fn property_lines(path: &Path, l: Lang) -> Result<Vec<String>, String> {
    let props = fileops::properties(path).map_err(|e| e.to_string())?;
    Ok(props
        .into_iter()
        .map(|(key, value)| format!("{}: {value}", tr(l, &format!("prop-{key}"))))
        .collect())
}

fn info(files: &[PathBuf], l: Lang) -> Result<Option<String>, String> {
    let lines = match files {
        [one] => property_lines(one, l)?,
        many => {
            let total: u64 = many.iter().filter_map(|f| std::fs::metadata(f).ok()).map(|m| m.len()).sum();
            vec![fill(tr(l, "info-many"), &[&many.len().to_string(), &human_size(total)])]
        }
    };
    let text = lines.join("\r\n");
    shell::copy_text(&text)?;
    // A balloon is short: the first lines now, everything on the clipboard.
    let shown: Vec<&str> = lines.iter().map(String::as_str).take(6).collect();
    Ok(Some(format!("{}\n{}", shown.join("\n"), tr(l, "copied-clipboard"))))
}

fn checksum(files: &[PathBuf], l: Lang) -> Result<Option<String>, String> {
    let mut lines = Vec::with_capacity(files.len());
    for f in files {
        let h = fileops::sha256_file(f).map_err(|e| format!("{}: {e}", name_of(f)))?;
        lines.push((h, name_of(f)));
    }
    let text = match lines.as_slice() {
        [(h, _)] => h.clone(),
        many => many.iter().map(|(h, n)| format!("{h}  {n}")).collect::<Vec<_>>().join("\r\n"),
    };
    shell::copy_text(&text)?;
    Ok(Some(match lines.as_slice() {
        [(h, n)] => fill(tr(l, "checksum-one"), &[&short_hash(h), n]),
        many => fill(tr(l, "checksum-many"), &[&many.len().to_string()]),
    }))
}

fn copy_path(files: &[PathBuf], l: Lang) -> Result<Option<String>, String> {
    let text = files.iter().map(|f| f.display().to_string()).collect::<Vec<_>>().join("\r\n");
    shell::copy_text(&text)?;
    Ok(Some(match files {
        [_] => tr(l, "path-copied").to_string(),
        many => fill(tr(l, "paths-copied"), &[&many.len().to_string()]),
    }))
}

fn reveal(files: &[PathBuf], l: Lang) -> Result<Option<String>, String> {
    let first = files.first().ok_or("nothing to show")?;
    shell::reveal(first).map_err(|e| format!("{} ({})", tr(l, "reveal-failed"), e))?;
    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-universal-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_hash_is_shortened_to_its_ends() {
        let h = "a".repeat(8) + &"0".repeat(48) + &"b".repeat(8);
        assert_eq!(short_hash(&h), "aaaaaaaa...bbbbbbbb");
        assert_eq!(short_hash("abc123"), "abc123");
    }

    #[test]
    fn archive_names_never_collide() {
        let d = temp("names");
        assert_eq!(free_name(&d, "data", Some("zip")), d.join("data.zip"));
        std::fs::write(d.join("data.zip"), b"x").unwrap();
        std::fs::write(d.join("data (1).zip"), b"x").unwrap();
        assert_eq!(free_name(&d, "data", Some("zip")), d.join("data (2).zip"));
        std::fs::create_dir_all(d.join("Photos")).unwrap();
        assert_eq!(free_name(&d, "Photos", None), d.join("Photos (1)"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_archive_is_named_after_the_file_or_after_the_folder_for_several() {
        let one = vec![PathBuf::from(r"C:\x\report.final.pdf")];
        assert_eq!(zip_base(&one), "report.final");
        let many = vec![PathBuf::from(r"C:\x\Photos\a.png"), PathBuf::from(r"C:\x\Photos\b.png")];
        assert_eq!(zip_base(&many), "Photos");
    }

    #[test]
    fn zip_then_unzip_through_the_actions_restores_every_file() {
        let d = temp("roundtrip");
        std::fs::write(d.join("a.txt"), "hello radial").unwrap();
        std::fs::create_dir_all(d.join("sub")).unwrap();
        std::fs::write(d.join("sub").join("b.txt"), "second").unwrap();
        let cfg = Config::default();
        let msg = zip(&[d.join("a.txt"), d.join("sub")], &cfg).unwrap().unwrap();
        assert!(msg.contains(".zip"), "{msg}");
        let archive = d.join(format!("{}.zip", zip_base(&[d.join("a.txt"), d.join("sub")])));
        assert!(archive.exists(), "{archive:?}");
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let cfg = Config { out_dir: Some(out.to_string_lossy().into_owned()), ..Config::default() };
        let msg = unzip(&[archive.clone()], &cfg).unwrap().unwrap();
        assert!(msg.contains('2'), "{msg}");
        let root = out.join(archive.file_stem().unwrap());
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hello radial");
        assert_eq!(std::fs::read_to_string(root.join("sub").join("b.txt")).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_checksum_of_a_known_file_is_the_known_hash() {
        let d = temp("hash");
        std::fs::write(d.join("abc.txt"), "abc").unwrap();
        let h = fileops::sha256_file(&d.join("abc.txt")).unwrap();
        assert_eq!(h, "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn info_lines_are_localized_and_in_order() {
        let d = temp("info");
        std::fs::write(d.join("note.txt"), "hello").unwrap();
        let en = property_lines(&d.join("note.txt"), Lang::En).unwrap();
        assert!(en[0].starts_with("Name: note.txt"), "{en:?}");
        assert!(en.iter().any(|l| l.starts_with("Size: ")));
        let es = property_lines(&d.join("note.txt"), Lang::Es).unwrap();
        assert!(es[0].starts_with("Nombre: note.txt"), "{es:?}");
        let _ = std::fs::remove_dir_all(&d);
    }
}
