//! PDF tools: merge several PDFs into one and split one into single-page files.
//!
//! Pages are copied, never re-rendered: content streams, fonts, images and vector graphics are carried over
//! byte for byte (stream data stays in whatever filter it already has). Only what a page reaches through its
//! references is written, in a fresh sequential object space with a classic `xref` table.
//!
//! The reader handles classic xref tables, cross-reference streams, object streams, hybrid files and
//! incremental updates (`/Prev`, the newest object wins). A damaged or missing xref is rebuilt by scanning for
//! `N G obj`. Encrypted files are refused. Nothing here panics on damaged input.
//!
//! Dropped on purpose, so nothing dangles or leaks: the document `/Info` and `/Metadata`, outlines, names,
//! the structure tree, forms (`/AcroForm`), page labels, link annotations and every action or destination
//! (they could point at pages that are not in the output).

use super::error::{Error, Result};
use std::cell::{Cell, RefCell};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::io::Read;
use std::rc::Rc;

/// Most pages a single merge or split will handle.
const MAX_PAGES: usize = 20_000;
/// Largest file the writer will produce.
const MAX_OUT: usize = 2 << 30;
/// Inflated size cap for the xref and object streams we have to read, against decompression bombs.
const MAX_INFLATE: u64 = 256 * 1024 * 1024;
/// Nesting depth of one parsed value, and of the page tree.
const MAX_DEPTH: usize = 64;
/// Page tree nodes visited before the tree is declared absurd (a DAG can fan out exponentially).
const MAX_NODES: usize = 200_000;

type Dict = BTreeMap<Vec<u8>, O>;

#[derive(Clone, Debug)]
enum O {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Name(Vec<u8>),
    Str(Vec<u8>),
    Arr(Vec<O>),
    Dict(Dict),
    Ref(u32),
    /// Dictionary plus the byte range of the raw (still encoded) data in the source buffer.
    Stream(Dict, usize, usize),
}

impl O {
    fn int(&self) -> Option<i64> {
        match self {
            O::Int(i) => Some(*i),
            O::Real(r) if r.is_finite() => Some(*r as i64),
            _ => None,
        }
    }
    fn name(&self) -> Option<&[u8]> {
        match self {
            O::Name(n) => Some(n),
            _ => None,
        }
    }
    fn dict(&self) -> Option<&Dict> {
        match self {
            O::Dict(d) | O::Stream(d, ..) => Some(d),
            _ => None,
        }
    }
}

fn key(s: &str) -> Vec<u8> {
    s.as_bytes().to_vec()
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}
fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}
fn is_regular(b: u8) -> bool {
    !is_ws(b) && !is_delim(b)
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() || hay.len() - from < needle.len() {
        return None;
    }
    let first = needle[0];
    let last_start = hay.len() - needle.len();
    let mut i = from;
    while i <= last_start {
        i += hay[i..=last_start].iter().position(|&b| b == first)?;
        if &hay[i..i + needle.len()] == needle {
            return Some(i);
        }
        i += 1;
    }
    None
}

fn rfind(hay: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || hay.len() < needle.len() {
        return None;
    }
    (0..=hay.len() - needle.len()).rev().find(|&i| &hay[i..i + needle.len()] == needle)
}

// ------------------------------------------------------------------ lexer / parser

struct P<'a> {
    d: &'a [u8],
    p: usize,
}

impl<'a> P<'a> {
    fn new(d: &'a [u8], p: usize) -> Self {
        P { d, p: p.min(d.len()) }
    }

    fn peek(&self) -> Option<u8> {
        self.d.get(self.p).copied()
    }

    fn skip_ws(&mut self) {
        while let Some(b) = self.peek() {
            if is_ws(b) {
                self.p += 1;
            } else if b == b'%' {
                while self.peek().is_some_and(|c| c != b'\n' && c != b'\r') {
                    self.p += 1;
                }
            } else {
                break;
            }
        }
    }

    fn word(&mut self) -> &'a [u8] {
        let s = self.p;
        while self.peek().is_some_and(is_regular) {
            self.p += 1;
        }
        &self.d[s..self.p]
    }

    /// An unsigned decimal integer token (and nothing else).
    fn uint(&mut self) -> Option<u64> {
        self.skip_ws();
        let w = self.word();
        if w.is_empty() || w.len() > 18 || !w.iter().all(u8::is_ascii_digit) {
            return None;
        }
        std::str::from_utf8(w).ok()?.parse().ok()
    }

    fn keyword(&mut self, kw: &[u8]) -> bool {
        self.skip_ws();
        let save = self.p;
        if self.word() == kw {
            true
        } else {
            self.p = save;
            false
        }
    }

    fn value(&mut self, depth: usize) -> Option<O> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_ws();
        let b = self.peek()?;
        match b {
            b'/' => {
                self.p += 1;
                Some(O::Name(unescape_name(self.word())))
            }
            b'(' => Some(O::Str(self.literal_string())),
            b'<' if self.d.get(self.p + 1) == Some(&b'<') => {
                self.p += 2;
                let mut dict = Dict::new();
                loop {
                    self.skip_ws();
                    match self.peek()? {
                        b'>' => {
                            self.p += if self.d.get(self.p + 1) == Some(&b'>') { 2 } else { 1 };
                            break;
                        }
                        b'/' => {
                            self.p += 1;
                            let k = unescape_name(self.word());
                            self.skip_ws();
                            if self.peek()? == b'>' {
                                dict.insert(k, O::Null);
                                continue;
                            }
                            let v = self.value(depth + 1)?;
                            dict.insert(k, v);
                        }
                        _ => return None,
                    }
                }
                Some(O::Dict(dict))
            }
            b'<' => {
                self.p += 1;
                let mut hex = Vec::new();
                loop {
                    let c = self.peek()?;
                    self.p += 1;
                    if c == b'>' {
                        break;
                    }
                    if let Some(v) = (c as char).to_digit(16) {
                        hex.push(v as u8);
                    }
                }
                Some(O::Str(hex.chunks(2).map(|c| (c[0] << 4) | c.get(1).copied().unwrap_or(0)).collect()))
            }
            b'[' => {
                self.p += 1;
                let mut v = Vec::new();
                loop {
                    self.skip_ws();
                    if self.peek()? == b']' {
                        self.p += 1;
                        break;
                    }
                    v.push(self.value(depth + 1)?);
                }
                Some(O::Arr(v))
            }
            b')' | b']' | b'>' | b'{' | b'}' => None,
            _ => {
                let w = self.word();
                match w {
                    b"true" => return Some(O::Bool(true)),
                    b"false" => return Some(O::Bool(false)),
                    b"null" => return Some(O::Null),
                    _ => {}
                }
                let first = *w.first()?;
                if !(first.is_ascii_digit() || matches!(first, b'+' | b'-' | b'.'))
                    || !w.iter().all(|c| c.is_ascii_digit() || matches!(c, b'+' | b'-' | b'.'))
                {
                    return None;
                }
                let s = std::str::from_utf8(w).ok()?;
                if let Ok(i) = s.parse::<i64>() {
                    if i >= 0 {
                        if let Some(r) = self.try_ref(i) {
                            return Some(r);
                        }
                    }
                    Some(O::Int(i))
                } else {
                    // "--5" style typos and out-of-range integers fall back to a real.
                    let f = s.trim_start_matches(['-', '+']).parse::<f64>().ok()?;
                    Some(O::Real(if s.starts_with('-') { -f } else { f }))
                }
            }
        }
    }

    /// After an integer: `G R` makes it an indirect reference.
    fn try_ref(&mut self, num: i64) -> Option<O> {
        let save = self.p;
        self.skip_ws();
        let gen = self.word();
        if !gen.is_empty() && gen.len() <= 5 && gen.iter().all(u8::is_ascii_digit) {
            self.skip_ws();
            if self.peek() == Some(b'R') && !self.d.get(self.p + 1).copied().is_some_and(is_regular) {
                self.p += 1;
                return u32::try_from(num).ok().map(O::Ref);
            }
        }
        self.p = save;
        None
    }

    fn literal_string(&mut self) -> Vec<u8> {
        self.p += 1; // (
        let mut out = Vec::new();
        let mut depth = 1;
        while let Some(c) = self.peek() {
            self.p += 1;
            match c {
                b'(' => {
                    depth += 1;
                    out.push(c);
                }
                b')' => {
                    depth -= 1;
                    if depth == 0 {
                        break;
                    }
                    out.push(c);
                }
                b'\\' => {
                    let Some(e) = self.peek() else { break };
                    self.p += 1;
                    match e {
                        b'n' => out.push(b'\n'),
                        b'r' => out.push(b'\r'),
                        b't' => out.push(b'\t'),
                        b'b' => out.push(8),
                        b'f' => out.push(12),
                        b'0'..=b'7' => {
                            let mut v = (e - b'0') as u32;
                            for _ in 0..2 {
                                match self.peek() {
                                    Some(d @ b'0'..=b'7') => {
                                        v = v * 8 + (d - b'0') as u32;
                                        self.p += 1;
                                    }
                                    _ => break,
                                }
                            }
                            out.push(v as u8);
                        }
                        b'\r' => {
                            if self.peek() == Some(b'\n') {
                                self.p += 1;
                            }
                        }
                        b'\n' => {}
                        other => out.push(other),
                    }
                }
                _ => out.push(c),
            }
        }
        out
    }
}

fn unescape_name(raw: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'#' && i + 2 < raw.len() {
            let hex = std::str::from_utf8(&raw[i + 1..i + 3]).ok().and_then(|s| u8::from_str_radix(s, 16).ok());
            if let Some(v) = hex {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    out
}

// ------------------------------------------------------------------ stream decoding (xref / object streams only)

fn dget<'a>(d: &'a Dict, k: &str) -> Option<&'a O> {
    d.get(k.as_bytes())
}

