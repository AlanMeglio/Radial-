//! Spreadsheets: read XLSX, CSV in and out, write XLSX.
//!
//! Everything is pure, blocking code over byte slices: no UI, no files, no new crates. The XML tokenizer is a few
//! dozen lines because OOXML parts are regular; it borrows from the input so a 70 MB sheet is read without
//! per-tag allocations.
//!
//! Limits (zip bombs, huge sheets): a part may inflate to 256 MiB at most and a workbook may hold 5,000,000
//! cells in total. Both refuse with `Error::Unsupported` and a message the UI can show as is.

use super::error::{Error, Result};
use super::zip::{ZipArchive, ZipWriter};
use std::borrow::Cow;
use std::collections::HashMap;
use std::fmt::Write as _;

/// One worksheet: its tab name and the text of every cell as a user would see it.
///
/// Rows keep the position of their cells (empty strings fill the gaps); trailing empty cells and rows are trimmed.
#[derive(Debug, Clone, PartialEq)]
pub struct Sheet {
    pub name: String,
    pub rows: Vec<Vec<String>>,
}

const MAX_PART: usize = 256 << 20;
const MAX_CELLS: usize = 5_000_000;
/// Excel's hard limits, also the write limits.
const MAX_ROWS: usize = 1_048_576;
const MAX_COLS: usize = 16_384;
/// Excel's limit for the text of one cell.
const MAX_CELL_CHARS: usize = 32_767;
const OLE2: [u8; 8] = [0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];

// ------------------------------------------------------------------------------------------------ XML

enum Tok<'a> {
    /// `name` is the local name (prefix removed); `attrs` is the raw attribute text.
    Open {
        name: &'a str,
        attrs: &'a str,
        empty: bool,
    },
    Close(&'a str),
    /// Raw text between tags (entities not yet decoded).
    Text(&'a str),
    Cdata(&'a str),
}

struct Lexer<'a> {
    s: &'a str,
    pos: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a str) -> Self {
        Lexer { s, pos: 0 }
    }
}

fn local(name: &str) -> &str {
    match name.rfind(':') {
        Some(i) => &name[i + 1..],
        None => name,
    }
}

impl<'a> Iterator for Lexer<'a> {
    type Item = Tok<'a>;

    fn next(&mut self) -> Option<Tok<'a>> {
        let s = self.s;
        let b = s.as_bytes();
        loop {
            if self.pos >= b.len() {
                return None;
            }
            if b[self.pos] != b'<' {
                let end = s[self.pos..].find('<').map_or(b.len(), |i| self.pos + i);
                let t = &s[self.pos..end];
                self.pos = end;
                return Some(Tok::Text(t));
            }
            let rest = &s[self.pos..];
            if rest.starts_with("<!--") {
                self.pos = rest.find("-->").map_or(b.len(), |i| self.pos + i + 3);
                continue;
            }
            if let Some(body) = rest.strip_prefix("<![CDATA[") {
                let (t, adv) = match body.find("]]>") {
                    Some(i) => (&body[..i], 9 + i + 3),
                    None => (body, rest.len()),
                };
                self.pos += adv;
                return Some(Tok::Cdata(t));
            }
            if rest.starts_with("<?") {
                self.pos = rest.find("?>").map_or(b.len(), |i| self.pos + i + 2);
                continue;
            }
            if rest.starts_with("<!") {
                self.pos = rest.find('>').map_or(b.len(), |i| self.pos + i + 1);
                continue;
            }
            // A tag: find its closing '>' outside quoted attribute values.
            let mut quote = 0u8;
            let mut end = None;
            for (i, &c) in b.iter().enumerate().skip(self.pos + 1) {
                if quote != 0 {
                    if c == quote {
                        quote = 0;
                    }
                } else if c == b'"' || c == b'\'' {
                    quote = c;
                } else if c == b'>' {
                    end = Some(i);
                    break;
                }
            }
            let Some(end) = end else {
                self.pos = b.len();
                return None;
            };
            let inner = &s[self.pos + 1..end];
            self.pos = end + 1;
            if let Some(name) = inner.strip_prefix('/') {
                return Some(Tok::Close(local(name.trim())));
            }
            let empty = inner.ends_with('/');
            let inner = if empty { &inner[..inner.len() - 1] } else { inner };
            let split = inner.find(|c: char| c.is_ascii_whitespace()).unwrap_or(inner.len());
            return Some(Tok::Open { name: local(&inner[..split]), attrs: &inner[split..], empty });
        }
    }
}

/// Iterates `name="value"` pairs: local attribute name and the raw value.
struct Attrs<'a>(&'a str);

impl<'a> Iterator for Attrs<'a> {
    type Item = (&'a str, &'a str);

    fn next(&mut self) -> Option<(&'a str, &'a str)> {
        let s = self.0.trim_start();
        let eq = s.find('=')?;
        let name = s[..eq].trim();
        let after = s[eq + 1..].trim_start();
        let q = after.chars().next()?;
        if q == '"' || q == '\'' {
            let body = &after[1..];
            let close = body.find(q).unwrap_or(body.len());
            self.0 = body.get(close + 1..).unwrap_or("");
            Some((local(name), &body[..close]))
        } else {
            let end = after.find(|c: char| c.is_ascii_whitespace()).unwrap_or(after.len());
            self.0 = &after[end..];
            Some((local(name), &after[..end]))
        }
    }
}

/// The decoded value of attribute `key` (compared by local name).
fn attr<'a>(attrs: &'a str, key: &str) -> Option<Cow<'a, str>> {
    Attrs(attrs).find(|(n, _)| *n == key).map(|(_, v)| unescape(v))
}

/// Decodes the five predefined entities and numeric character references; unknown entities stay as they are.
fn unescape(s: &str) -> Cow<'_, str> {
    if !s.contains('&') {
        return Cow::Borrowed(s);
    }
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let semi = rest.as_bytes().iter().take(12).position(|&c| c == b';');
        let decoded = semi.and_then(|semi| {
            let ent = &rest[1..semi];
            let c = match ent {
                "amp" => Some('&'),
                "lt" => Some('<'),
                "gt" => Some('>'),
                "quot" => Some('"'),
                "apos" => Some('\''),
                _ => {
                    let n = if let Some(h) = ent.strip_prefix("#x").or_else(|| ent.strip_prefix("#X")) {
                        u32::from_str_radix(h, 16).ok()
                    } else {
                        ent.strip_prefix('#').and_then(|d| d.parse::<u32>().ok())
                    };
                    n.filter(|&n| n != 0).and_then(char::from_u32)
                }
            };
            c.map(|c| (c, semi + 1))
        });
        match decoded {
            Some((c, adv)) => {
                out.push(c);
                rest = &rest[adv..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    Cow::Owned(out)
}

/// Excel writes control characters as `_xHHHH_` (and a literal `_x` as `_x005F_x`).
fn decode_x(s: &str) -> Cow<'_, str> {
    if !s.contains("_x") {
        return Cow::Borrowed(s);
    }
    let b = s.as_bytes();
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    let mut last = 0;
    while i + 7 <= b.len() {
        if b[i] == b'_' && b[i + 1] == b'x' && b[i + 6] == b'_' && b[i + 2..i + 6].iter().all(u8::is_ascii_hexdigit) {
            if let Some(c) = u32::from_str_radix(&s[i + 2..i + 6], 16).ok().and_then(char::from_u32) {
                out.push_str(&s[last..i]);
                out.push(c);
                i += 7;
                last = i;
                continue;
            }
        }
        i += 1;
    }
    out.push_str(&s[last..]);
    Cow::Owned(out)
}

fn utf8(v: Vec<u8>) -> String {
    String::from_utf8(v).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned())
}

// ------------------------------------------------------------------------------------------------ reading

/// How a number is shown: decided by the cell's style.
#[derive(Clone, Copy, PartialEq, Debug)]
enum NumKind {
    General,
    Date,
    DateTime,
    Time { elapsed: bool },
    Percent(u8),
}

fn builtin_kind(id: u32) -> NumKind {
    match id {
        9 => NumKind::Percent(0),
        10 => NumKind::Percent(2),
        14..=17 | 27..=36 | 50..=58 => NumKind::Date,
        18..=21 | 45 | 47 => NumKind::Time { elapsed: false },
        46 => NumKind::Time { elapsed: true },
        22 => NumKind::DateTime,
        _ => NumKind::General,
    }
}

/// Classifies a custom format code by its unquoted, un-bracketed letters.
fn classify_format(code: &str) -> NumKind {
    let c: Vec<char> = code.chars().collect();
    let (mut y, mut m, mut d, mut h, mut s) = (false, false, false, false, false);
    let (mut percent, mut dot, mut elapsed) = (false, false, false);
    let mut decimals = 0u8;
    let mut i = 0;
    while i < c.len() {
        match c[i] {
            ';' => break,
            '"' => {
                i += 1;
                while i < c.len() && c[i] != '"' {
                    i += 1;
                }
            }
            '\\' | '_' | '*' => i += 1,
            '[' => {
                let mut j = i + 1;
                while j < c.len() && c[j] != ']' {
                    j += 1;
                }
                let inner: String = c[i + 1..j.min(c.len())].iter().collect::<String>().to_ascii_lowercase();
                match inner.as_str() {
                    "h" | "hh" => {
                        h = true;
                        elapsed = true;
                    }
                    "m" | "mm" => {
                        m = true;
                        elapsed = true;
                    }
                    "s" | "ss" => {
                        s = true;
                        elapsed = true;
                    }
                    _ => {}
                }
                i = j;
            }
            ch => {
                let lower: String = c[i..c.len().min(i + 5)].iter().collect::<String>().to_ascii_lowercase();
                if lower.starts_with("am/pm") {
                    h = true;
                    i += 4;
                } else if lower.starts_with("a/p") {
                    h = true;
                    i += 2;
                } else {
                    match ch.to_ascii_lowercase() {
                        'y' => y = true,
                        'm' => m = true,
                        'd' => d = true,
                        'h' => h = true,
                        's' => s = true,
                        '%' => percent = true,
                        '.' => dot = true,
                        '0' | '#' | '?' if dot && decimals < 30 => decimals += 1,
                        _ => {}
                    }
                    if !matches!(ch, '0' | '#' | '?' | '.') {
                        dot = dot && ch == ',';
                    }
                }
            }
        }
        i += 1;
    }
    let has_time = h || s;
    let has_date = y || d || (m && !has_time);
    match (has_date, has_time) {
        (true, true) => NumKind::DateTime,
        (true, false) => NumKind::Date,
        (false, true) => NumKind::Time { elapsed },
        _ if percent => NumKind::Percent(decimals),
        _ => NumKind::General,
    }
}

fn parse_styles(xml: &str) -> Vec<NumKind> {
    let mut custom: HashMap<u32, NumKind> = HashMap::new();
    let mut xfs: Vec<u32> = Vec::new();
    let mut in_xfs = false;
    for t in Lexer::new(xml) {
        match t {
            Tok::Open { name: "numFmt", attrs, .. } => {
                let id = attr(attrs, "numFmtId").and_then(|v| v.trim().parse::<u32>().ok());
                if let (Some(id), Some(code)) = (id, attr(attrs, "formatCode")) {
                    custom.insert(id, classify_format(&code));
                }
            }
            Tok::Open { name: "cellXfs", empty, .. } => in_xfs = !empty,
            Tok::Close("cellXfs") => in_xfs = false,
            Tok::Open { name: "xf", attrs, .. } if in_xfs => {
                xfs.push(attr(attrs, "numFmtId").and_then(|v| v.trim().parse().ok()).unwrap_or(0));
            }
            _ => {}
        }
    }
    xfs.into_iter().map(|id| custom.get(&id).copied().unwrap_or_else(|| builtin_kind(id))).collect()
}

/// Shared strings stored back to back, indexed by offsets.
struct Shared {
    buf: String,
    spans: Vec<(usize, usize)>,
}

impl Shared {
    fn get(&self, i: usize) -> &str {
        self.spans.get(i).map_or("", |&(a, b)| &self.buf[a..b])
    }
}

