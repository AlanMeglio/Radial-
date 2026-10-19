//! The actions that work on any file: zip, unzip, info, checksum, copy path and show in Explorer. They run on a
//! worker thread (the drop has already returned) and report once through the tray notification path.

use std::path::{Path, PathBuf};

use crate::config::{tr, Config, Lang};
use crate::convert::human_size;
use crate::engine::{fileops, pdftools, rename, sortdir};
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
        ActionKind::CopyText => copy_text(&files, l),
        ActionKind::Split => split(&files, &cfg),
        ActionKind::Sort => sort(
            &files,
            &cfg,
            &|text| match shell::ask_yes_no_cancel("Radial", text) {
                shell::Answer::Yes => Some(sortdir::Mode::ByType),
                shell::Answer::No => Some(sortdir::Mode::ByMonth),
                shell::Answer::Cancel => None,
            },
            &|undo| undo.save().is_ok(),
        ),
        ActionKind::Merge => merge(&files, &cfg),
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
    // `place` names the result after the file it is given; the archive is named after `zip_base`, so pass that.
    let base = dir.join(zip_base(files));
    let l = cfg.lang;
    let Some(kept) = place(&base, dest, cfg)? else {
        return Ok(Some(fill(tr(l, "skipped"), &[&format!("{}.zip", zip_base(files))])));
    };
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
    Ok(props.into_iter().map(|(key, value)| format!("{}: {value}", tr(l, &format!("prop-{key}")))).collect())
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

/// The SHA-256 a piece of clipboard text stands for: 64 hex digits, alone or first on the line
/// (`sha256sum` output, `SHA256: abc...`, a hash copied from a download page with spaces around it).
pub fn hash_in(text: &str) -> Option<String> {
    let first = text.trim().lines().next()?.trim();
    let first = first.strip_prefix("SHA256:").or_else(|| first.strip_prefix("sha256:")).unwrap_or(first).trim();
    let token = first.split_whitespace().next()?;
    (token.len() == 64 && token.bytes().all(|b| b.is_ascii_hexdigit())).then(|| token.to_ascii_lowercase())
}

