//! Text extraction from PDF files: a small object parser plus a content-stream interpreter.
//!
//! Covers what most generated PDFs use: classic and compressed (object stream) files, FlateDecode and
//! ASCIIHex filters, simple fonts with Latin text and any font with a `ToUnicode` map. Scanned
//! pages, encrypted files and fonts without a `ToUnicode` map and a non-Latin encoding yield no text.

use super::error::{Error, Result};
use std::cell::RefCell;
use std::collections::{HashMap, HashSet};
use std::rc::Rc;

/// Inflated stream size cap, against decompression bombs.
const MAX_STREAM: usize = 256 * 1024 * 1024;
const MAX_DEPTH: usize = 48;

type Dict = HashMap<String, Obj>;

#[derive(Clone, Debug)]
enum Obj {
    Null,
    Bool(bool),
    Int(i64),
    Real(f64),
    Name(String),
    Str(Vec<u8>),
    Arr(Vec<Obj>),
    Dict(Dict),
    Ref(u32),
    /// Dictionary plus the byte range of the raw (still encoded) data in the source buffer.
    Stream(Dict, usize, usize),
}

impl Obj {
    fn num(&self) -> Option<f64> {
        match self {
            Obj::Int(i) => Some(*i as f64),
            Obj::Real(r) => Some(*r),
            _ => None,
        }
    }
    fn int(&self) -> Option<i64> {
        match self {
            Obj::Int(i) => Some(*i),
            Obj::Real(r) => Some(*r as i64),
            _ => None,
        }
    }
    fn name(&self) -> Option<&str> {
        match self {
            Obj::Name(n) => Some(n),
            _ => None,
        }
    }
    fn dict(&self) -> Option<&Dict> {
        match self {
            Obj::Dict(d) | Obj::Stream(d, ..) => Some(d),
            _ => None,
        }
    }
}

enum Tok {
    Obj(Obj),
    Op(String),
}

struct Lexer<'a> {
    d: &'a [u8],
    p: usize,
}

fn is_ws(b: u8) -> bool {
    matches!(b, 0 | 9 | 10 | 12 | 13 | 32)
}
fn is_delim(b: u8) -> bool {
    matches!(b, b'(' | b')' | b'<' | b'>' | b'[' | b']' | b'{' | b'}' | b'/' | b'%')
}

impl<'a> Lexer<'a> {
    fn new(d: &'a [u8], p: usize) -> Self {
        Lexer { d, p }
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
        while self.peek().is_some_and(|b| !is_ws(b) && !is_delim(b)) {
            self.p += 1;
        }
        &self.d[s..self.p]
    }