fn parse_shared(xml: &str) -> Shared {
    let mut sh = Shared { buf: String::new(), spans: Vec::new() };
    let (mut start, mut in_si, mut in_t, mut in_rph) = (0, false, false, false);
    for t in Lexer::new(xml) {
        match t {
            Tok::Open { name: "si", empty, .. } => {
                if empty {
                    sh.spans.push((sh.buf.len(), sh.buf.len()));
                } else {
                    in_si = true;
                    start = sh.buf.len();
                }
            }
            Tok::Open { name: "t", empty: false, .. } if in_si && !in_rph => in_t = true,
            Tok::Open { name: "rPh", empty: false, .. } => in_rph = true,
            Tok::Close("t") => in_t = false,
            Tok::Close("rPh") => in_rph = false,
            Tok::Text(x) if in_t => sh.buf.push_str(&unescape(x)),
            Tok::Cdata(x) if in_t => sh.buf.push_str(x),
            Tok::Close("si") if in_si => {
                in_si = false;
                in_t = false;
                if let Cow::Owned(d) = decode_x(&sh.buf[start..]) {
                    sh.buf.truncate(start);
                    sh.buf.push_str(&d);
                }
                sh.spans.push((start, sh.buf.len()));
            }
            _ => {}
        }
    }
    sh
}

/// Days since 1970-01-01 to (year, month, day), proleptic Gregorian.
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Inverse of `civil_from_days`.
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 } as i64;
    let doy = (153 * mp + 2) / 5 + d as i64 - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Serial day number to `YYYY-MM-DD`; reproduces Excel's non-existent 1900-02-29 (serial 60).
fn serial_date(day: i64, d1904: bool) -> String {
    let epoch_days = if d1904 {
        day - 24_107
    } else {
        match day {
            60 => return "1900-02-29".into(),
            0 => return "1900-01-00".into(),
            1..=59 => day - 25_568,
            _ => day - 25_569,
        }
    };
    let (y, m, d) = civil_from_days(epoch_days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// Shortest text that survives a round trip through 15 significant digits (what Excel shows in General).
fn plain_number(x: f64) -> String {
    if x == 0.0 {
        return "0".into();
    }
    let r: f64 = format!("{:.14e}", x).parse().unwrap_or(x);
    let a = r.abs();
    if !(1e-9..1e21).contains(&a) {
        let e = format!("{r:e}");
        return match e.split_once('e') {
            Some((m, ex)) => {
                let (sign, digits) = match ex.strip_prefix('-') {
                    Some(d) => ('-', d),
                    None => ('+', ex),
                };
                format!("{m}E{sign}{digits:0>2}")
            }
            None => e,
        };
    }
    r.to_string()
}

/// True when `raw` is already the canonical short form (no work needed).
fn is_canonical_number(raw: &str) -> bool {
    let b = raw.as_bytes();
    let (neg, b) = match b.split_first() {
        Some((b'-', rest)) => (true, rest),
        _ => (false, b),
    };
    let (int, frac) = match b.iter().position(|&c| c == b'.') {
        Some(p) => (&b[..p], Some(&b[p + 1..])),
        None => (b, None),
    };
    if int.is_empty() || !int.iter().all(u8::is_ascii_digit) || (int.len() > 1 && int[0] == b'0') {
        return false;
    }
    let mut digits = int.len();
    if let Some(f) = frac {
        if f.is_empty() || !f.iter().all(u8::is_ascii_digit) || f[f.len() - 1] == b'0' {
            return false;
        }
        digits += f.len();
    }
    digits <= 15 && !(neg && int == b"0" && frac.is_none())
}

fn render_number(raw: &str, kind: NumKind, d1904: bool) -> String {
    let raw = raw.trim();
    if kind == NumKind::General && is_canonical_number(raw) {
        return raw.to_string();
    }
    let Ok(x) = raw.parse::<f64>() else { return raw.to_string() };
    if !x.is_finite() {
        return raw.to_string();
    }
    match kind {
        NumKind::General => plain_number(x),
        NumKind::Percent(d) => {
            let s = format!("{:.*}", d as usize, x * 100.0);
            if s.starts_with('-') && !s.bytes().any(|c| (b'1'..=b'9').contains(&c)) {
                format!("{}%", &s[1..])
            } else {
                format!("{s}%")
            }
        }
        _ => date_text(x, kind, d1904).unwrap_or_else(|| plain_number(x)),
    }
}

fn date_text(x: f64, kind: NumKind, d1904: bool) -> Option<String> {
    if !(0.0..2_958_466.0).contains(&x) {
        return None;
    }
    let total = (x * 86_400.0).round() as i64;
    let (day, secs) = (total.div_euclid(86_400), total.rem_euclid(86_400));
    let (h, mi, s) = (secs / 3600, secs % 3600 / 60, secs % 60);
    match kind {
        NumKind::Time { elapsed } => {
            let hours = if elapsed { total / 3600 } else { h };
            Some(format!("{hours:02}:{mi:02}:{s:02}"))
        }
        NumKind::Date => Some(serial_date(day, d1904)),
        NumKind::DateTime if day == 0 && !d1904 => Some(format!("{h:02}:{mi:02}:{s:02}")),
        _ => Some(format!("{} {h:02}:{mi:02}:{s:02}", serial_date(day, d1904))),
    }
}

/// `B7` to zero-based (column, row).
fn parse_ref(r: &str) -> Option<(usize, usize)> {
    let b = r.as_bytes();
    let mut i = 0;
    let mut col = 0usize;
    while i < b.len() && b[i].is_ascii_alphabetic() {
        col = col * 26 + (b[i].to_ascii_uppercase() - b'A' + 1) as usize;
        i += 1;
        if i > 3 {
            return None;
        }
    }
    let digits = &b[i..];
    if i == 0 || digits.is_empty() || digits.len() > 8 || !digits.iter().all(u8::is_ascii_digit) {
        return None;
    }
    let row: usize = r[i..].parse().ok()?;
    (row >= 1).then(|| (col - 1, row - 1))
}

fn unsupported_cells(limit: usize) -> Error {
    let digits = limit.to_string();
    let mut grouped = String::new();
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i).is_multiple_of(3) {
            grouped.push(',');
        }
        grouped.push(c);
    }
    Error::Unsupported(format!("Workbook too large: more than {grouped} cells"))
}

/// Most characters of cell text a workbook may hold in memory at once: the cell count alone does not bound it, since
/// one 32,767 character string can be referenced by a million cells.
const MAX_TEXT: usize = 256 << 20;

struct Ctx<'a> {
    /// Characters of cell text still allowed.
    text_left: std::cell::Cell<usize>,
    max_cells: usize,
    shared: &'a Shared,
    styles: &'a [NumKind],
    d1904: bool,
}

fn parse_sheet(xml: &str, ctx: &Ctx, left: &mut usize) -> Result<Vec<Vec<String>>> {
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut cur_row: usize = 0;
    let mut have_row = false;
    let mut next_col = 0usize;
    // Current cell.
    let mut active = false;
    let (mut col, mut row_idx) = (0usize, 0usize);
    let (mut ty, mut style) = ("", usize::MAX);
    let (mut in_v, mut in_is, mut in_t, mut in_rph) = (false, false, false, false);
    let mut v = String::new();
    let mut is_text = String::new();
    for t in Lexer::new(xml) {
        match t {
            Tok::Open { name: "row", attrs, .. } => {
                match attr(attrs, "r").and_then(|r| r.trim().parse::<usize>().ok()).filter(|&r| r >= 1) {
                    Some(r) => cur_row = r - 1,
                    None if have_row => cur_row += 1,
                    None => {}
                }
                have_row = true;
                next_col = 0;
            }
            Tok::Open { name: "c", attrs, empty } => {
                let mut cell_ref = None;
                ty = "";
                style = usize::MAX;
                for (n, val) in Attrs(attrs) {
                    match n {
                        "r" => cell_ref = parse_ref(val),
                        "t" => ty = val,
                        "s" => style = val.trim().parse().unwrap_or(usize::MAX),
                        _ => {}
                    }
                }
                match cell_ref {
                    Some((c, r)) => {
                        col = c;
                        row_idx = r;
                        cur_row = r;
                        have_row = true;
                    }
                    None => {
                        col = next_col;
                        row_idx = if have_row { cur_row } else { 0 };
                    }
                }
                next_col = col + 1;
                active = !empty && row_idx < MAX_ROWS && col < MAX_COLS;
                v.clear();
                is_text.clear();
            }
            Tok::Open { name: "v", empty: false, .. } if active => in_v = true,
            Tok::Open { name: "is", empty: false, .. } if active => in_is = true,
            Tok::Open { name: "t", empty: false, .. } if active && in_is && !in_rph => in_t = true,
            Tok::Open { name: "rPh", empty: false, .. } if active => in_rph = true,
            Tok::Close("v") => in_v = false,
            Tok::Close("t") => in_t = false,
            Tok::Close("rPh") => in_rph = false,
            Tok::Close("is") => {
                in_is = false;
                in_t = false;
            }
            Tok::Text(x) if in_v => v.push_str(&unescape(x)),
            Tok::Cdata(x) if in_v => v.push_str(x),
            Tok::Text(x) if in_t => is_text.push_str(&unescape(x)),
            Tok::Cdata(x) if in_t => is_text.push_str(x),
            Tok::Close("c") if active => {
                active = false;
                in_v = false;
                in_is = false;
                in_t = false;
                in_rph = false;
                let text: String = match ty {
                    "s" => ctx.shared.get(v.trim().parse().unwrap_or(usize::MAX)).to_string(),
                    "inlineStr" => decode_x(&is_text).into_owned(),
                    "b" => match v.trim() {
                        "1" => "TRUE".into(),
                        "0" => "FALSE".into(),
                        o if o.eq_ignore_ascii_case("true") => "TRUE".into(),
                        o if o.eq_ignore_ascii_case("false") => "FALSE".into(),
                        o => o.to_string(),
                    },
                    "str" | "e" => v.clone(),
                    "d" => v.replace('T', " "),
                    _ if v.trim().is_empty() => {
                        if is_text.is_empty() {
                            String::new()
                        } else {
                            decode_x(&is_text).into_owned()
                        }
                    }
                    _ => {
                        let kind = ctx.styles.get(style).copied().unwrap_or(NumKind::General);
                        render_number(&v, kind, ctx.d1904)
                    }
                };
                if text.is_empty() {
                    continue;
                }
                if row_idx >= rows.len() {
                    let add = row_idx + 1 - rows.len();
                    if add > *left {
                        return Err(unsupported_cells(ctx.max_cells));
                    }
                    *left -= add;
                    rows.resize_with(row_idx + 1, Vec::new);
                }
                let r = &mut rows[row_idx];
                if col >= r.len() {
                    let add = col + 1 - r.len();
                    if add > *left {
                        return Err(unsupported_cells(ctx.max_cells));
                    }
                    *left -= add;
                    r.resize(col + 1, String::new());
                }
                let n = text.len();
                if n > ctx.text_left.get() {
                    return Err(Error::Unsupported("Workbook too large: it holds too much text".into()));
                }
                ctx.text_left.set(ctx.text_left.get() - n);
                r[col] = text;
            }
            _ => {}
        }
    }
    for r in &mut rows {
        while r.last().is_some_and(String::is_empty) {
            r.pop();
        }
    }
    while rows.last().is_some_and(Vec::is_empty) {
        rows.pop();
    }
    Ok(rows)
}

/// Resolves a relationship target against the directory of the part that owns the relationships file.
fn resolve(base_dir: &str, target: &str) -> String {
    let t = target.replace('\\', "/");
    let joined = match t.strip_prefix('/') {
        Some(abs) => abs.to_string(),
        None => format!("{base_dir}{t}"),
    };
    let mut out: Vec<&str> = Vec::new();
    for seg in joined.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                out.pop();
            }
            s => out.push(s),
        }
    }
    out.join("/")
}