fn inflate(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    // A damaged tail still leaves what was inflated before it usable.
    let _ = flate2::read::ZlibDecoder::new(data).take(MAX_INFLATE).read_to_end(&mut out);
    if out.is_empty() && !data.is_empty() {
        let _ = flate2::read::DeflateDecoder::new(data).take(MAX_INFLATE).read_to_end(&mut out);
    }
    out
}

/// Reverses the PNG (10-15) and TIFF (2, 8-bit) predictors.
fn unpredict(data: Vec<u8>, parms: &Dict) -> Result<Vec<u8>> {
    let pred = dget(parms, "Predictor").and_then(O::int).unwrap_or(1);
    if pred <= 1 {
        return Ok(data);
    }
    let colors = dget(parms, "Colors").and_then(O::int).unwrap_or(1);
    let bpc = dget(parms, "BitsPerComponent").and_then(O::int).unwrap_or(8);
    let columns = dget(parms, "Columns").and_then(O::int).unwrap_or(1);
    if !(1..=32).contains(&colors) || !matches!(bpc, 1 | 2 | 4 | 8 | 16) || !(1..=1 << 24).contains(&columns) {
        return Err(Error::Corrupt("PDF: bad predictor parameters".into()));
    }
    let (colors, bpc, columns) = (colors as usize, bpc as usize, columns as usize);
    let bpp = (colors * bpc).div_ceil(8).max(1);
    let row = (columns * colors * bpc).div_ceil(8);
    if row > 1 << 26 {
        return Err(Error::Corrupt("PDF: predictor rows are absurdly wide".into()));
    }
    if pred == 2 {
        if bpc != 8 {
            return Err(Error::Unsupported("PDF: TIFF predictor on non-8-bit data".into()));
        }
        let mut data = data;
        for r in data.chunks_mut(row) {
            for i in colors..r.len() {
                r[i] = r[i].wrapping_add(r[i - colors]);
            }
        }
        return Ok(data);
    }
    if !(10..=15).contains(&pred) {
        return Err(Error::Unsupported("PDF: unknown predictor".into()));
    }
    let mut out: Vec<u8> = Vec::with_capacity(data.len());
    let mut prev = vec![0u8; row];
    let mut cur = vec![0u8; row];
    for chunk in data.chunks(row + 1) {
        let (ft, raw) = (chunk[0], &chunk[1..]);
        cur.iter_mut().for_each(|b| *b = 0);
        cur[..raw.len()].copy_from_slice(raw);
        for i in 0..row {
            let a = if i >= bpp { cur[i - bpp] as u32 } else { 0 };
            let b = prev[i] as u32;
            let c = if i >= bpp { prev[i - bpp] as u32 } else { 0 };
            let x = cur[i];
            cur[i] = match ft {
                0 => x,
                1 => x.wrapping_add(a as u8),
                2 => x.wrapping_add(b as u8),
                3 => x.wrapping_add(((a + b) / 2) as u8),
                4 => {
                    let p = a as i32 + b as i32 - c as i32;
                    let (pa, pb, pc) = ((p - a as i32).abs(), (p - b as i32).abs(), (p - c as i32).abs());
                    let pr = if pa <= pb && pa <= pc {
                        a
                    } else if pb <= pc {
                        b
                    } else {
                        c
                    };
                    x.wrapping_add(pr as u8)
                }
                _ => x,
            };
        }
        out.extend_from_slice(&cur[..if raw.len() < row { raw.len() } else { row }]);
        std::mem::swap(&mut prev, &mut cur);
        if out.len() as u64 > MAX_INFLATE {
            return Err(Error::Unsupported("PDF: stream too large".into()));
        }
    }
    Ok(out)
}

// ------------------------------------------------------------------ document

#[derive(Clone, Copy, Debug)]
enum Entry {
    Free,
    Offset(usize),
    /// (object stream number, index inside it)
    InStm(u32, u32),
}

struct ObjStm {
    data: Vec<u8>,
    /// (object number, absolute offset inside `data`)
    offs: Vec<(u32, usize)>,
}

struct Doc<'a> {
    data: &'a [u8],
    xref: HashMap<u32, Entry>,
    trailer: Dict,
    stms: RefCell<HashMap<u32, Option<Rc<ObjStm>>>>,
    depth: Cell<u32>,
    rebuilt: bool,
}

struct Page {
    dict: Dict,
    resources: Option<O>,
    media_box: Option<O>,
    crop_box: Option<O>,
    rotate: Option<O>,
}

struct PageList {
    pages: Vec<Page>,
    /// Page and page-tree objects: references to them are never copied (they would drag the whole tree along).
    blocked: HashSet<u32>,
}

