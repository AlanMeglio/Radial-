//! The user's own sectors: "move to Invoices", "copy to the USB stick", "open with the editor". They live in
//! `%APPDATA%\radial\sectors.txt` and show up as the ring called "mine" for the files they apply to.
//!
//! The file is one sector per line, `label<TAB>move|copy|open<TAB>target<TAB>extensions`; `#` starts a comment.
//! Reading it is lenient and never executes anything. Running a sector never overwrites (a taken name becomes
//! `name (1).ext`), never follows junctions or symlinks and keeps an undo trail for moves only.
// Only the file reading, the checks and the summaries are used by the editor and Settings so far. `run`,
// `applies_to` and everything under them are reached from the ring, menu and overlay, which the parent wires.

use std::ffi::OsStr;
use std::io::Read;
use std::path::{Path, PathBuf};

use windows_sys::Win32::System::Com::{CoInitializeEx, CoUninitialize};
use windows_sys::Win32::UI::Shell::ShellExecuteW;

use crate::config::{tr, Lang};
use crate::engine::rename::{Undo, UndoEntry};
use crate::jobs::fill;

/// What a user sector does with the dropped files.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Moves the files into the folder `target`.
    MoveTo,
    /// Copies the files into the folder `target`.
    CopyTo,
    /// Opens the files with the program `target`.
    OpenWith,
}

impl Kind {
    pub const ALL: [Kind; 3] = [Kind::MoveTo, Kind::CopyTo, Kind::OpenWith];

    /// The word used in the file.
    fn word(self) -> &'static str {
        match self {
            Kind::MoveTo => "move",
            Kind::CopyTo => "copy",
            Kind::OpenWith => "open",
        }
    }

    fn from_word(w: &str) -> Option<Kind> {
        Kind::ALL.into_iter().find(|k| k.word() == w.trim().to_ascii_lowercase())
    }

    /// Does the target of this kind have to be a folder (otherwise it is a program)?
    pub fn wants_folder(self) -> bool {
        self != Kind::OpenWith
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Sector {
    /// Shown on the sector, a few characters.
    pub label: String,
    pub kind: Kind,
    /// A folder for `MoveTo` and `CopyTo`, a program for `OpenWith`.
    pub target: PathBuf,
    /// Lower-case extensions without the dot the sector applies to; empty means every file (and folders).
    pub exts: Vec<String>,
}

impl Sector {
    /// Does this sector apply to every one of `files`? A folder never matches a non-empty extension list.
    pub fn applies_to(&self, files: &[PathBuf]) -> bool {
        if self.exts.is_empty() {
            return true;
        }
        !files.is_empty() && files.iter().all(|f| !f.is_dir() && self.has_ext(f))
    }

    fn has_ext(&self, f: &Path) -> bool {
        let ext = f.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
        !ext.is_empty() && self.exts.contains(&ext)
    }
}

/// The most sectors a user can have.
pub const MAX_SECTORS: usize = 8;
/// Longest label, in characters.
pub const MAX_LABEL: usize = 12;
/// Most files "open with" takes in one drop.
pub const MAX_OPEN: usize = 10;
/// Deepest folder nesting a copy follows.
pub const MAX_DEPTH: usize = 32;
/// A sectors file bigger than this is treated as damaged.
const MAX_FILE: u64 = 256 * 1024;
const MAX_LINE: usize = 2048;
const HEADER: &str = "# radial sectors: label <TAB> move|copy|open <TAB> folder or program <TAB> extensions (pdf,docx)";
const PROGRAMS: [&str; 4] = ["exe", "bat", "cmd", "lnk"];

// ---------------------------------------------------------------------------------------------------------------
// The file
// ---------------------------------------------------------------------------------------------------------------

fn clean_label(s: &str) -> String {
    let flat: String = s.chars().map(|c| if c.is_control() { ' ' } else { c }).collect();
    flat.trim().chars().take(MAX_LABEL).collect::<String>().trim_end().to_string()
}

/// "pdf, .DOCX" -> `["pdf", "docx"]`: lower case, no dots, letters and digits only, no repeats.
pub fn parse_exts(text: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for part in text.split(|c: char| c == ',' || c == ';' || c.is_whitespace()) {
        let e = part.trim().trim_start_matches('.').to_lowercase();
        if !e.is_empty() && e.len() <= 16 && e.chars().all(|c| c.is_alphanumeric()) && !out.contains(&e) {
            out.push(e);
        }
    }
    out
}

/// `["pdf", "docx"]` -> `"pdf, docx"` (what the editor shows).
pub fn exts_text(exts: &[String]) -> String {
    exts.join(", ")
}

/// Parses the text of a sectors file. Bad lines are skipped; nothing is ever executed.
pub fn parse(text: &str) -> Vec<Sector> {
    let mut out = Vec::new();
    for line in text.trim_start_matches('\u{feff}').lines() {
        if out.len() >= MAX_SECTORS {
            break;
        }
        if line.len() > MAX_LINE || line.trim().is_empty() || line.trim_start().starts_with('#') {
            continue;
        }
        if line.chars().any(|c| c.is_control() && c != '\t') {
            continue;
        }
        let f: Vec<&str> = line.split('\t').collect();
        let (label, kind, target, exts) = match f.as_slice() {
            [l, k, t] => (*l, *k, *t, ""),
            [l, k, t, e] => (*l, *k, *t, *e),
            _ => continue,
        };
        let label = clean_label(label);
        let target = target.trim().trim_matches('"').trim();
        let Some(kind) = Kind::from_word(kind) else { continue };
        if label.is_empty() || target.is_empty() {
            continue;
        }
        out.push(Sector { label, kind, target: PathBuf::from(target), exts: parse_exts(exts) });
    }
    out
}

/// The text of a sectors file for `list` (at most [`MAX_SECTORS`]). A tab or newline inside a field becomes a space.
pub fn serialise(list: &[Sector]) -> String {
    let flat =
        |s: &str| -> String { s.chars().map(|c| if matches!(c, '\t' | '\n' | '\r') { ' ' } else { c }).collect() };
    let mut text = String::from(HEADER);
    text.push('\n');
    for s in list.iter().take(MAX_SECTORS) {
        let label = clean_label(&flat(&s.label));
        let target = flat(&s.target.to_string_lossy());
        text.push_str(&format!("{label}\t{}\t{target}\t{}\n", s.kind.word(), s.exts.join(",")));
    }
    text
}

/// Reads `sectors.txt` from `dir`; a missing, huge or damaged file is an empty list.
pub fn load_in(dir: &Path) -> Vec<Sector> {
    let path = dir.join("sectors.txt");
    let Ok(file) = std::fs::File::open(&path) else { return Vec::new() };
    let mut bytes = Vec::new();
    if file.take(MAX_FILE + 1).read_to_end(&mut bytes).is_err() || bytes.len() as u64 > MAX_FILE {
        return Vec::new();
    }
    parse(&String::from_utf8_lossy(&bytes))
}

/// Writes `sectors.txt` into `dir` atomically (temp file, then rename).
pub fn save_in(dir: &Path, list: &[Sector]) -> std::io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let tmp = dir.join("sectors.tmp");
    std::fs::write(&tmp, serialise(list))?;
    if let Err(e) = std::fs::rename(&tmp, dir.join("sectors.txt")) {
        let _ = std::fs::remove_file(&tmp);
        return Err(e);
    }
    Ok(())
}