fn read_with(bytes: &[u8], max_part: usize, max_cells: usize) -> Result<Vec<Sheet>> {
    if bytes.starts_with(&OLE2) {
        return Err(Error::Unsupported("Old .xls or password-protected workbook: save it as .xlsx first".into()));
    }
    let zip = ZipArchive::new(bytes)?;
    let part = |name: &str| -> Result<Option<String>> {
        match zip.read(name, max_part) {
            Ok(v) => Ok(v.map(utf8)),
            Err(Error::TooLarge { .. }) => Err(Error::Unsupported(format!(
                "Workbook part {name} is too large (over {} MiB uncompressed)",
                max_part >> 20
            ))),
            Err(e) => Err(e),
        }
    };
    let wb =
        part("xl/workbook.xml")?.ok_or_else(|| Error::Corrupt("not an xlsx workbook (no xl/workbook.xml)".into()))?;
    let rels = part("xl/_rels/workbook.xml.rels")?.unwrap_or_default();
    let mut targets: HashMap<String, (String, bool)> = HashMap::new();
    for t in Lexer::new(&rels) {
        if let Tok::Open { name: "Relationship", attrs, .. } = t {
            if let (Some(id), Some(target)) = (attr(attrs, "Id"), attr(attrs, "Target")) {
                let skip = attr(attrs, "Type").is_some_and(|t| t.ends_with("chartsheet") || t.ends_with("dialogsheet"));
                targets.insert(id.into_owned(), (resolve("xl/", &target), skip));
            }
        }
    }
    let mut d1904 = false;
    let mut ordinal = 0;
    let mut entries: Vec<(String, String)> = Vec::new(); // (name, part path)
    for t in Lexer::new(&wb) {
        match t {
            // Only ever turns the flag on: Excel also writes a second, unrelated `x15:workbookPr` in `extLst`.
            Tok::Open { name: "workbookPr", attrs, .. } => {
                d1904 |= attr(attrs, "date1904").is_some_and(|v| matches!(v.trim(), "1" | "true"));
            }
            Tok::Open { name: "sheet", attrs, .. } => {
                ordinal += 1;
                if attr(attrs, "state").is_some_and(|s| s == "hidden" || s == "veryHidden") {
                    continue;
                }
                let name = attr(attrs, "name").map(Cow::into_owned).unwrap_or_default();
                let path = match attr(attrs, "id").and_then(|id| targets.get(id.as_ref()).cloned()) {
                    Some((_, true)) => continue,
                    Some((p, false)) => p,
                    None => format!("xl/worksheets/sheet{ordinal}.xml"),
                };
                entries.push((name, path));
            }
            _ => {}
        }
    }
    let styles = part("xl/styles.xml")?.map(|x| parse_styles(&x)).unwrap_or_default();
    let shared = part("xl/sharedStrings.xml")?
        .map(|x| parse_shared(&x))
        .unwrap_or(Shared { buf: String::new(), spans: Vec::new() });
    let ctx = Ctx { text_left: std::cell::Cell::new(MAX_TEXT), max_cells, shared: &shared, styles: &styles, d1904 };
    let mut left = max_cells;
    let mut sheets = Vec::with_capacity(entries.len());
    for (name, path) in entries {
        let rows = match part(&path)? {
            Some(xml) => parse_sheet(&xml, &ctx, &mut left)?,
            None => Vec::new(),
        };
        sheets.push(Sheet { name, rows });
    }
    Ok(sheets)
}

/// Reads the visible worksheets of an XLSX/XLSM workbook.
///
/// Cell text is what a user sees: shared and inline strings as they are, booleans as `TRUE`/`FALSE`, errors as
/// their text, formulas as their cached value, numbers in the shortest form (15 significant digits, like
/// Excel's General format), dates and times as ISO `YYYY-MM-DD`, `YYYY-MM-DD HH:MM:SS` or `HH:MM:SS`
/// (1900 and 1904 systems, including Excel's fake 1900-02-29), percent formats as percentages.
///
/// Hidden and very hidden sheets are skipped. Old `.xls`/encrypted files give `Error::Unsupported`, as do
/// workbooks with more than 5,000,000 cells or a part that inflates beyond 256 MiB. Damaged input gives
/// `Error::Corrupt` and never panics.
pub fn read_xlsx(bytes: &[u8]) -> Result<Vec<Sheet>> {
    read_with(bytes, MAX_PART, MAX_CELLS)
}

// ------------------------------------------------------------------------------------------------ CSV

fn needs_quotes(f: &str, delim: char) -> bool {
    f.chars().any(|c| c == delim || matches!(c, '"' | '\r' | '\n'))
}

/// A cell that starts like a formula would be run by Excel when the CSV is opened (`=cmd|...`, `@SUM(...)`, `+1+1`).
/// Such a cell gets a leading apostrophe, which Excel shows as text; numbers (`-5`, `+3`) are left alone.
fn formula_safe(f: &str) -> std::borrow::Cow<'_, str> {
    let risky = match f.chars().next() {
        Some('=' | '@' | '\t' | '\r') => true,
        Some('+' | '-') => f.parse::<f64>().is_err(),
        _ => false,
    };
    if risky {
        std::borrow::Cow::Owned(format!("'{f}"))
    } else {
        std::borrow::Cow::Borrowed(f)
    }
}

/// Encodes rows as CSV with `,` between cells (see [`csv_bytes_with`]).
#[cfg(test)]
pub fn csv_bytes(rows: &[Vec<String>]) -> Vec<u8> {
    csv_bytes_with(rows, ',')
}

/// Encodes rows as CSV: UTF-8 with a BOM, `delim` between cells, CRLF line ends, RFC 4180 quoting only where needed.
///
/// Excel splits a CSV on the *list separator of the user's Windows* (`;` in much of Europe and Latin America), not on
/// `,`: pass that one (`os::list_separator`) and the file opens in columns. A cell that starts like a formula gets a
/// leading apostrophe so opening the file cannot run it.
pub fn csv_bytes_with(rows: &[Vec<String>], delim: char) -> Vec<u8> {
    let mut out = String::from("\u{FEFF}");
    for r in rows {
        for (i, f) in r.iter().enumerate() {
            if i > 0 {
                out.push(delim);
            }
            let f = formula_safe(f);
            if needs_quotes(&f, delim) {
                out.push('"');
                out.push_str(&f.replace('"', "\"\""));
                out.push('"');
            } else {
                out.push_str(&f);
            }
        }
        out.push_str("\r\n");
    }
    out.into_bytes()
}

/// Parses with one delimiter; `limit` stops after that many records.
fn parse_with(text: &str, delim: u8, limit: usize) -> Vec<Vec<String>> {
    let b = text.as_bytes();
    let mut rows: Vec<Vec<String>> = Vec::new();
    let mut fields: Vec<String> = Vec::new();
    let mut buf: Vec<u8> = Vec::new();
    let mut i = 0;
    let mut at_start = true; // at the start of a record
    let flush = |buf: &mut Vec<u8>| utf8(std::mem::take(buf));
    while i < b.len() && rows.len() < limit {
        at_start = false;
        match b[i] {
            b'"' if buf.is_empty() => {
                // Quoted field.
                i += 1;
                while i < b.len() {
                    if b[i] == b'"' {
                        if b.get(i + 1) == Some(&b'"') {
                            buf.push(b'"');
                            i += 2;
                            continue;
                        }
                        i += 1;
                        break;
                    }
                    buf.push(b[i]);
                    i += 1;
                }
            }
            c if c == delim => {
                fields.push(flush(&mut buf));
                i += 1;
            }
            b'\r' | b'\n' => {
                fields.push(flush(&mut buf));
                rows.push(std::mem::take(&mut fields));
                i += if b[i] == b'\r' && b.get(i + 1) == Some(&b'\n') { 2 } else { 1 };
                at_start = true;
            }
            c => {
                buf.push(c);
                i += 1;
            }
        }
    }
    if !at_start && rows.len() < limit {
        fields.push(flush(&mut buf));
        rows.push(fields);
    }
    rows
}

fn sniff_delimiter(text: &str) -> u8 {
    let mut best = (b',', 0usize);
    for &d in b",;\t|" {
        let recs = parse_with(text, d, 20);
        let mut freq: HashMap<usize, usize> = HashMap::new();
        for r in recs.iter().filter(|r| !(r.len() == 1 && r[0].is_empty())) {
            *freq.entry(r.len()).or_insert(0) += 1;
        }
        // Modal column count (larger wins a tie); it must be more than one column.
        let modal = freq.iter().filter(|&(&n, _)| n > 1).max_by_key(|&(&n, &f)| (f, n)).map(|(_, &f)| f).unwrap_or(0);
        if modal > best.1 {
            best = (d, modal);
        }
    }
    best.0
}

/// Parses CSV text (BOM, CRLF/LF/CR, quoted fields with embedded delimiters, quotes and newlines).
///
/// The delimiter is sniffed among `,` `;` TAB `|`: the one giving the most consistent column count (more than
/// one) over the first 20 records wins, ties going in that order. Ragged rows are kept as they are and a final
/// line break does not add an empty row.
pub fn parse_csv(text: &str) -> Vec<Vec<String>> {
    let text = text.strip_prefix('\u{FEFF}').unwrap_or(text);
    parse_with(text, sniff_delimiter(text), usize::MAX)
}

/// Tab-separated text, one line per row, each line ending in LF; tabs and line breaks inside a cell become a
/// single space.
pub fn rows_to_text(rows: &[Vec<String>]) -> String {
    let mut out = String::new();
    for r in rows {
        for (i, f) in r.iter().enumerate() {
            if i > 0 {
                out.push('\t');
            }
            let mut in_run = false;
            for c in f.chars() {
                if matches!(c, '\t' | '\r' | '\n') {
                    if !in_run {
                        out.push(' ');
                    }
                    in_run = true;
                } else {
                    in_run = false;
                    out.push(c);
                }
            }
        }
        out.push('\n');
    }
    out
}

// ------------------------------------------------------------------------------------------------ writing

fn esc(out: &mut String, s: &str) {
    for c in s.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\r' => out.push_str("&#13;"),
            '\t' | '\n' => out.push(c),
            c if (c as u32) < 0x20 || matches!(c as u32, 0xFFFE | 0xFFFF) => {}
            c => out.push(c),
        }
    }
}

fn col_name(mut c: usize) -> String {
    let mut v = Vec::new();
    loop {
        v.push(b'A' + (c % 26) as u8);
        if c < 26 {
            break;
        }
        c = c / 26 - 1;
    }
    v.reverse();
    utf8(v)
}

enum Cell<'a> {
    Text(&'a str),
    Num(&'a str),
    Date(String),
    DateTime(String),
}

/// Digits of a plain number: `-?\d+(\.\d+)?`, no leading zeros, at most 15 digits, not negative zero.
fn is_plain_number(s: &str) -> bool {
    let body = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = match body.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (body, None),
    };
    let digits = |x: &str| !x.is_empty() && x.bytes().all(|c| c.is_ascii_digit());
    if !digits(int) || frac.is_some_and(|f| !digits(f)) || (int.len() > 1 && int.starts_with('0')) {
        return false;
    }
    let count = if int == "0" { 0 } else { int.len() } + frac.map_or(0, str::len);
    let zero = body.bytes().all(|c| c == b'0' || c == b'.');
    count <= 15 && !(zero && s.starts_with('-'))
}

fn digits2(b: &[u8]) -> Option<u32> {
    (b.len() == 2 && b.iter().all(u8::is_ascii_digit)).then(|| ((b[0] - b'0') * 10 + (b[1] - b'0')) as u32)
}

/// `YYYY-MM-DD` as an Excel serial (valid calendar dates from 1900-03-01 on).
fn iso_date_serial(s: &str) -> Option<i64> {
    let b = s.as_bytes();
    if b.len() != 10 || b[4] != b'-' || b[7] != b'-' || !b[..4].iter().all(u8::is_ascii_digit) {
        return None;
    }
    let y: i64 = s[..4].parse().ok()?;
    let (m, d) = (digits2(&b[5..7])?, digits2(&b[8..10])?);
    let days = days_from_civil(y, m, d);
    // Round trip rejects 2024-02-30 and friends.
    if !(1..=12).contains(&m) || civil_from_days(days) != (y, m, d) {
        return None;
    }
    let serial = days + 25_569;
    (61..=2_958_465).contains(&serial).then_some(serial)
}