impl<'a> Doc<'a> {
    fn open(data: &'a [u8], force_rebuild: bool) -> Result<Doc<'a>> {
        if find(&data[..data.len().min(1024)], b"%PDF-", 0).is_none() {
            return Err(Error::Corrupt("not a PDF file".into()));
        }
        let mut doc = Doc {
            data,
            xref: HashMap::new(),
            trailer: Dict::new(),
            stms: RefCell::new(HashMap::new()),
            depth: Cell::new(0),
            rebuilt: false,
        };
        let ok = !force_rebuild && doc.load_xref_chain().is_some() && doc.validate();
        if !ok {
            doc.xref.clear();
            doc.trailer.clear();
            doc.stms.borrow_mut().clear();
            doc.rebuild()?;
            doc.rebuilt = true;
        }
        if doc.trailer.contains_key(b"Encrypt".as_slice()) {
            return Err(Error::Unsupported("password-protected PDF".into()));
        }
        Ok(doc)
    }

    /// Opens the file and lists its pages, falling back to a rebuilt xref when the first reading is unusable.
    fn open_pages(data: &'a [u8]) -> Result<(Doc<'a>, PageList)> {
        let first = Doc::open(data, false)?;
        match first.pages() {
            Ok(list) if !list.pages.is_empty() => Ok((first, list)),
            first_result => {
                if first.rebuilt {
                    return Self::finish_pages(first, first_result);
                }
                let second = Doc::open(data, true)?;
                match second.pages() {
                    Ok(list) if !list.pages.is_empty() => Ok((second, list)),
                    second_result => {
                        // Neither reading found pages: report the more informative failure.
                        let r =
                            if first_result.is_err() && second_result.is_ok() { second_result } else { first_result };
                        Self::finish_pages(second, r)
                    }
                }
            }
        }
    }

    fn finish_pages(doc: Doc<'a>, r: Result<PageList>) -> Result<(Doc<'a>, PageList)> {
        let list = r?;
        if list.pages.is_empty() {
            return Err(Error::InvalidArgument("the PDF has no pages".into()));
        }
        Ok((doc, list))
    }

    // ---- xref reading

    fn load_xref_chain(&mut self) -> Option<()> {
        let tail_from = self.data.len().saturating_sub(64 * 1024);
        let sx = tail_from + rfind(&self.data[tail_from..], b"startxref")?;
        let mut p = P::new(self.data, sx + 9);
        let mut next = Some(usize::try_from(p.uint()?).ok()?);
        let mut seen = HashSet::new();
        while let Some(off) = next {
            if !seen.insert(off) || seen.len() > 1000 {
                return None;
            }
            let mut local = HashMap::new();
            let t = self.section(off, &mut local)?;
            if let Some(x) = dget(&t, "XRefStm").and_then(O::int).and_then(|x| usize::try_from(x).ok()) {
                // Hybrid file: its stream entries stand in for objects the classic table lists as free.
                let mut extra = HashMap::new();
                if seen.insert(x) && self.section(x, &mut extra).is_some() {
                    for (k, v) in extra {
                        if matches!(local.get(&k), None | Some(Entry::Free)) {
                            local.insert(k, v);
                        }
                    }
                }
            }
            for (k, v) in local {
                self.xref.entry(k).or_insert(v);
            }
            for (k, v) in t.iter() {
                self.trailer.entry(k.clone()).or_insert_with(|| v.clone());
            }
            next = dget(&t, "Prev").and_then(O::int).and_then(|x| usize::try_from(x).ok());
        }
        Some(())
    }

    /// Reads one xref section (table or stream) at `off`; returns its trailer dictionary.
    fn section(&self, off: usize, out: &mut HashMap<u32, Entry>) -> Option<Dict> {
        let mut p = P::new(self.data, off);
        p.skip_ws();
        if p.keyword(b"xref") {
            return self.classic_section(&mut p, out);
        }
        let (_, obj) = self.obj_at(off)?;
        let O::Stream(d, s, e) = obj else { return None };
        dget(&d, "W")?;
        let bytes = self.decode(&d, &self.data[s..e]).ok()?;
        let w: Vec<usize> = match dget(&d, "W")? {
            O::Arr(a) => a
                .iter()
                .map(|x| x.int().unwrap_or(-1))
                .map(|x| if (0..=8).contains(&x) { x as usize } else { 99 })
                .collect(),
            _ => return None,
        };
        if w.len() != 3 || w.iter().any(|&x| x > 8) {
            return None;
        }
        let rowlen = w[0] + w[1] + w[2];
        if rowlen == 0 {
            return None;
        }
        let size = dget(&d, "Size").and_then(O::int).unwrap_or(0).max(0);
        let index: Vec<i64> = match dget(&d, "Index") {
            Some(O::Arr(a)) => a.iter().filter_map(O::int).collect(),
            _ => vec![0, size],
        };
        let rd = |b: &[u8]| b.iter().fold(0u64, |a, &x| (a << 8) | x as u64);
        let mut at = 0usize;
        for pair in index.chunks_exact(2) {
            let (start, count) = (pair[0], pair[1]);
            if start < 0 || count < 0 {
                return None;
            }
            for i in 0..count {
                let Some(row) = bytes.get(at..at + rowlen) else { break };
                at += rowlen;
                let t = if w[0] == 0 { 1 } else { rd(&row[..w[0]]) };
                let f2 = rd(&row[w[0]..w[0] + w[1]]);
                let f3 = rd(&row[w[0] + w[1]..]);
                let Ok(num) = u32::try_from(start + i) else { break };
                let e = match t {
                    0 => Entry::Free,
                    1 if f2 > 0 => Entry::Offset(usize::try_from(f2).ok()?),
                    1 => Entry::Free,
                    2 => Entry::InStm(u32::try_from(f2).ok()?, u32::try_from(f3).unwrap_or(u32::MAX)),
                    _ => continue,
                };
                out.entry(num).or_insert(e);
            }
        }
        Some(d)
    }

    fn classic_section(&self, p: &mut P, out: &mut HashMap<u32, Entry>) -> Option<Dict> {
        loop {
            p.skip_ws();
            if p.keyword(b"trailer") {
                return match p.value(0)? {
                    O::Dict(d) => Some(d),
                    _ => None,
                };
            }
            let mut start = p.uint()?;
            let count = p.uint()?;
            if count > (self.data.len() as u64) / 6 + 1 {
                return None;
            }
            for i in 0..count {
                let off = p.uint()?;
                let gen = p.uint()?;
                p.skip_ws();
                let kind = p.word();
                // Common generator bug: the section says it starts at 1 but the first row is the free head.
                if i == 0 && start == 1 && off == 0 && gen == 65535 && kind == b"f" {
                    start = 0;
                }
                let num = u32::try_from(start + i).ok()?;
                let e = match kind {
                    b"n" if off > 0 => Entry::Offset(usize::try_from(off).ok()?),
                    b"n" | b"f" => Entry::Free,
                    _ => return None,
                };
                out.entry(num).or_insert(e);
            }
        }
    }

    /// Cheap sanity pass over a freshly read xref: every in-use offset must start `N G obj`, and the catalog
    /// must lead to a page tree. Anything else means the table is damaged and the file is rebuilt by scanning.
    fn validate(&self) -> bool {
        for (&n, e) in &self.xref {
            if let Entry::Offset(off) = *e {
                if self.header_at(off).map(|(num, _)| num) != Some(n) {
                    return false;
                }
            }
        }
        self.catalog().is_some()
    }

    fn catalog(&self) -> Option<Dict> {
        let O::Ref(r) = dget(&self.trailer, "Root")? else { return None };
        match self.get(*r)? {
            O::Dict(d) if dget(&d, "Pages").is_some() => Some(d),
            _ => None,
        }
    }

    /// Parses `N G obj` at `off`; returns the object number and the position after the keyword.
    fn header_at(&self, off: usize) -> Option<(u32, usize)> {
        let mut p = P::new(self.data, off);
        let num = u32::try_from(p.uint()?).ok()?;
        p.uint()?;
        if !p.keyword(b"obj") {
            return None;
        }
        Some((num, p.p))
    }

    fn obj_at(&self, off: usize) -> Option<(u32, O)> {
        let (num, after) = self.header_at(off)?;
        let mut p = P::new(self.data, after);
        let v = p.value(0)?;
        let O::Dict(d) = v else { return Some((num, v)) };
        if !p.keyword(b"stream") {
            return Some((num, O::Dict(d)));
        }
        let mut s = p.p;
        if self.data.get(s) == Some(&b'\r') {
            s += 1;
        }
        if self.data.get(s) == Some(&b'\n') {
            s += 1;
        }
        let e = self.stream_end(&d, s);
        Some((num, O::Stream(d, s, e)))
    }

    fn stream_end(&self, d: &Dict, start: usize) -> usize {
        let len = match dget(d, "Length") {
            Some(O::Int(i)) => Some(*i),
            Some(r @ O::Ref(_)) if self.depth.get() < 4 => {
                self.depth.set(self.depth.get() + 1);
                let v = self.resolve(r).int();
                self.depth.set(self.depth.get() - 1);
                v
            }
            _ => None,
        };
        if let Some(len) = len.filter(|&l| l >= 0) {
            let end = start.saturating_add(len as usize);
            if end <= self.data.len() {
                let mut p = P::new(self.data, end);
                p.skip_ws();
                if self.data[p.p..].starts_with(b"endstream") {
                    return end;
                }
            }
        }
        match find(self.data, b"endstream", start) {
            Some(mut e) => {
                if e > start && self.data[e - 1] == b'\n' {
                    e -= 1;
                }
                if e > start && self.data[e - 1] == b'\r' {
                    e -= 1;
                }
                e
            }
            None => self.data.len(),
        }
    }

    fn decode(&self, d: &Dict, raw: &[u8]) -> Result<Vec<u8>> {
        let filters: Vec<Vec<u8>> = match dget(d, "Filter").map(|f| self.resolve(f)) {
            Some(O::Name(n)) => vec![n],
            Some(O::Arr(a)) => a.iter().filter_map(|f| self.resolve(f).name().map(<[u8]>::to_vec)).collect(),
            _ => vec![],
        };
        let parms: Vec<Dict> = match dget(d, "DecodeParms").or_else(|| dget(d, "DP")).map(|f| self.resolve(f)) {
            Some(O::Dict(p)) => vec![p],
            Some(O::Arr(a)) => a.iter().map(|x| self.resolve(x).dict().cloned().unwrap_or_default()).collect(),
            _ => vec![],
        };
        let mut data = raw.to_vec();
        for (i, f) in filters.iter().enumerate() {
            data = match f.as_slice() {
                b"FlateDecode" | b"Fl" => {
                    let out = inflate(&data);
                    if out.is_empty() && !data.is_empty() {
                        return Err(Error::Corrupt("PDF: a stream cannot be inflated".into()));
                    }
                    match parms.get(i) {
                        Some(p) => unpredict(out, p)?,
                        None => out,
                    }
                }
                b"ASCIIHexDecode" | b"AHx" => {
                    let hex: Vec<u8> = data
                        .iter()
                        .take_while(|&&b| b != b'>')
                        .filter_map(|&b| (b as char).to_digit(16).map(|v| v as u8))
                        .collect();
                    hex.chunks(2).map(|c| (c[0] << 4) | c.get(1).copied().unwrap_or(0)).collect()
                }
                _ => return Err(Error::Unsupported("PDF: unsupported filter on a structure stream".into())),
            };
        }
        Ok(data)
    }

    // ---- rebuild

    /// Finds every `N G obj` by scanning (skipping stream bodies), then trailers and object streams.
    fn rebuild(&mut self) -> Result<()> {
        let d = self.data;
        let mut best: HashMap<u32, (usize, Entry)> = HashMap::new();
        let mut trailers: Vec<(usize, Dict)> = Vec::new();
        let mut objstms: Vec<(usize, u32)> = Vec::new();
        let mut at = 0usize;
        while let Some(i) = find(d, b"obj", at) {
            at = i + 3;
            // "endobj" and words that merely end in "obj" are not headers.
            if i == 0 || !is_ws(d[i - 1]) {
                continue;
            }
            let mut j = i - 1;
            while j > 0 && is_ws(d[j]) {
                j -= 1;
            }
            let gen_end = j + 1;
            while j > 0 && d[j].is_ascii_digit() {
                j -= 1;
            }
            if gen_end == j + 1 || !is_ws(d[j]) {
                continue;
            }
            while j > 0 && is_ws(d[j]) {
                j -= 1;
            }
            let num_end = j + 1;
            while j > 0 && d[j].is_ascii_digit() {
                j -= 1;
            }
            let num_start = if d[j].is_ascii_digit() { j } else { j + 1 };
            if num_start >= num_end || num_end - num_start > 10 {
                continue;
            }
            let Some(num) = std::str::from_utf8(&d[num_start..num_end]).ok().and_then(|s| s.parse::<u32>().ok()) else {
                continue;
            };
            best.insert(num, (num_start, Entry::Offset(num_start)));
            // Parse to learn what it is and, for streams, where the body ends so its bytes are not scanned.
            if let Some((_, O::Stream(sd, _, e))) = self.obj_at(num_start) {
                match dget(&sd, "Type").and_then(O::name) {
                    Some(b"XRef") => trailers.push((num_start, sd.clone())),
                    Some(b"ObjStm") => objstms.push((num_start, num)),
                    _ => {}
                }
                at = at.max(e);
            }
        }
        let mut at = 0usize;
        while let Some(i) = find(d, b"trailer", at) {
            at = i + 7;
            let mut p = P::new(d, at);
            if let Some(O::Dict(t)) = p.value(0) {
                trailers.push((i, t));
            }
        }
        trailers.sort_by_key(|(pos, _)| *pos);
        for (_, t) in trailers {
            for k in ["Root", "Encrypt", "Info", "ID"] {
                if let Some(v) = dget(&t, k) {
                    self.trailer.insert(key(k), v.clone());
                }
            }
        }
        self.xref = best.iter().map(|(k, (_, e))| (*k, *e)).collect();
        // Compressed objects: a later container (or a later direct definition) wins.
        objstms.sort_unstable();
        for (pos, snum) in objstms {
            let Some(st) = self.objstm(snum) else { continue };
            for (idx, (n, _)) in st.offs.iter().enumerate() {
                if best.get(n).map_or(true, |(p, _)| *p < pos) {
                    best.insert(*n, (pos, Entry::InStm(snum, idx as u32)));
                }
            }
        }
        self.xref = best.into_iter().map(|(k, (_, e))| (k, e)).collect();
        if self.xref.is_empty() {
            return Err(Error::Corrupt("PDF: no objects found".into()));
        }
        if self.trailer.contains_key(b"Encrypt".as_slice()) {
            return Ok(());
        }
        if self.catalog().is_none() {
            let mut nums: Vec<u32> = self.xref.keys().copied().collect();
            nums.sort_unstable_by(|a, b| b.cmp(a));
            let found = nums.into_iter().find(|&n| {
                matches!(self.get(n), Some(O::Dict(ref d)) if dget(d, "Type").and_then(O::name) == Some(b"Catalog") && dget(d, "Pages").is_some())
            });
            match found {
                Some(n) => {
                    self.trailer.insert(key("Root"), O::Ref(n));
                }
                None => return Err(Error::Corrupt("PDF: no page catalog found".into())),
            }
        }
        Ok(())
    }

    // ---- objects

    fn exists(&self, num: u32) -> bool {
        !matches!(self.xref.get(&num), None | Some(Entry::Free))
    }

    fn get(&self, num: u32) -> Option<O> {
        match *self.xref.get(&num)? {
            Entry::Free => None,
            Entry::Offset(off) => match self.obj_at(off) {
                Some((n, o)) if n == num => Some(o),
                _ => None,
            },
            Entry::InStm(s, idx) => {
                let st = self.objstm(s)?;
                let (_, off) = match st.offs.get(idx as usize) {
                    Some(&(n, off)) if n == num => (n, off),
                    _ => *st.offs.iter().find(|(n, _)| *n == num)?,
                };
                let mut p = P::new(&st.data, off);
                p.value(0)
            }
        }
    }

    fn objstm(&self, snum: u32) -> Option<Rc<ObjStm>> {
        if let Some(c) = self.stms.borrow().get(&snum) {
            return c.clone();
        }
        let built = self.build_objstm(snum).map(Rc::new);
        self.stms.borrow_mut().insert(snum, built.clone());
        built
    }

    fn build_objstm(&self, snum: u32) -> Option<ObjStm> {
        // A container must itself be a plain object, never a compressed one.
        let Entry::Offset(off) = *self.xref.get(&snum)? else { return None };
        let (n, obj) = self.obj_at(off)?;
        let O::Stream(d, s, e) = obj else { return None };
        if n != snum || dget(&d, "Type").and_then(O::name) != Some(b"ObjStm") {
            return None;
        }
        let data = self.decode(&d, &self.data[s..e]).ok()?;
        let count = dget(&d, "N").and_then(O::int)?.clamp(0, 1_000_000) as usize;
        let first = dget(&d, "First").and_then(O::int)?.max(0) as usize;
        let mut p = P::new(&data, 0);
        let mut offs = Vec::new();
        for _ in 0..count {
            let (Some(a), Some(b)) = (p.uint(), p.uint()) else { break };
            offs.push((u32::try_from(a).ok()?, first.checked_add(usize::try_from(b).ok()?)?));
        }
        Some(ObjStm { data, offs })
    }

    fn resolve(&self, o: &O) -> O {
        let mut cur = o.clone();
        for _ in 0..32 {
            match cur {
                O::Ref(n) => cur = self.get(n).unwrap_or(O::Null),
                other => return other,
            }
        }
        O::Null
    }

    // ---- pages

    fn pages(&self) -> Result<PageList> {
        let cat = self.catalog().ok_or_else(|| Error::Corrupt("PDF: no page catalog".into()))?;
        let root = dget(&cat, "Pages").cloned().unwrap_or(O::Null);
        if !matches!(self.resolve(&root), O::Dict(_)) {
            return Err(Error::Corrupt("PDF: no page tree".into()));
        }
        let mut list = PageList { pages: Vec::new(), blocked: HashSet::new() };
        let mut nodes = 0usize;
        let mut inherited = Inherited::default();
        self.walk(&root, &mut inherited, &mut list, &mut nodes, 0)?;
        Ok(list)
    }

    /// Depth-first walk of the page tree. A node already seen as an intermediate is skipped, so cycles end.
    fn walk(&self, node: &O, inh: &mut Inherited, list: &mut PageList, nodes: &mut usize, depth: usize) -> Result<()> {
        *nodes += 1;
        if depth > MAX_DEPTH || *nodes > MAX_NODES {
            return Err(Error::Corrupt("PDF: page tree is too deep or too large".into()));
        }
        let num = if let O::Ref(n) = node { Some(*n) } else { None };
        let resolved = self.resolve(node);
        let Some(d) = resolved.dict() else { return Ok(()) };
        if let Some(Some(kids)) = dget(d, "Kids").map(|k| match self.resolve(k) {
            O::Arr(a) => Some(a),
            _ => None,
        }) {
            if let Some(n) = num {
                if !list.blocked.insert(n) {
                    return Ok(());
                }
            }
            let saved = inh.clone();
            inh.take_from(d);
            for k in &kids {
                self.walk(k, inh, list, nodes, depth + 1)?;
            }
            *inh = saved;
            return Ok(());
        }
        if dget(d, "Type").and_then(O::name) == Some(b"Pages") {
            return Ok(()); // an empty intermediate node
        }
        if let Some(n) = num {
            list.blocked.insert(n);
        }
        if list.pages.len() >= MAX_PAGES {
            return Err(Error::Unsupported(format!("more than {MAX_PAGES} pages")));
        }
        let mut here = inh.clone();
        here.take_from(d);
        list.pages.push(Page {
            dict: d.clone(),
            resources: here.resources,
            media_box: here.media_box,
            crop_box: here.crop_box,
            rotate: here.rotate,
        });
        Ok(())
    }
}

#[derive(Clone, Default)]
struct Inherited {
    resources: Option<O>,
    media_box: Option<O>,
    crop_box: Option<O>,
    rotate: Option<O>,
}

impl Inherited {
    fn take_from(&mut self, d: &Dict) {
        let pick = |k: &str, slot: &mut Option<O>| {
            if let Some(v) = dget(d, k) {
                *slot = Some(v.clone());
            }
        };
        pick("Resources", &mut self.resources);
        pick("MediaBox", &mut self.media_box);
        pick("CropBox", &mut self.crop_box);
        pick("Rotate", &mut self.rotate);
    }
}

// ------------------------------------------------------------------ writer

struct Writer {
    buf: Vec<u8>,
    /// Offset of object `n` at index `n - 1`; 0 until written.
    offsets: Vec<usize>,
}

impl Writer {
    fn new() -> Writer {
        let mut buf = Vec::with_capacity(1 << 16);
        buf.extend_from_slice(b"%PDF-1.7\n%\xE2\xE3\xCF\xD3\n");
        Writer { buf, offsets: Vec::new() }
    }

