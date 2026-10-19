//! Batch rename: rules, a plan, a safe apply and an undo.
//!
//! `plan` turns a rule into one [`Item`] per path (old name, new name, status) without touching the disk beyond
//! reading it; `apply` performs a plan all-or-nothing and returns an [`Undo`]; `undo` reverses it.
//!
//! The rule edits the file *stem* only (folders are renamed as a whole). Order of operations: find/replace,
//! accents, spaces, case, prefix/suffix, date, numbering. Two details are deliberate:
//! * numbering at `Replace` puts the number where the stem was *before* the prefix, suffix and date are added,
//!   so `Trip ` + `Replace` gives `Trip 001`;
//! * the separator between a stem and a number or date follows the spaces option (a space, `_`, `-` or `.`;
//!   `_` when spaces are removed).
//!
//! Reasons carried by [`Status`] are stable codes the UI localizes: `Invalid`: `missing`, `empty`, `illegal:<c>`,
//! `reserved`, `trailing`, `long`; `Conflict`: `duplicate` (another item ends up with the same name), `exists`
//! (the name is taken by a file that is not being renamed away).

use std::collections::{HashMap, HashSet};
use std::fs::{File, Metadata};
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use super::error::{Error, Result};

/// Longest accepted name, in characters (Windows allows 255; the margin leaves room for `(1)` style suffixes).
pub const MAX_NAME: usize = 240;
/// Undo files kept in the undo folder.
pub const KEEP_UNDO: usize = 20;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Case {
    #[default]
    None,
    Lower,
    Upper,
    Title,
    Sentence,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum Spaces {
    #[default]
    Keep,
    Underscore,
    Dash,
    Dot,
    Remove,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum NumPos {
    #[default]
    Prefix,
    Suffix,
    Replace,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Numbering {
    pub start: u64,
    pub step: u64,
    /// Minimum digits; shorter numbers are padded with zeros, longer ones are never cut.
    pub digits: usize,
    pub position: NumPos,
}

impl Default for Numbering {
    fn default() -> Numbering {
        Numbering { start: 1, step: 1, digits: 3, position: NumPos::Prefix }
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateSource {
    #[default]
    Modified,
    Created,
    /// EXIF DateTimeOriginal of a JPEG or TIFF, the modified time for everything else.
    Taken,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DateFormat {
    /// `2024-05-01`
    #[default]
    Iso,
    /// `20240501`
    Compact,
    /// `2024-05-01_13-45`
    IsoTime,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum DatePos {
    #[default]
    Prefix,
    Suffix,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct DateStamp {
    pub source: DateSource,
    pub format: DateFormat,
    pub position: DatePos,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Rule {
    /// Plain, case-insensitive text to find in the stem (no pattern syntax); empty does nothing.
    pub find: String,
    pub replace: String,
    pub case: Case,
    pub strip_accents: bool,
    pub spaces: Spaces,
    pub prefix: String,
    pub suffix: String,
    pub numbering: Option<Numbering>,
    pub date: Option<DateStamp>,
    pub ext_lowercase: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Status {
    /// Will be renamed.
    Ok,
    /// The name does not change.
    Unchanged,
    /// The new name collides with another item or an existing file (see the module docs for the codes).
    Conflict(String),
    /// The new name cannot exist on Windows, or the file cannot be read.
    Invalid(String),
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub from: PathBuf,
    pub to: PathBuf,
    pub status: Status,
}

impl Item {
    pub fn old_name(&self) -> String {
        name_of(&self.from)
    }

    pub fn new_name(&self) -> String {
        name_of(&self.to)
    }
}

fn name_of(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

// ================================================================================================ naming

/// `(stem, extension including its dot)`; folders and names without a dot after the first character have no extension.
fn split_name(name: &str, dir: bool) -> (&str, &str) {
    if dir {
        return (name, "");
    }
    match name.rfind('.') {
        Some(i) if i > 0 => (&name[..i], &name[i..]),
        _ => (name, ""),
    }
}

fn fold(c: char) -> char {
    c.to_lowercase().next().unwrap_or(c)
}

/// Replaces every case-insensitive occurrence of `find` (plain text) in `s`.
fn replace_ci(s: &str, find: &str, with: &str) -> String {
    let needle: Vec<char> = find.chars().map(fold).collect();
    if needle.is_empty() {
        return s.to_string();
    }
    let hay: Vec<char> = s.chars().collect();
    let folded: Vec<char> = hay.iter().map(|&c| fold(c)).collect();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < hay.len() {
        if folded[i..].starts_with(&needle) {
            out.push_str(with);
            i += needle.len();
        } else {
            out.push(hay[i]);
            i += 1;
        }
    }
    out
}

/// Latin letters with diacritics become their plain form; combining marks vanish; other scripts are untouched.
fn strip_accents(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        let plain = match c {
            '\u{300}'..='\u{36F}' => "",
            'À' | 'Á' | 'Â' | 'Ã' | 'Ä' | 'Å' | 'Ā' | 'Ă' | 'Ą' => "A",
            'à' | 'á' | 'â' | 'ã' | 'ä' | 'å' | 'ā' | 'ă' | 'ą' => "a",
            'Æ' => "AE",
            'æ' => "ae",
            'Ç' | 'Ć' | 'Ĉ' | 'Ċ' | 'Č' => "C",
            'ç' | 'ć' | 'ĉ' | 'ċ' | 'č' => "c",
            'Ď' | 'Đ' | 'Ð' => "D",
            'ď' | 'đ' | 'ð' => "d",
            'È' | 'É' | 'Ê' | 'Ë' | 'Ē' | 'Ĕ' | 'Ė' | 'Ę' | 'Ě' => "E",
            'è' | 'é' | 'ê' | 'ë' | 'ē' | 'ĕ' | 'ė' | 'ę' | 'ě' => "e",
            'Ĝ' | 'Ğ' | 'Ġ' | 'Ģ' => "G",
            'ĝ' | 'ğ' | 'ġ' | 'ģ' => "g",
            'Ĥ' | 'Ħ' => "H",
            'ĥ' | 'ħ' => "h",
            'Ì' | 'Í' | 'Î' | 'Ï' | 'Ĩ' | 'Ī' | 'Ĭ' | 'Į' | 'İ' => "I",
            'ì' | 'í' | 'î' | 'ï' | 'ĩ' | 'ī' | 'ĭ' | 'į' | 'ı' => "i",
            'Ĵ' => "J",
            'ĵ' => "j",
            'Ķ' => "K",
            'ķ' => "k",
            'Ĺ' | 'Ļ' | 'Ľ' | 'Ŀ' | 'Ł' => "L",
            'ĺ' | 'ļ' | 'ľ' | 'ŀ' | 'ł' => "l",
            'Ñ' | 'Ń' | 'Ņ' | 'Ň' => "N",
            'ñ' | 'ń' | 'ņ' | 'ň' => "n",
            'Ò' | 'Ó' | 'Ô' | 'Õ' | 'Ö' | 'Ø' | 'Ō' | 'Ŏ' | 'Ő' => "O",
            'ò' | 'ó' | 'ô' | 'õ' | 'ö' | 'ø' | 'ō' | 'ŏ' | 'ő' => "o",
            'Œ' => "OE",
            'œ' => "oe",
            'Ŕ' | 'Ŗ' | 'Ř' => "R",
            'ŕ' | 'ŗ' | 'ř' => "r",
            'Ś' | 'Ŝ' | 'Ş' | 'Š' => "S",
            'ś' | 'ŝ' | 'ş' | 'š' => "s",
            'ß' => "ss",
            'Ţ' | 'Ť' | 'Ŧ' => "T",
            'ţ' | 'ť' | 'ŧ' => "t",
            'Þ' => "Th",
            'þ' => "th",
            'Ù' | 'Ú' | 'Û' | 'Ü' | 'Ũ' | 'Ū' | 'Ŭ' | 'Ů' | 'Ű' | 'Ų' => "U",
            'ù' | 'ú' | 'û' | 'ü' | 'ũ' | 'ū' | 'ŭ' | 'ů' | 'ű' | 'ų' => "u",
            'Ŵ' => "W",
            'ŵ' => "w",
            'Ý' | 'Ŷ' | 'Ÿ' => "Y",
            'ý' | 'ÿ' | 'ŷ' => "y",
            'Ź' | 'Ż' | 'Ž' => "Z",
            'ź' | 'ż' | 'ž' => "z",
            _ => {
                out.push(c);
                continue;
            }
        };
        out.push_str(plain);
    }
    out
}

fn title_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut new_word = true;
    for c in s.chars() {
        if c.is_alphanumeric() {
            if new_word {
                out.extend(c.to_uppercase());
            } else {
                out.extend(c.to_lowercase());
            }
            new_word = false;
        } else {
            out.push(c);
            // An apostrophe stays inside its word (don't, o'brien).
            if c != '\'' && c != '\u{2019}' {
                new_word = true;
            }
        }
    }
    out
}

fn sentence_case(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut first = true;
    for c in s.chars() {
        if c.is_alphabetic() {
            if first {
                out.extend(c.to_uppercase());
                first = false;
            } else {
                out.extend(c.to_lowercase());
            }
        } else {
            out.push(c);
        }
    }
    out
}

fn apply_spaces(s: &str, mode: Spaces) -> String {
    let with = match mode {
        Spaces::Keep => return s.to_string(),
        Spaces::Underscore => "_",
        Spaces::Dash => "-",
        Spaces::Dot => ".",
        Spaces::Remove => "",
    };
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        if c.is_whitespace() {
            out.push_str(with);
        } else {
            out.push(c);
        }
    }
    out
}

/// What goes between a stem and a number or a date.
fn separator(mode: Spaces) -> &'static str {
    match mode {
        Spaces::Keep => " ",
        Spaces::Underscore | Spaces::Remove => "_",
        Spaces::Dash => "-",
        Spaces::Dot => ".",
    }
}

fn join(first: &str, sep: &str, second: &str) -> String {
    if first.is_empty() || second.is_empty() {
        format!("{first}{second}")
    } else {
        format!("{first}{sep}{second}")
    }
}

fn stamp_text(p: Parts, f: DateFormat) -> String {
    match f {
        DateFormat::Iso => format!("{:04}-{:02}-{:02}", p.y, p.mo, p.d),
        DateFormat::Compact => format!("{:04}{:02}{:02}", p.y, p.mo, p.d),
        DateFormat::IsoTime => format!("{:04}-{:02}-{:02}_{:02}-{:02}", p.y, p.mo, p.d, p.h, p.mi),
    }
}

/// The new stem for the item at `index` of the batch.
fn new_stem(stem: &str, rule: &Rule, index: usize, stamp: Option<&str>) -> String {
    let mut s = replace_ci(stem, &rule.find, &rule.replace);
    if rule.strip_accents {
        s = strip_accents(&s);
    }
    s = apply_spaces(&s, rule.spaces);
    s = match rule.case {
        Case::None => s,
        Case::Lower => s.to_lowercase(),
        Case::Upper => s.to_uppercase(),
        Case::Title => title_case(&s),
        Case::Sentence => sentence_case(&s),
    };
    let sep = separator(rule.spaces);
    let number = rule.numbering.map(|n| {
        let value = n.start.saturating_add((index as u64).saturating_mul(n.step));
        (format!("{value:0width$}", width = n.digits), n.position)
    });
    if let Some((num, NumPos::Replace)) = &number {
        s = num.clone();
    }
    s = format!("{}{}{}", rule.prefix, s, rule.suffix);
    if let (Some(d), Some(text)) = (&rule.date, stamp) {
        s = match d.position {
            DatePos::Prefix => join(text, sep, &s),
            DatePos::Suffix => join(&s, sep, text),
        };
    }
    match number {
        Some((num, NumPos::Prefix)) => join(&num, sep, &s),
        Some((num, NumPos::Suffix)) => join(&s, sep, &num),
        _ => s,
    }
}

fn is_reserved(name: &str) -> bool {
    let base = name.split('.').next().unwrap_or("").trim_end().to_ascii_uppercase();
    matches!(base.as_str(), "CON" | "PRN" | "AUX" | "NUL")
        || ((base.starts_with("COM") || base.starts_with("LPT"))
            && base.len() == 4
            && base.as_bytes()[3].is_ascii_digit()
            && base.as_bytes()[3] != b'0')
}

/// `Err(code)` when `name` cannot be a file name on Windows.
fn check_name(name: &str, stem: &str) -> std::result::Result<(), String> {
    if stem.is_empty() {
        return Err("empty".into());
    }
    let forbidden = |c: &char| matches!(c, '<' | '>' | ':' | '"' | '/' | '\\' | '|' | '?' | '*') || (*c as u32) < 32;
    if let Some(c) = name.chars().find(forbidden) {
        return Err(format!("illegal:{c}"));
    }
    if is_reserved(name) {
        return Err("reserved".into());
    }
    if name.ends_with('.') || name.ends_with(' ') {
        return Err("trailing".into());
    }
    if name.chars().count() > MAX_NAME {
        return Err("long".into());
    }
    Ok(())
}

// ================================================================================================ dates

/// A date and time of day, to the minute.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Parts {
    pub y: i32,
    pub mo: u32,
    pub d: u32,
    pub h: u32,
    pub mi: u32,
}

/// The wall-clock time in the user's time zone; `None` before 1970 or when Windows cannot convert it.
pub fn local_parts(t: SystemTime) -> Option<Parts> {
    use std::mem::zeroed;
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let d = t.duration_since(UNIX_EPOCH).ok()?;
    let ticks = (d.as_secs() + 11_644_473_600) * 10_000_000 + u64::from(d.subsec_nanos() / 100);
    let ft = FILETIME { dwLowDateTime: ticks as u32, dwHighDateTime: (ticks >> 32) as u32 };
    // SAFETY: plain out-parameters owned by this frame; both calls only write the structs they are given.
    unsafe {
        let mut utc: SYSTEMTIME = zeroed();
        if FileTimeToSystemTime(&ft, &mut utc) == 0 {
            return None;
        }
        let mut local: SYSTEMTIME = zeroed();
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut local) == 0 {
            return None;
        }
        Some(Parts {
            y: i32::from(local.wYear),
            mo: u32::from(local.wMonth),
            d: u32::from(local.wDay),
            h: u32::from(local.wHour),
            mi: u32::from(local.wMinute),
        })
    }
}

fn byte_order(tiff: &[u8]) -> Option<bool> {
    match tiff.get(..4)? {
        b"MM\0*" => Some(true),
        b"II*\0" => Some(false),
        _ => None,
    }
}

fn rd16(b: &[u8], at: usize, be: bool) -> Option<u16> {
    let v: [u8; 2] = b.get(at..at.checked_add(2)?)?.try_into().ok()?;
    Some(if be { u16::from_be_bytes(v) } else { u16::from_le_bytes(v) })
}

fn rd32(b: &[u8], at: usize, be: bool) -> Option<u32> {
    let v: [u8; 4] = b.get(at..at.checked_add(4)?)?.try_into().ok()?;
    Some(if be { u32::from_be_bytes(v) } else { u32::from_le_bytes(v) })
}

/// The entry with `tag` in the IFD at `ifd`: `(type, count, position of its value field)`.
fn ifd_entry(tiff: &[u8], ifd: usize, tag: u16, be: bool) -> Option<(u16, u32, usize)> {
    let n = usize::from(rd16(tiff, ifd, be)?);
    (0..n).find_map(|i| {
        let at = ifd + 2 + i * 12;
        if rd16(tiff, at, be)? != tag {
            return None;
        }
        Some((rd16(tiff, at + 2, be)?, rd32(tiff, at + 4, be)?, at + 8))
    })
}

/// DateTimeOriginal from a TIFF block (the body of an EXIF segment, or a whole TIFF file).
pub fn exif_taken(tiff: &[u8]) -> Option<Parts> {
    let be = byte_order(tiff)?;
    let ifd0 = rd32(tiff, 4, be)? as usize;
    let (_, _, field) = ifd_entry(tiff, ifd0, 0x8769, be)?;
    let exif = rd32(tiff, field, be)? as usize;
    let (kind, count, field) = ifd_entry(tiff, exif, 0x9003, be)?;
    if kind != 2 || count < 19 {
        return None;
    }
    let at = rd32(tiff, field, be)? as usize;
    let s = tiff.get(at..at.checked_add(19)?)?;
    let num = |r: std::ops::Range<usize>| -> Option<u32> {
        let t = std::str::from_utf8(&s[r]).ok()?;
        if t.bytes().all(|b| b.is_ascii_digit()) {
            t.parse().ok()
        } else {
            None
        }
    };
    let p = Parts { y: num(0..4)? as i32, mo: num(5..7)?, d: num(8..10)?, h: num(11..13)?, mi: num(14..16)? };
    ((1..=12).contains(&p.mo) && (1..=31).contains(&p.d) && p.h < 24 && p.mi < 60 && p.y > 0).then_some(p)
}

/// The TIFF block of a JPEG's EXIF segment, or the start of a TIFF file. Reads one segment, never the picture.
fn read_exif_block(f: &mut File) -> Option<Vec<u8>> {
    let mut head = [0u8; 4];
    f.read_exact(&mut head).ok()?;
    if byte_order(&head).is_some() {
        f.seek(SeekFrom::Start(0)).ok()?;
        let mut buf = Vec::new();
        f.take(256 * 1024).read_to_end(&mut buf).ok()?;
        return Some(buf);
    }
    if head[..2] != [0xFF, 0xD8] {
        return None;
    }
    f.seek(SeekFrom::Start(2)).ok()?;
    for _ in 0..32 {
        let mut m = [0u8; 4];
        f.read_exact(&mut m).ok()?;
        if m[0] != 0xFF {
            return None;
        }
        let len = usize::from(u16::from_be_bytes([m[2], m[3]]));
        match m[1] {
            0xDA | 0xD9 => return None,
            0xE1 if len >= 8 => {
                let mut seg = vec![0u8; len - 2];
                f.read_exact(&mut seg).ok()?;
                if seg.starts_with(b"Exif\0\0") {
                    seg.drain(..6);
                    return Some(seg);
                }
            }
            _ => {
                f.seek(SeekFrom::Current(len.saturating_sub(2) as i64)).ok()?;
            }
        }
    }
    None
}

/// What planning needs to know about one path, read once.
struct Info {
    dir: bool,
    modified: Option<SystemTime>,
    created: Option<SystemTime>,
    /// EXIF date, read the first time a rule asks for it (`Some(None)`: looked, none found).
    taken: Option<Option<Parts>>,
}

/// File facts remembered between plans. Planning runs on every keystroke of a dialog and must not stat or reread
/// thousands of files each time; the owner calls [`Scan::forget`] when the files may have changed (after an apply
/// or an undo).
#[derive(Default)]
pub struct Scan {
    files: HashMap<PathBuf, Option<Info>>,
}

impl Scan {
    pub fn forget(&mut self) {
        self.files.clear();
    }

    fn info(&mut self, p: &Path) -> Option<&mut Info> {
        self.files
            .entry(p.to_path_buf())
            .or_insert_with(|| {
                let m = std::fs::symlink_metadata(p).ok()?;
                Some(Info { dir: m.is_dir(), modified: m.modified().ok(), created: m.created().ok(), taken: None })
            })
            .as_mut()
    }
}

fn stamp_for(info: &mut Info, path: &Path, d: &DateStamp) -> Option<String> {
    let parts = match d.source {
        DateSource::Modified => info.modified.and_then(local_parts),
        DateSource::Created => info.created.and_then(local_parts),
        DateSource::Taken => {
            let taken = *info.taken.get_or_insert_with(|| {
                File::open(path).ok().and_then(|mut f| read_exif_block(&mut f)).and_then(|b| exif_taken(&b))
            });
            taken.or_else(|| info.modified.and_then(local_parts))
        }
    }?;
    Some(stamp_text(parts, d.format))
}

// ================================================================================================ plan

fn path_key(p: &Path) -> String {
    let parent = p.parent().map(|d| d.to_string_lossy().into_owned()).unwrap_or_default();
    format!("{}\u{1}{}", parent.to_lowercase(), name_of(p).to_lowercase())
}

/// Plans `rule` over `paths`; the numbering follows the order of `paths`. Reads the disk (metadata, EXIF, folder
/// listings) but never changes it.
pub fn plan(paths: &[PathBuf], rule: &Rule) -> Vec<Item> {
    plan_with(&mut Scan::default(), paths, rule)
}

/// [`plan`] that reuses what `scan` already knows about the files.
pub fn plan_with(scan: &mut Scan, paths: &[PathBuf], rule: &Rule) -> Vec<Item> {
    let mut items: Vec<Item> = Vec::with_capacity(paths.len());
    for (index, from) in paths.iter().enumerate() {
        let name = name_of(from);
        let Some(info) = scan.info(from) else {
            items.push(Item { from: from.clone(), to: from.clone(), status: Status::Invalid("missing".into()) });
            continue;
        };
        let dir = info.dir;
        let (stem, ext) = split_name(&name, dir);
        let stamp = rule.date.as_ref().and_then(|d| stamp_for(info, from, d));
        let stem2 = new_stem(stem, rule, index, stamp.as_deref());
        let ext2 = if rule.ext_lowercase && !dir { ext.to_lowercase() } else { ext.to_string() };
        let new_name = format!("{stem2}{ext2}");
        let status = match check_name(&new_name, &stem2) {
            Err(why) => Status::Invalid(why),
            Ok(()) if new_name == name => Status::Unchanged,
            Ok(()) => Status::Ok,
        };
        items.push(Item { from: from.clone(), to: from.with_file_name(&new_name), status });
    }
    mark_conflicts(&mut items);
    items
}

/// Turns `Ok` items whose target is taken into `Conflict`s.
fn mark_conflicts(items: &mut [Item]) {
    // Two items ending up with one name; an unchanged item keeps the name it has.
    let finals: Vec<Option<String>> =
        items.iter().map(|i| (!matches!(i.status, Status::Invalid(_))).then(|| path_key(&i.to))).collect();
    let mut count: HashMap<&str, usize> = HashMap::new();
    for k in finals.iter().flatten() {
        *count.entry(k.as_str()).or_insert(0) += 1;
    }
    let dup: Vec<bool> = finals.iter().map(|k| k.as_deref().is_some_and(|k| count[k] > 1)).collect();
    for (item, dup) in items.iter_mut().zip(dup) {
        if dup && item.status == Status::Ok {
            item.status = Status::Conflict("duplicate".into());
        }
    }
    // Names on disk, one listing per folder.
    let mut listing: HashMap<String, HashSet<String>> = HashMap::new();
    for item in items.iter().filter(|i| i.status == Status::Ok) {
        let parent = item.to.parent().map(Path::to_path_buf).unwrap_or_default();
        listing.entry(parent.to_string_lossy().to_lowercase()).or_insert_with(|| {
            std::fs::read_dir(&parent)
                .map(|rd| rd.flatten().map(|e| e.file_name().to_string_lossy().to_lowercase()).collect())
                .unwrap_or_default()
        });
    }
    let on_disk = |p: &Path| -> bool {
        let parent = p.parent().map(|d| d.to_string_lossy().to_lowercase()).unwrap_or_default();
        listing.get(&parent).is_some_and(|names| names.contains(&name_of(p).to_lowercase()))
    };
    // A name that is taken is free again when its owner is renamed away in this batch. That owner may itself
    // turn out blocked, so repeat until nothing changes.
    let mut leaving: HashSet<String> =
        items.iter().filter(|i| i.status == Status::Ok).map(|i| path_key(&i.from)).collect();
    loop {
        let mut changed = false;
        for item in items.iter_mut() {
            if item.status == Status::Ok && on_disk(&item.to) && !leaving.contains(&path_key(&item.to)) {
                item.status = Status::Conflict("exists".into());
                leaving.remove(&path_key(&item.from));
                changed = true;
            }
        }
        if !changed {
            break;
        }
    }
}

// ================================================================================================ apply

#[link(name = "kernel32")]
extern "system" {
    fn MoveFileExW(existing: *const u16, new: *const u16, flags: u32) -> i32;
}

fn wide(p: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt;
    p.as_os_str().encode_wide().chain(Some(0)).collect()
}

/// Renames without ever replacing: an existing target is an error, whatever its type.
fn move_no_replace(from: &Path, to: &Path) -> std::io::Result<()> {
    let (a, b) = (wide(from), wide(to));
    // SAFETY: both buffers are NUL-terminated and outlive the call; flags 0 means "do not replace".
    if unsafe { MoveFileExW(a.as_ptr(), b.as_ptr(), 0) } == 0 {
        return Err(std::io::Error::last_os_error());
    }
    Ok(())
}

fn temp_beside(p: &Path, seq: usize) -> PathBuf {
    let dir = p.parent().map(Path::to_path_buf).unwrap_or_default();
    let nanos = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos());
    let mut n = seq;
    loop {
        let t = dir.join(format!(".radial-rename-{}-{nanos}-{n}.tmp", std::process::id()));
        if std::fs::symlink_metadata(&t).is_err() {
            return t;
        }
        n += 1_000_000;
    }
}

type Moves = [(PathBuf, PathBuf)];

/// Performs `(from, to)` renames, handling chains and swaps through temporary names. On failure everything done
/// so far is undone, in reverse; the error says so when even that was not possible.
fn execute(moves: &Moves) -> Result<()> {
    let sources: HashSet<String> = moves.iter().map(|(f, _)| path_key(f)).collect();
    let mut log: Vec<(PathBuf, PathBuf)> = Vec::new(); // (where it is now, where it was)
    let mut staged: Vec<(PathBuf, PathBuf)> = Vec::new(); // (temporary name, final name)
    let step = |a: &Path, b: &Path, log: &mut Vec<(PathBuf, PathBuf)>| -> Result<()> {
        move_no_replace(a, b).map_err(|e| Error::Io(format!("{}: {e}", a.display())))?;
        log.push((b.to_path_buf(), a.to_path_buf()));
        Ok(())
    };
    let mut run = || -> Result<()> {
        for (seq, (from, to)) in moves.iter().enumerate() {
            if sources.contains(&path_key(to)) {
                let tmp = temp_beside(from, seq);
                step(from, &tmp, &mut log)?;
                staged.push((tmp, to.clone()));
            } else {
                step(from, to, &mut log)?;
            }
        }
        for (tmp, to) in &staged {
            step(tmp, to, &mut log)?;
        }
        Ok(())
    };
    if let Err(e) = run() {
        let stuck: Vec<String> = log
            .iter()
            .rev()
            .filter(|(now, before)| move_no_replace(now, before).is_err())
            .map(|(now, _)| now.display().to_string())
            .collect();
        return Err(if stuck.is_empty() {
            e
        } else {
            Error::Io(format!("{e}; could not put back: {}", stuck.join(", ")))
        });
    }
    Ok(())
}

fn unix_nanos(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_nanos() as u64)
}

/// Renames every `Ok` item. Refuses a plan that holds a `Conflict` or `Invalid` item. All or nothing as far as
/// the file system allows: a failure puts back what was already renamed.
pub fn apply(plan: &[Item]) -> Result<Undo> {
    if let Some(bad) = plan.iter().find(|i| matches!(i.status, Status::Conflict(_) | Status::Invalid(_))) {
        let why = match &bad.status {
            Status::Conflict(w) | Status::Invalid(w) => w.as_str(),
            _ => "",
        };
        return Err(Error::InvalidArgument(format!("{}: {why}", bad.from.display())));
    }
    let moves: Vec<(PathBuf, PathBuf)> =
        plan.iter().filter(|i| i.status == Status::Ok).map(|i| (i.from.clone(), i.to.clone())).collect();
    execute(&moves)?;
    let entries = moves
        .into_iter()
        .map(|(original, current)| {
            let m = std::fs::symlink_metadata(&current).ok();
            UndoEntry {
                dir: m.as_ref().is_some_and(|m| m.is_dir()),
                len: m.as_ref().map_or(0, Metadata::len),
                modified: m.as_ref().and_then(|m| m.modified().ok()).map_or(0, unix_nanos),
                current,
                original,
            }
        })
        .collect();
    Ok(Undo { entries })
}

// ================================================================================================ undo

/// One renamed file: where it is now and where it came from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct UndoEntry {
    pub current: PathBuf,
    pub original: PathBuf,
    pub dir: bool,
    /// Size and modified time (ns since 1970) right after the rename; a file that differs was edited since.
    pub len: u64,
    pub modified: u64,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Undo {
    pub entries: Vec<UndoEntry>,
}

/// What an undo did: how many names came back and which entries were left alone, with a code (`missing`,
/// `changed`, `taken`).
#[derive(Debug, Default)]
pub struct UndoReport {
    pub restored: usize,
    pub skipped: Vec<(PathBuf, String)>,
}

const UNDO_HEADER: &str = "radial-rename-undo\t1";

impl Undo {
    /// Writes to `%APPDATA%\radial\undo\rename-<unix-seconds>.txt`.
    pub fn save(&self) -> Result<PathBuf> {
        let dir = crate::config::data_dir().ok_or_else(|| Error::Io("no APPDATA folder".into()))?;
        self.save_in(&dir.join("undo"))
    }

    /// Atomic write into `dir`, then prunes the folder to the newest [`KEEP_UNDO`] files.
    pub fn save_in(&self, dir: &Path) -> Result<PathBuf> {
        std::fs::create_dir_all(dir)?;
        let mut text = String::from(UNDO_HEADER);
        text.push('\n');
        for e in &self.entries {
            let kind = if e.dir { 'D' } else { 'F' };
            text.push_str(&format!(
                "{}\t{}\t{kind}\t{}\t{}\n",
                e.current.display(),
                e.original.display(),
                e.len,
                e.modified
            ));
        }
        // Names only ever grow, so a new file is always the newest and never the one pruned.
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs());
        let secs = undo_files(dir).first().map_or(now, |(newest, _)| now.max(newest + 1));
        let target = dir.join(format!("rename-{secs}.txt"));
        let tmp = dir.join(format!("rename-{secs}.tmp"));
        std::fs::write(&tmp, text)?;
        if let Err(e) = std::fs::rename(&tmp, &target) {
            let _ = std::fs::remove_file(&tmp);
            return Err(e.into());
        }
        prune_undo(dir);
        Ok(target)
    }

    pub fn load(file: &Path) -> Result<Undo> {
        let text = std::fs::read_to_string(file)?;
        let mut lines = text.lines();
        if lines.next() != Some(UNDO_HEADER) {
            return Err(Error::Corrupt("not a rename undo file".into()));
        }
        let mut entries = Vec::new();
        for line in lines.filter(|l| !l.is_empty()) {
            let bad = || Error::Corrupt(format!("bad undo line: {line}"));
            let f: Vec<&str> = line.split('\t').collect();
            let [current, original, kind, len, modified] = f.as_slice() else { return Err(bad()) };
            entries.push(UndoEntry {
                current: PathBuf::from(current),
                original: PathBuf::from(original),
                dir: *kind == "D",
                len: len.parse().map_err(|_| bad())?,
                modified: modified.parse().map_err(|_| bad())?,
            });
        }
        Ok(Undo { entries })
    }
}

/// The `rename-<secs>.txt` files of `dir`, newest first.
fn undo_files(dir: &Path) -> Vec<(u64, PathBuf)> {
    let Ok(rd) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut files: Vec<(u64, PathBuf)> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            let secs = name.strip_prefix("rename-")?.strip_suffix(".txt")?.parse::<u64>().ok()?;
            Some((secs, e.path()))
        })
        .collect();
    files.sort_by_key(|f| std::cmp::Reverse(f.0));
    files
}