fn classify(s: &str) -> Cell<'_> {
    if is_plain_number(s) {
        return Cell::Num(s);
    }
    let b = s.as_bytes();
    if b.len() == 10 {
        if let Some(n) = iso_date_serial(s) {
            return Cell::Date(n.to_string());
        }
    } else if b.len() == 19 && b[10] == b' ' && b[13] == b':' && b[16] == b':' {
        if let (Some(n), Some(h), Some(mi), Some(se)) =
            (iso_date_serial(&s[..10]), digits2(&b[11..13]), digits2(&b[14..16]), digits2(&b[17..19]))
        {
            if h < 24 && mi < 60 && se < 60 {
                let secs = (h * 3600 + mi * 60 + se) as f64;
                return Cell::DateTime((n as f64 + secs / 86_400.0).to_string());
            }
        }
    }
    Cell::Text(s)
}

fn sanitize_sheet_name(name: &str) -> String {
    let cleaned: String = name
        .chars()
        .filter(|c| !c.is_control())
        .map(|c| if matches!(c, '[' | ']' | ':' | '*' | '?' | '/' | '\\') { '_' } else { c })
        .collect();
    let t = cleaned.trim().trim_matches('\'').trim();
    let cut: String = t.chars().take(31).collect();
    let cut = cut.trim_end().to_string();
    if cut.is_empty() {
        "Sheet1".into()
    } else {
        cut
    }
}

fn cap_chars(s: &str) -> &str {
    match s.char_indices().nth(MAX_CELL_CHARS) {
        Some((i, _)) => &s[..i],
        None => s,
    }
}

const XML_HEAD: &str = "<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>\n";
const NS_MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
const NS_REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
const NS_PKG_REL: &str = "http://schemas.openxmlformats.org/package/2006/relationships";