    fn alloc(&mut self) -> u32 {
        self.offsets.push(0);
        self.offsets.len() as u32
    }

    fn begin(&mut self, num: u32) {
        self.offsets[num as usize - 1] = self.buf.len();
        self.buf.extend_from_slice(format!("{num} 0 obj\n").as_bytes());
    }

    fn end(&mut self) -> Result<()> {
        self.buf.extend_from_slice(b"\nendobj\n");
        if self.buf.len() > MAX_OUT {
            return Err(Error::Unsupported("the result would be larger than 2 GiB".into()));
        }
        Ok(())
    }

    fn finish(mut self, root: u32) -> Result<Vec<u8>> {
        for n in 1..=self.offsets.len() as u32 {
            if self.offsets[n as usize - 1] == 0 {
                self.begin(n);
                self.buf.extend_from_slice(b"null");
                self.end()?;
            }
        }
        let xref_at = self.buf.len();
        let count = self.offsets.len() + 1;
        self.buf.extend_from_slice(format!("xref\n0 {count}\n0000000000 65535 f \n").as_bytes());
        for off in &self.offsets {
            self.buf.extend_from_slice(format!("{off:010} 00000 n \n").as_bytes());
        }
        self.buf.extend_from_slice(
            format!("trailer\n<< /Size {count} /Root {root} 0 R >>\nstartxref\n{xref_at}\n%%EOF\n").as_bytes(),
        );
        if self.buf.len() > MAX_OUT {
            return Err(Error::Unsupported("the result would be larger than 2 GiB".into()));
        }
        Ok(self.buf)
    }
}

fn write_real(out: &mut Vec<u8>, v: f64) {
    if !v.is_finite() {
        out.push(b'0');
        return;
    }
    let s = format!("{:.6}", v.clamp(-1e15, 1e15));
    let s = s.trim_end_matches('0').trim_end_matches('.');
    out.extend_from_slice(if s.is_empty() || s == "-" || s == "-0" { b"0" } else { s.as_bytes() });
}

fn write_name(out: &mut Vec<u8>, n: &[u8]) {
    out.push(b'/');
    for &b in n {
        if b <= 0x20 || b >= 0x7f || b == b'#' || is_delim(b) {
            out.extend_from_slice(format!("#{b:02X}").as_bytes());
        } else {
            out.push(b);
        }
    }
}

/// Keys dropped from every copied dictionary: they point back into the page tree, the structure tree or
/// document-level metadata, so keeping them would leak data or leave dangling references.
const STRIP: &[&str] =
    &["Parent", "Metadata", "PieceInfo", "StructParent", "StructParents", "Thumb", "AF", "SpiderInfo"];

/// Keys a page keeps. Everything else (`/Parent`, `/B`, `/AA`, `/Thumb`, `/Metadata`, ...) is rebuilt or dropped.
const PAGE_KEYS: &[&str] = &["Contents", "BleedBox", "TrimBox", "ArtBox", "UserUnit", "Group"];

/// Annotation subtypes that draw on their own; everything else (links, popups, media, ...) is removed.
const ANNOT_SUBTYPES: &[&[u8]] = &[
    b"Text",
    b"FreeText",
    b"Line",
    b"Square",
    b"Circle",
    b"Polygon",
    b"PolyLine",
    b"Highlight",
    b"Underline",
    b"Squiggly",
    b"StrikeOut",
    b"Stamp",
    b"Caret",
    b"Ink",
    b"Widget",
];

/// Annotation keys that survive: appearance and geometry only. No actions, destinations, parents, popups,
/// replies, authors or field values.
const ANNOT_KEYS: &[&str] = &[
    "Type",
    "Subtype",
    "Rect",
    "Contents",
    "NM",
    "M",
    "F",
    "AP",
    "AS",
    "Border",
    "C",
    "CA",
    "BS",
    "BE",
    "IC",
    "QuadPoints",
    "L",
    "LE",
    "IT",
    "Vertices",
    "InkList",
    "RD",
    "DA",
    "Q",
    "DS",
    "RC",
    "Name",
    "MK",
    "CL",
    "CP",
];

enum Work {
    Old(u32),
    Synth(u32, Dict),
}

/// Copies objects from one source document into a writer, renumbering them and following references.
struct Copier<'w, 'd, 'a> {
    w: &'w mut Writer,
    doc: &'d Doc<'a>,
    blocked: &'d HashSet<u32>,
    map: HashMap<u32, u32>,
    queue: Vec<Work>,
}