/// SHA-256 of every file. When the clipboard already holds a hash this *verifies* against it (a download page's
/// checksum, say) and leaves the clipboard alone; otherwise it copies the hash.
fn checksum(files: &[PathBuf], l: Lang) -> Result<Option<String>, String> {
    let mut lines = Vec::with_capacity(files.len());
    for f in files {
        let h = fileops::sha256_file(f).map_err(|e| format!("{}: {e}", name_of(f)))?;
        lines.push((h, name_of(f)));
    }
    if let Some(expected) = shell::paste_text().as_deref().and_then(hash_in) {
        let hit = lines.iter().find(|(h, _)| *h == expected);
        return Ok(Some(match (hit, lines.as_slice()) {
            (Some((_, n)), _) => fill(tr(l, "checksum-ok"), &[n, &short_hash(&expected)]),
            (None, [(h, n)]) => fill(tr(l, "checksum-bad"), &[n, &short_hash(h), &short_hash(&expected)]),
            (None, many) => fill(tr(l, "checksum-none"), &[&many.len().to_string(), &short_hash(&expected)]),
        }));
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

fn copy_text(files: &[PathBuf], l: Lang) -> Result<Option<String>, String> {
    let mut parts = Vec::with_capacity(files.len());
    for f in files {
        let text = crate::engine::read_text_file(f).map_err(|e| format!("{}: {e}", name_of(f)))?;
        parts.push((name_of(f), text));
    }
    let text = match parts.as_slice() {
        [(_, one)] => one.clone(),
        many => many.iter().map(|(n, t)| format!("--- {n} ---\r\n{t}")).collect::<Vec<_>>().join("\r\n\r\n"),
    };
    shell::copy_text(&text.replace('\n', "\r\n").replace("\r\r\n", "\r\n"))?;
    let chars = text.chars().count().to_string();
    let first: String = text.split_whitespace().take(8).collect::<Vec<_>>().join(" ");
    Ok(Some(fill(tr(l, "text-copied"), &[&chars, &first])))
}

/// Sorts the loose files of each dragged folder after asking how (`choose` shows the question and returns the mode,
/// or `None` for "leave it"). The moves are written to the undo log first, so the tray menu can put them back.
fn sort(
    files: &[PathBuf],
    cfg: &Config,
    choose: &dyn Fn(&str) -> Option<sortdir::Mode>,
    save: &dyn Fn(&rename::Undo) -> bool,
) -> Result<Option<String>, String> {
    let l = cfg.lang;
    let group = |g: sortdir::Group| tr(l, g.key()).to_string();
    let (mut moved, mut folders, mut left_alone) = (0usize, 0usize, 0usize);
    let mut undo = rename::Undo::default();
    for dir in files {
        let by_type = sortdir::plan(dir, sortdir::Mode::ByType, &group).map_err(|e| e.to_string())?;
        let by_month = sortdir::plan(dir, sortdir::Mode::ByMonth, &group).map_err(|e| e.to_string())?;
        let count = |p: &sortdir::Plan| p.groups.iter().map(|(_, n)| n).sum::<usize>();
        if count(&by_type) == 0 && count(&by_month) == 0 {
            return Err(fill(tr(l, "sort-nothing"), &[&name_of(dir)]));
        }
        let ask = fill(
            tr(l, "sort-ask"),
            &[
                &name_of(dir),
                &count(&by_type).to_string(),
                &by_type.groups.len().to_string(),
                &count(&by_month).to_string(),
                &by_month.groups.len().to_string(),
            ],
        );
        let Some(mode) = choose(&ask) else { return Ok(None) };
        let plan = if mode == sortdir::Mode::ByType { by_type } else { by_month };
        left_alone += plan.left + plan.moves.iter().filter(|i| i.status != rename::Status::Ok).count();
        let done = sortdir::apply(&plan).map_err(|e| format!("{}: {e}", name_of(dir)))?;
        moved += done.entries.len();
        folders += plan.groups.len();
        undo.entries.extend(done.entries);
    }
    let saved = save(&undo);
    let mut text = fill(tr(l, "sort-done"), &[&moved.to_string(), &folders.to_string()]);
    if left_alone > 0 {
        text += &fill(tr(l, "sort-left"), &[&left_alone.to_string()]);
    }
    text += tr(l, if saved { "sort-undo-hint" } else { "sort-undo-lost" });
    Ok(Some(text))
}

/// Puts back what the last rename or sort moved. Run from the tray menu.
pub fn undo_last(l: Lang) {
    let Some(file) = rename::Undo::latest_file() else {
        return notice(tr(l, "undo-none").to_string(), false);
    };
    let outcome = rename::Undo::load(&file).and_then(|u| rename::undo_report(&u));
    match outcome {
        Ok(report) => {
            if report.skipped.is_empty() {
                let _ = std::fs::remove_file(&file);
                notice(fill(tr(l, "undo-done"), &[&report.restored.to_string()]), false);
            } else {
                notice(
                    fill(tr(l, "undo-partial"), &[&report.restored.to_string(), &report.skipped.len().to_string()]),
                    true,
                );
            }
        }
        Err(e) => notice(fill(tr(l, "failed"), &[&e.to_string()]), true),
    }
}

/// One PDF per page, in a new folder `<name> pages` next to the PDF (or in the output folder).
fn split(files: &[PathBuf], cfg: &Config) -> Result<Option<String>, String> {
    let l = cfg.lang;
    let (mut pages, mut last) = (0usize, PathBuf::new());
    for f in files {
        let bytes = crate::engine::io::read_limited(f).map_err(|e| format!("{}: {e}", name_of(f)))?;
        let parts = pdftools::split_pdf(&bytes).map_err(|e| format!("{}: {e}", name_of(f)))?;
        if parts.len() < 2 {
            return Err(fill(tr(l, "split-single"), &[&name_of(f)]));
        }
        let stem = f.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Document".into());
        let parent = cfg.out_path().unwrap_or_else(|| f.parent().map(Path::to_path_buf).unwrap_or_default());
        let dir = free_name(&parent, &format!("{stem} pages"), None);
        std::fs::create_dir_all(&dir).map_err(|e| e.to_string())?;
        let digits = parts.len().to_string().len();
        for (i, part) in parts.iter().enumerate() {
            let target = dir.join(format!("{stem}-p{:0digits$}.pdf", i + 1));
            if let Err(e) = std::fs::write(&target, part) {
                let _ = std::fs::remove_dir_all(&dir);
                return Err(e.to_string());
            }
        }
        pages += parts.len();
        last = dir;
    }
    Ok(Some(match files {
        [_] => fill(tr(l, "split-done"), &[&pages.to_string(), &name_of(&last)]),
        _ => fill(tr(l, "split-many"), &[&pages.to_string(), &files.len().to_string()]),
    }))
}

/// All the files as one PDF, ordered the way a person counts (`file2` before `file10`).
fn merge(files: &[PathBuf], cfg: &Config) -> Result<Option<String>, String> {
    let l = cfg.lang;
    let mut sorted = files.to_vec();
    sorted.sort_by(|a, b| rename::natural_cmp(&name_of(a), &name_of(b)));
    let first = sorted.first().ok_or("nothing to merge")?;
    let dir = first.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stem = first.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Merged".into());
    let data = crate::engine::merge_to_pdf(&sorted).map_err(|e| e.to_string())?;
    let pages = pdftools::page_total(&data).map_err(|e| e.to_string())?;
    let base = format!("{stem} (merged)");
    let dest = crate::engine::io::commit_unique(dir, &base, "pdf", &data).map_err(|e| e.to_string())?;
    let Some(kept) = place(&dir.join(&base), dest, cfg)? else {
        return Ok(Some(fill(tr(l, "skipped"), &[&format!("{base}.pdf")])));
    };
    Ok(Some(fill(tr(l, "merge-done"), &[&pages.to_string(), &files.len().to_string(), &name_of(&kept)])))
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

    fn long_pdf(word: &str, lines: usize) -> Vec<u8> {
        let text: String = (0..lines).map(|i| format!("{word} line {i}\n")).collect();
        crate::engine::pdfwrite::text_to_pdf(&text)
    }

    #[test]
    fn a_pdf_splits_into_one_file_per_page_in_a_new_folder() {
        let d = temp("split");
        let pdf = d.join("report.pdf");
        std::fs::write(&pdf, long_pdf("alpha", 130)).unwrap();
        let total = pdftools::page_total(&std::fs::read(&pdf).unwrap()).unwrap() as usize;
        assert!(total >= 3, "the fixture needs several pages, got {total}");
        let msg = split(std::slice::from_ref(&pdf), &Config { lang: Lang::En, ..Config::default() }).unwrap().unwrap();
        assert!(msg.contains(&total.to_string()) && msg.contains("report pages"), "{msg}");
        let dir = d.join("report pages");
        let mut names: Vec<String> =
            std::fs::read_dir(&dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        names.sort();
        assert_eq!(names.len(), total);
        assert_eq!(names[0], "report-p1.pdf");
        for n in &names {
            let bytes = std::fs::read(dir.join(n)).unwrap();
            assert_eq!(pdftools::page_total(&bytes).unwrap(), 1, "{n}");
        }
        let first = crate::engine::pdfread::extract_pages(&std::fs::read(dir.join(&names[0])).unwrap()).unwrap();
        assert!(first[0].contains("alpha line 0"), "{first:?}");
        assert!(pdf.exists(), "the original stays");
        // Running it again never mixes with the first folder.
        split(std::slice::from_ref(&pdf), &Config::default()).unwrap();
        assert!(d.join("report pages (1)").is_dir());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_single_page_pdf_says_there_is_nothing_to_split() {
        let d = temp("splitone");
        let pdf = d.join("one.pdf");
        std::fs::write(&pdf, crate::engine::pdfwrite::text_to_pdf("just one page")).unwrap();
        let err = split(std::slice::from_ref(&pdf), &Config { lang: Lang::En, ..Config::default() }).unwrap_err();
        assert!(err.contains("one.pdf") && err.contains("single page"), "{err}");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no folder is left behind");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn pdfs_pictures_and_documents_merge_into_one_pdf_in_natural_order() {
        let d = temp("merge");
        std::fs::write(d.join("part10.pdf"), crate::engine::pdfwrite::text_to_pdf("TENTH part")).unwrap();
        std::fs::write(d.join("part2.pdf"), crate::engine::pdfwrite::text_to_pdf("SECOND part")).unwrap();
        std::fs::write(d.join("part3.docx"), crate::engine::docx::text_to_docx("THIRD part")).unwrap();
        image::RgbaImage::from_pixel(40, 30, image::Rgba([200, 30, 30, 255])).save(d.join("part4.png")).unwrap();
        let files: Vec<PathBuf> =
            ["part10.pdf", "part4.png", "part2.pdf", "part3.docx"].iter().map(|n| d.join(n)).collect();
        let cfg = Config { lang: Lang::En, ..Config::default() };
        let msg = merge(&files, &cfg).unwrap().unwrap();
        assert!(
            msg.contains("4 files") && msg.contains("part10 (merged).pdf") || msg.contains("part2 (merged).pdf"),
            "{msg}"
        );
        let out = d.join("part2 (merged).pdf");
        let bytes = std::fs::read(&out).unwrap();
        assert_eq!(pdftools::page_total(&bytes).unwrap(), 4);
        let pages = crate::engine::pdfread::extract_pages(&bytes).unwrap();
        assert!(pages[0].contains("SECOND"), "natural order puts part2 first: {pages:?}");
        assert!(pages[1].contains("THIRD") && pages[3].contains("TENTH"), "{pages:?}");
        for n in ["part10.pdf", "part4.png", "part2.pdf", "part3.docx"] {
            assert!(d.join(n).exists(), "{n} stays");
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn merging_something_that_is_not_a_page_fails_without_writing() {
        let d = temp("mergebad");
        std::fs::write(d.join("a.pdf"), crate::engine::pdfwrite::text_to_pdf("A")).unwrap();
        std::fs::write(d.join("b.ogg"), b"OggS\0\x02").unwrap();
        let err = merge(&[d.join("a.pdf"), d.join("b.ogg")], &Config::default()).unwrap_err();
        assert!(err.contains("b.ogg"), "{err}");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 2, "nothing was written");
        let _ = std::fs::remove_dir_all(&d);
    }

    fn english() -> Config {
        Config { lang: Lang::En, ..Config::default() }
    }

    #[test]
    fn sorting_asks_first_moves_by_the_answer_and_logs_it_for_undo() {
        let d = temp("sortui");
        for n in ["a.png", "b.png", "c.pdf", "x.xyz"] {
            std::fs::write(d.join(n), n).unwrap();
        }
        let asked = std::cell::RefCell::new(String::new());
        let log = std::cell::RefCell::new(None);
        let msg = sort(
            std::slice::from_ref(&d),
            &english(),
            &|q| {
                *asked.borrow_mut() = q.to_string();
                Some(sortdir::Mode::ByType)
            },
            &|u| {
                *log.borrow_mut() = Some(u.clone());
                true
            },
        )
        .unwrap()
        .unwrap();
        assert!(asked.borrow().contains("by type (3 files into 2 folders)"), "{}", asked.borrow());
        assert!(msg.starts_with("Sorted 3 files into 2 folders") && msg.contains("1 left where they were"), "{msg}");
        assert!(msg.contains("Undo last move"), "{msg}");
        assert!(d.join("Images").join("a.png").exists() && d.join("Documents").join("c.pdf").exists());
        let undo = log.borrow().clone().expect("the moves reach the undo log");
        assert_eq!(undo.entries.len(), 3);
        rename::undo(&undo).unwrap();
        assert!(d.join("a.png").exists() && d.join("c.pdf").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn cancelling_the_question_moves_nothing() {
        let d = temp("sortcancel");
        std::fs::write(d.join("a.png"), "x").unwrap();
        let out = sort(std::slice::from_ref(&d), &english(), &|_| None, &|_| panic!("nothing to log")).unwrap();
        assert!(out.is_none());
        assert!(d.join("a.png").exists() && std::fs::read_dir(&d).unwrap().count() == 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_with_nothing_to_sort_says_so_without_asking() {
        let d = temp("sortnothing");
        std::fs::create_dir(d.join("only a subfolder")).unwrap();
        let err = sort(std::slice::from_ref(&d), &english(), &|_| panic!("no question"), &|_| true).unwrap_err();
        assert!(err.starts_with("Nothing to sort in"), "{err}");
        let _ = std::fs::remove_dir_all(&d);
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
        let listing: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert!(archive.exists(), "{archive:?} not in {listing:?} ({msg})");
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let cfg = Config { out_dir: Some(out.to_string_lossy().into_owned()), ..Config::default() };
        let msg = unzip(std::slice::from_ref(&archive), &cfg).unwrap().unwrap();
        assert!(msg.contains('2'), "{msg}");
        let root = out.join(archive.file_stem().unwrap());
        assert_eq!(std::fs::read_to_string(root.join("a.txt")).unwrap(), "hello radial");
        assert_eq!(std::fs::read_to_string(root.join("sub").join("b.txt")).unwrap(), "second");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_hash_is_recognised_in_the_shapes_people_copy_it() {
        let h = "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad";
        assert_eq!(hash_in(h).as_deref(), Some(h));
        assert_eq!(hash_in(&format!("  {}\r\n", h.to_uppercase())).as_deref(), Some(h));
        assert_eq!(hash_in(&format!("{h}  abc.txt")).as_deref(), Some(h), "sha256sum output");
        assert_eq!(hash_in(&format!("SHA256: {h}")).as_deref(), Some(h));
        assert_eq!(hash_in(&format!("{h}\nsecond line")).as_deref(), Some(h));
        assert_eq!(hash_in("C:\\Users\\me\\file.txt"), None, "a path is not a hash");
        assert_eq!(hash_in(&h[..63]), None);
        assert_eq!(hash_in(&format!("{h}0")), None);
        assert_eq!(hash_in(&"g".repeat(64)), None);
        assert_eq!(hash_in(""), None);
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
    fn every_property_the_engine_can_report_has_a_label_in_both_languages() {
        for key in [
            "name",
            "type",
            "size",
            "created",
            "modified",
            "title",
            "author",
            "last-modified-by",
            "doc-created",
            "doc-modified",
            "company",
            "pages",
            "sheets",
            "slides",
            "creator",
            "producer",
        ] {
            for l in [Lang::En, Lang::Es] {
                assert!(!tr(l, &format!("prop-{key}")).is_empty(), "{key} {l:?}");
            }
        }
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