/// Reads `%APPDATA%\radial\sectors.txt`; a missing or damaged file is an empty list, never an error.
pub fn load() -> Vec<Sector> {
    crate::config::data_dir().map(|d| load_in(&d)).unwrap_or_default()
}

/// Writes the list atomically.
pub fn save(list: &[Sector]) -> std::io::Result<()> {
    match crate::config::data_dir() {
        Some(d) => save_in(&d, list),
        None => Ok(()),
    }
}

/// Adds `s` (`index` is `None`) or replaces the one at `index`. A list that is full refuses an addition.
pub fn upsert(list: &mut Vec<Sector>, index: Option<usize>, s: Sector) -> Result<(), Problem> {
    match index {
        Some(i) if i < list.len() => list[i] = s,
        _ if list.len() >= MAX_SECTORS => return Err(Problem::Full),
        _ => list.push(s),
    }
    Ok(())
}

// ---------------------------------------------------------------------------------------------------------------
// Checking
// ---------------------------------------------------------------------------------------------------------------

/// Why a sector cannot be saved or run.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Problem {
    NoLabel,
    NoTarget(Kind),
    NoFolder(PathBuf),
    NoProgram(PathBuf),
    NotProgram(PathBuf),
    TooMany(usize),
    NoFiles,
    Full,
}

impl Problem {
    pub fn text(&self, lang: Lang) -> String {
        match self {
            Problem::NoLabel => tr(lang, "sec-e-name").to_string(),
            Problem::NoTarget(k) if k.wants_folder() => tr(lang, "sec-e-nofolder").to_string(),
            Problem::NoTarget(_) => tr(lang, "sec-e-noprog").to_string(),
            Problem::NoFolder(p) => fill(tr(lang, "sec-e-folder"), &[&p.display().to_string()]),
            Problem::NoProgram(p) => fill(tr(lang, "sec-e-prog"), &[&p.display().to_string()]),
            Problem::NotProgram(_) => tr(lang, "sec-e-progtype").to_string(),
            Problem::TooMany(n) => fill(tr(lang, "sec-e-many"), &[&n.to_string()]),
            Problem::NoFiles => tr(lang, "sec-nothing").to_string(),
            Problem::Full => fill(tr(lang, "sec-e-full"), &[&MAX_SECTORS.to_string()]),
        }
    }
}

fn is_program(p: &Path) -> bool {
    let ext = p.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    PROGRAMS.contains(&ext.as_str())
}

/// What is wrong with `s`, if anything: a label, and a target that exists and fits the kind.
pub fn problem(s: &Sector) -> Option<Problem> {
    if s.label.trim().is_empty() {
        return Some(Problem::NoLabel);
    }
    if s.target.as_os_str().is_empty() {
        return Some(Problem::NoTarget(s.kind));
    }
    if s.kind.wants_folder() {
        return (!s.target.is_dir()).then(|| Problem::NoFolder(s.target.clone()));
    }
    if !is_program(&s.target) {
        return Some(Problem::NotProgram(s.target.clone()));
    }
    (!s.target.is_file()).then(|| Problem::NoProgram(s.target.clone()))
}

/// Checks `s` and words the problem in `lang`.
pub fn validate_in(s: &Sector, lang: Lang) -> Result<(), String> {
    problem(s).map_or(Ok(()), |p| Err(p.text(lang)))
}

/// Checks `s`; the problem is worded in English (the editor uses [`validate_in`]).
#[cfg(test)]
pub fn validate(s: &Sector) -> Result<(), String> {
    validate_in(s, Lang::En)
}

/// "Move to  D:\Invoices  .pdf .docx": the one-line summary of a sector.
#[cfg(test)]
pub fn summary(s: &Sector, lang: Lang) -> String {
    let what = match s.kind {
        Kind::MoveTo => tr(lang, "sec-sum-move"),
        Kind::CopyTo => tr(lang, "sec-sum-copy"),
        Kind::OpenWith => tr(lang, "sec-sum-open"),
    };
    let mut out = format!("{what}  {}", s.target.display());
    if !s.exts.is_empty() {
        let dotted: Vec<String> = s.exts.iter().map(|e| format!(".{e}")).collect();
        out.push_str("  ");
        out.push_str(&dotted.join(" "));
    }
    out
}

// ---------------------------------------------------------------------------------------------------------------
// Running
// ---------------------------------------------------------------------------------------------------------------

/// Why one file could not be processed.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Fail {
    Io(String),
    /// A folder cannot go inside itself.
    Itself,
    /// More than [`MAX_DEPTH`] levels of folders.
    TooDeep,
    /// The folder holds junctions or symlinks, which are never followed.
    Links,
    /// A character that cannot be passed safely to a program.
    Quote,
    /// Somebody took the free name between the check and the move; the caller tries the next one.
    Taken,
}

impl Fail {
    pub fn text(&self, lang: Lang) -> String {
        match self {
            Fail::Io(e) => e.clone(),
            Fail::Itself => tr(lang, "sec-f-itself").to_string(),
            Fail::TooDeep => tr(lang, "sec-f-deep").to_string(),
            Fail::Links => tr(lang, "sec-f-links").to_string(),
            Fail::Quote => tr(lang, "sec-f-quote").to_string(),
            Fail::Taken => tr(lang, "sec-f-taken").to_string(),
        }
    }
}

/// What a run did, without any wording.
#[derive(Debug, Default)]
pub struct RunReport {
    /// Where each file ended up (the file itself for "open with").
    pub done: Vec<PathBuf>,
    /// Files that were already in the target folder.
    pub skipped: Vec<PathBuf>,
    /// File name and the reason.
    pub failed: Vec<(String, Fail)>,
    /// The moves that can be undone (renames only).
    pub undo: Undo,
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| p.display().to_string())
}