impl<'w, 'd, 'a> Copier<'w, 'd, 'a> {
    fn new(w: &'w mut Writer, doc: &'d Doc<'a>, blocked: &'d HashSet<u32>) -> Self {
        Copier { w, doc, blocked, map: HashMap::new(), queue: Vec::new() }
    }

    fn reference(&mut self, old: u32) -> Option<u32> {
        if self.blocked.contains(&old) || !self.doc.exists(old) {
            return None;
        }
        if let Some(&n) = self.map.get(&old) {
            return Some(n);
        }
        let n = self.w.alloc();
        self.map.insert(old, n);
        self.queue.push(Work::Old(old));
        Some(n)
    }

    fn emit(&mut self, o: &O) {
        match o {
            O::Null => self.w.buf.extend_from_slice(b"null"),
            O::Bool(b) => self.w.buf.extend_from_slice(if *b { b"true" } else { b"false" }),
            O::Int(i) => self.w.buf.extend_from_slice(i.to_string().as_bytes()),
            O::Real(r) => write_real(&mut self.w.buf, *r),
            O::Name(n) => write_name(&mut self.w.buf, n),
            O::Str(s) => {
                self.w.buf.push(b'<');
                for b in s {
                    self.w.buf.extend_from_slice(format!("{b:02X}").as_bytes());
                }
                self.w.buf.push(b'>');
            }
            O::Arr(a) => {
                self.w.buf.push(b'[');
                for (i, x) in a.iter().enumerate() {
                    if i > 0 {
                        self.w.buf.push(b' ');
                    }
                    self.emit(x);
                }
                self.w.buf.push(b']');
            }
            O::Dict(d) | O::Stream(d, ..) => self.emit_dict(d, None),
            O::Ref(n) => match self.reference(*n) {
                Some(new) => self.w.buf.extend_from_slice(format!("{new} 0 R").as_bytes()),
                None => self.w.buf.extend_from_slice(b"null"),
            },
        }
    }

    fn emit_dict(&mut self, d: &Dict, stream_len: Option<usize>) {
        self.w.buf.extend_from_slice(b"<<");
        for (k, v) in d {
            if (stream_len.is_some() && k.as_slice() == b"Length") || STRIP.iter().any(|s| s.as_bytes() == k.as_slice())
            {
                continue;
            }
            self.w.buf.push(b' ');
            write_name(&mut self.w.buf, k);
            self.w.buf.push(b' ');
            self.emit(v);
        }
        if let Some(n) = stream_len {
            self.w.buf.extend_from_slice(format!(" /Length {n}").as_bytes());
        }
        self.w.buf.extend_from_slice(b" >>");
    }

    /// Writes every queued object (and whatever those reach) until nothing is left.
    fn drain(&mut self) -> Result<()> {
        while let Some(work) = self.queue.pop() {
            match work {
                Work::Old(old) => {
                    let new = self.map[&old];
                    self.w.begin(new);
                    match self.doc.get(old) {
                        Some(O::Stream(d, s, e)) => {
                            self.emit_dict(&d, Some(e - s));
                            self.w.buf.extend_from_slice(b"\nstream\n");
                            self.w.buf.extend_from_slice(&self.doc.data[s..e]);
                            self.w.buf.extend_from_slice(b"\nendstream");
                        }
                        Some(o) => self.emit(&o),
                        None => self.w.buf.extend_from_slice(b"null"),
                    }
                    self.w.end()?;
                }
                Work::Synth(new, d) => {
                    self.w.begin(new);
                    self.emit_dict(&d, None);
                    self.w.end()?;
                }
            }
        }
        Ok(())
    }

    /// Keeps an annotation only when it can stand alone; returns its sanitized dictionary.
    fn annotation(&self, o: &O) -> Option<Dict> {
        let O::Dict(d) = self.doc.resolve(o) else { return None };
        let sub = dget(&d, "Subtype").map(|s| self.doc.resolve(s))?;
        let sub = sub.name()?;
        if !ANNOT_SUBTYPES.contains(&sub) {
            return None;
        }
        if sub == b"Widget" && dget(&d, "AP").is_none() {
            return None;
        }
        let mut out = Dict::new();
        for k in ANNOT_KEYS {
            if let Some(v) = dget(&d, k) {
                out.insert(key(k), v.clone());
            }
        }
        Some(out)
    }

    /// Writes a flattened copy of `page` under `parent` and returns its new object number.
    fn page(&mut self, page: &Page, parent: u32) -> u32 {
        let num = self.w.alloc();
        // Annotations first: they allocate numbers of their own.
        let mut annots: Vec<u32> = Vec::new();
        if let Some(a) = dget(&page.dict, "Annots") {
            if let O::Arr(items) = self.doc.resolve(a) {
                for it in &items {
                    if let Some(d) = self.annotation(it) {
                        let n = self.w.alloc();
                        self.queue.push(Work::Synth(n, d));
                        annots.push(n);
                    }
                }
            }
        }
        self.w.begin(num);
        self.w.buf.extend_from_slice(format!("<< /Type /Page /Parent {parent} 0 R").as_bytes());
        self.w.buf.extend_from_slice(b" /Resources ");
        match &page.resources {
            Some(r) => self.emit(r),
            None => self.w.buf.extend_from_slice(b"<< >>"),
        }
        self.w.buf.extend_from_slice(b" /MediaBox ");
        match &page.media_box {
            Some(m) => self.emit(m),
            None => self.w.buf.extend_from_slice(b"[0 0 612 792]"),
        }
        for (k, v) in [("CropBox", &page.crop_box), ("Rotate", &page.rotate)] {
            if let Some(v) = v {
                self.w.buf.extend_from_slice(format!(" /{k} ").as_bytes());
                self.emit(v);
            }
        }
        for k in PAGE_KEYS {
            if let Some(v) = dget(&page.dict, k) {
                self.w.buf.extend_from_slice(format!(" /{k} ").as_bytes());
                self.emit(v);
            }
        }
        if !annots.is_empty() {
            self.w.buf.extend_from_slice(b" /Annots [");
            for (i, n) in annots.iter().enumerate() {
                if i > 0 {
                    self.w.buf.push(b' ');
                }
                self.w.buf.extend_from_slice(format!("{n} 0 R").as_bytes());
            }
            self.w.buf.push(b']');
        }
        self.w.buf.extend_from_slice(b" >>");
        // `end` only checks the size cap here; the queue is drained by the caller.
        let _ = self.w.end();
        num
    }
}

/// Starts a file whose object 1 is the catalog and object 2 the page tree; pages follow from 3.
fn new_writer() -> Writer {
    let mut w = Writer::new();
    w.alloc();
    w.alloc();
    w
}

fn close_writer(mut w: Writer, kids: &[u32]) -> Result<Vec<u8>> {
    w.begin(1);
    w.buf.extend_from_slice(b"<< /Type /Catalog /Pages 2 0 R >>");
    w.end()?;
    w.begin(2);
    w.buf.extend_from_slice(format!("<< /Type /Pages /Count {} /Kids [", kids.len()).as_bytes());
    for (i, k) in kids.iter().enumerate() {
        if i > 0 {
            w.buf.push(b' ');
        }
        w.buf.extend_from_slice(format!("{k} 0 R").as_bytes());
    }
    w.buf.extend_from_slice(b"] >>");
    w.end()?;
    w.finish(1)
}

// ------------------------------------------------------------------ public API

/// Joins PDFs into one, pages in input order. Content is copied, not re-rendered: text stays text, fonts and
/// vector graphics stay vector and images keep their original encoding. The result has no author or title.
///
/// Errors: no inputs is `InvalidArgument`; a password-protected input is `Unsupported("password-protected PDF")`;
/// a damaged input is `Corrupt`; more than 20,000 pages in total or a result over 2 GiB is `Unsupported`.
pub fn merge_pdfs(inputs: &[Vec<u8>]) -> Result<Vec<u8>> {
    if inputs.is_empty() {
        return Err(Error::InvalidArgument("nothing to merge: no PDF files given".into()));
    }
    let mut w = new_writer();
    let mut kids: Vec<u32> = Vec::new();
    for input in inputs {
        let (doc, list) = Doc::open_pages(input)?;
        if kids.len() + list.pages.len() > MAX_PAGES {
            return Err(Error::Unsupported(format!("more than {MAX_PAGES} pages")));
        }
        let mut c = Copier::new(&mut w, &doc, &list.blocked);
        for page in &list.pages {
            kids.push(c.page(page, 2));
        }
        c.drain()?;
    }
    close_writer(w, &kids)
}

/// Splits a PDF into one single-page PDF per page, in page order. Each output carries only what its page
/// uses, shares nothing with the others, and has no author or title.
pub fn split_pdf(input: &[u8]) -> Result<Vec<Vec<u8>>> {
    let (doc, list) = Doc::open_pages(input)?;
    let mut out = Vec::with_capacity(list.pages.len());
    for page in &list.pages {
        let mut w = new_writer();
        let mut c = Copier::new(&mut w, &doc, &list.blocked);
        let n = c.page(page, 2);
        c.drain()?;
        out.push(close_writer(w, &[n])?);
    }
    Ok(out)
}

/// Number of pages according to the page tree (nothing is rendered).
pub fn page_total(input: &[u8]) -> Result<u32> {
    let (_, list) = Doc::open_pages(input)?;
    Ok(list.pages.len() as u32)
}