    /// Next token. `refs` enables `N G R` references (not valid in content streams).
    fn token(&mut self, refs: bool, depth: usize) -> Option<Tok> {
        if depth > MAX_DEPTH {
            return None;
        }
        self.skip_ws();
        let b = self.peek()?;
        Some(Tok::Obj(match b {
            b'/' => {
                self.p += 1;
                Obj::Name(unescape_name(self.word()))
            }
            b'(' => Obj::Str(self.literal_string()),
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
                            let key = unescape_name(self.word());
                            match self.token(refs, depth + 1)? {
                                Tok::Obj(v) => {
                                    dict.insert(key, v);
                                }
                                Tok::Op(_) => return None,
                            }
                        }
                        _ => return None,
                    }
                }
                Obj::Dict(dict)
            }
            b'<' => {
                self.p += 1;
                let mut hex = Vec::new();
                while let Some(c) = self.peek() {
                    self.p += 1;
                    if c == b'>' {
                        break;
                    }
                    if let Some(v) = (c as char).to_digit(16) {
                        hex.push(v as u8);
                    }
                }
                Obj::Str(hex.chunks(2).map(|c| (c[0] << 4) | c.get(1).copied().unwrap_or(0)).collect())
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
                    match self.token(refs, depth + 1)? {
                        Tok::Obj(o) => v.push(o),
                        Tok::Op(_) => {} // stray operator inside an array: ignore
                    }
                }
                Obj::Arr(v)
            }
            b')' | b']' | b'>' | b'{' | b'}' => {
                self.p += 1;
                return Some(Tok::Op((b as char).to_string()));
            }
            _ => {
                let w = self.word();
                if w.is_empty() {
                    self.p += 1;
                    return Some(Tok::Op(String::new()));
                }
                match w {
                    b"true" => Obj::Bool(true),
                    b"false" => Obj::Bool(false),
                    b"null" => Obj::Null,
                    _ => {
                        let s = std::str::from_utf8(w).unwrap_or("");
                        if let Ok(i) = s.parse::<i64>() {
                            if refs && i >= 0 {
                                if let Some(r) = self.try_ref(i) {
                                    return Some(Tok::Obj(r));
                                }
                            }
                            Obj::Int(i)
                        } else if let Ok(f) = s.parse::<f64>() {
                            Obj::Real(f)
                        } else {
                            return Some(Tok::Op(s.to_string()));
                        }
                    }
                }
            }
        }))
    }

    fn try_ref(&mut self, num: i64) -> Option<Obj> {
        let save = self.p;
        self.skip_ws();
        let gen = self.word();
        if !gen.is_empty() && gen.iter().all(u8::is_ascii_digit) {
            self.skip_ws();
            if self.peek() == Some(b'R') && self.d.get(self.p + 1).is_none_or(|&c| is_ws(c) || is_delim(c)) {
                self.p += 1;
                return u32::try_from(num).ok().map(Obj::Ref);
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

fn unescape_name(raw: &[u8]) -> String {
    let mut out = Vec::with_capacity(raw.len());
    let mut i = 0;
    while i < raw.len() {
        if raw[i] == b'#' && i + 2 < raw.len() {
            if let Ok(v) = u8::from_str_radix(std::str::from_utf8(&raw[i + 1..i + 3]).unwrap_or("zz"), 16) {
                out.push(v);
                i += 3;
                continue;
            }
        }
        out.push(raw[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

enum Loc {
    Direct(usize),
    /// (decoded object stream index, byte offset inside it)
    Packed(usize, usize),
}

struct Pdf<'a> {
    data: &'a [u8],
    index: HashMap<u32, Loc>,
    packed: Vec<Vec<u8>>,
    cache: RefCell<HashMap<u32, Obj>>,
}

fn find(hay: &[u8], needle: &[u8], from: usize) -> Option<usize> {
    if from >= hay.len() || needle.is_empty() {
        return None;
    }
    hay[from..].windows(needle.len()).position(|w| w == needle).map(|i| i + from)
}

impl<'a> Pdf<'a> {
    fn open(data: &'a [u8]) -> Result<Pdf<'a>> {
        let mut pdf = Pdf { data, index: HashMap::new(), packed: Vec::new(), cache: RefCell::new(HashMap::new()) };
        // Scan for "N G obj": tolerant of broken or missing xref tables, and the last definition wins.
        let mut at = 0;
        while let Some(i) = find(data, b"obj", at) {
            at = i + 3;
            if i == 0 || !is_ws(data[i - 1]) {
                continue;
            }
            let mut j = i - 1;
            while j > 0 && is_ws(data[j]) {
                j -= 1;
            }
            let gen_end = j + 1;
            while j > 0 && data[j].is_ascii_digit() {
                j -= 1;
            }
            if gen_end == j + 1 || !is_ws(data[j]) {
                continue;
            }
            while j > 0 && is_ws(data[j]) {
                j -= 1;
            }
            let num_end = j + 1;
            while j > 0 && data[j].is_ascii_digit() {
                j -= 1;
            }
            let num_start = if data[j].is_ascii_digit() { j } else { j + 1 };
            if num_start >= num_end {
                continue;
            }
            if let Some(n) = std::str::from_utf8(&data[num_start..num_end]).ok().and_then(|s| s.parse::<u32>().ok()) {
                pdf.index.insert(n, Loc::Direct(i + 3));
            }
        }
        if pdf.index.is_empty() {
            return Err(Error::Corrupt("PDF: no objects found".into()));
        }
        if find(data, b"/Encrypt", 0).is_some() {
            let enc_ref = find(data, b"/Encrypt", 0).map(|i| &data[i + 8..(i + 24).min(data.len())]);
            if enc_ref.is_some_and(|s| s.iter().any(u8::is_ascii_digit)) {
                return Err(Error::Unsupported("encrypted PDF".into()));
            }
        }
        pdf.load_object_streams();
        Ok(pdf)
    }

    fn load_object_streams(&mut self) {
        let candidates: Vec<u32> = self
            .index
            .iter()
            .filter_map(|(n, loc)| match loc {
                Loc::Direct(off) => {
                    let head = &self.data[*off..(*off + 300).min(self.data.len())];
                    find(head, b"/ObjStm", 0).map(|_| *n)
                }
                _ => None,
            })
            .collect();
        let mut sorted = candidates;
        sorted.sort_unstable();
        for n in sorted {
            let Some(obj) = self.object(n) else { continue };
            let (Some(dict), Some(bytes)) = (obj.dict(), self.stream_data(&obj)) else { continue };
            if dict.get("Type").and_then(Obj::name) != Some("ObjStm") {
                continue;
            }
            let count = dict.get("N").and_then(Obj::int).unwrap_or(0).max(0) as usize;
            let first = dict.get("First").and_then(Obj::int).unwrap_or(0).max(0) as usize;
            let mut lx = Lexer::new(&bytes, 0);
            let mut pairs = Vec::new();
            for _ in 0..count.min(1_000_000) {
                let (Some(Tok::Obj(a)), Some(Tok::Obj(b))) = (lx.token(false, 0), lx.token(false, 0)) else { break };
                if let (Some(a), Some(b)) = (a.int(), b.int()) {
                    pairs.push((a as u32, first + b as usize));
                }
            }
            let slot = self.packed.len();
            self.packed.push(bytes);
            for (num, off) in pairs {
                self.index.entry(num).or_insert(Loc::Packed(slot, off));
            }
        }
    }

    fn object(&self, num: u32) -> Option<Obj> {
        if let Some(o) = self.cache.borrow().get(&num) {
            return Some(o.clone());
        }
        let obj = match self.index.get(&num)? {
            Loc::Direct(off) => {
                let mut lx = Lexer::new(self.data, *off);
                let Some(Tok::Obj(o)) = lx.token(true, 0) else { return None };
                lx.skip_ws();
                match (&o, lx.word()) {
                    (Obj::Dict(d), b"stream") => {
                        let mut s = lx.p;
                        if self.data.get(s) == Some(&b'\r') {
                            s += 1;
                        }
                        if self.data.get(s) == Some(&b'\n') {
                            s += 1;
                        }
                        let end = self.stream_end(d, s);
                        Obj::Stream(d.clone(), s, end)
                    }
                    _ => o,
                }
            }
            Loc::Packed(slot, off) => {
                let mut lx = Lexer::new(&self.packed[*slot], *off);
                let Some(Tok::Obj(o)) = lx.token(true, 0) else { return None };
                o
            }
        };
        self.cache.borrow_mut().insert(num, obj.clone());
        Some(obj)
    }

    fn stream_end(&self, d: &Dict, start: usize) -> usize {
        if let Some(len) = d.get("Length").and_then(|l| self.resolve(l).int()) {
            let end = start.saturating_add(len.max(0) as usize);
            if end <= self.data.len() {
                let mut lx = Lexer::new(self.data, end);
                lx.skip_ws();
                if self.data[lx.p..].starts_with(b"endstream") {
                    return end;
                }
            }
        }
        find(self.data, b"endstream", start).map_or(self.data.len(), |e| {
            // Drop the end-of-line that precedes the keyword.
            let mut e = e;
            if e > start && self.data[e - 1] == b'\n' {
                e -= 1;
            }
            if e > start && self.data[e - 1] == b'\r' {
                e -= 1;
            }
            e
        })
    }

    fn resolve(&self, o: &Obj) -> Obj {
        let mut cur = o.clone();
        for _ in 0..16 {
            match cur {
                Obj::Ref(n) => cur = self.object(n).unwrap_or(Obj::Null),
                other => return other,
            }
        }
        Obj::Null
    }

    fn get(&self, d: &Dict, key: &str) -> Obj {
        d.get(key).map(|o| self.resolve(o)).unwrap_or(Obj::Null)
    }

    /// Decoded bytes of a stream object; `None` for unsupported filters.
    fn stream_data(&self, o: &Obj) -> Option<Vec<u8>> {
        use std::io::Read;
        let Obj::Stream(d, s, e) = o else { return None };
        let mut data = self.data[*s..*e].to_vec();
        let filters: Vec<String> = match self.get(d, "Filter") {
            Obj::Name(n) => vec![n],
            Obj::Arr(a) => a.iter().filter_map(|f| self.resolve(f).name().map(str::to_string)).collect(),
            _ => vec![],
        };
        for f in filters {
            data = match f.as_str() {
                "FlateDecode" | "Fl" => {
                    let mut out = Vec::new();
                    let mut dec = flate2::read::ZlibDecoder::new(&data[..]).take(MAX_STREAM as u64);
                    // A damaged tail still leaves the text before it usable.
                    let _ = dec.read_to_end(&mut out);
                    if out.is_empty() && !data.is_empty() {
                        return None;
                    }
                    out
                }
                "ASCIIHexDecode" | "AHx" => {
                    let hex: Vec<u8> = data
                        .iter()
                        .take_while(|&&b| b != b'>')
                        .filter_map(|&b| (b as char).to_digit(16).map(|v| v as u8))
                        .collect();
                    hex.chunks(2).map(|c| (c[0] << 4) | c.get(1).copied().unwrap_or(0)).collect()
                }
                _ => return None,
            };
        }
        Some(data)
    }

    fn root_pages(&self) -> Option<Obj> {
        let mut nums: Vec<u32> = self.index.keys().copied().collect();
        nums.sort_unstable();
        for n in nums {
            if let Some(Obj::Dict(d)) = self.object(n) {
                if d.get("Type").and_then(Obj::name) == Some("Catalog") {
                    return d.get("Pages").cloned();
                }
            }
        }
        None
    }

    /// Depth-first walk of the page tree. Each referenced node is visited once, so cycles end.
    fn collect_pages(
        &self,
        node: &Obj,
        inherited: &Dict,
        out: &mut Vec<(Dict, Dict)>,
        seen: &mut HashSet<u32>,
        depth: usize,
    ) {
        if depth > 32 || out.len() > 100_000 {
            return;
        }
        if let Obj::Ref(n) = node {
            if !seen.insert(*n) {
                return;
            }
        }
        let resolved = self.resolve(node);
        let Some(d) = resolved.dict() else { return };
        let mut inh = inherited.clone();
        for k in ["Resources", "MediaBox"] {
            if let Some(v) = d.get(k) {
                inh.insert(k.to_string(), v.clone());
            }
        }
        match d.get("Kids").map(|k| self.resolve(k)) {
            Some(Obj::Arr(kids)) => {
                for k in &kids {
                    self.collect_pages(k, &inh, out, seen, depth + 1);
                }
            }
            _ => out.push((d.clone(), inh)),
        }
    }
}

// ------------------------------------------------------------------ fonts

#[derive(Default)]
struct CMap {
    map: HashMap<u32, String>,
    code_len: usize,
}

fn utf16be(bytes: &[u8]) -> String {
    let units: Vec<u16> =
        bytes.chunks(2).map(|c| ((c[0] as u16) << 8) | c.get(1).copied().unwrap_or(0) as u16).collect();
    String::from_utf16_lossy(&units)
}

fn hex_bytes(tok: &str) -> Vec<u8> {
    let digits: Vec<u8> = tok.chars().filter_map(|c| c.to_digit(16).map(|v| v as u8)).collect();
    digits.chunks(2).map(|c| (c[0] << 4) | c.get(1).copied().unwrap_or(0)).collect()
}

fn be_int(b: &[u8]) -> u32 {
    b.iter().fold(0u32, |a, &x| (a << 8) | x as u32)
}

fn parse_cmap(src: &[u8]) -> CMap {
    let text = String::from_utf8_lossy(src);
    let toks: Vec<&str> = text.split_whitespace().collect();
    let mut cm = CMap::default();
    let mut i = 0;
    while i < toks.len() {
        match toks[i] {
            "begincodespacerange" => {
                i += 1;
                while i + 1 < toks.len() && toks[i] != "endcodespacerange" {
                    let n = hex_bytes(toks[i]).len();
                    cm.code_len = cm.code_len.max(n);
                    i += 2;
                }
            }
            "beginbfchar" => {
                i += 1;
                while i + 1 < toks.len() && toks[i] != "endbfchar" {
                    cm.map.insert(be_int(&hex_bytes(toks[i])), utf16be(&hex_bytes(toks[i + 1])));
                    i += 2;
                }
            }
            "beginbfrange" => {
                i += 1;
                while i + 2 < toks.len() && toks[i] != "endbfrange" {
                    let (lo, hi) = (be_int(&hex_bytes(toks[i])), be_int(&hex_bytes(toks[i + 1])));
                    if hi < lo || hi - lo > 0xFFFF {
                        i += 3;
                        continue;
                    }
                    if toks[i + 2].starts_with('[') {
                        // [<d1> <d2> ...] form: tokens may be glued to the brackets.
                        let mut k = i + 2;
                        let mut code = lo;
                        loop {
                            let t = toks[k];
                            let closes = t.ends_with(']');
                            let inner = t.trim_start_matches('[').trim_end_matches(']');
                            if !inner.is_empty() {
                                cm.map.insert(code, utf16be(&hex_bytes(inner)));
                                code += 1;
                            }
                            k += 1;
                            if closes || k >= toks.len() {
                                break;
                            }
                        }
                        i = k;
                    } else {
                        let dst = utf16be(&hex_bytes(toks[i + 2]));
                        let mut chars: Vec<char> = dst.chars().collect();
                        for code in lo..=hi {
                            cm.map.insert(code, chars.iter().collect());
                            if let Some(last) = chars.last_mut() {
                                *last = char::from_u32(*last as u32 + 1).unwrap_or(*last);
                            }
                        }
                        i += 3;
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    cm
}

struct Font {
    cmap: Option<CMap>,
    composite: bool,
    /// Glyph advances in 1/1000 em, keyed by character code (simple fonts) or CID (composite fonts, Identity-H).
    widths: HashMap<u32, f64>,
    /// Advance for codes missing from `widths`; `None` means "unknown", which disables gap-based spacing.
    default_width: Option<f64>,
}

/// Windows-1252 decoding for simple fonts without a `ToUnicode` map.
fn winansi_char(b: u8) -> Option<char> {
    Some(match b {
        0x20..=0x7E | 0xA0..=0xFF => b as char,
        0x80 => '€',
        0x85 => '…',
        0x91 => '‘',
        0x92 => '’',
        0x93 => '“',
        0x94 => '”',
        0x95 => '•',
        0x96 => '–',
        0x97 => '—',
        0x99 => '™',
        _ => return None,
    })
}

const MAX_WIDTH_ENTRIES: usize = 70_000;

impl Font {
    fn load(pdf: &Pdf, dict: &Dict) -> Font {
        let composite = pdf.get(dict, "Subtype").name() == Some("Type0");
        let cmap = match pdf.get(dict, "ToUnicode") {
            s @ Obj::Stream(..) => pdf.stream_data(&s).map(|b| parse_cmap(&b)),
            _ => None,
        };
        let mut widths = HashMap::new();
        let mut default_width = None;
        if composite {
            // Identity-H: the code is the CID, widths come from the descendant font's /W array.
            let desc = match pdf.get(dict, "DescendantFonts") {
                Obj::Arr(a) => a.first().map(|d| pdf.resolve(d)),
                other => Some(other),
            };
            if let Some(d) = desc.as_ref().and_then(Obj::dict) {
                default_width = Some(pdf.get(d, "DW").num().unwrap_or(1000.0));
                if let Obj::Arr(w) = pdf.get(d, "W") {
                    parse_cid_widths(pdf, &w, &mut widths);
                }
            }
        } else if let Obj::Arr(w) = pdf.get(dict, "Widths") {
            let first = pdf.get(dict, "FirstChar").int().unwrap_or(0).max(0) as u32;
            for (i, v) in w.iter().take(MAX_WIDTH_ENTRIES).enumerate() {
                if let Some(x) = pdf.resolve(v).num() {
                    widths.insert(first + i as u32, x);
                }
            }
            default_width = pdf.get(dict, "MissingWidth").num().or(Some(0.0)).filter(|_| !widths.is_empty());
        }
        Font { cmap, composite, widths, default_width }
    }

    fn code_len(&self) -> usize {
        match &self.cmap {
            Some(cm) if cm.code_len > 0 => cm.code_len,
            _ if self.composite => 2,
            _ => 1,
        }
    }

    /// Appends the text of `s` to `out` and returns its total advance in 1/1000 em (`None` if any glyph is unknown).
    fn show(&self, s: &[u8], out: &mut String) -> Option<f64> {
        let step = self.code_len();
        let mut advance = Some(0.0);
        for chunk in s.chunks(step) {
            let code = be_int(chunk);
            match self.widths.get(&code).copied().or(self.default_width) {
                Some(w) => advance = advance.map(|a| a + w),
                None => advance = None,
            }
            match &self.cmap {
                Some(cm) => {
                    if let Some(t) = cm.map.get(&code) {
                        out.push_str(t);
                    }
                }
                None if !self.composite => out.extend(chunk.iter().filter_map(|&b| winansi_char(b))),
                None => {}
            }
        }
        advance
    }
}

/// `/W [ c [w1 w2 ...] c_first c_last w ... ]`
fn parse_cid_widths(pdf: &Pdf, w: &[Obj], out: &mut HashMap<u32, f64>) {
    let mut i = 0;
    while i < w.len() && out.len() < MAX_WIDTH_ENTRIES {
        let Some(first) = pdf.resolve(&w[i]).int().filter(|&f| f >= 0) else { break };
        match w.get(i + 1).map(|o| pdf.resolve(o)) {
            Some(Obj::Arr(list)) => {
                for (k, v) in list.iter().enumerate() {
                    if let Some(x) = pdf.resolve(v).num() {
                        out.insert(first as u32 + k as u32, x);
                    }
                }
                i += 2;
            }
            Some(range_end) => {
                let (Some(last), Some(x)) = (range_end.int(), w.get(i + 2).and_then(|o| pdf.resolve(o).num())) else {
                    break;
                };
                for c in first..=last.min(first + 0xFFFF) {
                    out.insert(c as u32, x);
                }
                i += 3;
            }
            None => break,
        }
    }
}

// ------------------------------------------------------------------ content interpreter

struct Interp<'p, 'a> {
    pdf: &'p Pdf<'a>,
    fonts: HashMap<String, Rc<Font>>,
    out: String,
    /// Origin of the current text line, in user space.
    x: f64,
    y: f64,
    /// Text matrix scale (a, d): turns text-space offsets into user space.
    sx: f64,
    sy: f64,
    /// Where the next glyph is drawn.
    cursor: f64,
    /// Where the pen ended after the last glyph run; `None` when a width was unknown.
    pen_x: Option<f64>,
    size: f64,
    leading: f64,
    last_y: Option<f64>,
    cur: Option<Rc<Font>>,
    /// A positioning operator moved the origin since the last text was shown.
    moved: bool,
    /// An explicit large kern asked for a word gap.
    gap_hint: bool,
}

impl<'p, 'a> Interp<'p, 'a> {
    fn font_for(&mut self, res: &Dict, name: &str) -> Option<Rc<Font>> {
        if let Some(f) = self.fonts.get(name) {
            return Some(f.clone());
        }
        let fonts = self.pdf.get(res, "Font");
        let fd = self.pdf.get(fonts.dict()?, name);
        let font = Rc::new(Font::load(self.pdf, fd.dict()?));
        self.fonts.insert(name.to_string(), font.clone());
        Some(font)
    }

    fn show(&mut self, s: &[u8]) {
        let em = (self.size * self.sx.abs()).max(0.1);
        let new_line = self.last_y.is_some_and(|ly| (ly - self.y).abs() > em * 0.5);
        if new_line {
            if !self.out.is_empty() && !self.out.ends_with('\n') {
                self.out.push('\n');
            }
        } else if self.moved || self.gap_hint {
            // Same line, new position: a gap wider than a fraction of an em separates words. When glyph
            // widths are unknown, any forward move is taken as a separator.
            let wide = match self.pen_x {
                Some(pen) => self.cursor - pen > em * 0.2,
                None => self.cursor > 0.0,
            };
            if (wide || self.gap_hint) && !self.out.is_empty() && !self.out.ends_with([' ', '\n']) {
                self.out.push(' ');
            }
        }
        self.moved = false;
        self.gap_hint = false;
        self.last_y = Some(self.y);
        let (adv, text) = match &self.cur {
            Some(f) => {
                let mut t = String::new();
                (f.show(s, &mut t), t)
            }
            None => (None, s.iter().filter_map(|&b| winansi_char(b)).collect()),
        };
        self.out.push_str(&text);
        // `Td` is relative to the line origin, so the origin stays; only the cursor follows the glyphs.
        if let Some(a) = adv {
            self.cursor += a / 1000.0 * self.size * self.sx;
        }
        self.pen_x = adv.map(|_| self.cursor);
    }

    fn position(&mut self) {
        self.cursor = self.x;
        self.moved = true;
    }

    fn run(&mut self, content: &[u8], res: &Dict, depth: usize) {
        if depth > 4 {
            return;
        }
        let mut lx = Lexer::new(content, 0);
        let mut args: Vec<Obj> = Vec::new();
        while let Some(t) = lx.token(false, 0) {
            let op = match t {
                Tok::Obj(o) => {
                    args.push(o);
                    if args.len() > 512 {
                        args.remove(0);
                    }
                    continue;
                }
                Tok::Op(op) => op,
            };
            let n = |i: usize| args.get(i).and_then(Obj::num).unwrap_or(0.0);
            match op.as_str() {
                "BT" => {
                    self.x = 0.0;
                    self.y = 0.0;
                    self.sx = 1.0;
                    self.sy = 1.0;
                    self.position();
                }
                "Tf" => {
                    if let Some(name) = args.first().and_then(Obj::name) {
                        let name = name.to_string();
                        self.cur = self.font_for(res, &name);
                    }
                    self.size = n(1).abs();
                }
                "TL" => self.leading = n(0),
                "Td" | "TD" => {
                    if op == "TD" {
                        self.leading = -n(1);
                    }
                    self.x += n(0) * self.sx;
                    self.y += n(1) * self.sy;
                    self.position();
                }
                "Tm" => {
                    self.sx = if n(0) != 0.0 { n(0) } else { 1.0 };
                    self.sy = if n(3) != 0.0 { n(3) } else { 1.0 };
                    self.x = n(4);
                    self.y = n(5);
                    self.position();
                }
                "T*" => {
                    self.y -= self.leading * self.sy;
                    self.position();
                }
                "Tj" => {
                    if let Some(Obj::Str(s)) = args.first() {
                        let s = s.clone();
                        self.show(&s);
                    }
                }
                "'" | "\"" => {
                    self.y -= self.leading * self.sy;
                    self.position();
                    if let Some(Obj::Str(s)) = args.last() {
                        let s = s.clone();
                        self.show(&s);
                    }
                }
                "TJ" => {
                    if let Some(Obj::Arr(items)) = args.first() {
                        for it in items.clone() {
                            match it {
                                Obj::Str(s) => self.show(&s),
                                // Large negative kerning is a word gap.
                                Obj::Int(_) | Obj::Real(_) => {
                                    let k = it.num().unwrap_or(0.0);
                                    self.cursor -= k / 1000.0 * self.size * self.sx;
                                    if k < -200.0 {
                                        self.gap_hint = true;
                                    }
                                }
                                _ => {}
                            }
                        }
                    }
                }
                "Do" => {
                    if let Some(name) = args.first().and_then(Obj::name) {
                        let xo = self.pdf.get(res, "XObject");
                        if let Some(xd) = xo.dict() {
                            let form = self.pdf.get(xd, name);
                            if let Some(fd) = form.dict() {
                                if self.pdf.get(fd, "Subtype").name() == Some("Form") {
                                    if let Some(data) = self.pdf.stream_data(&form) {
                                        let own = self.pdf.get(fd, "Resources");
                                        let r = own.dict().cloned().unwrap_or_else(|| res.clone());
                                        self.run(&data, &r, depth + 1);
                                    }
                                }
                            }
                        }
                    }
                }
                _ => {}
            }
            args.clear();
        }
    }
}

/// Per-page plain text, in page order.
pub fn extract_pages(bytes: &[u8]) -> Result<Vec<String>> {
    let pdf = Pdf::open(bytes)?;
    let root = pdf.root_pages().ok_or_else(|| Error::Corrupt("PDF: no page tree".into()))?;
    let mut pages = Vec::new();
    pdf.collect_pages(&root, &Dict::new(), &mut pages, &mut HashSet::new(), 0);
    let mut out = Vec::with_capacity(pages.len());
    for (page, inh) in pages {
        let res = pdf.get(&inh, "Resources");
        let res = res.dict().cloned().unwrap_or_default();
        let contents = match pdf.get(&page, "Contents") {
            Obj::Arr(a) => a.iter().map(|o| pdf.resolve(o)).collect(),
            s @ Obj::Stream(..) => vec![s],
            _ => vec![],
        };
        let mut data = Vec::new();
        for c in &contents {
            if let Some(b) = pdf.stream_data(c) {
                data.extend_from_slice(&b);
                data.push(b'\n');
            }
        }
        let mut it = Interp {
            pdf: &pdf,
            fonts: HashMap::new(),
            out: String::new(),
            x: 0.0,
            y: 0.0,
            sx: 1.0,
            sy: 1.0,
            cursor: 0.0,
            pen_x: None,
            size: 0.0,
            leading: 0.0,
            last_y: None,
            cur: None,
            moved: false,
            gap_hint: false,
        };
        it.run(&data, &res, 0);
        out.push(it.out.trim().to_string());
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::pdfwrite::text_to_pdf;
    use super::super::zip::ZipWriter;
    use super::*;
    use std::io::Write;

    fn words(s: &str) -> Vec<&str> {
        s.split_whitespace().collect()
    }

    fn zlib(data: &[u8]) -> Vec<u8> {
        let mut e = flate2::write::ZlibEncoder::new(Vec::new(), flate2::Compression::default());
        e.write_all(data).unwrap();
        e.finish().unwrap()
    }

    /// Assembles a PDF from (number, body) pairs without an xref table: the parser must not need one.
    fn raw_pdf(objs: &[(u32, Vec<u8>)]) -> Vec<u8> {
        let mut out = b"%PDF-1.5\n".to_vec();
        for (n, body) in objs {
            out.extend_from_slice(format!("{n} 0 obj\n").as_bytes());
            out.extend_from_slice(body);
            out.extend_from_slice(b"\nendobj\n");
        }
        out.extend_from_slice(b"trailer\n<< /Root 1 0 R >>\n%%EOF");
        out
    }

    fn stream(dict: &str, data: &[u8]) -> Vec<u8> {
        let mut b = format!("<< {dict} /Length {} >>\nstream\n", data.len()).into_bytes();
        b.extend_from_slice(data);
        b.extend_from_slice(b"\nendstream");
        b
    }

    fn simple_pdf(content: &str) -> Vec<u8> {
        raw_pdf(&[
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_vec()),
            (4, stream("", content.as_bytes())),
            (5, b"<< /Type /Font /Subtype /Type1 /BaseFont /Helvetica /Encoding /WinAnsiEncoding >>".to_vec()),
        ])
    }

    #[test]
    fn reads_back_what_our_own_writer_laid_out() {
        let text = "Hola mundo\nsegunda línea con acentos áéíóú ñ\n\ntercera (con paréntesis) y \\ barra";
        let pages = extract_pages(&text_to_pdf(text)).unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(words(&pages[0]), words(text));
        assert_eq!(pages[0].lines().next().unwrap(), "Hola mundo");
    }

    #[test]
    fn long_documents_keep_every_page_in_order() {
        let long: String = (0..150).map(|i| format!("row {i}\n")).collect();
        let pages = extract_pages(&text_to_pdf(&long)).unwrap();
        assert!(pages.len() >= 3);
        assert_eq!(words(&pages.join("\n")), words(&long));
    }

    #[test]
    fn kerned_arrays_become_words_and_lines_follow_positioning() {
        let pdf = simple_pdf("BT /F1 12 Tf 72 700 Td [(Hel) 20 (lo) -400 (wor) (ld)] TJ 0 -14 Td (next line) Tj ET");
        assert_eq!(extract_pages(&pdf).unwrap(), vec!["Hello world\nnext line"]);
    }

    #[test]
    fn handles_flate_streams_and_hex_strings() {
        let content = zlib(b"BT /F1 12 Tf 72 700 Td <48656C6C6F> Tj ET");
        let pdf = raw_pdf(&[
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_vec()),
            (4, stream("/Filter /FlateDecode", &content)),
            (5, b"<< /Type /Font /Subtype /Type1 >>".to_vec()),
        ]);
        assert_eq!(extract_pages(&pdf).unwrap(), vec!["Hello"]);
    }

    #[test]
    fn follows_the_page_tree_not_file_order_and_arrays_of_content_streams() {
        let pdf = raw_pdf(&[
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [6 0 R 3 0 R] /Count 2 >>".to_vec()),
            (3, b"<< /Type /Page /Parent 2 0 R /Contents [4 0 R 5 0 R] /Resources << >> >>".to_vec()),
            (4, stream("", b"BT 0 0 Td (second ")),
            (5, stream("", b"page) Tj ET")),
            (6, b"<< /Type /Page /Parent 2 0 R /Contents 7 0 R /Resources << >> >>".to_vec()),
            (7, stream("", b"BT 0 0 Td (first page) Tj ET")),
        ]);
        assert_eq!(extract_pages(&pdf).unwrap(), vec!["first page", "second page"]);
    }

    #[test]
    fn decodes_type0_fonts_through_tounicode_and_object_streams() {
        let cmap = "/CIDInit begin begincmap 1 begincodespacerange <0000> <FFFF> endcodespacerange \
                    2 beginbfchar <0001> <0048> <0002> <00E9> endbfchar \
                    1 beginbfrange <0003> <0005> <0061> endbfrange endcmap";
        // Objects 1-3 and 5 live inside an object stream, as in PDF 1.5 files.
        let inner = [
            (1u32, "<< /Type /Catalog /Pages 2 0 R >>"),
            (2, "<< /Type /Pages /Kids [3 0 R] /Count 1 >>"),
            (3, "<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>"),
            (5, "<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /ToUnicode 6 0 R >>"),
        ];
        let mut header = String::new();
        let mut body = String::new();
        for (n, o) in inner {
            header.push_str(&format!("{n} {} ", body.len()));
            body.push_str(o);
            body.push(' ');
        }
        let objstm = format!("{header}{body}");
        let content = b"BT /F1 12 Tf 10 10 Td <000100020003000400050001> Tj ET";
        let pdf = raw_pdf(&[
            (
                7,
                stream(
                    &format!("/Type /ObjStm /N 4 /First {} /Filter /FlateDecode", header.len()),
                    &zlib(objstm.as_bytes()),
                ),
            ),
            (4, stream("", content)),
            (6, stream("", cmap.as_bytes())),
        ]);
        assert_eq!(extract_pages(&pdf).unwrap(), vec!["HéabcH"]);
    }

    #[test]
    fn encrypted_files_are_refused_not_garbled() {
        let mut pdf = simple_pdf("BT (x) Tj ET");
        let at = pdf.windows(7).position(|w| w == b"trailer").unwrap();
        pdf.splice(at..at + 7, b"trailer\n<< /Encrypt 9 0 R >>\n".iter().copied());
        assert!(matches!(extract_pages(&pdf), Err(Error::Unsupported(_))));
    }

    #[test]
    fn garbage_and_truncation_are_typed_errors() {
        assert!(matches!(extract_pages(b""), Err(Error::Corrupt(_))));
        assert!(matches!(extract_pages(b"%PDF-1.4\nnothing useful"), Err(Error::Corrupt(_))));
        let good = text_to_pdf("some text to cut");
        for cut in [10, good.len() / 2, good.len() - 30] {
            let _ = extract_pages(&good[..cut]); // any outcome is fine, panicking is not
        }
        let mut z = ZipWriter::new();
        z.add("a", b"b", false);
        assert!(extract_pages(&z.finish()).is_err());
    }

    #[test]
    fn reference_cycles_in_the_page_tree_terminate() {
        let pdf = raw_pdf(&[
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [2 0 R 3 0 R] /Count 2 >>".to_vec()),
            (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R >>".to_vec()),
            (4, stream("", b"BT (ok) Tj ET")),
        ]);
        assert_eq!(extract_pages(&pdf).unwrap(), vec!["ok"]);
    }

    #[test]
    fn glyph_by_glyph_positioning_uses_widths_to_find_word_gaps() {
        // The layout browsers produce: one Td per glyph, spaces as glyphs, widths in the CID font's /W array.
        let cmap = "1 begincodespacerange <0000> <FFFF> endcodespacerange 5 beginbfchar                     <0001> <0048> <0002> <0069> <0003> <0020> <0004> <0079> <0005> <006F> endbfchar";
        let pdf = raw_pdf(&[
            (1, b"<< /Type /Catalog /Pages 2 0 R >>".to_vec()),
            (2, b"<< /Type /Pages /Kids [3 0 R] /Count 1 >>".to_vec()),
            (3, b"<< /Type /Page /Parent 2 0 R /Contents 4 0 R /Resources << /Font << /F1 5 0 R >> >> >>".to_vec()),
            (
                4,
                stream(
                    "",
                    b"BT /F1 10 Tf 1 0 0 1 0 100 Tm <0001> Tj 5 0 Td <0002> Tj 5 0 Td <0003> Tj 2.5 0 Td <0004> Tj                       5 0 Td <0005> Tj 10 0 Td <0001> Tj ET                       BT /F1 10 Tf 1 0 0 1 60 100 Tm <0002> Tj ET BT /F1 10 Tf 1 0 0 1 0 50 Tm <0005> Tj ET",
                ),
            ),
            (
                5,
                b"<< /Type /Font /Subtype /Type0 /Encoding /Identity-H /ToUnicode 6 0 R /DescendantFonts [<< /DW 1000 /W [1 [500 500 250 500 500]] >>] >>"
                    .to_vec(),
            ),
            (6, stream("", cmap.as_bytes())),
        ]);
        // "Hi yo" has no internal gaps; the jump before the second H is a word gap; a later block at the same
        // height is a separate word too; a block at another height starts a new line.
        assert_eq!(
            extract_pages(&pdf).unwrap(),
            vec![
                "Hi yo H i
o"
            ]
        );
    }
}