/// A path as a comparison key: canonical when it exists, lower case, no trailing separator.
fn key(p: &Path) -> String {
    let c = std::fs::canonicalize(p).unwrap_or_else(|_| p.to_path_buf());
    let s = c.to_string_lossy().to_lowercase();
    s.strip_prefix(r"\\?\").unwrap_or(&s).trim_end_matches('\\').to_string()
}

fn inside(child: &Path, parent: &Path) -> bool {
    let (c, p) = (key(child), key(parent));
    c == p || c.starts_with(&format!("{p}\\"))
}

/// `name`, then `name (1).ext`, `name (2).ext`... A folder keeps its whole name as the stem.
fn numbered(dir: &Path, name: &OsStr, is_dir: bool, n: u32) -> PathBuf {
    if n == 0 {
        return dir.join(name);
    }
    let whole = Path::new(name);
    let (stem, ext) = if is_dir {
        (name.to_string_lossy().into_owned(), String::new())
    } else {
        (
            whole.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_default(),
            whole.extension().map(|e| format!(".{}", e.to_string_lossy())).unwrap_or_default(),
        )
    };
    dir.join(format!("{stem} ({n}){ext}"))
}

fn exists(p: &Path) -> bool {
    std::fs::symlink_metadata(p).is_ok()
}

fn io_fail(e: std::io::Error) -> Fail {
    if e.kind() == std::io::ErrorKind::AlreadyExists {
        Fail::Taken
    } else {
        Fail::Io(e.to_string())
    }
}

#[link(name = "kernel32")]
extern "system" {
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
}

fn wide_path(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Renames without ever replacing: an existing target is an error, whatever its type.
fn rename_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let (a, b) = (wide_path(from), wide_path(to));
    // SAFETY: both buffers are NUL-terminated and outlive the call; flags 0 means "do not replace".
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

/// ERROR_NOT_SAME_DEVICE: the rename would cross drives.
fn crosses_drives(e: &std::io::Error) -> bool {
    e.raw_os_error() == Some(17)
}

#[derive(Debug, Default, PartialEq, Eq)]
struct Tally {
    files: u64,
    bytes: u64,
    links: u64,
}

/// Copies one file to a name that must not exist yet, then checks the size and keeps the modified time.
fn copy_file(from: &Path, to: &Path, tally: &mut Tally) -> Result<(), Fail> {
    let mut src = std::fs::File::open(from).map_err(io_fail)?;
    let len = src.metadata().map_err(io_fail)?.len();
    let mut dst = std::fs::OpenOptions::new().write(true).create_new(true).open(to).map_err(io_fail)?;
    let copied = std::io::copy(&mut src, &mut dst).map_err(io_fail);
    let modified = src.metadata().and_then(|m| m.modified()).ok();
    let done = copied.and_then(|n| {
        if n == len && std::fs::metadata(to).is_ok_and(|m| m.len() == len) {
            Ok(())
        } else {
            Err(Fail::Io("the copy is not the same size".into()))
        }
    });
    if let (Ok(()), Some(m)) = (&done, modified) {
        let _ = dst.set_modified(m);
    }
    drop(dst);
    if done.is_err() {
        let _ = std::fs::remove_file(to);
    }
    tally.files += 1;
    tally.bytes += len;
    done
}

/// Copies the contents of folder `from` into the existing folder `to`. Junctions and symlinks are counted and
/// skipped, never followed.
fn copy_tree(from: &Path, to: &Path, depth: usize, tally: &mut Tally) -> Result<(), Fail> {
    if depth >= MAX_DEPTH {
        return Err(Fail::TooDeep);
    }
    for entry in std::fs::read_dir(from).map_err(io_fail)? {
        let entry = entry.map_err(io_fail)?;
        let ft = entry.file_type().map_err(io_fail)?;
        let target = to.join(entry.file_name());
        if ft.is_symlink() {
            tally.links += 1;
        } else if ft.is_dir() {
            std::fs::create_dir(&target).map_err(io_fail)?;
            copy_tree(&entry.path(), &target, depth + 1, tally)?;
        } else {
            copy_file(&entry.path(), &target, tally)?;
        }
    }
    Ok(())
}

/// Files and bytes under `dir`, links not followed.
fn measure(dir: &Path, depth: usize) -> Tally {
    let mut t = Tally::default();
    if depth > MAX_DEPTH {
        return t;
    }
    if let Ok(rd) = std::fs::read_dir(dir) {
        for e in rd.flatten() {
            let Ok(ft) = e.file_type() else { continue };
            if ft.is_symlink() {
                t.links += 1;
            } else if ft.is_dir() {
                let sub = measure(&e.path(), depth + 1);
                (t.files, t.bytes, t.links) = (t.files + sub.files, t.bytes + sub.bytes, t.links + sub.links);
            } else {
                t.files += 1;
                t.bytes += e.metadata().map_or(0, |m| m.len());
            }
        }
    }
    t
}

/// Copies `from` to the name `to`, which must not exist (file or folder, links never followed). A failure removes
/// what this call created, and never a name somebody else holds (`Fail::Taken` creates nothing).
fn copy_any(from: &Path, to: &Path, is_dir: bool) -> Result<Tally, Fail> {
    let mut tally = Tally::default();
    if !is_dir {
        copy_file(from, to, &mut tally)?;
        return Ok(tally);
    }
    std::fs::create_dir(to).map_err(io_fail)?;
    match copy_tree(from, to, 0, &mut tally) {
        Ok(()) => Ok(tally),
        Err(e) => {
            let _ = std::fs::remove_dir_all(to);
            Err(e)
        }
    }
}

/// A move across drives: copy, verify, delete the original. Any failure leaves the original where it was and
/// removes the partial copy.
fn move_across_drives(from: &Path, to: &Path, is_dir: bool) -> Result<(), Fail> {
    let copied = copy_any(from, to, is_dir)?;
    if copied.links > 0 {
        let _ = std::fs::remove_dir_all(to);
        return Err(Fail::Links);
    }
    if is_dir {
        let seen = measure(to, 0);
        if (seen.files, seen.bytes) != (copied.files, copied.bytes) {
            let _ = std::fs::remove_dir_all(to);
            return Err(Fail::Io("the copy does not match the original".into()));
        }
        // A partly deleted original next to a whole copy loses nothing, so the copy stays and the error is reported.
        std::fs::remove_dir_all(from).map_err(|e| Fail::Io(format!("copied, but the original stays: {e}")))
    } else {
        std::fs::remove_file(from).map_err(|e| {
            let _ = std::fs::remove_file(to);
            io_fail(e)
        })
    }
}

/// Where a moved or copied item went and whether it was a plain rename (the only kind that can be undone).
struct Placed {
    path: PathBuf,
    renamed: bool,
}

fn place_in(dir: &Path, from: &Path, is_dir: bool, copy: bool) -> Result<Placed, Fail> {
    let name = from.file_name().ok_or_else(|| Fail::Io("no file name".into()))?;
    for n in 0..1000 {
        let to = numbered(dir, name, is_dir, n);
        if exists(&to) {
            continue;
        }
        let r = if copy {
            copy_any(from, &to, is_dir).map(|_| false)
        } else {
            match rename_no_replace(from, &to) {
                Ok(()) => Ok(true),
                Err(e) if crosses_drives(&e) => move_across_drives(from, &to, is_dir).map(|_| false),
                Err(e) => Err(io_fail(e)),
            }
        };
        match r {
            Ok(renamed) => return Ok(Placed { path: to, renamed }),
            Err(Fail::Taken) => continue,
            Err(e) => return Err(e),
        }
    }
    Err(Fail::Io("no free name".into()))
}

fn unix_nanos(t: std::time::SystemTime) -> u64 {
    t.duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

fn undo_entry(original: &Path, current: &Path) -> UndoEntry {
    let m = std::fs::symlink_metadata(current).ok();
    UndoEntry {
        dir: m.as_ref().is_some_and(|m| m.is_dir()),
        len: m.as_ref().map_or(0, |m| m.len()),
        modified: m.as_ref().and_then(|m| m.modified().ok()).map_or(0, unix_nanos),
        current: current.to_path_buf(),
        original: original.to_path_buf(),
    }
}

fn move_or_copy(sector: &Sector, files: &[PathBuf]) -> Result<RunReport, Problem> {
    let dir = &sector.target;
    if !dir.is_dir() {
        return Err(Problem::NoFolder(dir.clone()));
    }
    let copy = sector.kind == Kind::CopyTo;
    let mut report = RunReport::default();
    for f in files {
        let name = name_of(f);
        let Ok(meta) = std::fs::symlink_metadata(f) else {
            report.failed.push((name, Fail::Io("not found".into())));
            continue;
        };
        let is_dir = meta.is_dir();
        if f.parent().is_some_and(|p| key(p) == key(dir)) {
            report.skipped.push(f.clone());
            continue;
        }
        if is_dir && inside(dir, f) {
            report.failed.push((name, Fail::Itself));
            continue;
        }
        if copy && meta.file_type().is_symlink() {
            report.failed.push((name, Fail::Links));
            continue;
        }
        match place_in(dir, f, is_dir, copy) {
            Ok(p) => {
                if !copy && p.renamed {
                    report.undo.entries.push(undo_entry(f, &p.path));
                }
                report.done.push(p.path);
            }
            Err(e) => report.failed.push((name, e)),
        }
    }
    Ok(report)
}

/// Opens `file` with `program` through the shell: the path is quoted and is the only argument.
fn open_one(program: &Path, file: &Path) -> Result<(), Fail> {
    let path = file.to_string_lossy();
    let batch =
        program.extension().is_some_and(|e| matches!(e.to_string_lossy().to_lowercase().as_str(), "bat" | "cmd"));
    // A quote cannot be part of a Windows file name; a percent sign would be expanded by a batch file's command line.
    if path.contains('"') || (batch && path.contains('%')) {
        return Err(Fail::Quote);
    }
    let params = wide(&format!("\"{path}\""));
    let (verb, exe) = (wide("open"), wide_path(program));
    let dir = file.parent().map(wide_path);
    // SAFETY: every string is NUL terminated and outlives the call; ShellExecuteW returns a value above 32 on success.
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            dir.as_ref().map_or(std::ptr::null(), |d| d.as_ptr()),
            1, // SW_SHOWNORMAL
        ) as isize
    };
    if code > 32 {
        Ok(())
    } else {
        Err(Fail::Io(format!("the program could not be started (code {code})")))
    }
}

fn open_with(sector: &Sector, files: &[PathBuf]) -> Result<RunReport, Problem> {
    let program = &sector.target;
    if !is_program(program) {
        return Err(Problem::NotProgram(program.clone()));
    }
    if !program.is_file() {
        return Err(Problem::NoProgram(program.clone()));
    }
    if files.len() > MAX_OPEN {
        return Err(Problem::TooMany(files.len()));
    }
    let mut report = RunReport::default();
    // The shell wants COM on the calling thread; a thread that already has it in another mode keeps it as it is.
    // SAFETY: balanced with CoUninitialize below when the call succeeded.
    let com = unsafe { CoInitializeEx(std::ptr::null(), 0x2 | 0x4) } >= 0;
    for f in files {
        match open_one(program, f) {
            Ok(()) => report.done.push(f.clone()),
            Err(e) => report.failed.push((name_of(f), e)),
        }
    }
    if com {
        // SAFETY: the matching CoInitializeEx succeeded on this thread.
        unsafe { CoUninitialize() };
    }
    Ok(report)
}

/// Runs `sector` on `files`. `Err` is a problem with the sector itself (nothing was touched).
pub fn execute(sector: &Sector, files: &[PathBuf]) -> Result<RunReport, Problem> {
    if files.is_empty() {
        return Err(Problem::NoFiles);
    }
    match sector.kind {
        Kind::MoveTo | Kind::CopyTo => move_or_copy(sector, files),
        Kind::OpenWith => open_with(sector, files),
    }
}

/// "report.pdf" or "3 files".
fn what(paths: &[PathBuf], lang: Lang) -> String {
    match paths {
        [one] => name_of(one),
        many => fill(tr(lang, "sec-n-files"), &[&many.len().to_string()]),
    }
}

/// The notice for a finished run and whether it should look like a failure.
pub fn notice_text(sector: &Sector, r: &RunReport, lang: Lang) -> (String, bool) {
    let mut parts = Vec::new();
    if !r.done.is_empty() {
        let many = r.done.len() > 1;
        let key = match (sector.kind, many) {
            (Kind::MoveTo, false) => "sec-did-move",
            (Kind::MoveTo, true) => "sec-did-move-many",
            (Kind::CopyTo, false) => "sec-did-copy",
            (Kind::CopyTo, true) => "sec-did-copy-many",
            (Kind::OpenWith, false) => "sec-did-open",
            (Kind::OpenWith, true) => "sec-did-open-many",
        };
        parts.push(fill(tr(lang, key), &[&what(&r.done, lang), &sector.label]));
    }
    if !r.skipped.is_empty() {
        parts.push(fill(tr(lang, "sec-skipped"), &[&what(&r.skipped, lang)]));
    }
    if !r.failed.is_empty() {
        let mut names: Vec<String> = r.failed.iter().take(3).map(|(n, e)| format!("{n} ({})", e.text(lang))).collect();
        if r.failed.len() > 3 {
            names.push("...".into());
        }
        parts.push(fill(tr(lang, "sec-failed"), &[&r.failed.len().to_string(), &names.join(", ")]));
    }
    if parts.is_empty() {
        parts.push(tr(lang, "sec-nothing").to_string());
    }
    (parts.join(". "), !r.failed.is_empty())
}

#[cfg(not(test))]
fn current_lang() -> Lang {
    crate::current_config().lang
}

#[cfg(test)]
fn current_lang() -> Lang {
    Lang::En
}

/// Runs `sector` on `files` and returns the text of the notice, or why it could not. The moves are recorded so the
/// tray's "Undo last rename or sort" takes them back. `Err` carries a message too: nothing worked.
pub fn run(sector: &Sector, files: &[PathBuf]) -> Result<String, String> {
    let lang = current_lang();
    let report = execute(sector, files).map_err(|p| p.text(lang))?;
    if !report.undo.entries.is_empty() {
        let _ = report.undo.save();
    }
    let (text, failed) = notice_text(sector, &report, lang);
    if failed && report.done.is_empty() {
        Err(text)
    } else {
        Ok(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-sec-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sector(kind: Kind, target: &Path, exts: &[&str]) -> Sector {
        Sector {
            label: "Invoices".into(),
            kind,
            target: target.to_path_buf(),
            exts: exts.iter().map(|e| e.to_string()).collect(),
        }
    }

    fn file(d: &Path, name: &str, body: &str) -> PathBuf {
        let p = d.join(name);
        std::fs::write(&p, body).unwrap();
        p
    }

    fn read(p: &Path) -> String {
        std::fs::read_to_string(p).unwrap()
    }

    // ----- the file -----

    #[test]
    fn a_list_survives_a_round_trip_through_the_file() {
        let list = vec![
            sector(Kind::MoveTo, Path::new(r"D:\Invoices"), &["pdf", "docx"]),
            Sector { label: "USB".into(), ..sector(Kind::CopyTo, Path::new(r"E:\"), &[]) },
            Sector {
                label: "Editor".into(),
                ..sector(Kind::OpenWith, Path::new(r"C:\Program Files\Ed\ed.exe"), &["txt"])
            },
        ];
        assert_eq!(parse(&serialise(&list)), list);
        let d = dir("roundtrip");
        save_in(&d, &list).unwrap();
        assert_eq!(load_in(&d), list);
        assert!(!d.join("sectors.tmp").exists(), "the temp file is gone after the rename");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_file_format_is_the_documented_one() {
        let text = serialise(&[sector(Kind::MoveTo, Path::new(r"D:\Invoices"), &["pdf", "docx"])]);
        let line = text.lines().find(|l| !l.starts_with('#')).unwrap();
        assert_eq!(line, "Invoices\tmove\tD:\\Invoices\tpdf,docx");
    }

    #[test]
    fn loading_normalises_labels_extensions_and_comments() {
        let text = "\u{feff}# my sectors\r\n\r\n  A very long sector name  \tMOVE\t\"D:\\In\"\t.PDF, Docx ,pdf,*,a b\r\nx\tcopy\tE:\\\r\n";
        let got = parse(text);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].label, "A very long", "twelve characters, then the trailing space is trimmed");
        assert_eq!(got[0].kind, Kind::MoveTo);
        assert_eq!(got[0].target, PathBuf::from(r"D:\In"));
        assert_eq!(got[0].exts, ["pdf", "docx", "a", "b"]);
        assert_eq!((got[1].label.as_str(), got[1].kind, got[1].exts.len()), ("x", Kind::CopyTo, 0));
    }

    #[test]
    fn hostile_lines_are_skipped_and_nothing_is_executed() {
        let mut text = String::new();
        text.push_str("only one field\n");
        text.push_str("a\tmove\n");
        text.push_str("a\tdelete\tC:\\x\n");
        text.push_str("a\tmove\tC:\\x\textra\tfields\n");
        text.push_str("\tmove\tC:\\x\n");
        text.push_str("a\tmove\t\n");
        text.push_str("bin\u{0}ary\tmove\tC:\\x\n");
        text.push_str(&format!("big\tmove\t{}\n", "C".repeat(5000)));
        text.push_str("ok\topen\tC:\\Windows\\notepad.exe\n");
        let got = parse(&text);
        assert_eq!(got.len(), 1, "{got:?}");
        assert_eq!(got[0].label, "ok");
        assert!(parse("").is_empty() && parse("\n\n# only comments\n").is_empty());
    }

    #[test]
    fn a_hundred_lines_keep_the_first_eight() {
        let text: String = (0..100).map(|i| format!("s{i}\tmove\tC:\\x{i}\n")).collect();
        let got = parse(&text);
        assert_eq!(got.len(), MAX_SECTORS);
        assert_eq!(got[7].label, "s7");
        let many: Vec<Sector> = (0..20)
            .map(|i| Sector { label: format!("s{i}"), ..sector(Kind::MoveTo, Path::new("C:\\x"), &[]) })
            .collect();
        assert_eq!(parse(&serialise(&many)).len(), MAX_SECTORS, "saving also stops at eight");
    }

    #[test]
    fn a_missing_binary_or_huge_file_is_an_empty_list() {
        let d = dir("damaged");
        assert!(load_in(&d).is_empty(), "missing");
        std::fs::write(d.join("sectors.txt"), [0xffu8, 0xfe, 0x00, 0x9f, 0x92, 0x96, 0x0a, 0x00]).unwrap();
        assert!(load_in(&d).is_empty(), "binary");
        std::fs::write(d.join("sectors.txt"), "a\tmove\tC:\\x\n".repeat(30_000)).unwrap();
        assert!(load_in(&d).is_empty(), "bigger than the limit is treated as damaged");
        assert!(load_in(&d.join("nope")).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn saving_flattens_tabs_and_newlines_and_changes_nothing_else() {
        let s = Sector { label: "a\tb\nc".into(), ..sector(Kind::MoveTo, Path::new("C:\\my\tfolder\r\nx"), &["pdf"]) };
        let text = serialise(&[s]);
        let rows: Vec<&str> = text.lines().filter(|l| !l.starts_with('#')).collect();
        assert_eq!(rows.len(), 1, "one line per sector");
        assert_eq!(rows[0].split('\t').count(), 4);
        let back = parse(&text);
        assert_eq!(back[0].label, "a b c");
        assert_eq!(back[0].target, PathBuf::from("C:\\my folder  x"));
    }

    #[test]
    fn saving_over_an_existing_file_replaces_it_atomically() {
        let d = dir("replace");
        save_in(&d, &[sector(Kind::MoveTo, Path::new("C:\\a"), &[])]).unwrap();
        save_in(&d, &[sector(Kind::CopyTo, Path::new("C:\\b"), &[]), sector(Kind::CopyTo, Path::new("C:\\c"), &[])])
            .unwrap();
        assert_eq!(load_in(&d).len(), 2);
        save_in(&d, &[]).unwrap();
        assert!(load_in(&d).is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn upsert_adds_replaces_and_respects_the_limit() {
        let mut list = Vec::new();
        for i in 0..MAX_SECTORS {
            let s = Sector { label: format!("s{i}"), ..sector(Kind::MoveTo, Path::new("C:\\x"), &[]) };
            upsert(&mut list, None, s).unwrap();
        }
        assert_eq!(upsert(&mut list, None, sector(Kind::MoveTo, Path::new("C:\\y"), &[])), Err(Problem::Full));
        upsert(&mut list, Some(3), Sector { label: "new".into(), ..sector(Kind::CopyTo, Path::new("C:\\z"), &[]) })
            .unwrap();
        assert_eq!((list.len(), list[3].label.as_str()), (MAX_SECTORS, "new"));
        assert!(
            upsert(&mut list, Some(40), sector(Kind::MoveTo, Path::new("C:\\y"), &[])).is_err(),
            "an index past the end is an addition"
        );
    }

    // ----- extensions -----

    #[test]
    fn extensions_are_matched_case_insensitively_and_folders_never_match_a_list() {
        let d = dir("exts");
        let pdf = file(&d, "A.PDF", "x");
        let doc = file(&d, "b.docx", "x");
        let txt = file(&d, "c.txt", "x");
        let none = file(&d, "noext", "x");
        let folder = d.join("looks.pdf");
        std::fs::create_dir(&folder).unwrap();
        let s = sector(Kind::MoveTo, &d, &["pdf", "docx"]);
        assert!(s.applies_to(std::slice::from_ref(&pdf)));
        assert!(s.applies_to(&[pdf.clone(), doc]));
        assert!(!s.applies_to(&[pdf.clone(), txt.clone()]), "every file has to match");
        assert!(!s.applies_to(std::slice::from_ref(&none)) && !s.applies_to(std::slice::from_ref(&folder)));
        assert!(!s.applies_to(&[]));
        let all = sector(Kind::MoveTo, &d, &[]);
        assert!(all.applies_to(&[txt, none, folder]), "no list means everything, folders included");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn typed_extensions_accept_dots_commas_and_spaces() {
        assert_eq!(parse_exts("pdf, docx"), ["pdf", "docx"]);
        assert_eq!(parse_exts(".PDF .Docx;txt"), ["pdf", "docx", "txt"]);
        assert!(parse_exts("  , ,").is_empty() && parse_exts("").is_empty());
        assert_eq!(exts_text(&["pdf".into(), "docx".into()]), "pdf, docx");
    }

    // ----- validation -----

    #[test]
    fn validate_checks_the_label_and_that_the_target_fits_the_kind() {
        let d = dir("validate");
        let prog = file(&d, "p.cmd", "@echo off\r\n");
        let notes = file(&d, "notes.txt", "x");
        assert!(validate(&sector(Kind::MoveTo, &d, &[])).is_ok());
        assert!(validate(&sector(Kind::OpenWith, &prog, &[])).is_ok());
        let mut s = sector(Kind::MoveTo, &d, &[]);
        s.label = "  ".into();
        assert_eq!(validate(&s).unwrap_err(), "Give the sector a name");
        assert!(validate(&sector(Kind::MoveTo, &d.join("missing"), &[])).unwrap_err().contains("missing"));
        assert!(validate(&sector(Kind::CopyTo, &notes, &[])).is_err(), "a file is not a folder");
        assert!(validate(&sector(Kind::OpenWith, &d, &[])).is_err(), "a folder is not a program");
        assert!(validate(&sector(Kind::OpenWith, &notes, &[])).unwrap_err().contains(".exe"));
        assert!(validate(&sector(Kind::OpenWith, &d.join("gone.exe"), &[])).unwrap_err().contains("gone.exe"));
        assert_eq!(validate(&sector(Kind::MoveTo, Path::new(""), &[])).unwrap_err(), "Pick a folder");
        assert_eq!(validate_in(&sector(Kind::MoveTo, Path::new(""), &[]), Lang::Es).unwrap_err(), "Elija una carpeta");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_summary_is_one_line() {
        let s = sector(Kind::MoveTo, Path::new(r"D:\Invoices"), &["pdf", "docx"]);
        assert_eq!(summary(&s, Lang::En), "Move to  D:\\Invoices  .pdf .docx");
        assert_eq!(summary(&sector(Kind::CopyTo, Path::new(r"E:\"), &[]), Lang::Es), "Copiar a  E:\\");
        assert!(summary(&sector(Kind::OpenWith, Path::new(r"C:\a.exe"), &[]), Lang::En).starts_with("Open with"));
    }

    // ----- move and copy -----

    #[test]
    fn a_move_puts_the_file_in_the_folder_and_never_overwrites() {
        let d = dir("move");
        let target = d.join("Invoices");
        std::fs::create_dir(&target).unwrap();
        let a = file(&d, "bill.pdf", "new");
        file(&target, "bill.pdf", "old");
        file(&target, "bill (1).pdf", "old1");
        let r = execute(&sector(Kind::MoveTo, &target, &[]), std::slice::from_ref(&a)).unwrap();
        assert_eq!(r.done, [target.join("bill (2).pdf")]);
        assert_eq!(read(&target.join("bill.pdf")), "old");
        assert_eq!(read(&target.join("bill (1).pdf")), "old1");
        assert_eq!(read(&target.join("bill (2).pdf")), "new");
        assert!(!a.exists());
        assert!(r.failed.is_empty() && r.skipped.is_empty());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_file_already_in_the_folder_is_skipped_not_an_error() {
        let d = dir("same");
        let a = file(&d, "a.txt", "x");
        for kind in [Kind::MoveTo, Kind::CopyTo] {
            let r = execute(&sector(kind, &d, &[]), std::slice::from_ref(&a)).unwrap();
            assert_eq!((r.done.len(), r.skipped.clone(), r.failed.len()), (0, vec![a.clone()], 0));
        }
        // The same folder spelled differently is still the same folder.
        let spelled = d.join(".").join("");
        let r = execute(&sector(Kind::CopyTo, &spelled, &[]), std::slice::from_ref(&a)).unwrap();
        assert_eq!(r.skipped.len(), 1);
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1, "nothing was created");
        let (text, failed) = notice_text(&sector(Kind::MoveTo, &d, &[]), &r, Lang::En);
        assert_eq!((text.as_str(), failed), ("Already in that folder: a.txt", false));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_copy_keeps_the_original_and_numbers_clashes() {
        let d = dir("copy");
        let target = d.join("usb");
        std::fs::create_dir(&target).unwrap();
        let a = file(&d, "photo.jpg", "pixels");
        let one = execute(&sector(Kind::CopyTo, &target, &[]), std::slice::from_ref(&a)).unwrap();
        let two = execute(&sector(Kind::CopyTo, &target, &[]), std::slice::from_ref(&a)).unwrap();
        assert_eq!(one.done, [target.join("photo.jpg")]);
        assert_eq!(two.done, [target.join("photo (1).jpg")]);
        assert_eq!(read(&a), "pixels");
        assert_eq!(read(&target.join("photo (1).jpg")), "pixels");
        assert!(one.undo.entries.is_empty(), "copies leave no undo trail");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_is_moved_by_rename_and_copied_recursively() {
        let d = dir("folders");
        let target = d.join("dest");
        std::fs::create_dir(&target).unwrap();
        let src = d.join("project.v2");
        std::fs::create_dir_all(src.join("sub").join("deep")).unwrap();
        file(&src, "a.txt", "A");
        file(&src.join("sub").join("deep"), "b.txt", "B");
        let copied = execute(&sector(Kind::CopyTo, &target, &[]), std::slice::from_ref(&src)).unwrap();
        assert_eq!(copied.done, [target.join("project.v2")]);
        assert_eq!(read(&target.join("project.v2").join("sub").join("deep").join("b.txt")), "B");
        assert!(src.exists());
        // Copying again: a folder keeps its dotted name whole.
        let again = execute(&sector(Kind::CopyTo, &target, &[]), std::slice::from_ref(&src)).unwrap();
        assert_eq!(again.done, [target.join("project.v2 (1)")]);
        let moved = execute(&sector(Kind::MoveTo, &target, &[]), std::slice::from_ref(&src)).unwrap();
        assert_eq!(moved.done, [target.join("project.v2 (2)")]);
        assert!(!src.exists());
        assert_eq!(read(&target.join("project.v2 (2)").join("a.txt")), "A");
        assert_eq!(moved.undo.entries.len(), 1);
        assert!(moved.undo.entries[0].dir);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_folder_cannot_go_inside_itself() {
        let d = dir("itself");
        let src = d.join("src");
        let inner = src.join("inner");
        std::fs::create_dir_all(&inner).unwrap();
        file(&src, "a.txt", "A");
        for kind in [Kind::CopyTo, Kind::MoveTo] {
            for target in [&inner, &src] {
                let r = execute(&sector(kind, target, &[]), std::slice::from_ref(&src)).unwrap();
                assert_eq!(r.failed.len(), 1, "{kind:?} into {target:?}");
                assert_eq!(r.failed[0].1, Fail::Itself);
                assert!(r.done.is_empty());
            }
        }
        assert_eq!(std::fs::read_dir(&inner).unwrap().count(), 0, "nothing was created inside");
        assert!(src.join("a.txt").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    fn junction(link: &Path, to: &Path) {
        let out = std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &link.display().to_string(), &to.display().to_string()])
            .output()
            .expect("cmd runs");
        assert!(out.status.success() && link.exists(), "mklink /J failed: {}", String::from_utf8_lossy(&out.stdout));
    }

    #[test]
    fn a_junction_inside_a_copied_folder_is_not_followed() {
        let d = dir("junction");
        let secret = d.join("secret");
        std::fs::create_dir(&secret).unwrap();
        file(&secret, "private.txt", "keep out");
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();
        file(&src, "a.txt", "A");
        junction(&src.join("link"), &secret);
        let target = d.join("dest");
        std::fs::create_dir(&target).unwrap();
        let r = execute(&sector(Kind::CopyTo, &target, &[]), std::slice::from_ref(&src)).unwrap();
        assert!(r.failed.is_empty(), "{:?}", r.failed);
        let copy = target.join("src");
        assert_eq!(read(&copy.join("a.txt")), "A");
        assert!(!copy.join("link").exists(), "the junction was skipped, its target not copied");
        assert!(secret.join("private.txt").exists(), "and the target is untouched");
        std::fs::remove_dir(src.join("link")).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_junction_dropped_on_a_copy_sector_is_refused_and_one_inside_blocks_a_cross_drive_move() {
        let d = dir("junction2");
        let real = d.join("real");
        std::fs::create_dir(&real).unwrap();
        file(&real, "x.txt", "X");
        let link = d.join("link");
        junction(&link, &real);
        let target = d.join("dest");
        std::fs::create_dir(&target).unwrap();
        let r = execute(&sector(Kind::CopyTo, &target, &[]), std::slice::from_ref(&link)).unwrap();
        assert_eq!(r.failed.iter().map(|f| f.1.clone()).collect::<Vec<_>>(), [Fail::Links]);
        assert!(!target.join("link").exists());
        // The cross-drive path refuses a folder that holds links, and leaves it whole.
        let src = d.join("src");
        std::fs::create_dir(&src).unwrap();
        file(&src, "a.txt", "A");
        junction(&src.join("inner"), &real);
        let dst = d.join("moved");
        assert_eq!(move_across_drives(&src, &dst, true), Err(Fail::Links));
        assert!(src.join("a.txt").exists() && !dst.exists(), "the original stays and the partial copy is gone");
        std::fs::remove_dir(src.join("inner")).unwrap();
        std::fs::remove_dir(&link).unwrap();
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn copies_stop_at_the_depth_limit() {
        let d = dir("deep");
        let mut p = d.join("src");
        std::fs::create_dir(&p).unwrap();
        for i in 0..(MAX_DEPTH + 2) {
            p = p.join(format!("d{i}"));
        }
        std::fs::create_dir_all(&p).unwrap();
        let target = d.join("dest");
        std::fs::create_dir(&target).unwrap();
        let r = execute(&sector(Kind::CopyTo, &target, &[]), &[d.join("src")]).unwrap();
        assert_eq!(r.failed.iter().map(|f| f.1.clone()).collect::<Vec<_>>(), [Fail::TooDeep]);
        assert!(!target.join("src").exists(), "the partial copy is removed");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_failing_move_leaves_the_original_where_it_was() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = dir("failmove");
        let target = d.join("dest");
        std::fs::create_dir(&target).unwrap();
        let locked = file(&d, "locked.txt", "data");
        let free = file(&d, "free.txt", "data");
        let _hold = std::fs::OpenOptions::new().read(true).share_mode(0).open(&locked).unwrap();
        let r = execute(&sector(Kind::MoveTo, &target, &[]), &[locked.clone(), free.clone()]).unwrap();
        assert_eq!(r.failed.len(), 1);
        assert_eq!(r.failed[0].0, "locked.txt");
        assert!(locked.exists() && !target.join("locked.txt").exists());
        assert_eq!(r.done, [target.join("free.txt")], "the next file still goes");
        let (text, failed) = notice_text(&sector(Kind::MoveTo, &target, &[]), &r, Lang::En);
        assert!(failed && text.starts_with("Moved free.txt to Invoices. 1 failed: locked.txt ("), "{text}");
        drop(_hold);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_move_across_drives_deletes_the_original_only_after_a_good_copy() {
        use std::os::windows::fs::OpenOptionsExt;
        let d = dir("cross");
        let a = file(&d, "a.bin", "0123456789");
        let to = d.join("copy.bin");
        move_across_drives(&a, &to, false).unwrap();
        assert!(!a.exists());
        assert_eq!(read(&to), "0123456789");
        // The original cannot be deleted while it is open for reading without sharing delete: the move fails whole.
        let b = file(&d, "b.bin", "abc");
        let hold = std::fs::OpenOptions::new().read(true).share_mode(1).open(&b).unwrap();
        let to_b = d.join("b-copy.bin");
        assert!(move_across_drives(&b, &to_b, false).is_err());
        assert!(b.exists() && !to_b.exists(), "original kept, partial copy removed");
        drop(hold);
        // A folder goes the same way and is verified file by file.
        let src = d.join("tree");
        std::fs::create_dir_all(src.join("s")).unwrap();
        file(&src.join("s"), "x", "xx");
        let dst = d.join("tree2");
        move_across_drives(&src, &dst, true).unwrap();
        assert!(!src.exists() && dst.join("s").join("x").exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn undo_puts_a_moved_file_back() {
        let d = dir("undo");
        let target = d.join("dest");
        std::fs::create_dir(&target).unwrap();
        let a = file(&d, "a.txt", "A");
        let b = file(&d, "b.txt", "B");
        let r = execute(&sector(Kind::MoveTo, &target, &[]), &[a.clone(), b.clone()]).unwrap();
        assert_eq!(r.undo.entries.len(), 2);
        let saved = r.undo.save_in(&d.join("undo")).unwrap();
        let loaded = Undo::load(&saved).unwrap();
        crate::engine::rename::undo(&loaded).unwrap();
        assert_eq!((read(&a), read(&b)), ("A".into(), "B".into()));
        assert_eq!(std::fs::read_dir(&target).unwrap().count(), 0);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_target_that_is_not_a_folder_touches_nothing() {
        let d = dir("nofolder");
        let a = file(&d, "a.txt", "A");
        let missing = d.join("missing");
        for kind in [Kind::MoveTo, Kind::CopyTo] {
            let e = execute(&sector(kind, &missing, &[]), std::slice::from_ref(&a)).unwrap_err();
            assert_eq!(e, Problem::NoFolder(missing.clone()));
            assert!(e.text(Lang::En).contains("missing"));
        }
        assert_eq!(
            execute(&sector(Kind::MoveTo, &a, &[]), std::slice::from_ref(&a)).unwrap_err(),
            Problem::NoFolder(a.clone())
        );
        assert_eq!(execute(&sector(Kind::MoveTo, &d, &[]), &[]).unwrap_err(), Problem::NoFiles);
        assert!(a.exists());
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_missing_source_is_reported_and_the_rest_goes_on() {
        let d = dir("vanished");
        let target = d.join("dest");
        std::fs::create_dir(&target).unwrap();
        let a = file(&d, "a.txt", "A");
        let r = execute(&sector(Kind::CopyTo, &target, &[]), &[d.join("ghost.txt"), a]).unwrap();
        assert_eq!((r.failed.len(), r.done.len()), (1, 1));
        assert_eq!(r.failed[0].0, "ghost.txt");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn the_notice_counts_files_and_speaks_both_languages() {
        let s = sector(Kind::CopyTo, Path::new("C:\\x"), &[]);
        let r = RunReport {
            done: vec![PathBuf::from("C:\\x\\a"), PathBuf::from("C:\\x\\b"), PathBuf::from("C:\\x\\c")],
            ..Default::default()
        };
        assert_eq!(notice_text(&s, &r, Lang::En).0, "Copied 3 files to Invoices");
        assert_eq!(notice_text(&s, &r, Lang::Es).0, "Se copiaron 3 archivos a Invoices");
        let one = RunReport { done: vec![PathBuf::from("C:\\x\\a.pdf")], ..Default::default() };
        assert_eq!(
            notice_text(&sector(Kind::MoveTo, Path::new("C:\\x"), &[]), &one, Lang::En).0,
            "Moved a.pdf to Invoices"
        );
        assert_eq!(
            notice_text(&sector(Kind::OpenWith, Path::new("C:\\x.exe"), &[]), &one, Lang::En).0,
            "Opened a.pdf with Invoices"
        );
        assert_eq!(notice_text(&s, &RunReport::default(), Lang::En).0, "Nothing to do");
    }

    #[test]
    fn run_reports_a_problem_with_the_sector_as_an_error() {
        let d = dir("run");
        let a = file(&d, "a.txt", "A");
        let e = run(&sector(Kind::MoveTo, &d.join("nope"), &[]), std::slice::from_ref(&a)).unwrap_err();
        assert!(e.contains("nope"), "{e}");
        let target = d.join("t");
        std::fs::create_dir(&target).unwrap();
        let ok = run(&sector(Kind::CopyTo, &target, &[]), &[a]).unwrap();
        assert_eq!(ok, "Copied a.txt to Invoices");
        let _ = std::fs::remove_dir_all(&d);
    }

    // ----- open with -----

    /// A batch file that copies its first argument next to itself: proves the path arrived whole.
    fn fake_program(d: &Path) -> PathBuf {
        file(d, "fake.cmd", "@echo off\r\ncopy \"%~1\" \"%~dp0marker-%~nx1\" > nul\r\n")
    }

    fn wait_for(p: &Path) -> bool {
        for _ in 0..100 {
            if p.exists() {
                return true;
            }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        false
    }

    #[test]
    fn open_with_quotes_a_path_with_spaces_and_an_ampersand() {
        let d = dir("open");
        let prog = fake_program(&d);
        let odd = d.join("my files & more");
        std::fs::create_dir(&odd).unwrap();
        let doc = file(&odd, "the report (final).txt", "hello");
        let r = execute(&sector(Kind::OpenWith, &prog, &[]), std::slice::from_ref(&doc)).unwrap();
        assert_eq!(r.done, std::slice::from_ref(&doc));
        let marker = d.join("marker-the report (final).txt");
        assert!(wait_for(&marker), "the program never saw the file");
        assert_eq!(read(&marker), "hello");
        assert!(!d.join("more").exists() && !odd.join("more").exists(), "the ampersand did not start another command");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn open_with_refuses_more_than_ten_files_and_bad_programs() {
        let d = dir("openmany");
        let prog = fake_program(&d);
        let files: Vec<PathBuf> = (0..11).map(|i| file(&d, &format!("f{i}.txt"), "x")).collect();
        assert_eq!(execute(&sector(Kind::OpenWith, &prog, &[]), &files).unwrap_err(), Problem::TooMany(11));
        assert!(Problem::TooMany(11).text(Lang::En).contains("11"));
        std::thread::sleep(std::time::Duration::from_millis(300));
        assert!(!d.join("marker-f0.txt").exists(), "nothing was started");
        let txt = d.join("f0.txt");
        assert_eq!(execute(&sector(Kind::OpenWith, &txt, &[]), &files[..1]).unwrap_err(), Problem::NotProgram(txt));
        assert!(matches!(
            execute(&sector(Kind::OpenWith, &d.join("x.exe"), &[]), &files[..1]),
            Err(Problem::NoProgram(_))
        ));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_batch_file_never_gets_a_percent_sign_and_nobody_gets_a_quote() {
        let d = dir("openpct");
        let prog = fake_program(&d);
        let pct = file(&d, "100%.txt", "x");
        let r = execute(&sector(Kind::OpenWith, &prog, &[]), &[pct]).unwrap();
        assert_eq!(r.failed.iter().map(|f| f.1.clone()).collect::<Vec<_>>(), [Fail::Quote]);
        assert!(r.done.is_empty());
        assert_eq!(open_one(&prog, Path::new("C:\\a\"b.txt")), Err(Fail::Quote));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn every_sector_string_exists_in_both_languages() {
        for key in [
            "sec-e-name",
            "sec-e-nofolder",
            "sec-e-noprog",
            "sec-e-folder",
            "sec-e-prog",
            "sec-e-progtype",
            "sec-e-many",
            "sec-e-full",
            "sec-nothing",
            "sec-f-itself",
            "sec-f-deep",
            "sec-f-links",
            "sec-f-quote",
            "sec-f-taken",
            "sec-did-move",
            "sec-did-copy",
            "sec-did-open",
            "sec-did-move-many",
            "sec-did-copy-many",
            "sec-did-open-many",
            "sec-n-files",
            "sec-skipped",
            "sec-failed",
            "sec-sum-move",
            "sec-sum-copy",
            "sec-sum-open",
        ] {
            for l in [Lang::En, Lang::Es] {
                assert!(!tr(l, key).is_empty(), "{key}");
            }
        }
        for f in [Fail::Itself, Fail::TooDeep, Fail::Links, Fail::Quote, Fail::Taken] {
            assert_ne!(f.text(Lang::En), f.text(Lang::Es));
        }
    }
}