#[cfg(test)]
mod tests {
    use super::super::os;
    use super::super::pdfread::extract_pages;
    use super::super::pdfwrite::{images_to_pdf, text_to_pdf, PdfImage};
    use super::*;

    const EDGE: &[u8] = include_bytes!("../../tests/fixtures/edge3.pdf");
    const OBJSTM: &[u8] = include_bytes!("../../tests/fixtures/objstm4.pdf");
    const CLASSIC: &[u8] = include_bytes!("../../tests/fixtures/classic3.pdf");
    const INCR: &[u8] = include_bytes!("../../tests/fixtures/incr_base.pdf");

    fn fixtures() -> Vec<(&'static str, &'static [u8])> {
        vec![("edge", EDGE), ("objstm", OBJSTM), ("classic", CLASSIC), ("incremental", INCR)]
    }

    fn lots_of_text(tag: &str, lines: usize) -> String {
        (0..lines).map(|i| format!("{tag} line {i}\n")).collect()
    }

    fn contains(hay: &[u8], needle: &[u8]) -> bool {
        find(hay, needle, 0).is_some()
    }

    /// Mean absolute difference per channel between two renders (0 = identical).
    fn mean_diff(a: &image::RgbaImage, b: &image::RgbaImage) -> f64 {
        assert_eq!((a.width(), a.height()), (b.width(), b.height()), "render sizes differ");
        let sum: u64 =
            a.as_raw().iter().zip(b.as_raw()).map(|(x, y)| (*x as i32 - *y as i32).unsigned_abs() as u64).sum();
        sum as f64 / a.as_raw().len() as f64
    }

    fn render(pdf: &[u8], page: u32) -> image::RgbaImage {
        os::render_pdf_pages(pdf, &[page], 150, 40_000_000).unwrap().remove(0)
    }

    /// Every stream of `out` must be a byte-for-byte copy of a stream in `src` (nothing re-encoded).
    fn assert_streams_are_raw_copies(src: &[u8], out: &[u8]) {
        let doc = Doc::open(out, false).unwrap();
        let mut checked = 0;
        for &n in doc.xref.keys() {
            if let Some(O::Stream(_, s, e)) = doc.get(n) {
                if e - s >= 16 {
                    assert!(contains(src, &out[s..e]), "stream of object {n} is not a verbatim copy");
                    checked += 1;
                }
            }
        }
        assert!(checked > 0, "no stream was checked");
    }

