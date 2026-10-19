//! Work that runs off the UI thread and reports back through the tray notification path: conversions and the two
//! actions that need no window (COMPRESS and METADATA), plus the placement of results (output folder, collisions).

use std::path::{Path, PathBuf};

use windows_sys::Win32::Foundation::{HWND, LPARAM};
use windows_sys::Win32::UI::WindowsAndMessaging::PostMessageW;

use crate::config::{plan_destination, tr, Config, Dest, Lang};
use crate::convert::human_size;
use crate::engine::{self, compress, meta, ConvertOptions, Target};
use crate::overlay::{hwnd, WM_DONE};
use crate::ring::ActionKind;

/// Moves `from` to `to`, falling back to copy + delete across volumes.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::rename(from, to).or_else(|_| std::fs::copy(from, to).and_then(|_| std::fs::remove_file(from)))
}

/// Applies the output folder and name-collision settings to a freshly written file.
pub fn place(source: &Path, result: PathBuf, cfg: &Config) -> Result<Option<PathBuf>, String> {
    let out = cfg.out_path();
    match plan_destination(source, &result, out.as_deref(), cfg.collision, |p| p.exists()) {
        Dest::Keep(p) => Ok(Some(p)),
        Dest::Move(p) => move_file(&result, &p).map(|_| Some(p)).map_err(|e| e.to_string()),
        Dest::Replace(p) => {
            let _ = std::fs::remove_file(&p);
            move_file(&result, &p).map(|_| Some(p)).map_err(|e| e.to_string())
        }
        Dest::Discard => {
            let _ = std::fs::remove_file(&result);
            Ok(None)
        }
    }
}

/// Fills the `{}` placeholders of a localized string in order.
pub fn fill(template: &str, args: &[&str]) -> String {
    let mut out = String::with_capacity(template.len() + 16);
    let mut rest = template;
    let mut args = args.iter();
    while let Some(i) = rest.find("{}") {
        out.push_str(&rest[..i]);
        out.push_str(args.next().copied().unwrap_or(""));
        rest = &rest[i + 2..];
    }
    out.push_str(rest);
    out
}

/// Runs one job (a conversion, an action) so that a bug triggered by one odd file ends that job with a notice instead
/// of taking the whole tray program down: the panic is caught here, written to `crash.log` by the panic hook and
/// reported.
pub fn guarded(lang: Lang, job: impl FnOnce()) {
    if std::panic::catch_unwind(std::panic::AssertUnwindSafe(job)).is_err() {
        notice(tr(lang, "crashed").to_string(), true);
    }
}

/// Writes every panic (message and place) to `%APPDATA%\radial\crash.log`, keeping the file small, so a bug report
/// can include it. Radial has no console, so without this a crash would leave no trace at all.
pub fn install_crash_log() {
    std::panic::set_hook(Box::new(|info| {
        let Some(dir) = crate::config::data_dir() else { return };
        let _ = std::fs::create_dir_all(&dir);
        let file = dir.join("crash.log");
        let old = std::fs::read_to_string(&file).unwrap_or_default();
        let keep = old.len().saturating_sub(48 * 1024);
        let keep = (keep..old.len()).find(|&i| old.is_char_boundary(i)).unwrap_or(old.len());
        let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let thread = std::thread::current();
        let _ = std::fs::write(
            &file,
            format!("{}[{stamp}] thread {:?}: {info}\n", &old[keep..], thread.name().unwrap_or("?")),
        );
    }));
}

/// Asks the tray window to show a result: a balloon and/or a sound, as the user configured.
pub fn notice(text: String, failed: bool) {
    let boxed = Box::into_raw(Box::new((text, failed)));
    unsafe {
        if PostMessageW(hwnd() as HWND, WM_DONE, 0, boxed as LPARAM) == 0 {
            drop(Box::from_raw(boxed));
        }
    }
}

fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// "-34%" style change from `before` to `after` bytes.
pub fn percent_change(before: u64, after: u64) -> String {
    if before == 0 {
        return "0%".into();
    }
    let pct = (after as f64 - before as f64) / before as f64 * 100.0;
    format!("{pct:+.0}%")
}