/// Keeps the newest [`KEEP_UNDO`] undo files of `dir`.
fn prune_undo(dir: &Path) {
    for (_, p) in undo_files(dir).into_iter().skip(KEEP_UNDO) {
        let _ = std::fs::remove_file(p);
    }
}

/// Restores what it safely can and says what it left alone: an entry is skipped when its file is gone
/// (`missing`), was edited since (`changed`) or its original name is occupied again (`taken`).
pub fn undo_report(u: &Undo) -> Result<UndoReport> {
    let mut report = UndoReport::default();
    let mut todo: Vec<&UndoEntry> = Vec::new();
    for e in &u.entries {
        match std::fs::symlink_metadata(&e.current) {
            Err(_) => report.skipped.push((e.current.clone(), "missing".into())),
            Ok(m) => {
                let edited = m.is_dir() != e.dir
                    || (!e.dir && (m.len() != e.len || m.modified().map_or(0, unix_nanos) != e.modified));
                if edited {
                    report.skipped.push((e.current.clone(), "changed".into()));
                } else {
                    todo.push(e);
                }
            }
        }
    }
    let mut leaving: HashSet<String> = todo.iter().map(|e| path_key(&e.current)).collect();
    loop {
        let before = todo.len();
        todo.retain(|e| {
            let blocked = std::fs::symlink_metadata(&e.original).is_ok() && !leaving.contains(&path_key(&e.original));
            if blocked {
                leaving.remove(&path_key(&e.current));
                report.skipped.push((e.current.clone(), "taken".into()));
            }
            !blocked
        });
        if todo.len() == before {
            break;
        }
    }
    let moves: Vec<(PathBuf, PathBuf)> = todo.iter().map(|e| (e.current.clone(), e.original.clone())).collect();
    execute(&moves)?;
    report.restored = moves.len();
    Ok(report)
}