/// Writes a one-sheet XLSX that Excel and LibreOffice open without repair prompts.
///
/// * Plain integers and decimals (`-?\d+(\.\d+)?`) become numeric cells, except those with leading zeros
///   (`007`), more than 15 digits, or a leading `+`/space: those stay text so phone numbers and ids survive.
///   `TRUE`/`FALSE` stay text.
/// * `YYYY-MM-DD` and `YYYY-MM-DD HH:MM:SS` (valid calendar dates from 1900-03-01) become date cells.
/// * With more than one row the first row is bold and frozen; column widths fit the content (8 to 60).
/// * Text is XML-escaped, characters illegal in XML 1.0 are dropped, and a cell is cut at Excel's 32,767
///   characters. Excel's limits apply: rows beyond 1,048,576 and columns beyond 16,384 are dropped.
/// * The sheet name loses `[]:*?/\`, is cut to 31 characters and falls back to `Sheet1` when empty.
///
/// The package carries no personal data (creator "Radial") and the output is deterministic.
pub fn write_xlsx(rows: &[Vec<String>], sheet_name: &str) -> Vec<u8> {
    let rows = &rows[..rows.len().min(MAX_ROWS)];
    let ncols = rows.iter().map(|r| r.len().min(MAX_COLS)).max().unwrap_or(0);
    let names: Vec<String> = (0..ncols).map(col_name).collect();
    let header = rows.len() > 1;

    let mut index: HashMap<&str, u32> = HashMap::new();
    let mut strings: Vec<&str> = Vec::new();
    let mut refs = 0usize;
    let mut widths = vec![0usize; ncols];

    let mut sheet = String::with_capacity(rows.len() * ncols * 28 + 1024);
    sheet.push_str(XML_HEAD);
    let _ = write!(sheet, "<worksheet xmlns=\"{NS_MAIN}\">");
    if ncols == 0 {
        sheet.push_str("<dimension ref=\"A1\"/>");
    } else {
        let _ = write!(sheet, "<dimension ref=\"A1:{}{}\"/>", names[ncols - 1], rows.len());
    }
    sheet.push_str("<sheetViews><sheetView tabSelected=\"1\" workbookViewId=\"0\">");
    if header {
        sheet.push_str(
            "<pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/><selection pane=\"bottomLeft\" activeCell=\"A2\" sqref=\"A2\"/>",
        );
    }
    sheet.push_str("</sheetView></sheetViews><sheetFormatPr defaultRowHeight=\"15\"/>");
    let cols_at = sheet.len(); // the <cols> element is inserted here once widths are known
    sheet.push_str("<sheetData>");
    for (ri, r) in rows.iter().enumerate() {
        let mut opened = false;
        for (ci, text) in r.iter().take(MAX_COLS).enumerate() {
            let text = cap_chars(text);
            if text.is_empty() {
                continue;
            }
            if ri < 2000 {
                widths[ci] = widths[ci].max(text.chars().take(64).count());
            }
            if !opened {
                let _ = write!(sheet, "<row r=\"{}\">", ri + 1);
                opened = true;
            }
            let cell = if ri == 0 && header { Cell::Text(text) } else { classify(text) };
            let style = if ri == 0 && header { " s=\"1\"" } else { "" };
            match cell {
                Cell::Num(n) => {
                    let _ = write!(sheet, "<c r=\"{}{}\"><v>{n}</v></c>", names[ci], ri + 1);
                }
                Cell::Date(n) => {
                    let _ = write!(sheet, "<c r=\"{}{}\" s=\"2\"><v>{n}</v></c>", names[ci], ri + 1);
                }
                Cell::DateTime(n) => {
                    let _ = write!(sheet, "<c r=\"{}{}\" s=\"3\"><v>{n}</v></c>", names[ci], ri + 1);
                }
                Cell::Text(t) => {
                    let next = strings.len() as u32;
                    let id = *index.entry(t).or_insert_with(|| {
                        strings.push(t);
                        next
                    });
                    refs += 1;
                    let _ = write!(sheet, "<c r=\"{}{}\"{style} t=\"s\"><v>{id}</v></c>", names[ci], ri + 1);
                }
            }
        }
        if opened {
            sheet.push_str("</row>");
        }
    }
    sheet.push_str("</sheetData></worksheet>");
    if ncols > 0 {
        let mut cols = String::from("<cols>");
        for (i, w) in widths.iter().enumerate() {
            let width = ((*w + 2) as f64).clamp(8.0, 60.0);
            let _ = write!(cols, "<col min=\"{0}\" max=\"{0}\" width=\"{width}\" customWidth=\"1\"/>", i + 1);
        }
        cols.push_str("</cols>");
        sheet.insert_str(cols_at, &cols);
    }

    let mut sst = String::from(XML_HEAD);
    let _ = write!(sst, "<sst xmlns=\"{NS_MAIN}\" count=\"{refs}\" uniqueCount=\"{}\">", strings.len());
    for s in &strings {
        let edge = s.starts_with(char::is_whitespace) || s.ends_with(char::is_whitespace) || s.contains('\n');
        sst.push_str(if edge { "<si><t xml:space=\"preserve\">" } else { "<si><t>" });
        esc(&mut sst, s);
        sst.push_str("</t></si>");
    }
    sst.push_str("</sst>");

    let mut name = String::new();
    esc(&mut name, &sanitize_sheet_name(sheet_name));
    let workbook = format!(
        "{XML_HEAD}<workbook xmlns=\"{NS_MAIN}\" xmlns:r=\"{NS_REL}\"><bookViews><workbookView/></bookViews><sheets><sheet name=\"{name}\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>"
    );
    let wb_rels = format!(
        "{XML_HEAD}<Relationships xmlns=\"{NS_PKG_REL}\"><Relationship Id=\"rId1\" Type=\"{NS_REL}/worksheet\" Target=\"worksheets/sheet1.xml\"/><Relationship Id=\"rId2\" Type=\"{NS_REL}/styles\" Target=\"styles.xml\"/><Relationship Id=\"rId3\" Type=\"{NS_REL}/sharedStrings\" Target=\"sharedStrings.xml\"/></Relationships>"
    );
    let root_rels = format!(
        "{XML_HEAD}<Relationships xmlns=\"{NS_PKG_REL}\"><Relationship Id=\"rId1\" Type=\"{NS_REL}/officeDocument\" Target=\"xl/workbook.xml\"/><Relationship Id=\"rId2\" Type=\"{NS_PKG_REL}/metadata/core-properties\" Target=\"docProps/core.xml\"/><Relationship Id=\"rId3\" Type=\"{NS_REL}/extended-properties\" Target=\"docProps/app.xml\"/></Relationships>"
    );
    let content_types = format!(
        "{XML_HEAD}<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/>\
<Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/>\
<Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/>\
<Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/>\
<Override PartName=\"/xl/sharedStrings.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sharedStrings+xml\"/>\
<Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/>\
<Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/></Types>"
    );
    let styles = format!(
        "{XML_HEAD}<styleSheet xmlns=\"{NS_MAIN}\"><numFmts count=\"2\"><numFmt numFmtId=\"164\" formatCode=\"yyyy\\-mm\\-dd\"/><numFmt numFmtId=\"165\" formatCode=\"yyyy\\-mm\\-dd\\ hh:mm:ss\"/></numFmts>\
<fonts count=\"2\"><font><sz val=\"11\"/><name val=\"Calibri\"/><family val=\"2\"/></font><font><b/><sz val=\"11\"/><name val=\"Calibri\"/><family val=\"2\"/></font></fonts>\
<fills count=\"2\"><fill><patternFill patternType=\"none\"/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills>\
<borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders>\
<cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs>\
<cellXfs count=\"4\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\"/>\
<xf numFmtId=\"0\" fontId=\"1\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyFont=\"1\"/>\
<xf numFmtId=\"164\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/>\
<xf numFmtId=\"165\" fontId=\"0\" fillId=\"0\" borderId=\"0\" xfId=\"0\" applyNumberFormat=\"1\"/></cellXfs>\
<cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\"/></cellStyles></styleSheet>"
    );
    let core = format!(
        "{XML_HEAD}<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:dcterms=\"http://purl.org/dc/terms/\" xmlns:dcmitype=\"http://purl.org/dc/dcmitype/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\"><dc:creator>Radial</dc:creator><cp:lastModifiedBy>Radial</cp:lastModifiedBy></cp:coreProperties>"
    );
    let app = format!(
        "{XML_HEAD}<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>Radial</Application></Properties>"
    );

    let mut z = ZipWriter::new();
    z.add("[Content_Types].xml", content_types.as_bytes(), true);
    z.add("_rels/.rels", root_rels.as_bytes(), true);
    z.add("docProps/app.xml", app.as_bytes(), true);
    z.add("docProps/core.xml", core.as_bytes(), true);
    z.add("xl/workbook.xml", workbook.as_bytes(), true);
    z.add("xl/_rels/workbook.xml.rels", wb_rels.as_bytes(), true);
    z.add("xl/styles.xml", styles.as_bytes(), true);
    z.add("xl/sharedStrings.xml", sst.as_bytes(), true);
    z.add("xl/worksheets/sheet1.xml", sheet.as_bytes(), true);
    z.finish()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    const MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
    const REL: &str = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";

    /// Builds an XLSX package byte by byte, so every parser rule is pinned by a hand-written fixture.
    struct Fx {
        shared: Option<String>,
        styles: Option<String>,
        date1904: bool,
        sheets: Vec<(String, String, String)>, // name (XML-escaped), state, sheetData
    }

    impl Fx {
        fn new(sheet_data: &str) -> Fx {
            Fx { shared: None, styles: None, date1904: false, sheets: vec![("S".into(), "".into(), sheet_data.into())] }
        }
        fn shared(mut self, items: &str) -> Fx {
            self.shared = Some(format!("<sst xmlns=\"{MAIN}\">{items}</sst>"));
            self
        }
        fn styles(mut self, num_fmts: &str, xf_ids: &[u32]) -> Fx {
            let xfs: String = xf_ids.iter().map(|i| format!("<xf numFmtId=\"{i}\" fontId=\"0\"/>")).collect();
            self.styles = Some(format!(
                "<styleSheet xmlns=\"{MAIN}\"><numFmts>{num_fmts}</numFmts><cellStyleXfs><xf numFmtId=\"14\"/></cellStyleXfs><cellXfs>{xfs}</cellXfs></styleSheet>"
            ));
            self
        }
        fn d1904(mut self) -> Fx {
            self.date1904 = true;
            self
        }
        fn sheet(mut self, name: &str, state: &str, data: &str) -> Fx {
            self.sheets.push((name.into(), state.into(), data.into()));
            self
        }
        fn build(&self) -> Vec<u8> {
            let mut w = ZipWriter::new();
            let pr = if self.date1904 { "<workbookPr date1904=\"1\"/>" } else { "<workbookPr/>" };
            let mut sheets = String::new();
            let mut rels = String::new();
            for (i, (name, state, _)) in self.sheets.iter().enumerate() {
                let st = if state.is_empty() { String::new() } else { format!(" state=\"{state}\"") };
                sheets.push_str(&format!("<sheet name=\"{name}\" sheetId=\"{}\"{st} r:id=\"rId{}\"/>", i + 1, i + 1));
                rels.push_str(&format!(
                    "<Relationship Id=\"rId{}\" Type=\"{REL}/worksheet\" Target=\"worksheets/sheet{}.xml\"/>",
                    i + 1,
                    i + 1
                ));
            }
            rels.push_str(&format!("<Relationship Id=\"rIdS\" Type=\"{REL}/styles\" Target=\"styles.xml\"/>"));
            rels.push_str(&format!(
                "<Relationship Id=\"rIdH\" Type=\"{REL}/sharedStrings\" Target=\"/xl/sharedStrings.xml\"/>"
            ));
            let wb = format!(
                "<?xml version=\"1.0\" encoding=\"UTF-8\"?><workbook xmlns=\"{MAIN}\" xmlns:r=\"{REL}\">{pr}<sheets>{sheets}</sheets><extLst><ext uri=\"{{x}}\" xmlns:x15=\"urn:x15\"><x15:workbookPr chartTrackingRefBase=\"1\"/></ext></extLst></workbook>"
            );
            w.add("xl/workbook.xml", wb.as_bytes(), true);
            let rl = format!(
                "<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{rels}</Relationships>"
            );
            w.add("xl/_rels/workbook.xml.rels", rl.as_bytes(), true);
            if let Some(s) = &self.shared {
                w.add("xl/sharedStrings.xml", s.as_bytes(), true);
            }
            if let Some(s) = &self.styles {
                w.add("xl/styles.xml", s.as_bytes(), true);
            }
            for (i, (_, _, data)) in self.sheets.iter().enumerate() {
                let x = format!("<worksheet xmlns=\"{MAIN}\"><sheetData>{data}</sheetData></worksheet>");
                w.add(&format!("xl/worksheets/sheet{}.xml", i + 1), x.as_bytes(), true);
            }
            w.finish()
        }
    }

    fn read1(fx: Fx) -> Vec<Vec<String>> {
        let mut sheets = read_xlsx(&fx.build()).unwrap();
        assert_eq!(sheets.len(), 1);
        sheets.remove(0).rows
    }

    fn row(cells: &[&str]) -> Vec<String> {
        cells.iter().map(|s| s.to_string()).collect()
    }

    fn grid(rows: &[&[&str]]) -> Vec<Vec<String>> {
        rows.iter().map(|r| row(r)).collect()
    }

    // ------------------------------------------------------------------ reading: strings

    #[test]
    fn shared_strings_keep_rich_text_whitespace_entities_and_skip_phonetics() {
        let fx = Fx::new(
            "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" t=\"s\"><v>1</v></c><c r=\"C1\" t=\"s\"><v>2</v></c><c r=\"D1\" t=\"s\"><v>3</v></c><c r=\"E1\" t=\"s\"><v>4</v></c><c r=\"F1\" t=\"s\"><v>99</v></c></row>",
        )
        .shared(
            "<si><r><rPr><b/></rPr><t>Hel</t></r><r><t xml:space=\"preserve\">lo </t></r><r><t>World</t></r><rPh sb=\"0\" eb=\"1\"><t>ignored</t></rPh><phoneticPr fontId=\"1\"/></si>\
             <si><t xml:space=\"preserve\">  x  </t></si>\
             <si><t>a &amp; b &lt;c&gt; &#x1F600; &#65; &quot;q&quot;</t></si>\
             <si><t>a_x000D_b _x005F_x0041_</t></si>\
             <si><t><![CDATA[<raw & text>]]></t></si>",
        );
        assert_eq!(
            read1(fx),
            vec![row(&["Hello World", "  x  ", "a & b <c> \u{1F600} A \"q\"", "a\rb _x0041_", "<raw & text>"])]
        );
    }

    #[test]
    fn inline_strings_formula_strings_and_cached_formula_values() {
        let fx = Fx::new(
            "<row r=\"1\">\
             <c r=\"A1\" t=\"inlineStr\"><is><t>inline</t></is></c>\
             <c r=\"B1\" t=\"inlineStr\"><is><r><t>ri</t></r><r><t>ch</t></r></is></c>\
             <c r=\"C1\" t=\"str\"><f>A1&amp;\"x\"</f><v>inlinex</v></c>\
             <c r=\"D1\"><f>1+1</f><v>2</v></c>\
             <c r=\"E1\"><f>SUM(1,2)</f></c>\
             </row>",
        );
        assert_eq!(read1(fx), vec![row(&["inline", "rich", "inlinex", "2"])]);
    }

    #[test]
    fn booleans_errors_and_iso_dates_cells() {
        let fx = Fx::new(
            "<row r=\"1\"><c r=\"A1\" t=\"b\"><v>1</v></c><c r=\"B1\" t=\"b\"><v>0</v></c>\
             <c r=\"C1\" t=\"e\"><v>#DIV/0!</v></c><c r=\"D1\" t=\"d\"><v>2024-05-01T10:30:00</v></c>\
             <c r=\"E1\" t=\"n\"><v>7</v></c></row>",
        );
        assert_eq!(read1(fx), vec![row(&["TRUE", "FALSE", "#DIV/0!", "2024-05-01 10:30:00", "7"])]);
    }

    // ------------------------------------------------------------------ reading: placement

    #[test]
    fn cells_keep_their_position_and_trailing_empties_are_trimmed() {
        let fx = Fx::new(
            "<row r=\"2\"><c r=\"B2\" t=\"inlineStr\"><is><t>b2</t></is></c><c r=\"D2\"><v>4</v></c></row>\
             <row r=\"4\"><c r=\"A4\" t=\"str\"><v>a4</v></c><c r=\"C4\"/></row>\
             <row r=\"9\"><c r=\"F9\"><v></v></c></row>",
        );
        assert_eq!(read1(fx), vec![row(&[]), row(&["", "b2", "", "4"]), row(&[]), row(&["a4"])]);
    }

    #[test]
    fn cells_without_references_follow_the_previous_one() {
        let fx = Fx::new(
            "<row><c t=\"inlineStr\"><is><t>x</t></is></c><c><v>2</v></c></row><row><c><v>3</v></c><c r=\"D2\"><v>4</v></c><c><v>5</v></c></row>",
        );
        assert_eq!(read1(fx), vec![row(&["x", "2"]), row(&["3", "", "", "4", "5"])]);
    }

    #[test]
    fn prefixed_element_names_are_understood() {
        let mut w = ZipWriter::new();
        w.add(
            "xl/workbook.xml",
            format!("<x:workbook xmlns:x=\"{MAIN}\" xmlns:r=\"{REL}\"><x:sheets><x:sheet name=\"P\" sheetId=\"1\" r:id=\"rId1\"/></x:sheets></x:workbook>").as_bytes(),
            true,
        );
        w.add(
            "xl/_rels/workbook.xml.rels",
            format!("<Relationships><Relationship Id=\"rId1\" Type=\"{REL}/worksheet\" Target=\"worksheets/a.xml\"/></Relationships>").as_bytes(),
            true,
        );
        w.add(
            "xl/worksheets/a.xml",
            format!("<x:worksheet xmlns:x=\"{MAIN}\"><x:sheetData><x:row r=\"1\"><x:c r=\"A1\"><x:v>5</x:v></x:c></x:row></x:sheetData></x:worksheet>").as_bytes(),
            true,
        );
        let sheets = read_xlsx(&w.finish()).unwrap();
        assert_eq!((sheets[0].name.as_str(), sheets[0].rows.clone()), ("P", vec![row(&["5"])]));
    }

    #[test]
    fn hidden_sheets_are_skipped_and_order_and_names_are_kept() {
        let fx = Fx::new("<row r=\"1\"><c r=\"A1\"><v>1</v></c></row>")
            .sheet("Secret", "hidden", "<row r=\"1\"><c r=\"A1\"><v>2</v></c></row>")
            .sheet("Very", "veryHidden", "<row r=\"1\"><c r=\"A1\"><v>3</v></c></row>")
            .sheet("R&amp;D &lt;3", "visible", "<row r=\"1\"><c r=\"A1\"><v>4</v></c></row>");
        let sheets = read_xlsx(&fx.build()).unwrap();
        let got: Vec<_> = sheets.iter().map(|s| (s.name.as_str(), s.rows[0][0].as_str())).collect();
        assert_eq!(got, [("S", "1"), ("R&D <3", "4")]);
    }

    #[test]
    fn relationship_targets_may_be_absolute_or_climb_directories() {
        let mut w = ZipWriter::new();
        w.add(
            "xl/workbook.xml",
            format!("<workbook xmlns=\"{MAIN}\" xmlns:r=\"{REL}\"><sheets><sheet name=\"A\" sheetId=\"1\" r:id=\"r1\"/><sheet name=\"B\" sheetId=\"2\" r:id=\"r2\"/></sheets></workbook>").as_bytes(),
            true,
        );
        w.add(
            "xl/_rels/workbook.xml.rels",
            format!("<Relationships><Relationship Id=\"r1\" Type=\"{REL}/worksheet\" Target=\"/xl/data/one.xml\"/><Relationship Id=\"r2\" Type=\"{REL}/worksheet\" Target=\"sub/../two.xml\"/></Relationships>").as_bytes(),
            true,
        );
        for (n, v) in [("xl/data/one.xml", "1"), ("xl/two.xml", "2")] {
            w.add(
                n,
                format!("<worksheet xmlns=\"{MAIN}\"><sheetData><row r=\"1\"><c r=\"A1\"><v>{v}</v></c></row></sheetData></worksheet>").as_bytes(),
                true,
            );
        }
        let sheets = read_xlsx(&w.finish()).unwrap();
        assert_eq!(sheets[0].rows, vec![row(&["1"])]);
        assert_eq!(sheets[1].rows, vec![row(&["2"])]);
    }

    // ------------------------------------------------------------------ reading: numbers and dates

    #[test]
    fn numbers_come_out_the_way_a_user_sees_them() {
        let cases = [
            ("0.1000000000000000055", "0.1"),
            ("3.0", "3"),
            ("3", "3"),
            ("0.30000000000000004", "0.3"),
            ("-0", "0"),
            ("-12.50", "-12.5"),
            ("123456789012", "123456789012"),
            ("1E+20", "100000000000000000000"),
            ("1E+25", "1E+25"),
            ("1.5E-10", "1.5E-10"),
            ("0.000001", "0.000001"),
            ("1234.5678", "1234.5678"),
            ("not a number", "not a number"),
        ];
        for (raw, want) in cases {
            let fx = Fx::new(&format!("<row r=\"1\"><c r=\"A1\"><v>{raw}</v></c></row>"));
            assert_eq!(read1(fx), vec![row(&[want])], "{raw}");
        }
    }

    fn styled(xf_ids: &[u32], fmts: &str, cells: &[(usize, &str)], d1904: bool) -> Vec<String> {
        let mut data = String::from("<row r=\"1\">");
        for (i, (s, v)) in cells.iter().enumerate() {
            data.push_str(&format!("<c r=\"{}1\" s=\"{s}\"><v>{v}</v></c>", (b'A' + i as u8) as char));
        }
        data.push_str("</row>");
        let mut fx = Fx::new(&data).styles(fmts, xf_ids);
        if d1904 {
            fx = fx.d1904();
        }
        read1(fx).remove(0)
    }

    #[test]
    fn dates_in_the_1900_system_including_the_fake_leap_day() {
        let got = styled(
            &[0, 14],
            "",
            &[(1, "45000"), (1, "59"), (1, "60"), (1, "61"), (1, "1"), (1, "0"), (1, "2958465"), (0, "45000")],
            false,
        );
        assert_eq!(
            got,
            row(&[
                "2023-03-15",
                "1900-02-28",
                "1900-02-29",
                "1900-03-01",
                "1900-01-01",
                "1900-01-00",
                "9999-12-31",
                "45000"
            ])
        );
    }

    #[test]
    fn dates_in_the_1904_system() {
        let got = styled(&[0, 14], "", &[(1, "0"), (1, "1461"), (1, "43538")], true);
        assert_eq!(got, row(&["1904-01-01", "1908-01-01", "2023-03-15"]));
    }

    #[test]
    fn built_in_date_time_and_datetime_formats() {
        let got = styled(
            &[0, 22, 21, 46, 18, 15, 45],
            "",
            &[(1, "45000.75"), (2, "0.5"), (3, "1.5"), (4, "0.25"), (5, "45000.75"), (6, "0.000694444444444444")],
            false,
        );
        assert_eq!(got, row(&["2023-03-15 18:00:00", "12:00:00", "36:00:00", "06:00:00", "2023-03-15", "00:01:00"]));
    }

    #[test]
    fn midnight_rounding_carries_into_the_next_day() {
        let got = styled(&[0, 22], "", &[(1, "44999.999999999")], false);
        assert_eq!(got, row(&["2023-03-15 00:00:00"]));
    }

    #[test]
    fn custom_formats_are_classified_by_their_unquoted_letters() {
        let fmts = "<numFmt numFmtId=\"164\" formatCode=\"0.00\"/>\
                    <numFmt numFmtId=\"165\" formatCode=\"yyyy\\-mm\\-dd\"/>\
                    <numFmt numFmtId=\"166\" formatCode=\"0 &quot;days&quot;\"/>\
                    <numFmt numFmtId=\"167\" formatCode=\"[Red]#,##0.00\"/>\
                    <numFmt numFmtId=\"168\" formatCode=\"[$-409]d\\-mmm\\-yy\"/>\
                    <numFmt numFmtId=\"169\" formatCode=\"h:mm:ss\\ AM/PM\"/>\
                    <numFmt numFmtId=\"170\" formatCode=\"0.0%\"/>\
                    <numFmt numFmtId=\"171\" formatCode=\"&quot;Total: &quot;0\"/>\
                    <numFmt numFmtId=\"172\" formatCode=\"mm:ss\"/>\
                    <numFmt numFmtId=\"173\" formatCode=\"yyyy\\-mm\\-dd\\ hh:mm\"/>\
                    <numFmt numFmtId=\"174\" formatCode=\"[h]:mm\"/>\
                    <numFmt numFmtId=\"175\" formatCode=\"General\"/>";
        let ids = [0, 164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 174, 175];
        let cells = [
            (1, "1234.5"),
            (2, "45000"),
            (3, "7"),
            (4, "1234.5"),
            (5, "45000"),
            (6, "1.5"),
            (7, "0.256"),
            (8, "5"),
            (9, "0.5"),
            (10, "45000.5"),
            (11, "2.5"),
            (12, "9.25"),
        ];
        let got = styled(&ids, fmts, &cells, false);
        assert_eq!(
            got,
            row(&[
                "1234.5",
                "2023-03-15",
                "7",
                "1234.5",
                "2023-03-15",
                "12:00:00",
                "25.6%",
                "5",
                "12:00:00",
                "2023-03-15 12:00:00",
                "60:00:00",
                "9.25"
            ])
        );
    }

    #[test]
    fn percent_formats_use_the_decimals_of_the_format() {
        let fmts = "<numFmt numFmtId=\"164\" formatCode=\"0.000%\"/>";
        let got = styled(
            &[0, 9, 10, 164],
            fmts,
            &[(1, "0.5"), (2, "0.0525"), (3, "0.07"), (1, "-0.0000001"), (1, "1")],
            false,
        );
        assert_eq!(got, row(&["50%", "5.25%", "7.000%", "0%", "100%"]));
    }

    #[test]
    fn xf_entries_outside_cell_xfs_do_not_shift_style_indexes() {
        // The fixture has a decoy <cellStyleXfs><xf numFmtId="14"/>; style 0 must stay General.
        let got = styled(&[0, 14], "", &[(0, "45000"), (1, "45000")], false);
        assert_eq!(got, row(&["45000", "2023-03-15"]));
    }

    // ------------------------------------------------------------------ reading: robustness and limits

    #[test]
    fn old_xls_and_encrypted_workbooks_get_a_clear_message() {
        let mut ole = vec![0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1];
        ole.extend_from_slice(&[0u8; 600]);
        match read_xlsx(&ole) {
            Err(Error::Unsupported(m)) => {
                assert_eq!(m, "Old .xls or password-protected workbook: save it as .xlsx first")
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn not_a_workbook_is_a_typed_error() {
        assert!(matches!(read_xlsx(b""), Err(Error::Corrupt(_))));
        assert!(matches!(read_xlsx(b"hello,world\n1,2\n"), Err(Error::Corrupt(_))));
        let mut w = ZipWriter::new();
        w.add("readme.txt", b"just a zip", true);
        assert!(matches!(read_xlsx(&w.finish()), Err(Error::Corrupt(_))));
    }

    #[test]
    fn damaged_input_never_panics() {
        let fx = Fx::new(
            "<row r=\"1\"><c r=\"A1\" t=\"s\"><v>0</v></c><c r=\"B1\" s=\"1\"><v>45000</v></c></row><row r=\"2\"><c r=\"A2\" t=\"inlineStr\"><is><t>x &amp; y</t></is></c></row>",
        )
        .shared("<si><t>a&#x1F600;é</t></si>")
        .styles("", &[0, 14]);
        let good = fx.build();
        assert_eq!(read_xlsx(&good).unwrap()[0].rows[0], row(&["a\u{1F600}é", "2023-03-15"]));
        // Every truncation of the zip is an error or a result, never a panic.
        for n in 0..good.len() {
            let _ = read_xlsx(&good[..n]);
        }
        // Every truncation of every XML part.
        let xmls = [
            fx.shared.clone().unwrap(),
            fx.styles.clone().unwrap(),
            format!("<worksheet xmlns=\"{MAIN}\"><sheetData>{}</sheetData></worksheet>", fx.sheets[0].2),
        ];
        for (which, xml) in xmls.iter().enumerate() {
            let cuts = xml.char_indices().map(|(i, _)| i).chain(std::iter::once(xml.len()));
            for cut in cuts {
                let mut variant = Fx::new(&fx.sheets[0].2).styles("", &[0, 14]).shared("<si><t>q</t></si>");
                let part = &xml[..cut];
                match which {
                    0 => variant.shared = Some(part.to_string()),
                    1 => variant.styles = Some(part.to_string()),
                    _ => variant.sheets[0].2 = part.to_string(),
                }
                let _ = read_xlsx(&variant.build());
            }
        }
        for junk in [
            "<<<<",
            "<c r=\"",
            "<c r=\"A1\" t=\"s\"><v>99999999999999999999</v></c>",
            "&&&;;",
            "<![CDATA[",
            "<!--",
            "<?xml",
        ] {
            let _ = read_xlsx(&Fx::new(junk).shared(junk).styles(junk, &[0]).build());
        }
    }

    #[test]
    fn missing_workbook_part_and_missing_sheet_part() {
        let mut w = ZipWriter::new();
        w.add("[Content_Types].xml", b"<Types/>", true);
        assert!(matches!(read_xlsx(&w.finish()), Err(Error::Corrupt(_))));
        let mut w = ZipWriter::new();
        w.add(
            "xl/workbook.xml",
            format!("<workbook xmlns=\"{MAIN}\"><sheets><sheet name=\"Gone\" sheetId=\"1\"/></sheets></workbook>")
                .as_bytes(),
            true,
        );
        let s = read_xlsx(&w.finish()).unwrap();
        assert_eq!((s[0].name.as_str(), s[0].rows.len()), ("Gone", 0));
    }

    #[test]
    fn a_far_corner_cell_is_read_without_blowing_up_and_a_smaller_budget_refuses_it() {
        let bytes = Fx::new("<row r=\"1048576\"><c r=\"XFD1048576\"><v>1</v></c></row>").build();
        let rows = read_xlsx(&bytes).unwrap().remove(0).rows;
        assert_eq!((rows.len(), rows[1_048_575].len(), rows[1_048_575][16_383].as_str()), (1_048_576, 16_384, "1"));
        match read_with(&bytes, MAX_PART, 1_000_000) {
            Err(Error::Unsupported(m)) => assert!(m.contains("1,000,000 cells"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn the_default_budget_is_five_million_cells() {
        let e = unsupported_cells(MAX_CELLS);
        assert!(matches!(e, Error::Unsupported(m) if m == "Workbook too large: more than 5,000,000 cells"));
    }

    #[test]
    fn cell_budget_counts_all_sheets_and_small_limits_apply() {
        let data = "<row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"B1\"><v>2</v></c><c r=\"C1\"><v>3</v></c></row>";
        let bytes = Fx::new(data).sheet("T", "", data).build();
        let ok = read_with(&bytes, 1 << 20, 10).unwrap();
        assert_eq!(ok.len(), 2);
        match read_with(&bytes, 1 << 20, 6) {
            Err(Error::Unsupported(m)) => assert!(m.contains("cells"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn oversized_parts_are_refused_with_a_clear_message() {
        let bytes = Fx::new("<row r=\"1\"><c r=\"A1\"><v>1</v></c></row>").shared("<si><t>x</t></si>").build();
        match read_with(&bytes, 100, 1000) {
            Err(Error::Unsupported(m)) => assert!(m.contains("too large"), "{m}"),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn out_of_range_cells_are_ignored() {
        let fx = Fx::new(
            "<row r=\"1\"><c r=\"A1\"><v>1</v></c><c r=\"XFE1\"><v>2</v></c></row><row r=\"1048577\"><c r=\"A1048577\"><v>3</v></c></row>",
        );
        assert_eq!(read1(fx), vec![row(&["1"])]);
    }

    // ------------------------------------------------------------------ CSV

    #[test]
    fn csv_parses_quotes_embedded_newlines_and_line_endings() {
        let t = "\u{FEFF}name,note\r\n\"he said \"\"hi\"\"\",x\n\"line1\nline2\",\"a,b\"\r\nlast,row";
        assert_eq!(
            parse_csv(t),
            grid(&[&["name", "note"], &["he said \"hi\"", "x"], &["line1\nline2", "a,b"], &["last", "row"]])
        );
        assert_eq!(parse_csv("a,b\rc,d\r"), grid(&[&["a", "b"], &["c", "d"]]));
        assert_eq!(parse_csv("a,b\nc,d\n"), grid(&[&["a", "b"], &["c", "d"]]));
    }

    #[test]
    fn csv_edge_cases_keep_ragged_rows_and_do_not_invent_rows() {
        assert!(parse_csv("").is_empty());
        assert!(parse_csv("\u{FEFF}").is_empty());
        assert_eq!(parse_csv("a,b\n"), grid(&[&["a", "b"]]));
        assert_eq!(parse_csv("a,b\n\n"), grid(&[&["a", "b"], &[""]]));
        assert_eq!(parse_csv("a,b,\n"), grid(&[&["a", "b", ""]]));
        assert_eq!(parse_csv("a,b,c\n1,2\n1,2,3,4\n"), grid(&[&["a", "b", "c"], &["1", "2"], &["1", "2", "3", "4"]]));
        assert_eq!(parse_csv("a,\"b"), grid(&[&["a", "b"]]));
        assert_eq!(parse_csv("x\"y,z"), grid(&[&["x\"y", "z"]]));
        assert_eq!(parse_csv("\"a\"b,c"), grid(&[&["ab", "c"]]));
        assert_eq!(parse_csv("ñandú,日本\n"), grid(&[&["ñandú", "日本"]]));
    }

    #[test]
    fn csv_delimiter_is_sniffed_from_the_first_records() {
        assert_eq!(parse_csv("a;b;c\n1;2;3\n"), grid(&[&["a", "b", "c"], &["1", "2", "3"]]));
        assert_eq!(parse_csv("a\tb\n1\t2\n"), grid(&[&["a", "b"], &["1", "2"]]));
        assert_eq!(parse_csv("a|b|c\n1|2|3"), grid(&[&["a", "b", "c"], &["1", "2", "3"]]));
        // Tie between , and ; goes to the comma.
        assert_eq!(parse_csv("a,b;c\n1,2;3\n"), grid(&[&["a", "b;c"], &["1", "2;3"]]));
        // European decimals: ; is the consistent one.
        assert_eq!(parse_csv("name;price\nx;1,5\ny;2,5\n"), grid(&[&["name", "price"], &["x", "1,5"], &["y", "2,5"]]));
        // Delimiters inside quotes do not count.
        assert_eq!(parse_csv("\"a,b\";c\n\"d,e\";f\n"), grid(&[&["a,b", "c"], &["d,e", "f"]]));
        // One column stays one column.
        assert_eq!(parse_csv("one\ntwo\n"), grid(&[&["one"], &["two"]]));
        // Only the first 20 records decide.
        let mut t = String::from("a;b\n").repeat(20);
        t.push_str("x,y,z,w\n");
        assert_eq!(parse_csv(&t)[20], row(&["x,y,z,w"]));
    }

    #[test]
    fn csv_encoding_has_bom_crlf_and_quotes_only_when_needed() {
        let rows = grid(&[&["a,b", "c\"d", "e\nf", "plain", "", "x;y", "g\rh"], &["1", "2"]]);
        let got = csv_bytes(&rows);
        let want = "\u{FEFF}\"a,b\",\"c\"\"d\",\"e\nf\",plain,,x;y,\"g\rh\"\r\n1,2\r\n";
        assert_eq!(got, want.as_bytes());
        assert_eq!(csv_bytes(&[]), b"\xEF\xBB\xBF");
        assert_eq!(csv_bytes(&grid(&[&["ñ", "日本"]])), "\u{FEFF}ñ,日本\r\n".as_bytes());
    }

    #[test]
    fn csv_round_trips() {
        let cases = [
            grid(&[&["a", "b", "c"], &["1", "2", "3"]]),
            grid(&[&["he said \"hi\"", "x,y", "line1\nline2"], &["", "", ""], &["ñ", "日本", "\u{1F600}"]]),
            grid(&[&["a", "b"], &["only"], &["1", "2", "3"]]),
            grid(&[&["lead ", " space", "mid\ttab"], &["a;b", "c|d", "e"]]),
            grid(&[&["x"], &[""], &["y"]]),
        ];
        for rows in cases {
            let bytes = csv_bytes(&rows);
            assert_eq!(parse_csv(std::str::from_utf8(&bytes).unwrap()), rows);
        }
    }

    #[test]
    fn text_is_tab_separated_with_line_feeds() {
        let rows = grid(&[&["a", "b\tc"], &["x\r\ny", "z\n\nw"], &["", ""]]);
        assert_eq!(rows_to_text(&rows), "a\tb c\nx y\tz w\n\t\n");
        assert_eq!(rows_to_text(&[]), "");
    }

    // ------------------------------------------------------------------ writing

    fn parts(bytes: &[u8]) -> HashMap<String, String> {
        let z = ZipArchive::new(bytes).unwrap();
        let names: Vec<String> = z.names().map(str::to_string).collect();
        names
            .into_iter()
            .map(|n| {
                let d = z.read(&n, 1 << 28).unwrap().unwrap();
                (n, String::from_utf8(d).unwrap())
            })
            .collect()
    }

    /// Checks that the XML is balanced and every tag closes in order.
    fn well_formed(xml: &str) -> bool {
        let mut stack = Vec::new();
        for t in Lexer::new(xml) {
            match t {
                Tok::Open { name, empty, .. } => {
                    if !empty {
                        stack.push(name.to_string());
                    }
                }
                Tok::Close(n) if stack.pop().as_deref() != Some(n) => return false,
                _ => {}
            }
        }
        stack.is_empty()
    }

    #[test]
    fn written_package_has_every_part_well_formed_and_every_relationship_target_present() {
        let bytes = write_xlsx(&grid(&[&["Name", "Qty"], &["a & b", "5"]]), "Data");
        let p = parts(&bytes);
        let mut names: Vec<_> = p.keys().cloned().collect();
        names.sort();
        assert_eq!(
            names,
            [
                "[Content_Types].xml",
                "_rels/.rels",
                "docProps/app.xml",
                "docProps/core.xml",
                "xl/_rels/workbook.xml.rels",
                "xl/sharedStrings.xml",
                "xl/styles.xml",
                "xl/workbook.xml",
                "xl/worksheets/sheet1.xml"
            ]
        );
        for (n, x) in &p {
            assert!(x.starts_with("<?xml version=\"1.0\" encoding=\"UTF-8\" standalone=\"yes\"?>"), "{n}");
            assert!(well_formed(x), "{n} is not well formed");
        }
        // Targets of both relationship files exist.
        for (rels, base) in [("_rels/.rels", ""), ("xl/_rels/workbook.xml.rels", "xl/")] {
            let mut seen = 0;
            for t in Lexer::new(&p[rels]) {
                if let Tok::Open { name: "Relationship", attrs, .. } = t {
                    let target = attr(attrs, "Target").unwrap();
                    let full = resolve(base, &target);
                    assert!(p.contains_key(&full), "{rels}: missing {full}");
                    seen += 1;
                }
            }
            assert!(seen >= 2, "{rels}");
        }
        // Content types declare every part.
        for n in p.keys().filter(|n| *n != "[Content_Types].xml" && !n.ends_with(".rels")) {
            assert!(p["[Content_Types].xml"].contains(&format!("PartName=\"/{n}\"")), "{n} undeclared");
        }
        assert!(p["xl/workbook.xml"].contains("<sheet name=\"Data\" sheetId=\"1\" r:id=\"rId1\"/>"));
    }

    #[test]
    fn written_package_carries_no_personal_data() {
        let p = parts(&write_xlsx(&grid(&[&["a"]]), "S"));
        assert!(p["docProps/core.xml"].contains("<dc:creator>Radial</dc:creator>"));
        assert!(p["docProps/app.xml"].contains("<Application>Radial</Application>"));
        for x in p.values() {
            let user = std::env::var("USERNAME").unwrap_or_default();
            assert!((user.is_empty() || !x.contains(&user)) && !x.contains('@') && !x.contains("Users"), "{x}");
        }
        assert_eq!(write_xlsx(&grid(&[&["a"]]), "S"), write_xlsx(&grid(&[&["a"]]), "S"), "deterministic output");
    }

    #[test]
    fn cell_typing_keeps_ids_phone_numbers_and_booleans_as_text() {
        let texts = [
            "007",
            "+34600111222",
            " 5",
            "5 ",
            "TRUE",
            "FALSE",
            "12345678901234567",
            "1e5",
            ".5",
            "5.",
            "-0",
            "1,5",
            "0x10",
            "00.5",
            "1_000",
            "--1",
            "2024-13-01",
            "2024-02-30",
            "1899-12-31",
            "1900-02-28",
            "2024-5-1",
            "2024-05-01 24:00:00",
        ];
        let numbers = ["0", "5", "-5", "0.5", "-0.25", "123456789012345", "0.12345678901234", "1.50", "100", "9999999"];
        let mut rows = vec![row(&["header"])];
        rows.extend(texts.iter().chain(numbers.iter()).map(|s| row(&[s])));
        let p = parts(&write_xlsx(&rows, "S"));
        let sheet = &p["xl/worksheets/sheet1.xml"];
        let shared = &p["xl/sharedStrings.xml"];
        for t in texts {
            assert!(shared.contains(&format!(">{t}</t>")), "{t} must be text");
        }
        for (i, n) in numbers.iter().enumerate() {
            let r = 2 + texts.len() + i;
            assert!(sheet.contains(&format!("<c r=\"A{r}\"><v>{n}</v></c>")), "{n} must be numeric");
        }
        let back = read_xlsx(&write_xlsx(&rows, "S")).unwrap().remove(0).rows;
        // "1.50" is a number: it comes back as the number Excel shows.
        let mut want = rows.clone();
        want[1 + texts.len() + 7] = row(&["1.5"]);
        assert_eq!(back, want);
    }

    #[test]
    fn dates_and_datetimes_become_date_serials_with_a_date_style() {
        let rows = grid(&[
            &["when", "what"],
            &["2024-05-01", "x"],
            &["2023-03-15 18:00:00", "y"],
            &["1900-03-01", "z"],
            &["9999-12-31", "w"],
        ]);
        let bytes = write_xlsx(&rows, "S");
        let p = parts(&bytes);
        let sheet = &p["xl/worksheets/sheet1.xml"];
        assert!(sheet.contains("<c r=\"A2\" s=\"2\"><v>45413</v></c>"), "{sheet}");
        assert!(sheet.contains("<c r=\"A3\" s=\"3\"><v>45000.75</v></c>"), "{sheet}");
        assert!(sheet.contains("<c r=\"A4\" s=\"2\"><v>61</v></c>"), "{sheet}");
        assert!(sheet.contains("<c r=\"A5\" s=\"2\"><v>2958465</v></c>"), "{sheet}");
        assert_eq!(read_xlsx(&bytes).unwrap()[0].rows, rows);
    }

    #[test]
    fn header_is_bold_and_frozen_only_with_more_than_one_row() {
        let two = parts(&write_xlsx(&grid(&[&["h1", "h2"], &["1", "x"]]), "S"));
        let s = &two["xl/worksheets/sheet1.xml"];
        assert!(s.contains("<c r=\"A1\" s=\"1\" t=\"s\">") && s.contains("<c r=\"B1\" s=\"1\" t=\"s\">"), "{s}");
        assert!(
            s.contains("<pane ySplit=\"1\" topLeftCell=\"A2\" activePane=\"bottomLeft\" state=\"frozen\"/>"),
            "{s}"
        );
        assert!(two["xl/styles.xml"].contains("<b/>"));
        let one = parts(&write_xlsx(&grid(&[&["h1", "h2"]]), "S"));
        let s = &one["xl/worksheets/sheet1.xml"];
        assert!(!s.contains("<pane") && !s.contains("s=\"1\""), "{s}");
        // Numbers in the header stay text so the bold style is not misread as a number format.
        let hdr = write_xlsx(&grid(&[&["2024", "2024-05-01"], &["1", "2"]]), "S");
        assert_eq!(read_xlsx(&hdr).unwrap()[0].rows[0], row(&["2024", "2024-05-01"]));
    }

    #[test]
    fn column_widths_fit_content_between_8_and_60() {
        let long = "x".repeat(200);
        let p = parts(&write_xlsx(&grid(&[&["a", "medium text here", &long], &["b", "c", "d"]]), "S"));
        let s = &p["xl/worksheets/sheet1.xml"];
        assert!(s.contains("<col min=\"1\" max=\"1\" width=\"8\" customWidth=\"1\"/>"), "{s}");
        assert!(s.contains("<col min=\"2\" max=\"2\" width=\"18\" customWidth=\"1\"/>"), "{s}");
        assert!(s.contains("<col min=\"3\" max=\"3\" width=\"60\" customWidth=\"1\"/>"), "{s}");
    }

    #[test]
    fn sheet_names_are_sanitized() {
        let name_of = |n: &str| read_xlsx(&write_xlsx(&grid(&[&["a"]]), n)).unwrap().remove(0).name;
        assert_eq!(name_of("Ventas"), "Ventas");
        assert_eq!(name_of(""), "Sheet1");
        assert_eq!(name_of("   "), "Sheet1");
        assert_eq!(name_of("a[b]c:d*e?f/g\\h"), "a_b_c_d_e_f_g_h");
        assert_eq!(name_of(&"n".repeat(50)).chars().count(), 31);
        assert_eq!(name_of("'quoted'"), "quoted");
        assert_eq!(name_of("R&D \"x\" <y>"), "R&D \"x\" <y>");
        assert_eq!(name_of("ñandú\u{1F600}").chars().count(), 6);
    }

    #[test]
    fn text_is_escaped_and_illegal_characters_are_stripped() {
        let rows = grid(&[
            &["h"],
            &["a & <b> \"q\" 'a'"],
            &["tab\there"],
            &["line1\nline2"],
            &["cr\r\nlf"],
            &[" lead"],
            &["trail "],
            &["bell\u{7}gone\u{0}nul\u{1F}x"],
            &["\u{1F600} ñ 日本"],
            &["\u{FFFE}\u{FFFF}z"],
        ]);
        let bytes = write_xlsx(&rows, "S");
        for x in parts(&bytes).values() {
            assert!(well_formed(x));
            assert!(!x.chars().any(|c| (c as u32) < 0x20 && !matches!(c, '\t' | '\n')), "raw control char in XML");
        }
        let back = read_xlsx(&bytes).unwrap().remove(0).rows;
        assert_eq!(back[1..4], rows[1..4]);
        assert_eq!(back[4], row(&["cr\r\nlf"]));
        assert_eq!(back[5], row(&[" lead"]));
        assert_eq!(back[6], row(&["trail "]));
        assert_eq!(back[7], row(&["bellgonenulx"]));
        assert_eq!(back[8], row(&["\u{1F600} ñ 日本"]));
        assert_eq!(back[9], row(&["z"]));
    }

    #[test]
    fn round_trip_keeps_text_numbers_dates_and_empty_cells() {
        let rows = grid(&[
            &["Name", "Qty", "Price", "Date", "Note"],
            &["Ana", "3", "1.5", "2024-02-29", "007"],
            &["", "", "-2.25", "", "multi\nline"],
            &["Bob", "1000000", "0.001", "2023-03-15 06:30:15", "+34 600"],
            &[],
            &["gap above", "", "", "", "x"],
        ]);
        let back = read_xlsx(&write_xlsx(&rows, "S")).unwrap().remove(0).rows;
        assert_eq!(back, rows);
    }

    #[test]
    fn excel_limits_drop_extra_columns_and_rows() {
        let wide: Vec<String> = (0..16_390).map(|i| i.to_string()).collect();
        let back = read_xlsx(&write_xlsx(&[wide], "S")).unwrap().remove(0).rows;
        assert_eq!(back[0].len(), 16_384);
        assert_eq!(back[0][16_383], "16383");
        let mut tall = vec![Vec::new(); 1_048_577];
        tall[1_048_575] = row(&["last"]);
        tall[1_048_576] = row(&["dropped"]);
        let p = parts(&write_xlsx(&tall, "S"));
        let s = &p["xl/worksheets/sheet1.xml"];
        assert!(s.contains("r=\"A1048576\"") && !s.contains("1048577"), "row limit");
        assert!(!p["xl/sharedStrings.xml"].contains("dropped"));
    }

    #[test]
    fn empty_input_gives_a_valid_empty_workbook() {
        let bytes = write_xlsx(&[], "Empty");
        for x in parts(&bytes).values() {
            assert!(well_formed(x));
        }
        let s = read_xlsx(&bytes).unwrap();
        assert_eq!((s[0].name.as_str(), s[0].rows.len()), ("Empty", 0));
    }

    #[test]
    fn long_cell_text_is_cut_at_the_excel_limit() {
        let rows = grid(&[&["h"], &[&"é".repeat(40_000)]]);
        let back = read_xlsx(&write_xlsx(&rows, "S")).unwrap().remove(0).rows;
        assert_eq!(back[1][0].chars().count(), 32_767);
    }

    #[test]
    fn shared_strings_are_deduplicated() {
        let p = parts(&write_xlsx(&grid(&[&["h"], &["same"], &["same"], &["same"]]), "S"));
        assert!(p["xl/sharedStrings.xml"].contains("count=\"4\" uniqueCount=\"2\""), "{}", p["xl/sharedStrings.xml"]);
    }

    // ------------------------------------------------------------------ performance (manual)
    //
    // cargo test --release perf_ -- --ignored --nocapture --test-threads=1
    // with RADIAL_PERF_XLSX pointing at a scratch file; the three steps are separate tests so each can be
    // measured as its own process.

    fn perf_path() -> Option<String> {
        std::env::var("RADIAL_PERF_XLSX").ok()
    }

    #[test]
    #[ignore]
    fn perf_1_generate_200k_by_10() {
        let Some(path) = perf_path() else { return };
        let mut rows = vec![row(&["Id", "Name", "Price", "Date", "Active", "Note", "Qty", "Ratio", "Code", "City"])];
        for i in 0..200_000usize {
            rows.push(vec![
                i.to_string(),
                format!("Customer {}", i % 997),
                format!("{}.{:02}", i % 5000, i % 100),
                format!("20{:02}-{:02}-{:02}", 10 + i % 15, 1 + i % 12, 1 + i % 28),
                if i % 3 == 0 { "TRUE".into() } else { "FALSE".into() },
                format!("row {i} note, with \"quotes\""),
                (i * 7 % 1000).to_string(),
                format!("0.{}", 1 + i % 99),
                format!("{:05}", i % 100_000),
                ["Madrid", "Lima", "Quito", "Bogotá", "Santiago"][i % 5].to_string(),
            ]);
        }
        let t = std::time::Instant::now();
        let bytes = write_xlsx(&rows, "Perf");
        println!("write_xlsx 200000x10: {:?}, {} bytes", t.elapsed(), bytes.len());
        std::fs::write(path, bytes).unwrap();
    }

    /// Writes a small tricky workbook for opening in other programs (Excel, LibreOffice, openpyxl).
    #[test]
    #[ignore]
    fn perf_0_sample_for_other_programs() {
        let Some(path) = perf_path() else { return };
        let rows = grid(&[
            &["Name", "Qty", "Price", "Date", "Stamp", "Phone", "Note"],
            &["Ana & <Co>", "3", "1.5", "2024-02-29", "2023-03-15 18:00:00", "+34 600 111", "multi\nline \"quoted\""],
            &["Bób 日本", "1000000", "-2.25", "1900-03-01", "2024-12-31 23:59:59", "007", "TRUE"],
            &["", "12", "0.001", "", "", "1e5", "  padded  "],
        ]);
        std::fs::write(path, write_xlsx(&rows, "Ventas: 2024")).unwrap();
    }

    /// Prints every visible sheet of the workbook in RADIAL_PERF_XLSX (a file made by another program).
    #[test]
    #[ignore]
    fn perf_4_dump_any_workbook() {
        let Some(path) = perf_path() else { return };
        for s in read_xlsx(&std::fs::read(path).unwrap()).unwrap() {
            println!("sheet {:?}", s.name);
            for r in s.rows {
                println!("  {r:?}");
            }
        }
    }

    #[test]
    #[ignore]
    fn perf_2_read_xlsx() {
        let Some(path) = perf_path() else { return };
        let bytes = std::fs::read(path).unwrap();
        let t = std::time::Instant::now();
        let sheets = read_xlsx(&bytes).unwrap();
        println!("read_xlsx: {:?} ({} rows)", t.elapsed(), sheets[0].rows.len());
        assert_eq!(sheets[0].rows.len(), 200_001);
    }

    #[test]
    #[ignore]
    fn perf_3_csv_bytes() {
        let Some(path) = perf_path() else { return };
        let rows = read_xlsx(&std::fs::read(path).unwrap()).unwrap().remove(0).rows;
        let t = std::time::Instant::now();
        let csv = csv_bytes(&rows);
        println!("csv_bytes: {:?} ({} bytes)", t.elapsed(), csv.len());
        let t = std::time::Instant::now();
        let back = parse_csv(std::str::from_utf8(&csv).unwrap());
        println!("parse_csv: {:?} ({} rows)", t.elapsed(), back.len());
        assert_eq!(back.len(), rows.len());
    }

    #[test]
    fn one_long_string_shared_by_many_cells_cannot_exhaust_memory() {
        // 32,767 characters referenced by 8,200 cells would be 268 MB of text from a few KB of file.
        let ns = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
        let rel = "http://schemas.openxmlformats.org/officeDocument/2006/relationships";
        let mut zip = ZipWriter::new();
        zip.add(
            "xl/workbook.xml",
            format!("<workbook xmlns=\"{ns}\" xmlns:r=\"{rel}\"><sheets><sheet name=\"S\" sheetId=\"1\" r:id=\"rId1\"/></sheets></workbook>").as_bytes(),
            true,
        );
        zip.add(
            "xl/_rels/workbook.xml.rels",
            format!("<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\"><Relationship Id=\"rId1\" Type=\"{rel}/worksheet\" Target=\"worksheets/sheet1.xml\"/></Relationships>").as_bytes(),
            true,
        );
        zip.add(
            "xl/sharedStrings.xml",
            format!("<sst xmlns=\"{ns}\"><si><t>{}</t></si></sst>", "x".repeat(32_767)).as_bytes(),
            true,
        );
        let mut sheet = format!("<worksheet xmlns=\"{ns}\"><sheetData>");
        for r in 1..=8_200 {
            sheet += &format!("<row r=\"{r}\"><c r=\"A{r}\" t=\"s\"><v>0</v></c></row>");
        }
        sheet += "</sheetData></worksheet>";
        zip.add("xl/worksheets/sheet1.xml", sheet.as_bytes(), true);
        match read_xlsx(&zip.finish()) {
            Err(Error::Unsupported(m)) => assert!(m.contains("too much text"), "{m}"),
            other => panic!("expected the text budget to refuse it, got {:?}", other.map(|s| s.len())),
        }
    }

    #[test]
    fn csv_uses_the_delimiter_it_is_given_and_quotes_only_what_that_delimiter_needs() {
        let rows = vec![vec!["a,b".to_string(), "c;d".to_string(), "e".to_string()]];
        assert_eq!(String::from_utf8(csv_bytes_with(&rows, ';')).unwrap(), "\u{FEFF}a,b;\"c;d\";e\r\n");
        assert_eq!(String::from_utf8(csv_bytes_with(&rows, ',')).unwrap(), "\u{FEFF}\"a,b\",c;d,e\r\n");
        assert_eq!(csv_bytes(&rows), csv_bytes_with(&rows, ','));
        // Either delimiter reads back (two rows give the sniffer a consistent column count to see).
        let grid =
            vec![vec!["a".to_string(), "c d".to_string(), "e".to_string()], vec!["1".into(), "2".into(), "3".into()]];
        for d in [',', ';', '\t'] {
            let back = parse_csv(&String::from_utf8(csv_bytes_with(&grid, d)).unwrap());
            assert_eq!(back, grid, "delimiter {d:?}");
        }
    }

    #[test]
    fn cells_that_look_like_formulas_cannot_run_when_the_csv_is_opened_in_excel() {
        let row = |c: &str| vec![vec![c.to_string()]];
        for (cell, want) in [
            ("=1+1", "'=1+1"),
            ("@SUM(A1)", "'@SUM(A1)"),
            ("+cmd|' /C calc'!A0", "'+cmd|' /C calc'!A0"),
            ("-2+3", "'-2+3"),
            ("\tx", "'\tx"),
        ] {
            let text = String::from_utf8(csv_bytes(&row(cell))).unwrap();
            assert!(text.contains(want), "{cell:?} -> {text:?}");
        }
        for plain in ["-5", "+3", "-0.5", "1e5", "hello", "a=b", "5-3", ""] {
            let text = String::from_utf8(csv_bytes(&row(plain))).unwrap();
            assert_eq!(text, format!("{}{plain}\r\n", '\u{FEFF}'), "{plain:?} stays as it is");
        }
    }
}