/// Converts every file to `to` and reports once.
pub fn convert(to: Target, files: Vec<PathBuf>, cfg: Config) {
    let (mut ok, mut skipped, mut errs) = (Vec::new(), Vec::new(), Vec::new());
    for f in &files {
        let run = engine::convert_file(f, to, &ConvertOptions::default()).map_err(|e| e.to_string());
        match run {
            Ok(paths) => {
                for p in paths {
                    match place(f, p, &cfg) {
                        Ok(Some(p)) => ok.push(p),
                        Ok(None) => skipped.push(f.clone()),
                        Err(e) => errs.push(e),
                    }
                }
            }
            Err(e) => errs.push(e),
        }
    }
    let l = cfg.lang;
    let text = match (ok.as_slice(), errs.first(), skipped.first()) {
        (_, Some(e), _) => fill(tr(l, "failed"), &[e]),
        ([one], None, _) => fill(tr(l, "saved"), &[&name(one)]),
        ([], None, Some(s)) => fill(tr(l, "skipped"), &[&name(s)]),
        (_, None, _) => fill(tr(l, "converted"), &[&ok.len().to_string(), to.label()]),
    };
    notice(text, !errs.is_empty());
}

/// What COMPRESS did to one file.
pub enum Squeezed {
    Smaller { path: PathBuf, before: u64, after: u64 },
    Optimal { size: u64 },
}

/// COMPRESS and METADATA on every file, reported in a single notice.
pub fn immediate(action: ActionKind, files: Vec<PathBuf>, cfg: Config) {
    let l = cfg.lang;
    let mut lines: Vec<String> = Vec::new();
    let mut failed = false;
    let (mut before_sum, mut after_sum) = (0u64, 0u64);
    for f in &files {
        let line = match action {
            ActionKind::Compress => compress_one(f, &cfg).map(|s| match s {
                Squeezed::Smaller { path, before, after } => {
                    before_sum += before;
                    after_sum += after;
                    fill(
                        tr(l, "compressed"),
                        &[&name(&path), &human_size(before), &human_size(after), &percent_change(before, after)],
                    )
                }
                Squeezed::Optimal { size } => {
                    before_sum += size;
                    after_sum += size;
                    fill(tr(l, "optimal"), &[&name(f), &human_size(size)])
                }
            }),
            _ => metadata_one(f, &cfg, l),
        };
        match line {
            Ok(t) => lines.push(t),
            Err(e) => {
                failed = true;
                lines.push(fill(tr(l, "failed"), &[&e]));
            }
        }
    }
    let text = match lines.as_slice() {
        [one] => one.clone(),
        _ if action == ActionKind::Compress && !failed => fill(
            tr(l, "compressed-many"),
            &[
                &files.len().to_string(),
                &human_size(before_sum),
                &human_size(after_sum),
                &percent_change(before_sum, after_sum),
            ],
        ),
        _ => lines.join("\n"),
    };
    notice(text, failed);
}

fn compress_one(src: &Path, cfg: &Config) -> Result<Squeezed, String> {
    match compress::compress_file(src, &compress::CompressOptions::default()).map_err(|e| e.to_string())? {
        compress::Outcome::Written { path, before, after } => {
            let kept = place(src, path, cfg)?;
            Ok(Squeezed::Smaller { path: kept.unwrap_or_else(|| src.to_path_buf()), before, after })
        }
        compress::Outcome::AlreadyOptimal { before, .. } => Ok(Squeezed::Optimal { size: before }),
    }
}