/// Like [`undo_report`], but entries left alone make it an error (after restoring the rest).
pub fn undo(u: &Undo) -> Result<()> {
    let r = undo_report(u)?;
    match r.skipped.first() {
        None => Ok(()),
        Some((p, why)) => {
            Err(Error::Io(format!("{} of {} not restored: {}: {why}", r.skipped.len(), u.entries.len(), p.display())))
        }
    }
}

// ================================================================================================ order

/// File-manager order: case-insensitive, digit runs compared as numbers; exact ties fall back to plain order.
pub fn natural_cmp(a: &str, b: &str) -> std::cmp::Ordering {
    use std::cmp::Ordering;
    let (x, y): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let (mut i, mut j) = (0, 0);
    while i < x.len() && j < y.len() {
        if x[i].is_ascii_digit() && y[j].is_ascii_digit() {
            let (si, sj) = (i, j);
            while i < x.len() && x[i].is_ascii_digit() {
                i += 1;
            }
            while j < y.len() && y[j].is_ascii_digit() {
                j += 1;
            }
            let (da, db): (String, String) = (x[si..i].iter().collect(), y[sj..j].iter().collect());
            let (da, db) = (da.trim_start_matches('0'), db.trim_start_matches('0'));
            let o = da.len().cmp(&db.len()).then_with(|| da.cmp(db));
            if o != Ordering::Equal {
                return o;
            }
        } else {
            let o = fold(x[i]).cmp(&fold(y[j]));
            if o != Ordering::Equal {
                return o;
            }
            i += 1;
            j += 1;
        }
    }
    (x.len() - i).cmp(&(y.len() - j)).then_with(|| a.cmp(b))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::windows::fs::OpenOptionsExt;
    use std::sync::atomic::{AtomicU32, Ordering};
    use std::time::Duration;

    struct Dir(PathBuf);

    impl Dir {
        fn new() -> Dir {
            static N: AtomicU32 = AtomicU32::new(0);
            let nanos = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_nanos();
            let p = std::env::temp_dir().join(format!(
                "radial-rename-test-{}-{}-{nanos}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Dir(p)
        }

        fn file(&self, name: &str, body: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::write(&p, body).unwrap();
            p
        }

        fn folder(&self, name: &str) -> PathBuf {
            let p = self.0.join(name);
            std::fs::create_dir_all(&p).unwrap();
            p
        }

        fn names(&self) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .collect();
            v.sort();
            v
        }
    }

    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    fn rule() -> Rule {
        Rule::default()
    }

    /// New names of a plan, in order.
    fn news(items: &[Item]) -> Vec<String> {
        items.iter().map(Item::new_name).collect()
    }

    /// Plans one file called `name` in a fresh folder and returns the item.
    fn one(name: &str, r: &Rule) -> Item {
        let d = Dir::new();
        let p = d.file(name, "x");
        plan(&[p], r).remove(0)
    }

    fn at(secs: u64) -> SystemTime {
        UNIX_EPOCH + Duration::from_secs(secs)
    }

    fn set_mtime(p: &Path, t: SystemTime) {
        File::options().write(true).open(p).unwrap().set_modified(t).unwrap();
    }

    fn invalid(name_rule: &Rule, file: &str) -> String {
        match one(file, name_rule).status {
            Status::Invalid(why) => why,
            other => panic!("expected Invalid, got {other:?}"),
        }
    }

    // ---------------------------------------------------------------------------------------------- the rule

    #[test]
    fn find_and_replace_is_a_case_insensitive_plain_substring_replace_on_the_stem() {
        let r = Rule { find: "img_".into(), replace: "photo-".into(), ..rule() };
        let it = one("Holiday IMG_1.JPG", &r);
        assert_eq!(it.new_name(), "Holiday photo-1.JPG", "the extension is untouched");
        assert_eq!(it.status, Status::Ok);
        let dots = Rule { find: ".".into(), replace: "-".into(), ..rule() };
        assert_eq!(one("a.b.c.txt", &dots).new_name(), "a-b-c.txt", "no pattern syntax: a dot is a dot");
        let none = Rule { find: "zzz".into(), replace: "y".into(), ..rule() };
        assert_eq!(one("a.txt", &none).status, Status::Unchanged);
        let ext = Rule { find: "txt".into(), replace: "md".into(), ..rule() };
        assert_eq!(one("txt.txt", &ext).new_name(), "md.txt", "only the stem is searched");
    }

    #[test]
    fn find_and_replace_handles_every_occurrence_and_unicode() {
        let r = Rule { find: "ñ".into(), replace: "n".into(), ..rule() };
        assert_eq!(one("AÑo ñoño.txt", &r).new_name(), "Ano nono.txt");
        let empty = Rule { find: "".into(), replace: "zzz".into(), ..rule() };
        assert_eq!(one("a.txt", &empty).status, Status::Unchanged, "an empty find does nothing");
    }

    #[test]
    fn accents_are_stripped_from_latin_letters() {
        let r = Rule { strip_accents: true, ..rule() };
        assert_eq!(one("Ñandú Ç.txt", &r).new_name(), "Nandu C.txt");
        assert_eq!(one("crème brûlée ÅÄÖ ß œ.txt", &r).new_name(), "creme brulee AAO ss oe.txt");
        assert_eq!(one("n\u{303}u.txt", &r).new_name(), "nu.txt", "combining marks go too");
        assert_eq!(one("日本語.txt", &r).status, Status::Unchanged, "other scripts are left alone");
    }

    #[test]
    fn spaces_can_become_underscores_dashes_dots_or_nothing() {
        let with = |s| Rule { spaces: s, ..rule() };
        assert_eq!(one("a b  c.txt", &with(Spaces::Underscore)).new_name(), "a_b__c.txt");
        assert_eq!(one("a b c.txt", &with(Spaces::Dash)).new_name(), "a-b-c.txt");
        assert_eq!(one("a b c.txt", &with(Spaces::Dot)).new_name(), "a.b.c.txt");
        assert_eq!(one("a b c.txt", &with(Spaces::Remove)).new_name(), "abc.txt");
        assert_eq!(one("a b c.txt", &with(Spaces::Keep)).status, Status::Unchanged);
    }

    #[test]
    fn case_options_follow_the_words_and_leave_apostrophes_alone() {
        let with = |c| Rule { case: c, ..rule() };
        assert_eq!(one("Hello World.TXT", &with(Case::Lower)).new_name(), "hello world.TXT");
        assert_eq!(one("Hello World.txt", &with(Case::Upper)).new_name(), "HELLO WORLD.txt");
        assert_eq!(
            one("don't STOP me_please-now 3rd.txt", &with(Case::Title)).new_name(),
            "Don't Stop Me_Please-Now 3rd.txt"
        );
        assert_eq!(one("the o'brien's CAR.txt", &with(Case::Title)).new_name(), "The O'brien's Car.txt");
        assert_eq!(one("HELLO wORLD. ok.txt", &with(Case::Sentence)).new_name(), "Hello world. ok.txt");
        assert_eq!(one("  123 abc.txt", &with(Case::Sentence)).new_name(), "  123 Abc.txt");
        assert_eq!(one("Hello.txt", &with(Case::Title)).status, Status::Unchanged);
    }

    #[test]
    fn prefix_and_suffix_wrap_the_stem_and_are_not_touched_by_case() {
        let r = Rule { prefix: "pre_".into(), suffix: "_Z".into(), case: Case::Upper, ..rule() };
        assert_eq!(one("name.txt", &r).new_name(), "pre_NAME_Z.txt");
    }

    #[test]
    fn find_runs_before_case_and_accents_before_spaces() {
        let r = Rule { find: "ab".into(), replace: "CD".into(), case: Case::Lower, ..rule() };
        assert_eq!(one("ab.txt", &r).new_name(), "cd.txt");
        let r = Rule { strip_accents: true, spaces: Spaces::Underscore, case: Case::Title, ..rule() };
        assert_eq!(one("ñandú ÇA.txt", &r).new_name(), "Nandu_Ca.txt");
    }

    #[test]
    fn the_extension_is_only_touched_when_asked() {
        let r = Rule { ext_lowercase: true, ..rule() };
        assert_eq!(one("Photo.JPEG", &r).new_name(), "Photo.jpeg");
        assert_eq!(one("archive.tar.GZ", &r).new_name(), "archive.tar.gz");
        assert_eq!(one(".Hidden", &r).status, Status::Unchanged, "a leading dot is part of the stem");
        assert_eq!(one("NOEXT", &r).status, Status::Unchanged);
    }

    #[test]
    fn folders_are_renamed_as_a_whole() {
        let d = Dir::new();
        let f = d.folder("my.Folder");
        let r = Rule { case: Case::Upper, ext_lowercase: true, ..rule() };
        let it = plan(&[f], &r).remove(0);
        assert_eq!(it.new_name(), "MY.FOLDER");
        assert_eq!(it.status, Status::Ok);
    }

    // ---------------------------------------------------------------------------------------------- numbering

    #[test]
    fn numbering_pads_and_follows_the_order_of_the_paths() {
        let d = Dir::new();
        let (a, b, c) = (d.file("a.txt", ""), d.file("b.txt", ""), d.file("c.txt", ""));
        let n = |position, digits| Numbering { start: 5, step: 5, digits, position };
        let r = Rule { numbering: Some(n(NumPos::Prefix, 3)), spaces: Spaces::Underscore, ..rule() };
        assert_eq!(news(&plan(&[a.clone(), b.clone(), c.clone()], &r)), ["005_a.txt", "010_b.txt", "015_c.txt"]);
        let r = Rule { numbering: Some(n(NumPos::Suffix, 2)), ..rule() };
        assert_eq!(news(&plan(&[c.clone(), a.clone(), b.clone()], &r)), ["c 05.txt", "a 10.txt", "b 15.txt"]);
        let r = Rule { numbering: Some(n(NumPos::Replace, 4)), ..rule() };
        assert_eq!(news(&plan(&[a.clone(), b.clone(), c.clone()], &r)), ["0005.txt", "0010.txt", "0015.txt"]);
        let big = Rule {
            numbering: Some(Numbering { start: 99_999, step: 1, digits: 2, position: NumPos::Replace }),
            ..rule()
        };
        assert_eq!(news(&plan(&[a, b], &big)), ["99999.txt", "100000.txt"], "numbers are never cut");
    }

    #[test]
    fn numbering_at_replace_keeps_prefix_suffix_and_date_around_the_number() {
        let d = Dir::new();
        let (a, b) = (d.file("holiday.jpg", ""), d.file("beach.jpg", ""));
        let r = Rule {
            prefix: "Trip ".into(),
            suffix: " ok".into(),
            numbering: Some(Numbering { start: 1, step: 1, digits: 3, position: NumPos::Replace }),
            ..rule()
        };
        assert_eq!(news(&plan(&[a, b], &r)), ["Trip 001 ok.jpg", "Trip 002 ok.jpg"]);
    }

    #[test]
    fn numbering_with_a_zero_step_makes_every_target_the_same_and_flags_them() {
        let d = Dir::new();
        let (a, b) = (d.file("a.txt", ""), d.file("b.txt", ""));
        let r =
            Rule { numbering: Some(Numbering { start: 1, step: 0, digits: 1, position: NumPos::Replace }), ..rule() };
        let items = plan(&[a, b], &r);
        assert_eq!(items[0].status, Status::Conflict("duplicate".into()));
        assert_eq!(items[1].status, Status::Conflict("duplicate".into()));
    }

    // ---------------------------------------------------------------------------------------------- dates

    #[test]
    fn local_time_conversion_is_a_whole_quarter_hour_offset_from_utc() {
        let secs = 1_714_557_600; // 2024-05-01 10:00:00 UTC
        let p = local_parts(at(secs)).unwrap();
        // Naive seconds of the local wall clock, via the days-from-civil algorithm.
        let (y, m, d) = (p.y as i64 - (p.mo <= 2) as i64, p.mo as i64, p.d as i64);
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        let local = days * 86_400 + p.h as i64 * 3600 + p.mi as i64 * 60;
        let offset = local - (secs as i64 - secs as i64 % 60);
        assert!(offset.abs() <= 14 * 3600 && offset % 900 == 0, "offset {offset}");
        assert!(local_parts(UNIX_EPOCH - Duration::from_secs(5)).is_none(), "before 1970 there is no date");
    }

    #[test]
    fn dates_come_from_the_modified_time_in_every_format_and_position() {
        let d = Dir::new();
        let p = d.file("x.txt", "");
        set_mtime(&p, at(1_714_557_600));
        let l = local_parts(at(1_714_557_600)).unwrap();
        let (iso, compact, time) = (
            format!("{:04}-{:02}-{:02}", l.y, l.mo, l.d),
            format!("{:04}{:02}{:02}", l.y, l.mo, l.d),
            format!("{:04}-{:02}-{:02}_{:02}-{:02}", l.y, l.mo, l.d, l.h, l.mi),
        );
        let with = |format, position, spaces| Rule {
            date: Some(DateStamp { source: DateSource::Modified, format, position }),
            spaces,
            ..rule()
        };
        let one_name = |r: &Rule| plan(std::slice::from_ref(&p), r).remove(0).new_name();
        assert_eq!(one_name(&with(DateFormat::Iso, DatePos::Prefix, Spaces::Keep)), format!("{iso} x.txt"));
        assert_eq!(one_name(&with(DateFormat::Compact, DatePos::Suffix, Spaces::Keep)), format!("x {compact}.txt"));
        assert_eq!(one_name(&with(DateFormat::IsoTime, DatePos::Prefix, Spaces::Underscore)), format!("{time}_x.txt"));
        assert_eq!(one_name(&with(DateFormat::Iso, DatePos::Suffix, Spaces::Dash)), format!("x-{iso}.txt"));
    }

    #[test]
    fn the_created_time_is_read_from_the_file() {
        let d = Dir::new();
        let p = d.file("x.txt", "");
        let r = Rule {
            date: Some(DateStamp {
                source: DateSource::Created,
                format: DateFormat::Compact,
                position: DatePos::Suffix,
            }),
            ..rule()
        };
        let l = local_parts(std::fs::metadata(&p).unwrap().created().unwrap()).unwrap();
        assert_eq!(plan(&[p], &r)[0].new_name(), format!("x {:04}{:02}{:02}.txt", l.y, l.mo, l.d));
    }

    /// A tiny JPEG whose only content is an EXIF block holding `DateTimeOriginal`.
    fn exif_jpeg(date: &str, big_endian: bool) -> Vec<u8> {
        let tiff = exif_tiff(date, big_endian);
        let mut seg = b"Exif\0\0".to_vec();
        seg.extend(&tiff);
        let mut out = vec![0xFF, 0xD8, 0xFF, 0xE0, 0x00, 0x04, 0x4A, 0x46]; // SOI + a stub APP0 first
        out.extend([0xFF, 0xE1]);
        out.extend(((seg.len() + 2) as u16).to_be_bytes());
        out.extend(seg);
        out.extend([0xFF, 0xD9]);
        out
    }

    fn exif_tiff(date: &str, be: bool) -> Vec<u8> {
        let u16b = |v: u16| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let u32b = |v: u32| if be { v.to_be_bytes() } else { v.to_le_bytes() };
        let mut t = Vec::new();
        t.extend(if be { *b"MM" } else { *b"II" });
        t.extend(u16b(42));
        t.extend(u32b(8));
        // IFD0: one entry, ExifIFD pointer -> 26
        t.extend(u16b(1));
        t.extend(u16b(0x8769));
        t.extend(u16b(4));
        t.extend(u32b(1));
        t.extend(u32b(26));
        t.extend(u32b(0));
        // ExifIFD: one entry, DateTimeOriginal (ASCII, 20 bytes) -> 44
        t.extend(u16b(1));
        t.extend(u16b(0x9003));
        t.extend(u16b(2));
        t.extend(u32b(20));
        t.extend(u32b(44));
        t.extend(u32b(0));
        t.extend(date.as_bytes());
        t.push(0);
        t
    }

    #[test]
    fn the_exif_date_is_read_from_both_byte_orders() {
        let want = Parts { y: 2023, mo: 7, d: 14, h: 9, mi: 8 };
        assert_eq!(exif_taken(&exif_tiff("2023:07:14 09:08:07", false)), Some(want));
        assert_eq!(exif_taken(&exif_tiff("2023:07:14 09:08:07", true)), Some(want));
        assert_eq!(exif_taken(&exif_tiff("0000:00:00 00:00:00", false)), None, "an unset camera clock is no date");
        assert_eq!(exif_taken(&exif_tiff("2023:13:40 09:08:07", false)), None);
        assert_eq!(exif_taken(b"II*\0\xFF\xFF\xFF\xFF"), None, "offsets outside the block are survived");
        assert_eq!(exif_taken(b""), None);
    }

    #[test]
    fn taken_uses_exif_and_falls_back_to_the_modified_time() {
        let d = Dir::new();
        let jpg = d.0.join("p.jpg");
        std::fs::write(&jpg, exif_jpeg("2023:07:14 09:08:07", false)).unwrap();
        let tif = d.0.join("q.tif");
        std::fs::write(&tif, exif_tiff("2022:01:02 03:04:05", true)).unwrap();
        let txt = d.file("n.txt", "");
        let when = at(1_714_557_600);
        for p in [&jpg, &tif, &txt] {
            set_mtime(p, when);
        }
        let r = |format| Rule {
            date: Some(DateStamp { source: DateSource::Taken, format, position: DatePos::Prefix }),
            spaces: Spaces::Underscore,
            ..rule()
        };
        let items = plan(&[jpg.clone(), tif.clone()], &r(DateFormat::IsoTime));
        assert_eq!(news(&items), ["2023-07-14_09-08_p.jpg", "2022-01-02_03-04_q.tif"]);
        let l = local_parts(when).unwrap();
        let fallback = plan(&[txt], &r(DateFormat::Iso)).remove(0).new_name();
        assert_eq!(fallback, format!("{:04}-{:02}-{:02}_n.txt", l.y, l.mo, l.d), "no EXIF: modified time");
        let corrupt = d.0.join("bad.jpg");
        std::fs::write(&corrupt, [0xFF, 0xD8, 0xFF, 0xE1, 0x00]).unwrap();
        set_mtime(&corrupt, when);
        assert_eq!(
            plan(&[corrupt], &r(DateFormat::Iso))[0].new_name(),
            format!("{:04}-{:02}-{:02}_bad.jpg", l.y, l.mo, l.d)
        );
    }

    // ---------------------------------------------------------------------------------------------- status

    #[test]
    fn unchanged_when_the_rule_does_nothing() {
        let it = one("a.txt", &rule());
        assert_eq!((it.new_name(), &it.status), ("a.txt".to_string(), &Status::Unchanged));
    }

    #[test]
    fn an_empty_result_is_invalid() {
        let r = Rule { find: "a".into(), replace: "".into(), ..rule() };
        assert_eq!(invalid(&r, "a.txt"), "empty");
    }

    #[test]
    fn characters_windows_forbids_are_invalid() {
        for c in ['<', '>', ':', '"', '/', '\\', '|', '?', '*', '\u{7}'] {
            let r = Rule { suffix: c.to_string(), ..rule() };
            assert_eq!(invalid(&r, "a.txt"), format!("illegal:{c}"), "{c:?}");
        }
        let ok = Rule { suffix: "! #$%&'()+,;=@[]^_`{}~-".into(), ..rule() };
        assert_eq!(one("a.txt", &ok).status, Status::Ok);
    }

    #[test]
    fn device_names_are_reserved_with_or_without_an_extension() {
        for stem in ["con", "PRN", "aux", "NuL", "com1", "COM9", "lpt1", "LPT9"] {
            let r = Rule { find: "x".into(), replace: stem.into(), ..rule() };
            assert_eq!(invalid(&r, "x.txt"), "reserved", "{stem}");
        }
        let r = Rule { find: "x".into(), replace: "console".into(), ..rule() };
        assert_eq!(one("x.txt", &r).status, Status::Ok);
        let r = Rule { find: "x".into(), replace: "com10".into(), ..rule() };
        assert_eq!(one("x.txt", &r).status, Status::Ok, "COM10 is not a device name");
    }

    #[test]
    fn a_trailing_dot_or_space_is_invalid() {
        let r = Rule { suffix: ".".into(), ..rule() };
        assert_eq!(invalid(&r, "noext"), "trailing");
        let r = Rule { suffix: " ".into(), ..rule() };
        assert_eq!(invalid(&r, "noext"), "trailing");
        let r = Rule { suffix: ".".into(), ..rule() };
        assert_eq!(one("a.txt", &r).new_name(), "a..txt");
        assert_eq!(one("a.txt", &r).status, Status::Ok, "the dot is inside the name when there is an extension");
    }

    #[test]
    fn names_over_the_limit_are_invalid() {
        let r = |n: usize| Rule { suffix: "x".repeat(n), ..rule() };
        assert_eq!(invalid(&r(240), "a.txt"), "long"); // 1 + 240 + 4 chars
        assert_eq!(one("a.txt", &r(MAX_NAME - 5)).status, Status::Ok);
        assert_eq!(invalid(&r(MAX_NAME - 4), "a.txt"), "long");
        let multibyte = Rule { suffix: "ñ".repeat(MAX_NAME - 5), ..rule() };
        assert_eq!(one("a.txt", &multibyte).status, Status::Ok, "the limit counts characters, not bytes");
    }

    #[test]
    fn a_missing_file_is_invalid() {
        let d = Dir::new();
        let it = plan(&[d.0.join("ghost.txt")], &Rule { case: Case::Upper, ..rule() }).remove(0);
        assert_eq!(it.status, Status::Invalid("missing".into()));
    }

    #[test]
    fn two_items_with_the_same_new_name_conflict_case_insensitively() {
        let d = Dir::new();
        let (a, b) = (d.file("A b.txt", ""), d.file("a_B.txt", ""));
        let r = Rule { spaces: Spaces::Underscore, case: Case::Lower, ..rule() };
        let items = plan(&[a, b], &r);
        assert_eq!(news(&items), ["a_b.txt", "a_b.txt"]);
        assert!(items.iter().all(|i| i.status == Status::Conflict("duplicate".into())));
    }

    #[test]
    fn an_item_that_stays_put_keeps_its_name_against_a_newcomer() {
        let d = Dir::new();
        let (a, b) = (d.file("x-1.txt", ""), d.file("x 1.txt", ""));
        let r = Rule { spaces: Spaces::Dash, ..rule() };
        let items = plan(&[a, b], &r);
        assert_eq!(items[0].status, Status::Unchanged);
        assert_eq!(items[1].status, Status::Conflict("duplicate".into()));
    }

    #[test]
    fn a_target_that_exists_outside_the_batch_conflicts_even_as_a_folder() {
        let d = Dir::new();
        let a = d.file("a.txt", "");
        d.file("b.txt", "");
        let r = Rule { find: "a".into(), replace: "b".into(), ..rule() };
        assert_eq!(plan(std::slice::from_ref(&a), &r)[0].status, Status::Conflict("exists".into()));
        let d2 = Dir::new();
        let f = d2.file("a", "");
        d2.folder("B");
        let items = plan(&[f], &r);
        assert_eq!(items[0].status, Status::Conflict("exists".into()), "case-insensitive, and a folder counts");
    }

    #[test]
    fn a_target_that_is_renamed_away_in_the_same_batch_is_free() {
        let d = Dir::new();
        let ps: Vec<PathBuf> = ["1.txt", "2.txt", "3.txt"].iter().map(|n| d.file(n, n)).collect();
        let r =
            Rule { numbering: Some(Numbering { start: 2, step: 1, digits: 1, position: NumPos::Replace }), ..rule() };
        let items = plan(&ps, &r);
        assert_eq!(news(&items), ["2.txt", "3.txt", "4.txt"]);
        assert!(items.iter().all(|i| i.status == Status::Ok), "{items:?}");
    }

    #[test]
    fn a_target_held_by_a_blocked_item_is_not_free() {
        let d = Dir::new();
        let ps: Vec<PathBuf> = ["1.txt", "2.txt"].iter().map(|n| d.file(n, n)).collect();
        d.file("3.txt", "outside the batch");
        let r =
            Rule { numbering: Some(Numbering { start: 2, step: 1, digits: 1, position: NumPos::Replace }), ..rule() };
        let items = plan(&ps, &r); // 1 -> 2, 2 -> 3 (taken): so 2 never leaves and 1 cannot take its place
        assert_eq!(items[1].status, Status::Conflict("exists".into()));
        assert_eq!(items[0].status, Status::Conflict("exists".into()));
    }

    #[test]
    fn a_case_only_rename_is_ok_and_not_a_conflict() {
        let d = Dir::new();
        let a = d.file("a.txt", "keep me");
        let r = Rule { case: Case::Upper, ..rule() };
        let items = plan(&[a], &r);
        assert_eq!((items[0].new_name().as_str(), &items[0].status), ("A.txt", &Status::Ok));
        let undo = apply(&items).unwrap();
        assert_eq!(d.names(), ["A.txt"], "the case really changed on disk");
        assert_eq!(std::fs::read_to_string(d.0.join("A.txt")).unwrap(), "keep me");
        undo_report(&undo).unwrap();
        assert_eq!(d.names(), ["a.txt"]);
    }

    #[test]
    fn five_thousand_files_plan_quickly() {
        let d = Dir::new();
        let ps: Vec<PathBuf> = (0..5000).map(|i| d.file(&format!("IMG {i:05}.JPG"), "")).collect();
        let r = Rule { case: Case::Lower, spaces: Spaces::Underscore, numbering: Some(Numbering::default()), ..rule() };
        let t = std::time::Instant::now();
        let items = plan(&ps, &r);
        let took = t.elapsed();
        let mut scan = Scan::default();
        plan_with(&mut scan, &ps, &r);
        let t2 = std::time::Instant::now();
        plan_with(&mut scan, &ps, &r);
        let warm = t2.elapsed();
        assert!(warm < took, "a warm scan skips the stats: {warm:?} vs {took:?}");
        assert_eq!(items.len(), 5000);
        assert!(items.iter().all(|i| i.status == Status::Ok));
        assert_eq!(items[4999].new_name(), "5000_img_04999.JPG");
        assert!(took < Duration::from_millis(1500), "planning 5000 files took {took:?}");
    }

    // ---------------------------------------------------------------------------------------------- apply

    #[test]
    fn apply_renames_in_place_and_leaves_unchanged_files_alone() {
        let d = Dir::new();
        let (a, b) = (d.file("a one.txt", "A"), d.file("keep.txt", "K"));
        let r = Rule { spaces: Spaces::Underscore, ..rule() };
        let items = plan(&[a, b], &r);
        let u = apply(&items).unwrap();
        assert_eq!(d.names(), ["a_one.txt", "keep.txt"]);
        assert_eq!(std::fs::read_to_string(d.0.join("a_one.txt")).unwrap(), "A");
        assert_eq!(u.entries.len(), 1, "unchanged files are not in the undo");
    }

    #[test]
    fn apply_renames_folders_and_their_contents_follow() {
        let d = Dir::new();
        let f = d.folder("old dir");
        std::fs::write(f.join("inner.txt"), "i").unwrap();
        let items = plan(&[f], &Rule { spaces: Spaces::Dash, ..rule() });
        apply(&items).unwrap();
        assert_eq!(std::fs::read_to_string(d.0.join("old-dir").join("inner.txt")).unwrap(), "i");
    }

    #[test]
    fn apply_handles_a_chain_where_names_move_into_each_other() {
        let d = Dir::new();
        let ps: Vec<PathBuf> = ["1.txt", "2.txt", "3.txt"].iter().map(|n| d.file(n, n)).collect();
        let r =
            Rule { numbering: Some(Numbering { start: 2, step: 1, digits: 1, position: NumPos::Replace }), ..rule() };
        apply(&plan(&ps, &r)).unwrap();
        assert_eq!(d.names(), ["2.txt", "3.txt", "4.txt"]);
        let body = |n: &str| std::fs::read_to_string(d.0.join(n)).unwrap();
        assert_eq!((body("2.txt"), body("3.txt"), body("4.txt")), ("1.txt".into(), "2.txt".into(), "3.txt".into()));
    }

    #[test]
    fn apply_swaps_two_files_without_losing_either() {
        let d = Dir::new();
        let (a, b) = (d.file("a.txt", "AAA"), d.file("b.txt", "BBB"));
        let swap = vec![
            Item { from: a.clone(), to: b.clone(), status: Status::Ok },
            Item { from: b.clone(), to: a.clone(), status: Status::Ok },
        ];
        let u = apply(&swap).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "BBB");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "AAA");
        assert_eq!(d.names(), ["a.txt", "b.txt"], "no temporary name is left behind");
        undo_report(&u).unwrap();
        assert_eq!(std::fs::read_to_string(&a).unwrap(), "AAA", "undoing a swap swaps back");
        assert_eq!(std::fs::read_to_string(&b).unwrap(), "BBB");
    }

    #[test]
    fn apply_handles_a_swap_next_to_a_chain() {
        let d = Dir::new();
        let (a, b, c) = (d.file("a", "A"), d.file("b", "B"), d.file("c", "C"));
        let moves = vec![
            Item { from: a.clone(), to: b.clone(), status: Status::Ok },
            Item { from: b.clone(), to: d.0.join("c2"), status: Status::Ok },
            Item { from: c.clone(), to: d.0.join("c3"), status: Status::Ok },
        ];
        apply(&moves).unwrap();
        assert_eq!(d.names(), ["b", "c2", "c3"]);
        assert_eq!(std::fs::read_to_string(d.0.join("b")).unwrap(), "A");
        assert_eq!(std::fs::read_to_string(d.0.join("c2")).unwrap(), "B");
    }

    #[test]
    fn apply_refuses_a_plan_with_conflicts_or_invalid_items_and_changes_nothing() {
        let d = Dir::new();
        let (a, b) = (d.file("a.txt", ""), d.file("b.txt", ""));
        let bad = vec![
            Item { from: a.clone(), to: d.0.join("z.txt"), status: Status::Ok },
            Item { from: b.clone(), to: d.0.join("b?.txt"), status: Status::Invalid("illegal:?".into()) },
        ];
        assert!(matches!(apply(&bad), Err(Error::InvalidArgument(_))));
        let clash = vec![Item { from: a, to: b, status: Status::Conflict("exists".into()) }];
        assert!(matches!(apply(&clash), Err(Error::InvalidArgument(_))));
        assert_eq!(d.names(), ["a.txt", "b.txt"]);
    }

    #[test]
    fn apply_never_overwrites_a_file_that_appeared_after_the_plan() {
        let d = Dir::new();
        let a = d.file("a.txt", "mine");
        let items = plan(&[a], &Rule { find: "a".into(), replace: "z".into(), ..rule() });
        d.file("z.txt", "theirs"); // created between the preview and the click
        assert!(apply(&items).is_err());
        assert_eq!(std::fs::read_to_string(d.0.join("z.txt")).unwrap(), "theirs");
        assert_eq!(std::fs::read_to_string(d.0.join("a.txt")).unwrap(), "mine");
    }

    #[test]
    fn apply_rolls_back_when_one_rename_fails() {
        let d = Dir::new();
        let ps: Vec<PathBuf> = ["a.txt", "b.txt", "c.txt", "d.txt"].iter().map(|n| d.file(n, n)).collect();
        let items = plan(&ps, &Rule { prefix: "new-".into(), ..rule() });
        // A handle that shares nothing makes the rename of c.txt fail.
        let _hold = File::options().read(true).share_mode(0).open(&ps[2]).unwrap();
        let err = apply(&items).unwrap_err();
        assert!(matches!(err, Error::Io(_)), "{err:?}");
        drop(_hold);
        assert_eq!(d.names(), ["a.txt", "b.txt", "c.txt", "d.txt"], "everything is back, no temporary files");
        for n in ["a.txt", "b.txt", "c.txt", "d.txt"] {
            assert_eq!(std::fs::read_to_string(d.0.join(n)).unwrap(), n);
        }
    }

    #[test]
    fn apply_rolls_back_a_chain_that_cannot_complete() {
        let d = Dir::new();
        let ps: Vec<PathBuf> = ["1.txt", "2.txt"].iter().map(|n| d.file(n, n)).collect();
        let r =
            Rule { numbering: Some(Numbering { start: 2, step: 1, digits: 1, position: NumPos::Replace }), ..rule() };
        let items = plan(&ps, &r); // 1 -> 2, 2 -> 3
        let _hold = File::options().read(true).share_mode(0).open(&ps[1]).unwrap();
        assert!(apply(&items).is_err());
        drop(_hold);
        assert_eq!(d.names(), ["1.txt", "2.txt"]);
        assert_eq!(std::fs::read_to_string(d.0.join("1.txt")).unwrap(), "1.txt");
    }

    // ---------------------------------------------------------------------------------------------- undo

    #[test]
    fn undo_restores_the_original_names() {
        let d = Dir::new();
        let (a, b) = (d.file("a b.txt", "A"), d.file("c d.txt", "C"));
        let u = apply(&plan(&[a, b], &Rule { spaces: Spaces::Underscore, ..rule() })).unwrap();
        assert_eq!(d.names(), ["a_b.txt", "c_d.txt"]);
        undo(&u).unwrap();
        assert_eq!(d.names(), ["a b.txt", "c d.txt"]);
        assert_eq!(std::fs::read_to_string(d.0.join("a b.txt")).unwrap(), "A");
    }

    #[test]
    fn undo_skips_and_reports_files_that_changed_or_vanished() {
        let d = Dir::new();
        let ps: Vec<PathBuf> = ["a b.txt", "c d.txt", "e f.txt", "g h.txt"].iter().map(|n| d.file(n, "body")).collect();
        let u = apply(&plan(&ps, &Rule { spaces: Spaces::Underscore, ..rule() })).unwrap();
        std::fs::write(d.0.join("a_b.txt"), "edited since").unwrap(); // changed
        std::fs::remove_file(d.0.join("c_d.txt")).unwrap(); // vanished
        d.file("g h.txt", "someone else's"); // original name taken again
        let rep = undo_report(&u).unwrap();
        assert_eq!(rep.restored, 1);
        let why: HashMap<String, String> = rep.skipped.iter().map(|(p, w)| (name_of(p), w.clone())).collect();
        assert_eq!(why.get("a_b.txt").map(String::as_str), Some("changed"));
        assert_eq!(why.get("c_d.txt").map(String::as_str), Some("missing"));
        assert_eq!(why.get("g_h.txt").map(String::as_str), Some("taken"));
        assert_eq!(d.names(), ["a_b.txt", "e f.txt", "g h.txt", "g_h.txt"]);
        assert_eq!(std::fs::read_to_string(d.0.join("a_b.txt")).unwrap(), "edited since", "edits are kept");
        assert_eq!(std::fs::read_to_string(d.0.join("g h.txt")).unwrap(), "someone else's");
        let strict = undo(&u);
        assert!(matches!(strict, Err(Error::Io(_))), "the plain undo reports skipped files as an error");
    }

    #[test]
    fn undo_files_round_trip_and_only_the_last_twenty_are_kept() {
        let d = Dir::new();
        let a = d.file("tab é ñ.txt", "x");
        let u = apply(&plan(&[a], &Rule { prefix: "p ".into(), ..rule() })).unwrap();
        let store = d.0.join("undo");
        let first = u.save_in(&store).unwrap();
        assert!(first.file_name().unwrap().to_string_lossy().starts_with("rename-"));
        assert_eq!(Undo::load(&first).unwrap(), u);
        let mut last = first;
        for _ in 0..24 {
            last = u.save_in(&store).unwrap();
        }
        let kept: Vec<String> = std::fs::read_dir(&store).unwrap().map(|e| name_of(&e.unwrap().path())).collect();
        assert_eq!(kept.len(), KEEP_UNDO);
        assert!(last.exists(), "the newest survives: {last:?} in {kept:?}");
        assert!(!kept.iter().any(|n| n.ends_with(".tmp")), "no half-written file is left");
        let not_undo = d.file("junk.txt", "nope");
        assert!(Undo::load(&not_undo).is_err());
    }

    // ---------------------------------------------------------------------------------------------- order

    #[test]
    fn natural_order_compares_digit_runs_as_numbers_and_ignores_case() {
        use std::cmp::Ordering::*;
        assert_eq!(natural_cmp("img2.jpg", "img10.jpg"), Less);
        assert_eq!(natural_cmp("B.txt", "a.txt"), Greater);
        assert_eq!(natural_cmp("a01", "a1"), Less, "ties break deterministically");
        assert_eq!(natural_cmp("a1", "a01"), Greater);
        assert_eq!(natural_cmp("x", "x"), Equal);
        let mut v = vec!["f10", "f9", "F1", "f100", "f2"];
        v.sort_by(|a, b| natural_cmp(a, b));
        assert_eq!(v, ["F1", "f2", "f9", "f10", "f100"]);
    }
}