    fn raw_pdf(objs: &[(u32, &str)], trailer: &str) -> Vec<u8> {
        let mut out = b"%PDF-1.4\n".to_vec();
        for (n, body) in objs {
            out.extend_from_slice(format!("{n} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        out.extend_from_slice(format!("trailer\n<< {trailer} >>\n%%EOF").as_bytes());
        out
    }

    // ---- basic behaviour on our own writer

    #[test]
    fn page_total_counts_pages_from_the_tree() {
        let pdf = text_to_pdf(&lots_of_text("count", 150));
        let total = page_total(&pdf).unwrap();
        assert!(total >= 3);
        assert_eq!(total, os::pdf_page_count(&pdf).unwrap());
        assert_eq!(page_total(&text_to_pdf("one")).unwrap(), 1);
    }

    #[test]
    fn merge_keeps_pages_in_input_order_and_text_searchable() {
        let a = text_to_pdf("alpha first document");
        let b = text_to_pdf(&lots_of_text("bravo", 150));
        let c = text_to_pdf("charlie last document");
        let merged = merge_pdfs(&[a.clone(), b.clone(), c.clone()]).unwrap();
        let expect: Vec<String> = [&a, &b, &c].iter().flat_map(|p| extract_pages(p).unwrap()).collect();
        assert_eq!(extract_pages(&merged).unwrap(), expect);
        assert_eq!(page_total(&merged).unwrap() as usize, expect.len());
        assert_eq!(os::pdf_page_count(&merged).unwrap() as usize, expect.len());
        assert!(merged.starts_with(b"%PDF-1.7"));
        assert!(merged.ends_with(b"%%EOF\n"));
        assert_streams_are_raw_copies(&[a, b, c].concat(), &merged);
    }

    #[test]
    fn merge_of_one_file_is_a_valid_copy() {
        let a = text_to_pdf("only one");
        let merged = merge_pdfs(std::slice::from_ref(&a)).unwrap();
        assert_eq!(extract_pages(&merged).unwrap(), extract_pages(&a).unwrap());
    }

    #[test]
    fn split_yields_one_page_files_in_order_no_bigger_than_the_source() {
        let src = text_to_pdf(&lots_of_text("split", 200));
        let pages = extract_pages(&src).unwrap();
        let parts = split_pdf(&src).unwrap();
        assert_eq!(parts.len(), pages.len());
        assert!(parts.len() >= 3);
        for (i, part) in parts.iter().enumerate() {
            assert_eq!(page_total(part).unwrap(), 1);
            assert_eq!(os::pdf_page_count(part).unwrap(), 1);
            assert_eq!(extract_pages(part).unwrap(), vec![pages[i].clone()], "page {i}");
            assert!(part.len() <= src.len(), "part {i} is {} bytes, source {}", part.len(), src.len());
        }
    }

    #[test]
    fn images_are_not_reencoded() {
        let img = image::DynamicImage::ImageRgb8(image::RgbImage::from_fn(40, 30, |x, y| {
            image::Rgb([x as u8 * 6, y as u8 * 8, 99])
        }));
        let a = images_to_pdf(&[PdfImage::from_image(&img, None).unwrap(), PdfImage::from_image(&img, None).unwrap()]);
        let merged = merge_pdfs(&[a.clone(), a.clone()]).unwrap();
        assert_eq!(page_total(&merged).unwrap(), 4);
        assert_streams_are_raw_copies(&a, &merged);
        for part in split_pdf(&a).unwrap() {
            assert_streams_are_raw_copies(&a, &part);
            assert!(contains(&part, b"/FlateDecode"));
        }
    }

    // ---- real-world files

    #[test]
    fn fixtures_are_the_shapes_they_claim_to_be() {
        assert!(contains(OBJSTM, b"/ObjStm") && contains(OBJSTM, b"/XRef"));
        assert!(!contains(CLASSIC, b"/ObjStm") && contains(CLASSIC, b"\nxref"));
        assert_eq!(INCR.windows(9).filter(|w| *w == b"startxref").count(), 2, "incremental update");
        assert!(contains(INCR, b"/Rotate 90"));
    }

    #[test]
    fn real_files_report_the_same_page_count_as_windows() {
        for (name, pdf) in fixtures() {
            assert_eq!(page_total(pdf).unwrap(), os::pdf_page_count(pdf).unwrap(), "{name}");
        }
    }

    #[test]
    fn real_files_split_with_text_unchanged_and_streams_copied_raw() {
        for (name, pdf) in fixtures() {
            let text = extract_pages(pdf).unwrap();
            let parts = split_pdf(pdf).unwrap();
            assert_eq!(parts.len(), text.len(), "{name}");
            for (i, part) in parts.iter().enumerate() {
                assert_eq!(extract_pages(part).unwrap(), vec![text[i].clone()], "{name} page {i}");
                assert_eq!(os::pdf_page_count(part).unwrap(), 1, "{name} page {i}");
                assert!(part.len() <= pdf.len(), "{name} page {i}: {} > {}", part.len(), pdf.len());
                assert_streams_are_raw_copies(pdf, part);
            }
        }
    }

    #[test]
    fn real_files_merge_in_order_with_text_unchanged() {
        let inputs: Vec<Vec<u8>> = fixtures().iter().map(|(_, p)| p.to_vec()).collect();
        let merged = merge_pdfs(&inputs).unwrap();
        let expect: Vec<String> = inputs.iter().flat_map(|p| extract_pages(p).unwrap()).collect();
        assert_eq!(extract_pages(&merged).unwrap(), expect);
        assert_eq!(page_total(&merged).unwrap() as usize, expect.len());
        assert_eq!(os::pdf_page_count(&merged).unwrap() as usize, expect.len());
        let total_in: usize = inputs.iter().map(Vec::len).sum();
        assert!(merged.len() <= total_in, "{} > {}", merged.len(), total_in);
    }

    #[test]
    fn rendered_pixels_match_the_source_for_split_and_merge() {
        let mut offset = 0;
        let inputs: Vec<Vec<u8>> = fixtures().iter().map(|(_, p)| p.to_vec()).collect();
        let merged = merge_pdfs(&inputs).unwrap();
        for (name, pdf) in fixtures() {
            let parts = split_pdf(pdf).unwrap();
            for (i, part) in parts.iter().enumerate() {
                let original = render(pdf, i as u32);
                let d = mean_diff(&original, &render(part, 0));
                assert!(d < 0.5, "{name} page {i} split differs by {d}");
                let d = mean_diff(&original, &render(&merged, (offset + i) as u32));
                assert!(d < 0.5, "{name} page {i} merged differs by {d}");
            }
            offset += parts.len();
        }
    }

    #[test]
    fn incremental_updates_win_and_rotation_and_highlights_survive() {
        let pages = extract_pages(INCR).unwrap();
        assert!(pages[2].contains("UPDATED-LATER"), "the update is in the source");
        let parts = split_pdf(INCR).unwrap();
        assert!(extract_pages(&parts[2]).unwrap()[0].contains("UPDATED-LATER"));
        // Page 2 is rotated 90 degrees: the render is landscape and the key is in the page dictionary.
        assert!(contains(&parts[1], b"/Rotate 90"));
        let r = render(&parts[1], 0);
        assert!(r.width() > r.height());
        assert!(!contains(&parts[1], b"/Link"));
        assert!(contains(&parts[0], b"/Highlight"), "a self-contained annotation is kept");
    }

    #[test]
    fn nothing_leaks_from_info_outlines_links_or_destinations() {
        let inputs: Vec<Vec<u8>> = fixtures().iter().map(|(_, p)| p.to_vec()).collect();
        let mut outs = vec![merge_pdfs(&inputs).unwrap()];
        outs.extend(split_pdf(OBJSTM).unwrap());
        outs.extend(split_pdf(CLASSIC).unwrap());
        for out in outs {
            for bad in [
                &b"SECRET"[..],
                b"/Info",
                b"/Outlines",
                b"/Metadata",
                b"/Names",
                b"/Dests",
                b"/StructTreeRoot",
                b"/AcroForm",
                b"/PageLabels",
                b"/Link",
                b"/GoTo",
                b"/Dest ",
                b"/Producer",
                b"/Author",
                b"/Title",
                b"/ObjStm",
                b"/XRef",
            ] {
                assert!(!contains(&out, bad), "{} found in output", String::from_utf8_lossy(bad));
            }
            assert!(!contains(&out, b"/Encrypt"));
        }
    }

    #[test]
    fn splitting_shares_nothing_between_outputs() {
        let parts = split_pdf(CLASSIC).unwrap();
        for part in &parts {
            // Only the objects one page reaches: catalog, tree, page, contents, resources, fonts... never the
            // other pages' content.
            let doc = Doc::open(part, false).unwrap();
            let list = doc.pages().unwrap();
            assert_eq!(list.pages.len(), 1);
            assert!(doc.xref.len() < 20, "{} objects", doc.xref.len());
        }
        let own: Vec<String> = parts.iter().map(|p| extract_pages(p).unwrap().remove(0)).collect();
        for (i, t) in own.iter().enumerate() {
            for (j, u) in own.iter().enumerate() {
                if i != j {
                    assert_ne!(t, u);
                }
            }
        }
    }

    // ---- structure handling

    #[test]
    fn inherited_attributes_are_pushed_onto_each_page() {
        let pdf = raw_pdf(
            &[
                (1, "<< /Type /Catalog /Pages 2 0 R >>"),
                (
                    2,
                    "<< /Type /Pages /Kids [3 0 R 4 0 R] /Count 2 /Resources << /Font << /F1 5 0 R >> >> /MediaBox [0 0 200 100] /Rotate 90 /CropBox [0 0 150 100] >>",
                ),
                (3, "<< /Type /Page /Parent 2 0 R /Contents 6 0 R >>"),
                (4, "<< /Type /Page /Parent 2 0 R /Contents 6 0 R /Rotate 0 >>"),
                (5, "<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"),
                (6, "<< /Length 40 >>\nstream\nBT /F1 12 Tf 10 10 Td (inherit) Tj ET\nendstream"),
            ],
            "/Root 1 0 R",
        );
        let parts = split_pdf(&pdf).unwrap();
        assert_eq!(parts.len(), 2);
        let (a, b) = (render(&parts[0], 0), render(&parts[1], 0));
        assert!(a.width() < a.height(), "rotated 90: portrait");
        assert!(b.width() > b.height(), "own /Rotate 0 wins: landscape");
        assert_eq!(extract_pages(&parts[0]).unwrap(), vec!["inherit".to_string()]);
        for p in &parts {
            assert!(contains(p, b"/MediaBox [0 0 200 100]") && contains(p, b"/Resources") && contains(p, b"/CropBox"));
            assert!(!contains(p, b"/Kids [3 0 R 4 0 R") && !contains(p, b"/Count 2"));
        }
    }

    #[test]
    fn page_tree_cycles_and_reference_loops_terminate() {
        let pdf = raw_pdf(
            &[
                (1, "<< /Type /Catalog /Pages 2 0 R >>"),
                (2, "<< /Type /Pages /Kids [3 0 R 2 0 R 4 0 R] /Count 2 >>"),
                (3, "<< /Type /Page /Parent 2 0 R /Resources 5 0 R /Contents 6 0 R >>"),
                (4, "<< /Type /Pages /Kids [2 0 R] /Count 0 >>"),
                (5, "<< /Self 5 0 R /Other 7 0 R /Font << /F1 7 0 R >> >>"),
                (6, "<< /Length 4 >>\nstream\n q Q\nendstream"),
                (7, "<< /Self 5 0 R /Parent 2 0 R /Type /Font /Subtype /Type1 /BaseFont /Helvetica >>"),
            ],
            "/Root 1 0 R",
        );
        assert_eq!(page_total(&pdf).unwrap(), 1);
        let out = merge_pdfs(&[pdf.clone(), pdf]).unwrap();
        assert_eq!(page_total(&out).unwrap(), 2);
        assert!(!contains(&out, b"/Kids [3 0 R 2 0 R"), "the original tree is not copied through a back reference");
    }

    #[test]
    fn exponential_page_tree_fan_out_is_refused_not_expanded() {
        let mut objs: Vec<(u32, String)> = vec![(1, "<< /Type /Catalog /Pages 2 0 R >>".into())];
        for i in 0..40u32 {
            let next = i + 3;
            objs.push((i + 2, format!("<< /Type /Pages /Kids [{next} 0 R {next} 0 R] /Count 0 >>")));
        }
        objs.push((42, "<< /Type /Page /Parent 41 0 R >>".into()));
        let refs: Vec<(u32, &str)> = objs.iter().map(|(n, s)| (*n, s.as_str())).collect();
        let pdf = raw_pdf(&refs, "/Root 1 0 R");
        // Intermediate nodes are visited once, so the fan-out never multiplies.
        assert!(page_total(&pdf).unwrap() <= 2);
    }

    #[test]
    fn classic_xref_free_objects_and_missing_references_are_tolerated() {
        let mut pdf = b"%PDF-1.4\n".to_vec();
        let mut offs = vec![];
        for (n, body) in [
            (1, "<< /Type /Catalog /Pages 2 0 R >>"),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            (
                3,
                "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Resources << /Font << /F1 9 0 R >> /XObject << /X 4 0 R >> >> >>",
            ),
        ] {
            offs.push(pdf.len());
            pdf.extend_from_slice(format!("{n} 0 obj\n{body}\nendobj\n").as_bytes());
        }
        let xref = pdf.len();
        pdf.extend_from_slice(b"xref\n0 5\n0000000000 65535 f \n");
        for o in &offs {
            pdf.extend_from_slice(format!("{o:010} 00000 n \n").as_bytes());
        }
        pdf.extend_from_slice(b"0000000000 00001 f \n");
        pdf.extend_from_slice(format!("trailer\n<< /Size 5 /Root 1 0 R >>\nstartxref\n{xref}\n%%EOF").as_bytes());
        let parts = split_pdf(&pdf).unwrap();
        assert_eq!(parts.len(), 1);
        assert!(!contains(&parts[0], b"9 0 R") && !contains(&parts[0], b"4 0 R"), "dangling refs become null");
        assert_eq!(os::pdf_page_count(&parts[0]).unwrap(), 1);
    }

    #[test]
    fn a_damaged_or_missing_xref_is_rebuilt() {
        for (name, pdf) in fixtures() {
            let expect = extract_pages(pdf).unwrap();
            // Point startxref into nowhere.
            let mut broken = pdf.to_vec();
            let at = rfind(&broken, b"startxref").unwrap();
            broken.truncate(at);
            broken.extend_from_slice(b"startxref\n7\n%%EOF\n");
            let merged = merge_pdfs(&[broken.clone()]).unwrap();
            assert_eq!(extract_pages(&merged).unwrap(), expect, "{name}: bad startxref");
            // Cut the whole tail: no xref, no trailer.
            if name == "classic" {
                let cut = pdf[..rfind(pdf, b"\nxref").unwrap()].to_vec();
                assert_eq!(split_pdf(&cut).unwrap().len(), expect.len(), "{name}: no xref at all");
            }
            // Offsets that are all off by a few bytes.
            let shifted = [b"\n\n\n".as_slice(), pdf].concat();
            assert_eq!(page_total(&shifted).unwrap() as usize, expect.len(), "{name}: shifted offsets");
        }
    }

    #[test]
    fn hybrid_files_use_their_xref_stream_for_hidden_objects() {
        // Objects 3 and 4 live only in the xref stream; the classic table lists them as free.
        let mut pdf = b"%PDF-1.5\n".to_vec();
        let bodies = [
            (1, "<< /Type /Catalog /Pages 2 0 R >>"),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            (3, "<< /Type /Page /Parent 2 0 R /MediaBox [0 0 100 100] /Contents 4 0 R >>"),
        ];
        let mut offs = vec![];
        for (n, b) in bodies {
            offs.push(pdf.len());
            pdf.extend_from_slice(format!("{n} 0 obj\n{b}\nendobj\n").as_bytes());
        }
        let o4 = pdf.len();
        pdf.extend_from_slice(b"4 0 obj\n<< /Length 4 >>\nstream\n q Q\nendstream\nendobj\n");
        // xref stream (uncompressed) covering objects 3 and 4, object number 5.
        let mut rows = Vec::new();
        for off in [offs[2], o4] {
            rows.push(1u8);
            rows.extend_from_slice(&(off as u32).to_be_bytes());
        }
        let xs = pdf.len();
        pdf.extend_from_slice(
            format!("5 0 obj\n<< /Type /XRef /Size 6 /W [1 4 0] /Index [3 2] /Length {} >>\nstream\n", rows.len())
                .as_bytes(),
        );
        pdf.extend_from_slice(&rows);
        pdf.extend_from_slice(b"\nendstream\nendobj\n");
        let xref = pdf.len();
        pdf.extend_from_slice(
            format!("xref\n0 3\n0000000000 65535 f \n{:010} 00000 n \n{:010} 00000 n \n", offs[0], offs[1]).as_bytes(),
        );
        pdf.extend_from_slice(
            format!("trailer\n<< /Size 6 /Root 1 0 R /XRefStm {xs} >>\nstartxref\n{xref}\n%%EOF").as_bytes(),
        );
        let doc = Doc::open(&pdf, false).unwrap();
        assert!(!doc.rebuilt, "read through the hybrid chain, not by scanning");
        assert_eq!(page_total(&pdf).unwrap(), 1);
    }

    #[test]
    fn png_predictors_decode_xref_and_object_streams() {
        // Up predictor on a 3-column table: rows are deltas against the row above.
        let rows: [[u8; 3]; 3] = [[1, 0, 10], [1, 0, 20], [1, 0, 30]];
        let mut enc = Vec::new();
        let mut prev = [0u8; 3];
        for r in rows {
            enc.push(2);
            for i in 0..3 {
                enc.push(r[i].wrapping_sub(prev[i]));
            }
            prev = r;
        }
        let mut parms = Dict::new();
        parms.insert(key("Predictor"), O::Int(12));
        parms.insert(key("Columns"), O::Int(3));
        let dec = unpredict(enc, &parms).unwrap();
        assert_eq!(dec, vec![1, 0, 10, 1, 0, 20, 1, 0, 30]);
        parms.insert(key("Predictor"), O::Int(99));
        assert!(unpredict(vec![0; 8], &parms).is_err());
    }

    #[test]
    fn object_streams_with_predictors_are_read_from_real_files() {
        // MuPDF writes /Predictor 12 xref streams; reading them without the rebuild fallback proves they decode.
        let doc = Doc::open(OBJSTM, false).unwrap();
        assert!(!doc.rebuilt);
        assert!(doc.xref.values().any(|e| matches!(e, Entry::InStm(..))));
        assert_eq!(doc.pages().unwrap().pages.len(), 4);
    }

    // ---- errors and limits

    #[test]
    fn encrypted_pdfs_are_refused_with_a_clear_message() {
        let pdf = raw_pdf(
            &[
                (1, "<< /Type /Catalog /Pages 2 0 R >>"),
                (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
                (3, "<< /Type /Page /Parent 2 0 R >>"),
                (4, "<< /Filter /Standard /V 1 /R 2 /O (x) /U (y) /P -4 >>"),
            ],
            "/Root 1 0 R /Encrypt 4 0 R",
        );
        for r in [
            page_total(&pdf).map(|_| ()),
            split_pdf(&pdf).map(|_| ()),
            merge_pdfs(&[text_to_pdf("ok"), pdf]).map(|_| ()),
        ] {
            match r {
                Err(Error::Unsupported(m)) => assert_eq!(m, "password-protected PDF"),
                other => panic!("expected Unsupported, got {other:?}"),
            }
        }
    }

    #[test]
    fn empty_input_garbage_and_pageless_files_are_rejected() {
        assert!(matches!(merge_pdfs(&[]), Err(Error::InvalidArgument(m)) if m.contains("no PDF")));
        for bad in [&b""[..], b"hello", b"%PDF-1.4 garbage only", &[0u8; 100]] {
            assert!(matches!(page_total(bad), Err(Error::Corrupt(_))), "{:?}", String::from_utf8_lossy(bad));
            assert!(matches!(split_pdf(bad), Err(Error::Corrupt(_))));
            assert!(matches!(merge_pdfs(&[text_to_pdf("ok"), bad.to_vec()]), Err(Error::Corrupt(_))));
        }
        let none = raw_pdf(
            &[(1, "<< /Type /Catalog /Pages 2 0 R >>"), (2, "<< /Type /Pages /Kids [] /Count 0 >>")],
            "/Root 1 0 R",
        );
        assert!(matches!(page_total(&none), Err(Error::InvalidArgument(m)) if m.contains("no pages")));
        assert!(matches!(split_pdf(&none), Err(Error::InvalidArgument(_))));
        assert!(matches!(merge_pdfs(&[none]), Err(Error::InvalidArgument(_))));
    }

    fn pdf_with_pages(n: u32) -> Vec<u8> {
        let kids: String = (0..n).map(|i| format!("{} 0 R ", i + 3)).collect();
        let mut o: Vec<(u32, String)> = vec![
            (1, "<< /Type /Catalog /Pages 2 0 R >>".into()),
            (2, format!("<< /Type /Pages /Kids [{kids}] /Count {n} >>")),
        ];
        (0..n).for_each(|i| o.push((i + 3, "<< /Type /Page /Parent 2 0 R >>".into())));
        let r: Vec<(u32, &str)> = o.iter().map(|(a, b)| (*a, b.as_str())).collect();
        raw_pdf(&r, "/Root 1 0 R")
    }

    #[test]
    fn more_than_twenty_thousand_pages_is_unsupported() {
        let pdf = pdf_with_pages(20_001);
        assert!(matches!(page_total(&pdf), Err(Error::Unsupported(m)) if m.contains("20000")));
        assert!(matches!(split_pdf(&pdf), Err(Error::Unsupported(_))));
        assert_eq!(page_total(&pdf_with_pages(20_000)).unwrap(), 20_000);
        // The cap counts pages across all inputs of a merge.
        let ten_k = pdf_with_pages(10_000);
        assert!(matches!(merge_pdfs(&[ten_k.clone(), ten_k.clone(), ten_k]), Err(Error::Unsupported(_))));
    }

    #[test]
    fn truncation_and_bit_flips_never_panic() {
        let mut seed = 0x9E37_79B9_7F4A_7C15u64;
        let mut rnd = move || {
            seed ^= seed << 13;
            seed ^= seed >> 7;
            seed ^= seed << 17;
            seed
        };
        let mut sources: Vec<Vec<u8>> = fixtures().iter().map(|(_, p)| p.to_vec()).collect();
        sources.push(text_to_pdf(&lots_of_text("fuzz", 120)));
        let (mut ok, mut err) = (0, 0);
        let rounds: usize = std::env::var("PDFTOOLS_FUZZ_ROUNDS").ok().and_then(|v| v.parse().ok()).unwrap_or(60);
        for src in &sources {
            for k in 0..rounds {
                let cut = (rnd() as usize) % (src.len() + 1);
                let mut v = src[..cut].to_vec();
                if k % 2 == 1 {
                    for _ in 0..(1 + rnd() % 12) {
                        if !v.is_empty() {
                            let i = (rnd() as usize) % v.len();
                            v[i] ^= 1 << (rnd() % 8);
                        }
                    }
                }
                let mut v2 = src.clone();
                for _ in 0..(1 + rnd() % 30) {
                    let i = (rnd() as usize) % v2.len();
                    v2[i] = rnd() as u8;
                }
                for input in [v, v2] {
                    for r in [
                        page_total(&input).map(|_| ()),
                        split_pdf(&input).map(|_| ()),
                        merge_pdfs(&[input.clone(), src.clone()]).map(|m| {
                            // Whatever comes out must itself be a readable PDF.
                            assert!(page_total(&m).unwrap() >= 1);
                        }),
                    ] {
                        match r {
                            Ok(()) => ok += 1,
                            Err(_) => err += 1,
                        }
                    }
                }
            }
        }
        eprintln!("fuzz: {ok} ok, {err} errors");
        assert!(ok > 0 && err > 0);
    }

    // ---- performance (run with: cargo test --release perf -- --ignored --nocapture)

    #[test]
    #[ignore]
    fn perf_merge_twenty_by_fifty_and_split_five_hundred() {
        use std::time::Instant;
        let doc50 = |i: usize| text_to_pdf(&lots_of_text(&format!("doc{i}"), 50 * 48));
        let inputs: Vec<Vec<u8>> = (0..20).map(doc50).collect();
        let pages: u32 = inputs.iter().map(|p| page_total(p).unwrap()).sum();
        let bytes: usize = inputs.iter().map(Vec::len).sum();
        let t = Instant::now();
        let merged = merge_pdfs(&inputs).unwrap();
        eprintln!(
            "PERF merge: {} files, {pages} pages, {} KB in -> {} KB out in {:?}",
            inputs.len(),
            bytes / 1024,
            merged.len() / 1024,
            t.elapsed()
        );
        assert_eq!(page_total(&merged).unwrap(), pages);
        let t = Instant::now();
        let parts = split_pdf(&merged).unwrap();
        eprintln!(
            "PERF split: {} pages -> {} files, {} KB total in {:?}",
            pages,
            parts.len(),
            parts.iter().map(Vec::len).sum::<usize>() / 1024,
            t.elapsed()
        );
        let big = merge_pdfs(&inputs[..10]).unwrap();
        let t = Instant::now();
        let n = page_total(&big).unwrap();
        eprintln!("PERF page_total {n} pages {:?}", t.elapsed());
        // Real files with fonts and images, 500+ pages.
        let many: Vec<Vec<u8>> = (0..130).map(|i| fixtures()[i % 4].1.to_vec()).collect();
        let t = Instant::now();
        let m = merge_pdfs(&many).unwrap();
        eprintln!(
            "PERF real merge: {} files -> {} pages, {} KB in {:?}",
            many.len(),
            page_total(&m).unwrap(),
            m.len() / 1024,
            t.elapsed()
        );
        let t = Instant::now();
        let p = split_pdf(&m).unwrap();
        eprintln!(
            "PERF real split: {} files, {} KB total in {:?}",
            p.len(),
            p.iter().map(Vec::len).sum::<usize>() / 1024,
            t.elapsed()
        );
        if let Ok(path) = std::env::var("PDFTOOLS_DUMP") {
            std::fs::write(format!("{path}/merged20x50.pdf"), &merged).unwrap();
            std::fs::write(format!("{path}/real_merged.pdf"), &m).unwrap();
            for (name, pdf) in fixtures() {
                std::fs::write(format!("{path}/fx_{name}_merge1.pdf"), merge_pdfs(&[pdf.to_vec()]).unwrap()).unwrap();
                for (k, x) in split_pdf(pdf).unwrap().iter().enumerate() {
                    std::fs::write(format!("{path}/fx_{name}_split_{k}.pdf"), x).unwrap();
                }
            }
            let all: Vec<Vec<u8>> = fixtures().iter().map(|(_, p)| p.to_vec()).collect();
            std::fs::write(format!("{path}/fx_all_merged.pdf"), merge_pdfs(&all).unwrap()).unwrap();
        }
    }
}