/// Inspects and strips: writes a clean copy only when something removable is there.
fn metadata_one(src: &Path, cfg: &Config, l: Lang) -> Result<String, String> {
    let bytes = engine::io::read_limited(src).map_err(|e| e.to_string())?;
    let report = meta::inspect(&bytes).map_err(|e| e.to_string())?;
    if report.entries.is_empty() && report.removable_bytes == 0 {
        return Ok(fill(tr(l, "meta-none"), &[&name(src)]));
    }
    let clean = meta::strip_file(src).map_err(|e| e.to_string())?;
    let kept = place(src, clean, cfg)?.unwrap_or_else(|| src.to_path_buf());
    let key = if report.has_gps { "meta-gps" } else { "meta-done" };
    Ok(fill(tr(l, key), &[&name(&kept), &report.entries.len().to_string(), &human_size(report.removable_bytes)]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders_are_filled_in_order_and_extra_ones_vanish() {
        assert_eq!(fill("{} -> {} ({})", &["a", "b", "c"]), "a -> b (c)");
        assert_eq!(fill("{} and {}", &["x"]), "x and ");
        assert_eq!(fill("none", &["x"]), "none");
    }

    #[test]
    fn size_changes_read_as_signed_percentages() {
        assert_eq!(percent_change(1000, 660), "-34%");
        assert_eq!(percent_change(1000, 1000), "+0%");
        assert_eq!(percent_change(0, 5), "0%");
    }

    fn temp(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-jobs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn placing_into_a_custom_folder_never_overwrites() {
        let d = temp("place");
        let out = d.join("out");
        std::fs::create_dir_all(&out).unwrap();
        let src = d.join("a.png");
        std::fs::write(&src, b"src").unwrap();
        std::fs::write(out.join("a.png"), b"taken").unwrap();
        let result = d.join("a (cropped).png");
        std::fs::write(&result, b"new").unwrap();
        let cfg = Config { out_dir: Some(out.to_string_lossy().into_owned()), ..Config::default() };
        let placed = place(&src, result, &cfg).unwrap().unwrap();
        assert_eq!(placed, out.join("a (1).png"));
        assert_eq!(std::fs::read(out.join("a.png")).unwrap(), b"taken");
        assert_eq!(std::fs::read(&placed).unwrap(), b"new");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn compress_reports_before_and_after_for_a_real_file() {
        let d = temp("compress");
        let src = d.join("flat.png");
        // A flat 400x400 RGBA PNG written with the fastest settings compresses much further.
        let img = image::RgbaImage::from_pixel(400, 400, image::Rgba([10, 20, 30, 255]));
        let mut raw = Vec::new();
        let enc = image::codecs::png::PngEncoder::new_with_quality(
            &mut raw,
            image::codecs::png::CompressionType::Fast,
            image::codecs::png::FilterType::NoFilter,
        );
        image::ImageEncoder::write_image(enc, img.as_raw(), 400, 400, image::ExtendedColorType::Rgba8).unwrap();
        std::fs::write(&src, &raw).unwrap();
        match compress_one(&src, &Config::default()).unwrap() {
            Squeezed::Smaller { path, before, after } => {
                assert!(after < before && path.exists());
                assert_eq!(path.file_name().unwrap(), "flat (compressed).png");
            }
            Squeezed::Optimal { .. } => panic!("a flat image must shrink"),
        }
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn metadata_on_a_png_with_a_text_chunk_writes_a_clean_copy_and_keeps_the_original() {
        let d = temp("metaclean");
        let src = d.join("tagged.png");
        let mut raw = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut raw, 4, 4);
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            enc.add_text_chunk("Author".into(), "someone".into()).unwrap();
            let mut w = enc.write_header().unwrap();
            w.write_image_data(&[200u8; 4 * 4 * 4]).unwrap();
        }
        std::fs::write(&src, &raw).unwrap();
        let msg = metadata_one(&src, &Config::default(), Lang::En).unwrap();
        assert!(msg.contains("tagged (clean).png"), "{msg}");
        let clean = std::fs::read(d.join("tagged (clean).png")).unwrap();
        assert!(!clean.windows(6).any(|w| w == b"Author"), "the text chunk is gone");
        assert_eq!(std::fs::read(&src).unwrap(), raw, "the original is untouched");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn metadata_on_a_clean_png_writes_nothing() {
        let d = temp("meta");
        let src = d.join("c.png");
        image::RgbaImage::from_pixel(8, 8, image::Rgba([1, 2, 3, 255])).save(&src).unwrap();
        let msg = metadata_one(&src, &Config::default(), Lang::En).unwrap();
        assert!(msg.contains("c.png"), "{msg}");
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "no clean copy for a file with nothing to remove");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_panic_in_a_job_is_caught_and_the_program_goes_on() {
        let ran = std::cell::Cell::new(false);
        // The hook of the test harness prints the panic; what matters is that it does not escape.
        guarded(Lang::En, || {
            ran.set(true);
            if ran.get() {
                panic!("a bug triggered by one odd file");
            }
        });
        assert!(ran.get(), "the job ran, panicked, and guarded returned");
        let mut finished = false;
        guarded(Lang::En, || finished = true);
        assert!(finished, "a job that does not panic just runs");
    }
}
