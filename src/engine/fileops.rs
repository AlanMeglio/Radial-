//! File operations that work on any file: streaming ZIP, safe unzip, SHA-256 and properties.

use super::error::{Error, Result};
use std::path::{Path, PathBuf};

use std::collections::HashSet;
use std::fs::File;
use std::io::{BufReader, BufWriter, Read, Seek, SeekFrom, Write};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Streaming buffer size for hashing, zipping and unzipping: big enough to keep the disk busy, small enough
/// that memory stays flat whatever the input size.
const CHUNK: usize = 256 * 1024;

fn path_err(path: &Path, e: std::io::Error) -> Error {
    Error::Io(format!("{}: {e}", path.display()))
}

fn bad(msg: impl AsRef<str>) -> Error {
    Error::Corrupt(format!("zip: {}", msg.as_ref()))
}

/// Reads until `buf` is full or the input ends; returns the bytes read.
fn read_full(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    let mut n = 0;
    while n < buf.len() {
        match r.read(&mut buf[n..]) {
            Ok(0) => break,
            Ok(k) => n += k,
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
    Ok(n)
}

/// One `read` that retries on interruption.
fn read_some(r: &mut impl Read, buf: &mut [u8]) -> std::io::Result<usize> {
    loop {
        match r.read(buf) {
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            other => return other,
        }
    }
}

// ================================================================ SHA-256

const K: [u32; 64] = [
    0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4, 0xab1c5ed5, 0xd807aa98,
    0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe, 0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786,
    0x0fc19dc6, 0x240ca1cc, 0x2de92c6f, 0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8,
    0xbf597fc7, 0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc, 0x53380d13,
    0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b, 0xc24b8b70, 0xc76c51a3, 0xd192e819,
    0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116, 0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a,
    0x5b9cca4f, 0x682e6ff3, 0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
    0xc67178f2,
];

/// Incremental SHA-256 (FIPS 180-4).
pub struct Sha256 {
    h: [u32; 8],
    buf: [u8; 64],
    filled: usize,
    total: u64,
}

impl Default for Sha256 {
    fn default() -> Self {
        Self::new()
    }
}

impl Sha256 {
    pub fn new() -> Self {
        Sha256 {
            h: [0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab, 0x5be0cd19],
            buf: [0; 64],
            filled: 0,
            total: 0,
        }
    }

    pub fn update(&mut self, mut data: &[u8]) {
        self.total = self.total.wrapping_add(data.len() as u64);
        if self.filled > 0 {
            let take = (64 - self.filled).min(data.len());
            self.buf[self.filled..self.filled + take].copy_from_slice(&data[..take]);
            self.filled += take;
            data = &data[take..];
            if self.filled < 64 {
                return;
            }
            let block = self.buf;
            compress(&mut self.h, &block);
            self.filled = 0;
        }
        let whole = data.len() / 64 * 64;
        for block in data[..whole].chunks_exact(64) {
            compress(&mut self.h, block);
        }
        let rest = &data[whole..];
        self.buf[..rest.len()].copy_from_slice(rest);
        self.filled = rest.len();
    }

    pub fn finish(mut self) -> [u8; 32] {
        let bits = self.total.wrapping_mul(8);
        let mut pad = [0u8; 72];
        pad[0] = 0x80;
        let pad_len = if self.filled < 56 { 56 - self.filled } else { 120 - self.filled };
        self.update(&pad[..pad_len]);
        self.update(&bits.to_be_bytes());
        debug_assert_eq!(self.filled, 0);
        let mut out = [0u8; 32];
        for (chunk, word) in out.chunks_exact_mut(4).zip(self.h) {
            chunk.copy_from_slice(&word.to_be_bytes());
        }
        out
    }
}

#[inline(always)]
fn compress(h: &mut [u32; 8], block: &[u8]) {
    let mut w = [0u32; 64];
    for (i, c) in block.chunks_exact(4).enumerate() {
        w[i] = u32::from_be_bytes([c[0], c[1], c[2], c[3]]);
    }
    for i in 16..64 {
        let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
        let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
        w[i] = w[i - 16].wrapping_add(s0).wrapping_add(w[i - 7]).wrapping_add(s1);
    }
    let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = *h;
    for i in 0..64 {
        let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
        let ch = (e & f) ^ (!e & g);
        let t1 = hh.wrapping_add(s1).wrapping_add(ch).wrapping_add(K[i]).wrapping_add(w[i]);
        let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
        let maj = (a & b) ^ (a & c) ^ (b & c);
        let t2 = s0.wrapping_add(maj);
        hh = g;
        g = f;
        f = e;
        e = d.wrapping_add(t1);
        d = c;
        c = b;
        b = a;
        a = t1.wrapping_add(t2);
    }
    for (slot, v) in h.iter_mut().zip([a, b, c, d, e, f, g, hh]) {
        *slot = slot.wrapping_add(v);
    }
}

/// Lowercase hexadecimal rendering of `bytes`.
pub fn hex(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push(DIGITS[(b >> 4) as usize] as char);
        s.push(DIGITS[(b & 15) as usize] as char);
    }
    s
}

/// SHA-256 of `data` as 64 lowercase hex digits.
#[cfg(test)]
pub fn sha256_hex(data: &[u8]) -> String {
    let mut h = Sha256::new();
    h.update(data);
    hex(&h.finish())
}

/// SHA-256 of a file, streamed through a 256 KiB buffer so memory use does not depend on the file size.
pub fn sha256_file(path: &Path) -> Result<String> {
    let mut f = File::open(path).map_err(|e| path_err(path, e))?;
    let mut buf = vec![0u8; CHUNK];
    let mut h = Sha256::new();
    loop {
        let n = read_some(&mut f, &mut buf).map_err(|e| path_err(path, e))?;
        if n == 0 {
            break;
        }
        h.update(&buf[..n]);
    }
    Ok(hex(&h.finish()))
}

// ================================================================ local time

/// A calendar time broken into fields.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Tm {
    pub year: u16,
    pub month: u16,
    pub day: u16,
    pub hour: u16,
    pub minute: u16,
    pub second: u16,
}

impl Tm {
    /// `YYYY-MM-DD HH:MM`.
    fn stamp(&self) -> String {
        format!("{:04}-{:02}-{:02} {:02}:{:02}", self.year, self.month, self.day, self.hour, self.minute)
    }
}

const FILETIME_UNIX_EPOCH: u64 = 116_444_736_000_000_000;

fn filetime_of(t: SystemTime) -> Option<u64> {
    match t.duration_since(UNIX_EPOCH) {
        Ok(d) => {
            FILETIME_UNIX_EPOCH.checked_add(d.as_secs().checked_mul(10_000_000)? + u64::from(d.subsec_nanos() / 100))
        }
        Err(e) => {
            let d = e.duration();
            FILETIME_UNIX_EPOCH.checked_sub(d.as_secs().checked_mul(10_000_000)? + u64::from(d.subsec_nanos() / 100))
        }
    }
}

/// Converts a UTC instant to local civil time with the Windows time-zone rules in force for that date.
pub fn local_time(t: SystemTime) -> Option<Tm> {
    use windows_sys::Win32::Foundation::{FILETIME, SYSTEMTIME};
    use windows_sys::Win32::System::Time::{FileTimeToSystemTime, SystemTimeToTzSpecificLocalTime};
    let ft = filetime_of(t)?;
    let ft = FILETIME { dwLowDateTime: ft as u32, dwHighDateTime: (ft >> 32) as u32 };
    // SAFETY: plain out-parameters on stack values; both calls only write the SYSTEMTIME they are given.
    unsafe {
        let mut utc: SYSTEMTIME = std::mem::zeroed();
        if FileTimeToSystemTime(&ft, &mut utc) == 0 {
            return None;
        }
        let mut loc: SYSTEMTIME = std::mem::zeroed();
        if SystemTimeToTzSpecificLocalTime(std::ptr::null(), &utc, &mut loc) == 0 {
            return None;
        }
        Some(Tm {
            year: loc.wYear,
            month: loc.wMonth,
            day: loc.wDay,
            hour: loc.wHour,
            minute: loc.wMinute,
            second: loc.wSecond,
        })
    }
}

/// MS-DOS `(time, date)` words for a modification time; 1980-01-01 when it cannot be represented.
fn dos_stamp(t: Option<SystemTime>) -> (u16, u16) {
    match t.and_then(local_time) {
        Some(tm) if tm.year >= 1980 => {
            let year = tm.year.min(2107) - 1980;
            ((tm.hour << 11) | (tm.minute << 5) | (tm.second / 2), (year << 9) | (tm.month << 5) | tm.day)
        }
        _ => (0, 0x21),
    }
}

// ================================================================ zip_paths

/// What `zip_paths` produced.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ZipReport {
    /// Files stored in the archive.
    pub files: u32,
    /// Explicit folder entries (only empty folders need one).
    pub dirs: u32,
    /// Uncompressed bytes read from the files.
    pub bytes_in: u64,
    /// Size of the finished archive.
    pub bytes_out: u64,
    /// Inputs left out: links, files that could not be read, the archive itself.
    pub skipped: u32,
}

/// Format limits of a classic (non-ZIP64) archive.
#[derive(Clone, Debug)]
pub struct ZipLimits {
    pub max_entries: usize,
    pub max_bytes: u64,
}

impl Default for ZipLimits {
    fn default() -> Self {
        ZipLimits { max_entries: 65_535, max_bytes: u64::from(u32::MAX) }
    }
}

/// Extensions of formats that are already compressed: deflating them wastes time for no gain.
const STORED_EXTS: [&str; 30] = [
    "jpg", "jpeg", "png", "webp", "gif", "heic", "avif", "mp3", "mp4", "m4a", "mkv", "mov", "avi", "ogg", "opus",
    "flac", "zip", "7z", "rar", "gz", "xz", "zst", "docx", "xlsx", "pptx", "odt", "jar", "apk", "webm", "bz2",
];

/// A file whose first 64 KiB deflate to more than this share of their size is stored instead.
const SAMPLE: usize = 64 * 1024;
const LEVEL: u32 = 3;

struct Item {
    src: Option<PathBuf>,
    name: String,
    size: u64,
    time: u16,
    date: u16,
    deflate: bool,
}

struct Scan<'a> {
    items: Vec<Item>,
    skipped: u32,
    estimate: u64,
    lim: &'a ZipLimits,
    tmp: &'a Path,
    dest: &'a Path,
}

fn is_link(md: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    const REPARSE_POINT: u32 = 0x400;
    md.file_type().is_symlink() || md.file_attributes() & REPARSE_POINT != 0
}

fn same_file(a: &Path, b: &Path) -> bool {
    if a.file_name().map(|n| n.to_ascii_lowercase()) != b.file_name().map(|n| n.to_ascii_lowercase()) {
        return false;
    }
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(x), Ok(y)) => x == y,
        _ => false,
    }
}

impl Scan<'_> {
    fn push(&mut self, item: Item) -> Result<()> {
        if self.items.len() >= self.lim.max_entries {
            return Err(Error::Unsupported(format!(
                "too many entries for a ZIP file (more than {}); zip folders separately",
                self.lim.max_entries
            )));
        }
        self.estimate = self.estimate.saturating_add(item.size + item.size / 1000 + 120 + 2 * item.name.len() as u64);
        if item.size > u64::from(u32::MAX) || self.estimate > self.lim.max_bytes {
            return Err(Error::Unsupported("the archive would be larger than 4 GiB (ZIP64 is not supported)".into()));
        }
        self.items.push(item);
        Ok(())
    }

    fn add(&mut self, path: &Path, name: String, depth: u32) -> Result<()> {
        let Ok(md) = std::fs::symlink_metadata(path) else {
            self.skipped += 1;
            return Ok(());
        };
        if is_link(&md) {
            self.skipped += 1;
        } else if md.is_dir() {
            self.walk(path, &name, depth)?;
        } else if md.is_file() {
            if same_file(path, self.tmp) {
                return Ok(()); // our own work file, not a user file
            }
            if same_file(path, self.dest) {
                self.skipped += 1;
                return Ok(());
            }
            let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase).unwrap_or_default();
            let (time, date) = dos_stamp(md.modified().ok());
            let deflate = md.len() > 0 && !STORED_EXTS.contains(&ext.as_str());
            self.push(Item { src: Some(path.to_path_buf()), name, size: md.len(), time, date, deflate })?;
        } else {
            self.skipped += 1; // device, pipe, ...
        }
        Ok(())
    }

    /// Adds everything below `dir`; an empty folder becomes a folder entry. Returns the entries added.
    fn walk(&mut self, dir: &Path, prefix: &str, depth: u32) -> Result<usize> {
        const MAX_DEPTH: u32 = 200;
        let before = self.items.len();
        if depth > MAX_DEPTH {
            self.skipped += 1;
            return Ok(1);
        }
        let Ok(rd) = std::fs::read_dir(dir) else {
            self.skipped += 1;
            return Ok(1);
        };
        let mut kids: Vec<(String, PathBuf)> =
            rd.filter_map(|e| e.ok()).map(|e| (e.file_name().to_string_lossy().into_owned(), e.path())).collect();
        kids.sort();
        for (n, p) in kids {
            self.add(&p, format!("{prefix}/{n}"), depth + 1)?;
        }
        if self.items.len() == before {
            let (time, date) = dos_stamp(std::fs::metadata(dir).and_then(|m| m.modified()).ok());
            self.push(Item { src: None, name: format!("{prefix}/"), size: 0, time, date, deflate: false })?;
        }
        Ok(self.items.len() - before)
    }
}

/// `a.txt` -> `a (2).txt`; folders just get the suffix.
fn suffixed(name: &str, n: u32, is_dir: bool) -> String {
    match name.rfind('.') {
        Some(dot) if !is_dir && dot > 0 => format!("{} ({n}){}", &name[..dot], &name[dot..]),
        _ => format!("{name} ({n})"),
    }
}

struct Count<'a, W: Write> {
    w: &'a mut W,
    n: u64,
}

impl<W: Write> Write for Count<'_, W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        let k = self.w.write(b)?;
        self.n += k as u64;
        Ok(k)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        self.w.flush()
    }
}

/// Where one entry's bytes go: straight to the archive, or through the deflate encoder.
enum Sink<'a, W: Write> {
    Raw(Count<'a, W>),
    Deflate(flate2::write::DeflateEncoder<Count<'a, W>>),
}

impl<W: Write> Write for Sink<'_, W> {
    fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
        match self {
            Sink::Raw(c) => c.write(b),
            Sink::Deflate(e) => e.write(b),
        }
    }
    fn flush(&mut self) -> std::io::Result<()> {
        match self {
            Sink::Raw(c) => c.flush(),
            Sink::Deflate(e) => e.flush(),
        }
    }
}

impl<W: Write> Sink<'_, W> {
    /// Completes the entry and returns the bytes it occupies in the archive.
    fn finish(self) -> std::io::Result<u64> {
        match self {
            Sink::Raw(c) => Ok(c.n),
            Sink::Deflate(e) => Ok(e.finish()?.n),
        }
    }
}

struct TempGuard {
    path: PathBuf,
    armed: bool,
}

impl Drop for TempGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_file(&self.path);
        }
    }
}

fn put16(v: &mut Vec<u8>, x: u16) {
    v.extend_from_slice(&x.to_le_bytes());
}
fn put32(v: &mut Vec<u8>, x: u32) {
    v.extend_from_slice(&x.to_le_bytes());
}

const FLAG_UTF8: u16 = 1 << 11;
const FLAG_DESCRIPTOR: u16 = 1 << 3;

/// Compresses `paths` (files and folders, recursively) into the new archive `dest`.
///
/// Streaming and single pass: every file is read in 256 KiB chunks and deflated (or stored, when its extension
/// is already compressed or its first 64 KiB barely compress) straight into the output, with sizes and CRC
/// written in a data descriptor afterwards, so memory stays constant. Entry names are relative to the parent
/// of each dropped path (`Photos/a.jpg`), use `/`, are flagged UTF-8 and carry the file's modification time.
/// Links, unreadable files and `dest` itself are skipped. There is no ZIP64: more than 65535 entries or more
/// than 4 GiB return `Error::Unsupported` before anything is written. The archive is built in
/// `dest.tmp-<pid>` and published with a rename that never replaces; an existing `dest` is an `Error::Io`.
pub fn zip_paths(paths: &[PathBuf], dest: &Path) -> Result<ZipReport> {
    zip_paths_with(paths, dest, &ZipLimits::default())
}

pub fn zip_paths_with(paths: &[PathBuf], dest: &Path, lim: &ZipLimits) -> Result<ZipReport> {
    if paths.is_empty() {
        return Err(Error::InvalidArgument("nothing to compress".into()));
    }
    if std::fs::symlink_metadata(dest).is_ok() {
        return Err(Error::Io(format!("{} already exists", dest.display())));
    }
    let (tmp, file) = create_temp(dest)?;
    let mut guard = TempGuard { path: tmp.clone(), armed: true };

    let mut scan = Scan { items: Vec::new(), skipped: 0, estimate: 22, lim, tmp: &tmp, dest };
    let mut used: HashSet<String> = HashSet::new();
    for p in paths {
        let Some(base) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else {
            scan.skipped += 1;
            continue;
        };
        let is_dir = std::fs::symlink_metadata(p).map(|m| m.is_dir()).unwrap_or(false);
        let mut name = base.clone();
        let mut n = 0;
        while !used.insert(name.to_lowercase()) {
            n += 1;
            name = suffixed(&base, n, is_dir);
        }
        scan.add(p, name, 0)?;
    }
    let (items, mut skipped) = (std::mem::take(&mut scan.items), scan.skipped);

    let mut out = BufWriter::with_capacity(CHUNK, file);
    let mut cd: Vec<u8> = Vec::with_capacity(items.len() * 80);
    let mut pos: u64 = 0;
    let mut buf = vec![0u8; CHUNK];
    let (mut files, mut dirs, mut bytes_in, mut entries) = (0u32, 0u32, 0u64, 0u16);

    for item in &items {
        let mut input = None;
        let mut first = 0;
        if let Some(src) = &item.src {
            let Ok(mut f) = File::open(src) else {
                skipped += 1;
                continue;
            };
            first = read_full(&mut f, &mut buf[..SAMPLE]).map_err(|e| path_err(src, e))?;
            input = Some((f, src));
        }
        let name = item.name.as_bytes();
        let offset = pos;
        let mut method: u16 = 0;
        if item.deflate && first > 0 {
            let mut probe = flate2::write::DeflateEncoder::new(Vec::with_capacity(first), flate2::Compression::new(1));
            let packed = probe.write_all(&buf[..first]).and_then(|()| probe.finish()).map(|v| v.len()).unwrap_or(first);
            if packed * 100 < first * 95 {
                method = 8;
            }
        }
        let flags = FLAG_UTF8 | if input.is_some() { FLAG_DESCRIPTOR } else { 0 };
        let mut head = Vec::with_capacity(30 + name.len());
        put32(&mut head, 0x0403_4b50);
        for v in [20, flags, method, item.time, item.date] {
            put16(&mut head, v);
        }
        for _ in 0..3 {
            put32(&mut head, 0); // crc and sizes: in the data descriptor
        }
        put16(&mut head, name.len() as u16);
        put16(&mut head, 0);
        head.extend_from_slice(name);
        out.write_all(&head)?;
        pos += head.len() as u64;

        let (mut crc, mut comp, mut size) = (0u32, 0u64, 0u64);
        if let Some((mut f, src)) = input {
            let mut hasher = crc32fast::Hasher::new();
            let counted = Count { w: &mut out, n: 0 };
            let mut sink = if method == 8 {
                Sink::Deflate(flate2::write::DeflateEncoder::new(counted, flate2::Compression::new(LEVEL)))
            } else {
                Sink::Raw(counted)
            };
            let mut n = first;
            while n > 0 {
                hasher.update(&buf[..n]);
                size += n as u64;
                if size > u64::from(u32::MAX) {
                    return Err(Error::Unsupported("a file grew beyond 4 GiB while it was being compressed".into()));
                }
                sink.write_all(&buf[..n])?;
                n = read_some(&mut f, &mut buf).map_err(|e| path_err(src, e))?;
            }
            comp = sink.finish()?;
            crc = hasher.finalize();
            let mut tail = Vec::with_capacity(16);
            put32(&mut tail, 0x0807_4b50);
            put32(&mut tail, crc);
            put32(&mut tail, comp as u32);
            put32(&mut tail, size as u32);
            out.write_all(&tail)?;
            pos += comp + 16;
            files += 1;
            bytes_in += size;
        } else {
            dirs += 1;
        }
        if pos > lim.max_bytes {
            return Err(Error::Unsupported("the archive would be larger than 4 GiB (ZIP64 is not supported)".into()));
        }

        put32(&mut cd, 0x0201_4b50);
        for v in [20, 20, flags, method, item.time, item.date] {
            put16(&mut cd, v);
        }
        put32(&mut cd, crc);
        put32(&mut cd, comp as u32);
        put32(&mut cd, size as u32);
        put16(&mut cd, name.len() as u16);
        for _ in 0..4 {
            put16(&mut cd, 0); // extra, comment, disk, internal attributes
        }
        put32(&mut cd, if input_is_dir(item) { 0x10 } else { 0x20 });
        put32(&mut cd, offset as u32);
        cd.extend_from_slice(name);
        entries += 1;
    }

    let cd_offset = pos;
    out.write_all(&cd)?;
    let mut end = Vec::with_capacity(22);
    put32(&mut end, 0x0605_4b50);
    for v in [0, 0, entries, entries] {
        put16(&mut end, v);
    }
    put32(&mut end, cd.len() as u32);
    put32(&mut end, cd_offset as u32);
    put16(&mut end, 0);
    out.write_all(&end)?;
    let total = cd_offset + cd.len() as u64 + end.len() as u64;
    if total > lim.max_bytes {
        return Err(Error::Unsupported("the archive would be larger than 4 GiB (ZIP64 is not supported)".into()));
    }
    let file = out.into_inner().map_err(|e| Error::Io(e.to_string()))?;
    drop(file);

    publish(&tmp, dest)?;
    guard.armed = false;
    let _ = std::fs::remove_file(&tmp);
    Ok(ZipReport { files, dirs, bytes_in, bytes_out: total, skipped })
}

fn input_is_dir(item: &Item) -> bool {
    item.src.is_none()
}

/// Creates the work file next to `dest` (`dest.tmp-<pid>`); fails when it cannot be created exclusively.
fn create_temp(dest: &Path) -> Result<(PathBuf, File)> {
    let name = dest.file_name().ok_or_else(|| Error::InvalidArgument("destination has no file name".into()))?;
    for n in 0..64u32 {
        let mut t = name.to_os_string();
        if n == 0 {
            t.push(format!(".tmp-{}", std::process::id()));
        } else {
            t.push(format!(".tmp-{}-{n}", std::process::id()));
        }
        let tmp = dest.with_file_name(t);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&tmp) {
            Ok(f) => return Ok((tmp, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(path_err(dest, e)),
        }
    }
    Err(Error::Io(format!("{}: could not create a work file", dest.display())))
}

/// Moves `tmp` to `dest` without ever replacing an existing file.
fn publish(tmp: &Path, dest: &Path) -> Result<()> {
    match std::fs::hard_link(tmp, dest) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
            Err(Error::Io(format!("{} already exists", dest.display())))
        }
        Err(_) => {
            // No hard links on this volume: rename is only safe while the name is still free.
            if std::fs::symlink_metadata(dest).is_ok() {
                return Err(Error::Io(format!("{} already exists", dest.display())));
            }
            std::fs::rename(tmp, dest).map_err(|e| path_err(dest, e))
        }
    }
}

// ================================================================ zip directory (file based)

struct CdEntry {
    name: String,
    method: u16,
    crc: u32,
    comp: u64,
    size: u64,
    offset: u64,
    made_by: u16,
    attr: u32,
}

const CP437_HIGH: &str = "ÇüéâäàåçêëèïîìÄÅÉæÆôöòûùÿÖÜ¢£¥₧ƒáíóúñÑªº¿⌐¬½¼¡«»░▒▓│┤╡╢╖╕╣║╗╝╜╛┐└┴┬├─┼╞╟╚╔╩╦╠═╬╧╨╤╥╙╘╒╓╫╪┘┌█▄▌▐▀αßΓπΣσµτΦΘΩδ∞φε∩≡±≥≤⌠⌡÷≈°∙·√ⁿ²■\u{a0}";

fn decode_name(raw: &[u8], utf8_flag: bool) -> String {
    match std::str::from_utf8(raw) {
        Ok(s) => s.to_owned(),
        Err(_) if utf8_flag => String::from_utf8_lossy(raw).into_owned(),
        Err(_) => {
            let high: Vec<char> = CP437_HIGH.chars().collect();
            raw.iter().map(|&b| if b < 0x80 { b as char } else { high[(b - 0x80) as usize] }).collect()
        }
    }
}

fn le16(b: &[u8], at: usize) -> u16 {
    u16::from_le_bytes([b[at], b[at + 1]])
}
fn le32(b: &[u8], at: usize) -> u32 {
    u32::from_le_bytes([b[at], b[at + 1], b[at + 2], b[at + 3]])
}

fn map_read(e: std::io::Error) -> Error {
    match e.kind() {
        std::io::ErrorKind::UnexpectedEof | std::io::ErrorKind::InvalidData | std::io::ErrorKind::InvalidInput => {
            bad("damaged or truncated data")
        }
        _ => Error::Io(e.to_string()),
    }
}

/// Reads the central directory of a classic zip: entry count, offsets and sizes, nothing else.
fn read_directory(f: &mut File, len: u64, max_entries: usize) -> Result<Vec<CdEntry>> {
    if len < 22 {
        return Err(bad("not a ZIP archive (too short)"));
    }
    let tail_len = len.min(22 + 65_535);
    f.seek(SeekFrom::Start(len - tail_len)).map_err(map_read)?;
    let mut tail = vec![0u8; tail_len as usize];
    f.read_exact(&mut tail).map_err(map_read)?;
    let at = (0..=tail.len() - 22)
        .rev()
        .find(|&i| tail[i..i + 4] == [0x50, 0x4b, 5, 6] && i + 22 + le16(&tail, i + 20) as usize <= tail.len())
        .ok_or_else(|| bad("no end-of-directory record (not a ZIP file, or truncated)"))?;
    let (disk, cd_disk) = (le16(&tail, at + 4), le16(&tail, at + 6));
    let (total, cd_size, cd_off) = (le16(&tail, at + 10), le32(&tail, at + 12), le32(&tail, at + 16));
    let has_locator = at >= 20 && tail[at - 20..at - 16] == [0x50, 0x4b, 6, 7];
    if total == u16::MAX || cd_size == u32::MAX || cd_off == u32::MAX || has_locator {
        return Err(Error::Unsupported("zip64 archives are not supported".into()));
    }
    if disk != 0 || cd_disk != 0 {
        return Err(Error::Unsupported("multi-part zip archives are not supported".into()));
    }
    if total as usize > max_entries {
        return Err(Error::Unsupported(format!("the archive has {total} entries; the limit is {max_entries}")));
    }
    if u64::from(cd_off) + u64::from(cd_size) > len {
        return Err(bad("directory lies outside the file"));
    }
    f.seek(SeekFrom::Start(u64::from(cd_off))).map_err(map_read)?;
    let mut r = BufReader::with_capacity(64 * 1024, Read::by_ref(f).take(u64::from(cd_size)));
    let mut entries = Vec::with_capacity(total as usize);
    let mut comp_sum = 0u64;
    let mut scratch = Vec::new();
    for _ in 0..total {
        let mut h = [0u8; 46];
        r.read_exact(&mut h).map_err(|_| bad("directory is truncated"))?;
        if le32(&h, 0) != 0x0201_4b50 {
            return Err(bad("bad directory entry"));
        }
        let (flags, method) = (le16(&h, 8), le16(&h, 10));
        let (crc, comp, size) = (le32(&h, 16), le32(&h, 20), le32(&h, 24));
        let (nlen, xlen, clen) = (le16(&h, 28) as usize, le16(&h, 30) as usize, le16(&h, 32) as usize);
        let (attr, offset) = (le32(&h, 38), le32(&h, 42));
        scratch.resize(nlen + xlen + clen, 0);
        r.read_exact(&mut scratch).map_err(|_| bad("directory is truncated"))?;
        let name = decode_name(&scratch[..nlen], flags & FLAG_UTF8 != 0);
        if flags & (1 | 1 << 6 | 1 << 13) != 0 {
            return Err(Error::Unsupported(format!(
                "encrypted entry {name:?}; password-protected zips are not supported"
            )));
        }
        if comp == u32::MAX || size == u32::MAX || offset == u32::MAX {
            return Err(Error::Unsupported(format!("zip64 entry {name:?}")));
        }
        if method != 0 && method != 8 {
            return Err(Error::Unsupported(format!(
                "compression method {method} in entry {name:?} (only stored and deflate)"
            )));
        }
        comp_sum += u64::from(comp);
        entries.push(CdEntry {
            name,
            method,
            crc,
            comp: u64::from(comp),
            size: u64::from(size),
            offset: u64::from(offset),
            made_by: le16(&h, 4),
            attr,
        });
    }
    if comp_sum > len {
        return Err(bad("entries overlap (their data adds up to more than the file)"));
    }
    Ok(entries)
}

/// Positions `f` at the first data byte of `e`, after validating its local header.
fn seek_to_data(f: &mut File, len: u64, e: &CdEntry) -> Result<()> {
    if e.offset + 30 > len {
        return Err(bad("entry lies outside the file"));
    }
    f.seek(SeekFrom::Start(e.offset)).map_err(map_read)?;
    let mut h = [0u8; 30];
    f.read_exact(&mut h).map_err(map_read)?;
    if le32(&h, 0) != 0x0403_4b50 {
        return Err(bad("bad local header"));
    }
    let start = e.offset + 30 + u64::from(le16(&h, 26)) + u64::from(le16(&h, 28));
    if start + e.comp > len {
        return Err(bad("entry data is truncated"));
    }
    f.seek(SeekFrom::Start(start)).map_err(map_read)?;
    Ok(())
}

// ================================================================ unzip_to

/// What `unzip_to` extracted.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct UnzipReport {
    pub files: u32,
    pub dirs: u32,
    /// Uncompressed bytes written.
    pub bytes: u64,
}

/// Safety limits for extraction.
#[derive(Clone, Debug)]
pub struct UnzipLimits {
    pub max_entries: usize,
    /// Total uncompressed bytes.
    pub max_total: u64,
    /// An entry larger than this and more than `bomb_ratio` times its compressed size aborts.
    pub bomb_min: u64,
    pub bomb_ratio: u64,
}

impl Default for UnzipLimits {
    fn default() -> Self {
        UnzipLimits { max_entries: 20_000, max_total: 8 << 30, bomb_min: 100 << 20, bomb_ratio: 1000 }
    }
}

fn unsafe_name(name: &str) -> Error {
    Error::Corrupt(format!(
        "zip: unsafe entry name {name:?} (it would escape the destination or is not a valid Windows name)"
    ))
}

fn is_device_name(comp: &str) -> bool {
    let base = comp.split('.').next().unwrap_or("").trim_end_matches([' ', '.']).to_ascii_uppercase();
    match base.as_str() {
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$" => true,
        _ => {
            let digits = base.strip_prefix("COM").or_else(|| base.strip_prefix("LPT"));
            digits.is_some_and(|d| {
                matches!(d, "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "\u{b9}" | "\u{b2}" | "\u{b3}")
            })
        }
    }
}

/// Turns an archive entry name into safe path components, or rejects it.
fn safe_components(name: &str) -> Result<Vec<String>> {
    if name.chars().any(|c| c < ' ' || c == '\u{7f}') {
        return Err(unsafe_name(name));
    }
    let s = name.replace('\\', "/");
    if s.starts_with('/') {
        return Err(unsafe_name(name)); // absolute, rooted or UNC
    }
    let mut out = Vec::new();
    for c in s.split('/') {
        if c.is_empty() || c == "." {
            continue;
        }
        if c == ".."
            || c.chars().any(|ch| matches!(ch, '<' | '>' | ':' | '"' | '|' | '?' | '*'))
            || c.ends_with('.')
            || c.ends_with(' ')
            || c.chars().count() > 255
            || is_device_name(c)
        {
            return Err(unsafe_name(name));
        }
        out.push(c.to_owned());
    }
    Ok(out)
}

/// Creates `base/comps...` one level at a time, refusing links and files in the way.
fn ensure_dir(
    base: &Path,
    comps: &[String],
    created: &mut Vec<(PathBuf, bool)>,
    seen: &mut HashSet<PathBuf>,
) -> Result<PathBuf> {
    let mut cur = base.to_path_buf();
    for c in comps {
        cur.push(c);
        if seen.contains(&cur) {
            continue;
        }
        match std::fs::symlink_metadata(&cur) {
            Ok(md) if is_link(&md) => {
                return Err(Error::Corrupt(format!(
                    "zip: unsafe entry: {} is a link; refusing to follow it",
                    cur.display()
                )));
            }
            Ok(md) if !md.is_dir() => {
                return Err(bad(format!(
                    "entries conflict: {} is a file but the archive needs a folder there",
                    cur.display()
                )));
            }
            Ok(_) => {}
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                std::fs::create_dir(&cur).map_err(|e| path_err(&cur, e))?;
                created.push((cur.clone(), true));
            }
            Err(e) => return Err(path_err(&cur, e)),
        }
        seen.insert(cur.clone());
    }
    Ok(cur)
}

/// Creates a new file `dir/name`, or `dir/name (n)` when taken. Never overwrites.
fn create_unique(dir: &Path, name: &str) -> Result<(PathBuf, File)> {
    for n in 0..10_000u32 {
        let candidate = if n == 0 { name.to_owned() } else { suffixed(name, n, false) };
        let path = dir.join(candidate);
        match std::fs::OpenOptions::new().write(true).create_new(true).open(&path) {
            Ok(f) => return Ok((path, f)),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(path_err(&path, e)),
        }
    }
    Err(Error::Io(format!("{}: could not pick a free name for {name}", dir.display())))
}

struct Planned {
    idx: usize,
    comps: Vec<String>,
    is_dir: bool,
}

/// Extracts the zip `zip` into the folder `dest_dir` (created if missing).
///
/// Streaming: the central directory is read with `Seek`, then every entry is inflated straight to disk in
/// 256 KiB chunks. Only stored and deflate entries are supported; encrypted, ZIP64 and other methods return
/// `Error::Unsupported`. Entries are checked before anything is written: `..`, absolute, drive, UNC, `:`/stream,
/// reserved device names, trailing dots or spaces and control characters reject the whole archive as
/// `Error::Corrupt`. Links inside the archive are skipped and never created, links already inside
/// `dest_dir` are never followed, and existing files are never overwritten (`name (1).ext`). Limits: 20000
/// entries, 8 GiB of output, and a bomb guard (an entry over 100 MiB and 1000 times its compressed size). Sizes
/// declared by the archive are not trusted: real bytes are counted and each entry's CRC-32 is verified.
/// On any error everything extracted so far is removed.
pub fn unzip_to(zip: &Path, dest_dir: &Path) -> Result<UnzipReport> {
    unzip_with(zip, dest_dir, &UnzipLimits::default())
}

pub fn unzip_with(zip: &Path, dest_dir: &Path, lim: &UnzipLimits) -> Result<UnzipReport> {
    let mut f = File::open(zip).map_err(|e| path_err(zip, e))?;
    let len = f.metadata().map_err(|e| path_err(zip, e))?.len();
    let entries = read_directory(&mut f, len, lim.max_entries)?;

    let mut plan = Vec::with_capacity(entries.len());
    let mut declared = 0u64;
    for (idx, e) in entries.iter().enumerate() {
        let unix_link = e.made_by >> 8 == 3 && (e.attr >> 16) & 0xF000 == 0xA000;
        if unix_link {
            continue; // never create links
        }
        let is_dir = e.name.ends_with('/') || e.name.ends_with('\\');
        let comps = safe_components(&e.name)?;
        if comps.is_empty() {
            if is_dir {
                continue;
            }
            return Err(unsafe_name(&e.name));
        }
        declared = declared.saturating_add(e.size);
        plan.push(Planned { idx, comps, is_dir });
    }
    if declared > lim.max_total {
        return Err(Error::Unsupported(format!(
            "the archive expands to more than {} of uncompressed data",
            crate::convert::human_size(lim.max_total)
        )));
    }

    let existed = match std::fs::symlink_metadata(dest_dir) {
        Ok(md) if md.is_dir() && !is_link(&md) => true,
        Ok(_) => return Err(Error::Io(format!("{} exists and is not a folder", dest_dir.display()))),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => false,
        Err(e) => return Err(path_err(dest_dir, e)),
    };
    if !existed {
        std::fs::create_dir_all(dest_dir).map_err(|e| path_err(dest_dir, e))?;
    }
    let mut created: Vec<(PathBuf, bool)> = Vec::new();
    let result = extract_all(&mut f, len, &entries, &plan, dest_dir, lim, &mut created);
    if result.is_err() {
        for (p, is_dir) in created.iter().rev() {
            let _ = if *is_dir { std::fs::remove_dir(p) } else { std::fs::remove_file(p) };
        }
        if !existed {
            let _ = std::fs::remove_dir(dest_dir);
        }
    }
    result
}

fn extract_all(
    f: &mut File,
    len: u64,
    entries: &[CdEntry],
    plan: &[Planned],
    dest_dir: &Path,
    lim: &UnzipLimits,
    created: &mut Vec<(PathBuf, bool)>,
) -> Result<UnzipReport> {
    let mut seen: HashSet<PathBuf> = HashSet::new();
    let mut buf = vec![0u8; CHUNK];
    let mut rep = UnzipReport::default();
    for p in plan {
        let e = &entries[p.idx];
        if p.is_dir {
            ensure_dir(dest_dir, &p.comps, created, &mut seen)?;
            rep.dirs += 1;
            continue;
        }
        let (last, parents) = p.comps.split_last().expect("file entries have a name");
        let dir = ensure_dir(dest_dir, parents, created, &mut seen)?;
        let (path, out) = create_unique(&dir, last)?;
        created.push((path, false));
        seek_to_data(f, len, e)?;
        let reader = BufReader::with_capacity(CHUNK, Read::by_ref(f).take(e.comp));
        let mut out = BufWriter::with_capacity(CHUNK, out);
        let written = if e.method == 0 {
            copy_checked(reader, &mut out, e, lim, rep.bytes, &mut buf)?
        } else {
            copy_checked(flate2::bufread::DeflateDecoder::new(reader), &mut out, e, lim, rep.bytes, &mut buf)?
        };
        out.flush()?;
        rep.bytes += written;
        rep.files += 1;
    }
    Ok(rep)
}

/// Copies `r` to `out` counting the real bytes, enforcing the limits and verifying size and CRC-32.
fn copy_checked(
    mut r: impl Read,
    out: &mut impl Write,
    e: &CdEntry,
    lim: &UnzipLimits,
    total_so_far: u64,
    buf: &mut [u8],
) -> Result<u64> {
    let mut crc = crc32fast::Hasher::new();
    let mut written = 0u64;
    loop {
        let n = read_some(&mut r, buf).map_err(map_read)?;
        if n == 0 {
            break;
        }
        written += n as u64;
        if written > e.size {
            return Err(bad(format!("entry {:?} is larger than the size its directory record declares", e.name)));
        }
        if total_so_far + written > lim.max_total {
            return Err(Error::Unsupported(format!(
                "the archive expands to more than {} of uncompressed data",
                crate::convert::human_size(lim.max_total)
            )));
        }
        if written > lim.bomb_min && written > lim.bomb_ratio.saturating_mul(e.comp.max(1)) {
            return Err(Error::Unsupported(format!(
                "entry {:?} has an extreme compression ratio (possible zip bomb)",
                e.name
            )));
        }
        crc.update(&buf[..n]);
        out.write_all(&buf[..n])?;
    }
    if written != e.size {
        return Err(bad(format!("entry {:?} is shorter than its directory record declares", e.name)));
    }
    if crc.finalize() != e.crc {
        return Err(bad(format!("checksum mismatch in entry {:?}", e.name)));
    }
    Ok(written)
}

// ================================================================ properties

fn group_thousands(n: u64) -> String {
    let digits = n.to_string();
    let mut out = String::with_capacity(digits.len() + digits.len() / 3);
    for (i, c) in digits.chars().enumerate() {
        if i > 0 && (digits.len() - i) % 3 == 0 {
            out.push(',');
        }
        out.push(c);
    }
    out
}

fn size_text(n: u64) -> String {
    match n {
        1 => "1 byte".into(),
        0..=1023 => format!("{n} bytes"),
        _ => format!("{} ({} bytes)", crate::convert::human_size(n), group_thousands(n)),
    }
}

fn type_label(ext: Option<&str>, head: &[u8]) -> String {
    use super::kind::{detect_head, SourceKind, HEAD_LEN};
    let ext_lc = ext.map(str::to_ascii_lowercase);
    match detect_head(ext, &head[..head.len().min(HEAD_LEN)]) {
        Some(SourceKind::Image(f)) => return format!("{} image", f.label()),
        Some(SourceKind::Pdf) => return "PDF document".into(),
        Some(SourceKind::Docx) => return "Word document".into(),
        Some(SourceKind::Text) => return "Text file".into(),
        _ => {}
    }
    match ext_lc.as_deref() {
        None | Some("") => "File".into(),
        Some("pdf") => "PDF document".into(),
        Some("doc" | "docx" | "docm") => "Word document".into(),
        Some("xls" | "xlsx" | "xlsm") => "Excel workbook".into(),
        Some("ppt" | "pptx" | "pptm") => "PowerPoint presentation".into(),
        Some("zip") => "ZIP archive".into(),
        Some(e) => format!("{} file", e.to_ascii_uppercase()),
    }
}

fn xml_unescape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    while let Some(i) = rest.find('&') {
        out.push_str(&rest[..i]);
        rest = &rest[i..];
        let Some(semi) = rest.find(';').filter(|&k| k <= 10) else {
            out.push('&');
            rest = &rest[1..];
            continue;
        };
        let ent = &rest[1..semi];
        let ch = match ent {
            "amp" => Some('&'),
            "lt" => Some('<'),
            "gt" => Some('>'),
            "quot" => Some('"'),
            "apos" => Some('\''),
            _ => ent
                .strip_prefix("#x")
                .or_else(|| ent.strip_prefix("#X"))
                .and_then(|h| u32::from_str_radix(h, 16).ok())
                .or_else(|| ent.strip_prefix('#').and_then(|d| d.parse().ok()))
                .and_then(char::from_u32),
        };
        match ch {
            Some(c) => {
                out.push(c);
                rest = &rest[semi + 1..];
            }
            None => {
                out.push('&');
                rest = &rest[1..];
            }
        }
    }
    out.push_str(rest);
    out
}

/// Text of the first `<tag ...>text</tag>` element, entity-decoded; `None` for missing, empty or self-closing.
fn xml_tag(xml: &str, tag: &str) -> Option<String> {
    let open = format!("<{tag}");
    let mut from = 0;
    while let Some(i) = xml[from..].find(&open) {
        let at = from + i + open.len();
        match xml.as_bytes().get(at) {
            Some(b'>' | b' ' | b'/' | b'\t' | b'\r' | b'\n') => {
                let gt = at + xml[at..].find('>')?;
                if xml.as_bytes()[gt - 1] == b'/' {
                    return None;
                }
                let end = gt + 1 + xml[gt + 1..].find(&format!("</{tag}>"))?;
                let text = xml_unescape(&xml[gt + 1..end]);
                let text = text.trim();
                return (!text.is_empty()).then(|| text.to_owned());
            }
            _ => from = at,
        }
    }
    None
}

/// Days since 1970-01-01 of a proleptic Gregorian date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2023-11-14T22:13:20Z` (UTC) as local `YYYY-MM-DD HH:MM`; the raw text when it cannot be parsed.
fn ooxml_date(raw: &str) -> String {
    let b = raw.as_bytes();
    let num = |a: usize, z: usize| raw.get(a..z).and_then(|s| s.parse::<i64>().ok());
    let parsed = (|| {
        if b.len() < 19
            || b[4] != b'-'
            || b[7] != b'-'
            || (b[10] != b'T' && b[10] != b' ')
            || b[13] != b':'
            || b[16] != b':'
        {
            return None;
        }
        let (y, mo, d, h, mi, s) = (num(0, 4)?, num(5, 7)?, num(8, 10)?, num(11, 13)?, num(14, 16)?, num(17, 19)?);
        let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + s;
        let t = if secs >= 0 {
            UNIX_EPOCH + Duration::from_secs(secs as u64)
        } else {
            UNIX_EPOCH - Duration::from_secs(secs.unsigned_abs())
        };
        local_time(t)
    })();
    parsed.map_or_else(|| raw.to_owned(), |tm| tm.stamp())
}

/// Inflates a small entry fully, refusing anything over `cap` bytes.
fn read_entry(f: &mut File, len: u64, e: &CdEntry, cap: u64) -> Option<Vec<u8>> {
    if e.size > cap {
        return None;
    }
    seek_to_data(f, len, e).ok()?;
    let src = Read::by_ref(f).take(e.comp);
    let mut out = Vec::with_capacity(e.size as usize);
    if e.method == 0 {
        BufReader::new(src).take(cap + 1).read_to_end(&mut out).ok()?;
    } else {
        flate2::bufread::DeflateDecoder::new(BufReader::new(src)).take(cap + 1).read_to_end(&mut out).ok()?;
    }
    (out.len() as u64 <= cap).then_some(out)
}

fn ooxml_properties(path: &Path, ext: &str, out: &mut Vec<(&'static str, String)>) -> Option<()> {
    const CAP: u64 = 1 << 20;
    let mut f = File::open(path).ok()?;
    let len = f.metadata().ok()?.len();
    let entries = read_directory(&mut f, len, 100_000).ok()?;
    let mut load = |name: &str| -> Option<String> {
        let e = entries.iter().find(|e| e.name == name)?;
        String::from_utf8(read_entry(&mut f, len, e, CAP)?).ok()
    };
    let core = load("docProps/core.xml");
    let app = load("docProps/app.xml");
    let mut put = |key: &'static str, v: Option<String>| {
        if let Some(v) = v {
            out.push((key, v));
        }
    };
    if let Some(c) = &core {
        put("title", xml_tag(c, "dc:title"));
        put("author", xml_tag(c, "dc:creator"));
        put("last-modified-by", xml_tag(c, "cp:lastModifiedBy"));
        put("doc-created", xml_tag(c, "dcterms:created").map(|d| ooxml_date(&d)));
        put("doc-modified", xml_tag(c, "dcterms:modified").map(|d| ooxml_date(&d)));
    }
    if let Some(a) = &app {
        put("company", xml_tag(a, "Company"));
        put("pages", xml_tag(a, "Pages"));
    }
    if ext.starts_with("xls") {
        let n =
            entries.iter().filter(|e| e.name.starts_with("xl/worksheets/sheet") && e.name.ends_with(".xml")).count();
        if n > 0 {
            put("sheets", Some(n.to_string()));
        }
    }
    if let Some(a) = &app {
        put("slides", xml_tag(a, "Slides"));
    }
    Some(())
}

/// Decodes the bytes of a PDF string: UTF-16BE/LE with BOM, UTF-8 with BOM, otherwise Latin-1 (PDFDocEncoding).
fn pdf_text(bytes: &[u8]) -> String {
    let s = if let Some(b) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        char::decode_utf16(b.chunks_exact(2).map(|c| u16::from_be_bytes([c[0], c[1]])))
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect()
    } else if let Some(b) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        char::decode_utf16(b.chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])))
            .map(|r| r.unwrap_or('\u{FFFD}'))
            .collect()
    } else if let Some(b) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        String::from_utf8_lossy(b).into_owned()
    } else {
        bytes.iter().map(|&b| b as char).collect()
    };
    s.chars().filter(|c| !c.is_control() || *c == ' ').take(512).collect::<String>().trim().to_owned()
}

/// Parses the PDF string starting at `b[0]` (`(literal)` or `<hex>`).
fn pdf_string(b: &[u8]) -> Option<String> {
    match *b.first()? {
        b'(' => {
            let (mut out, mut depth, mut i) = (Vec::new(), 1u32, 1);
            while i < b.len() && out.len() < 4096 {
                let c = b[i];
                i += 1;
                match c {
                    b'(' => {
                        depth += 1;
                        out.push(c);
                    }
                    b')' => {
                        depth -= 1;
                        if depth == 0 {
                            return Some(pdf_text(&out));
                        }
                        out.push(c);
                    }
                    b'\\' => {
                        let e = *b.get(i)?;
                        i += 1;
                        match e {
                            b'n' => out.push(b'\n'),
                            b'r' => out.push(b'\r'),
                            b't' => out.push(b'\t'),
                            b'b' => out.push(8),
                            b'f' => out.push(12),
                            b'0'..=b'7' => {
                                let mut v = u32::from(e - b'0');
                                for _ in 0..2 {
                                    match b.get(i) {
                                        Some(d @ b'0'..=b'7') => {
                                            v = v * 8 + u32::from(d - b'0');
                                            i += 1;
                                        }
                                        _ => break,
                                    }
                                }
                                out.push(v as u8);
                            }
                            b'\r' => {
                                if b.get(i) == Some(&b'\n') {
                                    i += 1;
                                }
                            }
                            b'\n' => {}
                            other => out.push(other),
                        }
                    }
                    _ => out.push(c),
                }
            }
            None
        }
        b'<' if b.get(1) != Some(&b'<') => {
            let end = b.iter().position(|&c| c == b'>')?;
            let digits: Vec<u8> = b[1..end].iter().copied().filter(|c| c.is_ascii_hexdigit()).collect();
            let bytes: Vec<u8> = digits
                .chunks(2)
                .take(4096)
                .map(|p| {
                    let hi = (p[0] as char).to_digit(16).unwrap_or(0) as u8;
                    let lo = p.get(1).map_or(0, |&c| (c as char).to_digit(16).unwrap_or(0) as u8);
                    hi * 16 + lo
                })
                .collect();
            Some(pdf_text(&bytes))
        }
        _ => None,
    }
}

fn find_last(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).rposition(|w| w == needle)
}

/// The `/Info N G R` reference of the last trailer-like occurrence in `hay`.
fn info_ref(hay: &[u8]) -> Option<(u64, u64)> {
    fn skip_ws(h: &[u8], mut i: usize) -> usize {
        while h.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        i
    }
    fn number(h: &[u8], i: usize) -> Option<(u64, usize)> {
        let len = h[i..].iter().take_while(|c| c.is_ascii_digit()).count();
        let v = std::str::from_utf8(&h[i..i + len]).ok()?.parse::<u64>().ok()?;
        Some((v, i + len))
    }
    let mut end = hay.len();
    while let Some(i) = find_last(&hay[..end], b"/Info") {
        end = i;
        let at = skip_ws(hay, i + 5);
        let Some((num, at)) = number(hay, at) else { continue };
        let at = skip_ws(hay, at);
        let Some((generation, at)) = number(hay, at) else { continue };
        let at = skip_ws(hay, at);
        if hay.get(at) == Some(&b'R') {
            return Some((num, generation));
        }
    }
    None
}

/// Slice of the object `num gen obj ... endobj` (last definition wins), capped at 16 KiB.
fn object_body(hay: &[u8], num: u64, generation: u64) -> Option<&[u8]> {
    let needle = format!("{num} {generation} obj");
    let n = needle.as_bytes();
    let mut end = hay.len();
    while let Some(i) = find_last(&hay[..end], n) {
        end = i;
        let boundary = i == 0 || !hay[i - 1].is_ascii_digit();
        if boundary {
            let body = &hay[i + n.len()..];
            let stop = body.windows(6).position(|w| w == b"endobj").unwrap_or(body.len()).min(16 * 1024);
            return Some(&body[..stop]);
        }
    }
    None
}

fn pdf_info_value(dict: &[u8], key: &str) -> Option<String> {
    let pat = format!("/{key}");
    let p = pat.as_bytes();
    let mut from = 0;
    while let Some(i) = dict[from..].windows(p.len()).position(|w| w == p) {
        let at = from + i + p.len();
        let next = dict.get(at).copied().unwrap_or(b' ');
        if next.is_ascii_alphanumeric() || next == b'_' || next == b'-' || next == b'.' {
            from = at;
            continue;
        }
        let rest = &dict[at..];
        let skip = rest.iter().take_while(|c| c.is_ascii_whitespace()).count();
        return pdf_string(&rest[skip..]).filter(|s| !s.is_empty());
    }
    None
}

fn pdf_properties(path: &Path, len: u64, head: &[u8], out: &mut Vec<(&'static str, String)>) -> Option<()> {
    const TAIL: u64 = 1 << 20;
    if len <= 16 << 20 {
        if let Ok(bytes) = std::fs::read(path) {
            if let Ok(n) = super::os::pdf_page_count(&bytes) {
                out.push(("pages", n.to_string()));
            }
        }
    }
    let mut f = File::open(path).ok()?;
    let take = len.min(TAIL);
    f.seek(SeekFrom::Start(len - take)).ok()?;
    let mut tail = vec![0u8; take as usize];
    f.read_exact(&mut tail).ok()?;
    let bufs: [&[u8]; 2] = [&tail, head];
    let (num, generation) = bufs.iter().find_map(|b| info_ref(b))?;
    let body = bufs.iter().find_map(|b| object_body(b, num, generation))?;
    for (id, key) in [("title", "Title"), ("author", "Author"), ("creator", "Creator"), ("producer", "Producer")] {
        if let Some(v) = pdf_info_value(body, key) {
            out.push((id, v));
        }
    }
    Some(())
}

/// Facts about any file, in display order, as `(id, value)` pairs: `name`, `type`, `size`, `created`,
/// `modified` (local `YYYY-MM-DD HH:MM`), then for OOXML documents (docx/xlsx/pptx) `title`, `author`,
/// `last-modified-by`, `doc-created`, `doc-modified`, `company`, `pages`, `sheets`, `slides`, and for PDFs
/// `pages`, `title`, `author`, `creator`, `producer`. Values that are missing are left out, never
/// "unknown". Damaged files only lose their document fields; nothing here panics on bad input.
pub fn properties(path: &Path) -> Result<Vec<(&'static str, String)>> {
    let md = std::fs::metadata(path).map_err(|e| path_err(path, e))?;
    let mut out: Vec<(&'static str, String)> = Vec::with_capacity(12);
    out.push((
        "name",
        path.file_name().map_or_else(|| path.display().to_string(), |n| n.to_string_lossy().into_owned()),
    ));
    let stamps = |out: &mut Vec<(&'static str, String)>| {
        if let Some(tm) = md.created().ok().and_then(local_time) {
            out.push(("created", tm.stamp()));
        }
        if let Some(tm) = md.modified().ok().and_then(local_time) {
            out.push(("modified", tm.stamp()));
        }
    };
    if md.is_dir() {
        out.push(("type", "Folder".into()));
        stamps(&mut out);
        return Ok(out);
    }
    let ext = path.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    let mut head = Vec::with_capacity(SAMPLE);
    File::open(path)
        .map_err(|e| path_err(path, e))?
        .take(SAMPLE as u64)
        .read_to_end(&mut head)
        .map_err(|e| path_err(path, e))?;
    out.push(("type", type_label(ext.as_deref(), &head)));
    out.push(("size", size_text(md.len())));
    stamps(&mut out);
    match ext.as_deref() {
        Some(e @ ("docx" | "docm" | "xlsx" | "xlsm" | "pptx" | "pptm")) if head.starts_with(b"PK") => {
            let mut doc = Vec::new();
            let _ = ooxml_properties(path, e, &mut doc);
            out.extend(doc);
        }
        _ if head.windows(5).take(1024).any(|w| w == b"%PDF-") => {
            let mut doc = Vec::new();
            let _ = pdf_properties(path, md.len(), &head, &mut doc);
            out.extend(doc);
        }
        _ => {}
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::zip::{ZipArchive, ZipWriter};
    use std::alloc::{GlobalAlloc, Layout, System};
    use std::cell::Cell;
    use std::io::Write;

    // ------------------------------------------------------------ allocation tracking (memory-flat proofs)

    thread_local! {
        static CUR: Cell<isize> = const { Cell::new(0) };
        static PEAK: Cell<isize> = const { Cell::new(0) };
    }

    struct Track;
    // SAFETY: forwards to the system allocator and only touches const-initialised thread-local counters.
    unsafe impl GlobalAlloc for Track {
        unsafe fn alloc(&self, l: Layout) -> *mut u8 {
            let p = System.alloc(l);
            if !p.is_null() {
                CUR.with(|c| {
                    c.set(c.get() + l.size() as isize);
                    PEAK.with(|k| k.set(k.get().max(c.get())));
                });
            }
            p
        }
        unsafe fn dealloc(&self, p: *mut u8, l: Layout) {
            System.dealloc(p, l);
            CUR.with(|c| c.set(c.get() - l.size() as isize));
        }
    }
    #[global_allocator]
    static ALLOC: Track = Track;

    /// Runs `f` and returns its result with the peak heap growth (bytes) seen on this thread meanwhile.
    fn peak_during<R>(f: impl FnOnce() -> R) -> (R, isize) {
        CUR.with(|c| c.set(0));
        PEAK.with(|k| k.set(0));
        let r = f();
        (r, PEAK.with(|k| k.get()))
    }

    // ------------------------------------------------------------ helpers

    struct Tmp(PathBuf);
    impl Tmp {
        fn new(name: &str) -> Tmp {
            let d = std::env::temp_dir().join(format!("radial-fileops-{}-{}", std::process::id(), name));
            let _ = std::fs::remove_dir_all(&d);
            std::fs::create_dir_all(&d).unwrap();
            Tmp(d)
        }
        fn p(&self, rel: &str) -> PathBuf {
            self.0.join(rel)
        }
        fn write(&self, rel: &str, data: &[u8]) -> PathBuf {
            let p = self.p(rel);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(&p, data).unwrap();
            p
        }
    }
    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// Creates a directory junction (no privileges needed); false when the volume refuses.
    fn junction(link: &Path, target: &Path) -> bool {
        let win = |p: &Path| p.to_string_lossy().replace('/', "\\");
        std::process::Command::new("cmd")
            .args(["/C", "mklink", "/J", &win(link), &win(target)])
            .output()
            .map(|o| o.status.success())
            .unwrap_or(false)
    }

    fn lcg(seed: &mut u64) -> u8 {
        *seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
        (*seed >> 33) as u8
    }
    fn random_bytes(n: usize, mut seed: u64) -> Vec<u8> {
        (0..n).map(|_| lcg(&mut seed)).collect()
    }
    fn pattern(n: usize) -> Vec<u8> {
        (0..n).map(|i| (i % 251) as u8).collect()
    }

    /// Raw central-directory rows, parsed independently of the code under test.
    #[derive(Debug)]
    struct Row {
        name: String,
        flags: u16,
        method: u16,
        time: u16,
        date: u16,
        crc: u32,
        comp: u32,
        size: u32,
    }
    fn rows(z: &[u8]) -> Vec<Row> {
        let u16at = |i: usize| u16::from_le_bytes([z[i], z[i + 1]]);
        let u32at = |i: usize| u32::from_le_bytes([z[i], z[i + 1], z[i + 2], z[i + 3]]);
        let eocd = (0..z.len() - 3).rev().find(|&i| z[i..i + 4] == [0x50, 0x4b, 5, 6]).unwrap();
        let n = u16at(eocd + 10) as usize;
        let mut pos = u32at(eocd + 16) as usize;
        let mut out = Vec::new();
        for _ in 0..n {
            assert_eq!(u32at(pos), 0x0201_4b50);
            let nl = u16at(pos + 28) as usize;
            let xl = u16at(pos + 30) as usize;
            let cl = u16at(pos + 32) as usize;
            out.push(Row {
                name: String::from_utf8(z[pos + 46..pos + 46 + nl].to_vec()).unwrap(),
                flags: u16at(pos + 8),
                method: u16at(pos + 10),
                time: u16at(pos + 12),
                date: u16at(pos + 14),
                crc: u32at(pos + 16),
                comp: u32at(pos + 20),
                size: u32at(pos + 24),
            });
            pos += 46 + nl + xl + cl;
        }
        out
    }
    fn names(rows: &[Row]) -> Vec<&str> {
        rows.iter().map(|r| r.name.as_str()).collect()
    }
    fn leftovers(dir: &Path) -> Vec<String> {
        let mut v: Vec<String> =
            std::fs::read_dir(dir).unwrap().map(|e| e.unwrap().file_name().to_string_lossy().into_owned()).collect();
        v.sort();
        v
    }

    // ------------------------------------------------------------ SHA-256

    #[test]
    fn sha256_matches_the_nist_vectors() {
        assert_eq!(sha256_hex(b""), "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855");
        assert_eq!(sha256_hex(b"abc"), "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad");
        assert_eq!(
            sha256_hex(b"abcdbcdecdefdefgefghfghighijhijkijkljklmklmnlmnomnopnopq"),
            "248d6a61d20638b8e5c026930c3e6039a33ce45964ff2167f6ecedd419db06c1"
        );
        let million = vec![b'a'; 1_000_000];
        assert_eq!(sha256_hex(&million), "cdc76e5c9914fb9281a1c7e284d73e67f1809a48a497200e046d39ccc7112cd0");
    }

    #[test]
    fn sha256_is_right_around_the_64_byte_block_boundary() {
        let cases = [
            (55, "9f4390f8d30c2dd92ec9f095b65e2b9ae9b0a925a5258e241c9f1e910f734318"),
            (56, "b35439a4ac6f0948b6d6f9e3c6af0f5f590ce20f1bde7090ef7970686ec6738a"),
            (64, "ffe054fe7ae0cb6dc65c3af9b61d5209f439851db43d0ba5997337df154668eb"),
            (65, "635361c48bb9eab14198e76ea8ab7f1a41685d6ad62aa9146d301d4f17eb0ae0"),
            (128, "6836cf13bac400e9105071cd6af47084dfacad4e5e302c94bfed24e013afb73e"),
        ];
        for (n, want) in cases {
            assert_eq!(sha256_hex(&vec![b'a'; n]), want, "{n} bytes");
        }
    }

    #[test]
    fn sha256_streaming_in_odd_chunks_equals_one_shot() {
        let data = pattern(10_000);
        for step in [1usize, 7, 63, 64, 65, 1000] {
            let mut h = Sha256::new();
            for c in data.chunks(step) {
                h.update(c);
            }
            assert_eq!(hex(&h.finish()), sha256_hex(&data), "step {step}");
        }
    }

    #[test]
    fn sha256_file_streams_files_bigger_than_the_buffer_and_on_block_boundaries() {
        let t = Tmp::new("sha-file");
        let cases = [
            (64, "fdeab9acf3710362bd2658cdc9a29e8f9c757fcf9811603a8c447cd1d9151108"),
            (262_144, "31a1f9dea0169551092d05e8bf4a446228c8c3eb4c9b713c66adcb7fd53c89be"),
            (262_145, "6a102a35ef13d267bf487c1520d82b4ff541787bc8c288ccf98aeebe7686016a"),
            (2 * 262_144 + 17, "ef0be44b34790245e8531fcac682bd90c1e9f5e055f04a4127343e31735470e9"),
        ];
        for (n, want) in cases {
            let p = t.write("f.bin", &pattern(n));
            assert_eq!(sha256_file(&p).unwrap(), want, "{n} bytes");
        }
        let empty = t.write("empty", b"");
        assert_eq!(sha256_file(&empty).unwrap(), sha256_hex(b""));
    }

    #[test]
    fn sha256_file_uses_constant_memory() {
        let t = Tmp::new("sha-mem");
        let p = t.write("big.bin", &pattern(8 * 1024 * 1024));
        let (r, peak) = peak_during(|| sha256_file(&p));
        assert_eq!(r.unwrap(), sha256_hex(&pattern(8 * 1024 * 1024)));
        assert!(peak < 1024 * 1024, "peak heap {peak}");
    }

    #[test]
    fn sha256_file_reports_missing_files_as_io() {
        let t = Tmp::new("sha-missing");
        assert!(matches!(sha256_file(&t.p("nope")), Err(Error::Io(_))));
    }

    // ------------------------------------------------------------ zip_paths

    #[test]
    fn zipping_a_folder_keeps_its_name_and_recurses() {
        let t = Tmp::new("zip-folder");
        t.write("Photos/a.txt", b"hello hello hello hello");
        t.write("Photos/sub/b.txt", b"second");
        std::fs::create_dir_all(t.p("Photos/empty dir")).unwrap();
        let dest = t.p("out.zip");
        let rep = zip_paths(&[t.p("Photos")], &dest).unwrap();
        let z = std::fs::read(&dest).unwrap();
        let r = rows(&z);
        let mut n = names(&r);
        n.sort();
        assert_eq!(n, ["Photos/a.txt", "Photos/empty dir/", "Photos/sub/b.txt"]);
        assert_eq!((rep.files, rep.dirs, rep.skipped), (2, 1, 0));
        assert_eq!(rep.bytes_in, 23 + 6);
        assert_eq!(rep.bytes_out, z.len() as u64);
        let a = ZipArchive::new(&z).unwrap();
        assert_eq!(a.read("Photos/a.txt", 100).unwrap().unwrap(), b"hello hello hello hello");
        assert_eq!(a.read("Photos/sub/b.txt", 100).unwrap().unwrap(), b"second");
        assert_eq!(leftovers(&t.0), ["Photos", "out.zip"], "no temp file may remain");
    }

    #[test]
    fn several_files_get_bare_names_and_clashes_a_suffix() {
        let t = Tmp::new("zip-bare");
        let a = t.write("one/a.txt", b"1");
        let b = t.write("two/a.txt", b"2");
        let c = t.write("three/A.TXT", b"3");
        let d = t.write("two/folder/x", b"4");
        let dest = t.p("out.zip");
        zip_paths(&[a, b, c, d.parent().unwrap().to_path_buf()], &dest).unwrap();
        let z = std::fs::read(&dest).unwrap();
        let r = rows(&z);
        assert_eq!(names(&r), ["a.txt", "a (1).txt", "A (2).TXT", "folder/x"]);
        let arch = ZipArchive::new(&z).unwrap();
        assert_eq!(arch.read("a (1).txt", 10).unwrap().unwrap(), b"2");
    }

    #[test]
    fn entries_use_utf8_data_descriptors_and_the_files_mtime() {
        let t = Tmp::new("zip-meta");
        let p = t.write("niño con espacio.txt", "contenido ñ".as_bytes());
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(when).unwrap();
        let dest = t.p("out.zip");
        zip_paths(&[p], &dest).unwrap();
        let z = std::fs::read(&dest).unwrap();
        let r = rows(&z);
        assert_eq!(r.len(), 1);
        let row = &r[0];
        assert_eq!(row.name, "niño con espacio.txt");
        assert_ne!(row.flags & (1 << 11), 0, "UTF-8 flag");
        assert_ne!(row.flags & (1 << 3), 0, "data descriptor flag");
        let tm = local_time(when).unwrap();
        assert_eq!(row.date, ((tm.year - 1980) << 9) | (tm.month << 5) | tm.day);
        assert_eq!(row.time, (tm.hour << 11) | (tm.minute << 5) | (tm.second / 2));
        // Local header carries zero sizes, the descriptor after the data carries the real ones.
        assert_eq!(u32::from_le_bytes(z[14..18].try_into().unwrap()), 0);
        assert_eq!(row.crc, crc32fast::hash("contenido ñ".as_bytes()));
        assert_eq!(row.size as usize, "contenido ñ".len());
        let desc = z.windows(4).position(|w| w == [0x50, 0x4b, 7, 8]).expect("descriptor");
        assert_eq!(u32::from_le_bytes(z[desc + 4..desc + 8].try_into().unwrap()), row.crc);
        assert_eq!(u32::from_le_bytes(z[desc + 8..desc + 12].try_into().unwrap()), row.comp);
        assert_eq!(u32::from_le_bytes(z[desc + 12..desc + 16].try_into().unwrap()), row.size);
    }

    #[test]
    fn empty_files_and_empty_folders_are_kept() {
        let t = Tmp::new("zip-empty");
        t.write("d/empty.txt", b"");
        std::fs::create_dir_all(t.p("d/nothing")).unwrap();
        let dest = t.p("out.zip");
        let rep = zip_paths(&[t.p("d")], &dest).unwrap();
        let z = std::fs::read(&dest).unwrap();
        let r = rows(&z);
        let mut n = names(&r);
        n.sort();
        assert_eq!(n, ["d/empty.txt", "d/nothing/"]);
        assert_eq!((rep.files, rep.dirs, rep.bytes_in), (1, 1, 0));
        let a = ZipArchive::new(&z).unwrap();
        assert_eq!(a.read("d/empty.txt", 10).unwrap().unwrap(), b"");
    }

    #[test]
    fn repetitive_data_shrinks_random_and_media_data_is_stored() {
        let t = Tmp::new("zip-method");
        let rep = b"the quick brown fox jumps over the lazy dog\n".repeat(5 * 1024 * 1024 / 44 + 1);
        let rep = rep[..5 * 1024 * 1024].to_vec();
        let rnd = random_bytes(300_000, 7);
        let a = t.write("text.txt", &rep);
        let b = t.write("noise.bin", &rnd);
        let c = t.write("photo.JPG", &rep[..100_000]);
        let dest = t.p("out.zip");
        let r = zip_paths(&[a, b, c], &dest).unwrap();
        let z = std::fs::read(&dest).unwrap();
        let rw = rows(&z);
        assert_eq!((rw[0].method, rw[1].method, rw[2].method), (8, 0, 0));
        assert!((rw[0].comp as usize) < rep.len() / 20, "5 MiB of repetition must shrink: {}", rw[0].comp);
        assert_eq!(rw[1].comp as usize, rnd.len());
        assert_eq!(rw[2].comp, 100_000);
        assert_eq!(r.bytes_in as usize, rep.len() + rnd.len() + 100_000);
        let arch = ZipArchive::new(&z).unwrap();
        assert!(arch.read("text.txt", 6 << 20).unwrap().unwrap() == rep);
        assert!(arch.read("noise.bin", 1 << 20).unwrap().unwrap() == rnd);
    }

    #[test]
    fn an_existing_destination_is_refused_and_untouched() {
        let t = Tmp::new("zip-exists");
        let f = t.write("a.txt", b"x");
        let dest = t.write("out.zip", b"precious");
        assert!(matches!(zip_paths(&[f], &dest), Err(Error::Io(_))));
        assert_eq!(std::fs::read(&dest).unwrap(), b"precious");
        assert_eq!(leftovers(&t.0), ["a.txt", "out.zip"]);
    }

    #[test]
    fn a_failure_removes_the_temporary_file_and_publishes_nothing() {
        let t = Tmp::new("zip-fail");
        t.write("d/a", b"1");
        t.write("d/b", b"2");
        t.write("d/c", b"3");
        let dest = t.p("out.zip");
        let lim = ZipLimits { max_entries: 2, ..ZipLimits::default() };
        let e = zip_paths_with(&[t.p("d")], &dest, &lim).unwrap_err();
        assert!(matches!(e, Error::Unsupported(ref m) if m.contains("entries")), "{e}");
        assert_eq!(leftovers(&t.0), ["d"]);
        // A byte budget that is crossed while streaming also cleans up.
        let lim = ZipLimits { max_bytes: 1, ..ZipLimits::default() };
        assert!(matches!(zip_paths_with(&[t.p("d")], &dest, &lim), Err(Error::Unsupported(_))));
        assert_eq!(leftovers(&t.0), ["d"]);
        // Missing parent for the destination is a plain I/O error.
        assert!(matches!(zip_paths(&[t.p("d")], &t.p("no/such/out.zip")), Err(Error::Io(_))));
    }

    #[test]
    fn archives_over_4_gib_or_65535_entries_are_refused_before_writing() {
        let t = Tmp::new("zip-zip64");
        let big = t.p("huge.bin");
        let f = std::fs::File::create(&big).unwrap();
        f.set_len(4 * 1024 * 1024 * 1024 + 1).unwrap();
        drop(f);
        let dest = t.p("out.zip");
        let e = zip_paths(&[big], &dest).unwrap_err();
        assert!(matches!(e, Error::Unsupported(ref m) if m.contains("4 GiB")), "{e}");
        assert_eq!(leftovers(&t.0), ["huge.bin"], "nothing may be created");
        assert_eq!(ZipLimits::default().max_entries, 65_535);
    }

    #[test]
    fn the_destination_inside_the_zipped_folder_is_not_zipped() {
        let t = Tmp::new("zip-self");
        t.write("d/a.txt", b"1");
        let dest = t.p("d/out.zip");
        let rep = zip_paths(&[t.p("d")], &dest).unwrap();
        let r = rows(&std::fs::read(&dest).unwrap());
        assert_eq!(names(&r), ["d/a.txt"]);
        assert_eq!(rep.files, 1);
        assert_eq!(leftovers(&t.p("d")), ["a.txt", "out.zip"]);
    }

    #[test]
    fn unreadable_missing_and_junction_inputs_are_skipped_and_counted() {
        use std::os::windows::fs::OpenOptionsExt;
        let t = Tmp::new("zip-skip");
        let ok = t.write("ok.txt", b"ok");
        let locked = t.write("locked.txt", b"secret");
        let _hold = std::fs::OpenOptions::new().read(true).share_mode(0).open(&locked).unwrap();
        t.write("target/inner.txt", b"inside the junction target");
        let mut expected_skipped = 2; // locked + missing
        let link = t.p("d/link");
        std::fs::create_dir_all(t.p("d")).unwrap();
        let made = junction(&link, &t.p("target"));
        t.write("d/keep.txt", b"keep");
        if made {
            expected_skipped += 1;
        }
        let dest = t.p("out.zip");
        let rep = zip_paths(&[ok, locked, t.p("missing.txt"), t.p("d")], &dest).unwrap();
        let z = std::fs::read(&dest).unwrap();
        let r = rows(&z);
        assert_eq!(names(&r), ["ok.txt", "d/keep.txt"]);
        assert_eq!(rep.skipped, expected_skipped);
        assert_eq!(rep.files, 2);
        if made {
            std::fs::remove_dir(&link).unwrap(); // removes the junction only, never its target
        }
    }

    #[test]
    fn zipping_a_forty_mib_file_keeps_memory_flat() {
        let t = Tmp::new("zip-mem");
        let p = t.p("big.txt");
        {
            let mut f = std::io::BufWriter::new(std::fs::File::create(&p).unwrap());
            let line = b"memory must stay flat no matter how large the input file is\n";
            let block = line.repeat(1024 * 1024 / line.len());
            for _ in 0..40 {
                f.write_all(&block).unwrap();
            }
        }
        let dest = t.p("out.zip");
        let (rep, peak) = peak_during(|| zip_paths(std::slice::from_ref(&p), &dest).unwrap());
        assert!(rep.bytes_in >= 40 * 1000 * 1000);
        assert!(rep.bytes_out < rep.bytes_in / 20);
        assert!(peak < 8 * 1024 * 1024, "peak heap while zipping: {peak}");
        let out = t.p("out");
        let (rep2, peak2) = peak_during(|| unzip_to(&dest, &out).unwrap());
        assert_eq!(rep2.bytes, rep.bytes_in);
        assert!(peak2 < 8 * 1024 * 1024, "peak heap while unzipping: {peak2}");
        assert_eq!(std::fs::metadata(out.join("big.txt")).unwrap().len(), rep.bytes_in);
    }

    // ------------------------------------------------------------ crafted archives

    #[derive(Clone)]
    struct E {
        name: Vec<u8>,
        body: Vec<u8>,
        method: u16,
        flags: u16,
        crc: u32,
        size: u32,
        comp: u32,
        made_by: u16,
        attr: u32,
    }
    impl E {
        fn stored(name: &str, data: &[u8]) -> E {
            E {
                name: name.as_bytes().to_vec(),
                body: data.to_vec(),
                method: 0,
                flags: 1 << 11,
                crc: crc32fast::hash(data),
                size: data.len() as u32,
                comp: data.len() as u32,
                made_by: 20,
                attr: 0,
            }
        }
        fn deflated(name: &str, data: &[u8]) -> E {
            let mut enc = flate2::write::DeflateEncoder::new(Vec::new(), flate2::Compression::new(6));
            enc.write_all(data).unwrap();
            let body = enc.finish().unwrap();
            let mut e = E::stored(name, data);
            e.method = 8;
            e.comp = body.len() as u32;
            e.body = body;
            e
        }
    }
    fn build(es: &[E]) -> Vec<u8> {
        let mut out = Vec::new();
        let mut cd = Vec::new();
        for e in es {
            let off = out.len() as u32;
            out.extend_from_slice(&0x0403_4b50u32.to_le_bytes());
            for v in [20u16, e.flags, e.method, 0, 0x21] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            for v in [e.crc, e.comp, e.size] {
                out.extend_from_slice(&v.to_le_bytes());
            }
            out.extend_from_slice(&(e.name.len() as u16).to_le_bytes());
            out.extend_from_slice(&0u16.to_le_bytes());
            out.extend_from_slice(&e.name);
            out.extend_from_slice(&e.body);
            cd.extend_from_slice(&0x0201_4b50u32.to_le_bytes());
            for v in [e.made_by, 20, e.flags, e.method, 0, 0x21] {
                cd.extend_from_slice(&v.to_le_bytes());
            }
            for v in [e.crc, e.comp, e.size] {
                cd.extend_from_slice(&v.to_le_bytes());
            }
            for v in [e.name.len() as u16, 0, 0, 0, 0] {
                cd.extend_from_slice(&v.to_le_bytes());
            }
            cd.extend_from_slice(&e.attr.to_le_bytes());
            cd.extend_from_slice(&off.to_le_bytes());
            cd.extend_from_slice(&e.name);
        }
        let cd_off = out.len() as u32;
        out.extend_from_slice(&cd);
        out.extend_from_slice(&0x0605_4b50u32.to_le_bytes());
        out.extend_from_slice(&[0, 0, 0, 0]);
        out.extend_from_slice(&(es.len() as u16).to_le_bytes());
        out.extend_from_slice(&(es.len() as u16).to_le_bytes());
        out.extend_from_slice(&(cd.len() as u32).to_le_bytes());
        out.extend_from_slice(&cd_off.to_le_bytes());
        out.extend_from_slice(&0u16.to_le_bytes());
        out
    }

    /// Unzips `bytes` into `root/sandbox/out`; returns the result and the sandbox (cleaned on drop).
    fn run(tag: &str, bytes: &[u8]) -> (Result<UnzipReport>, Tmp) {
        run_with(tag, bytes, &UnzipLimits::default())
    }
    fn run_with(tag: &str, bytes: &[u8], lim: &UnzipLimits) -> (Result<UnzipReport>, Tmp) {
        let t = Tmp::new(tag);
        // The archive lives one level above the output so a zip-slip would be visible next to it.
        let zip = t.write("in.zip", bytes);
        std::fs::create_dir_all(t.p("sandbox")).unwrap();
        let r = unzip_with(&zip, &t.p("sandbox/out"), lim);
        (r, t)
    }
    fn assert_rejected(tag: &str, name: &str) {
        let (r, t) = run(tag, &build(&[E::stored("fine.txt", b"ok"), E::stored(name, b"evil")]));
        match r {
            Err(Error::Corrupt(m)) => assert!(m.contains("unsafe"), "{name:?}: {m}"),
            other => panic!("{name:?} must be rejected as Corrupt, got {other:?}"),
        }
        assert_eq!(leftovers(&t.0), ["in.zip", "sandbox"], "{name:?} left files outside");
        assert!(leftovers(&t.p("sandbox")).is_empty(), "{name:?}: partial output must be removed");
    }

    // ------------------------------------------------------------ unzip_to: behaviour

    #[test]
    fn zip_then_unzip_round_trips_names_content_and_empty_entries() {
        let t = Tmp::new("round");
        let uni = "Mi carpeta ñandú/documento número 1 – final.txt";
        t.write(&format!("src/{uni}"), "contenido: áéíóú ✓".as_bytes());
        t.write("src/Mi carpeta ñandú/empty.bin", b"");
        t.write("src/Mi carpeta ñandú/deep/er/data.bin", &random_bytes(70_000, 3));
        std::fs::create_dir_all(t.p("src/Mi carpeta ñandú/vacía")).unwrap();
        let zip = t.p("a.zip");
        zip_paths(&[t.p("src/Mi carpeta ñandú")], &zip).unwrap();
        let out = t.p("out");
        let rep = unzip_to(&zip, &out).unwrap();
        let base = out.join("Mi carpeta ñandú");
        let text = "contenido: áéíóú ✓".as_bytes();
        assert_eq!(std::fs::read(base.join("documento número 1 – final.txt")).unwrap(), text);
        assert_eq!(std::fs::read(base.join("empty.bin")).unwrap(), b"");
        assert_eq!(std::fs::read(base.join("deep/er/data.bin")).unwrap(), random_bytes(70_000, 3));
        assert!(base.join("vacía").is_dir());
        assert_eq!(rep.files, 3);
        assert!(rep.dirs >= 1);
        assert_eq!(rep.bytes, 70_000 + text.len() as u64);
    }

    #[test]
    fn reads_an_archive_made_by_another_implementation() {
        let t = Tmp::new("foreign");
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/sample.zip"));
        let rep = unzip_to(fixture, &t.p("o")).unwrap();
        assert_eq!(std::fs::read(t.p("o/hello.txt")).unwrap(), b"hello from another zip writer\n");
        assert_eq!(rep.files, 1);
    }

    #[test]
    fn never_overwrites_and_suffixes_clashes_even_inside_the_archive() {
        let t = Tmp::new("clash");
        t.write("out/a.txt", b"mine");
        let es =
            [E::stored("a.txt", b"one"), E::stored("A.TXT", b"two"), E::stored("d/b", b"3"), E::stored("d/b", b"4")];
        let zip = t.write("in.zip", &build(&es));
        let rep = unzip_to(&zip, &t.p("out")).unwrap();
        assert_eq!(std::fs::read(t.p("out/a.txt")).unwrap(), b"mine");
        assert_eq!(std::fs::read(t.p("out/a (1).txt")).unwrap(), b"one");
        assert_eq!(std::fs::read(t.p("out/A (2).TXT")).unwrap(), b"two");
        assert_eq!(std::fs::read(t.p("out/d/b")).unwrap(), b"3");
        assert_eq!(std::fs::read(t.p("out/d/b (1)")).unwrap(), b"4");
        assert_eq!(rep.files, 4);
    }

    #[test]
    fn destination_that_is_a_file_is_refused() {
        let t = Tmp::new("destfile");
        let zip = t.write("in.zip", &build(&[E::stored("a", b"1")]));
        let f = t.write("out", b"i am a file");
        assert!(matches!(unzip_to(&zip, &f), Err(Error::Io(_))));
        assert_eq!(std::fs::read(&f).unwrap(), b"i am a file");
    }

    #[test]
    fn missing_archive_is_io() {
        let t = Tmp::new("nozip");
        assert!(matches!(unzip_to(&t.p("none.zip"), &t.p("o")), Err(Error::Io(_))));
        assert!(!t.p("o").exists());
    }

    // ------------------------------------------------------------ unzip_to: security

    #[test]
    fn zip_slip_dotdot_and_backslash_tricks_are_rejected() {
        let bad = [
            "../evil.txt",
            "a/../../evil.txt",
            "..\\evil.txt",
            "a\\..\\..\\evil.txt",
            "a/b/../../../evil.txt",
            "..",
            "sub/..",
        ];
        for (i, n) in bad.iter().enumerate() {
            assert_rejected(&format!("slip{i}"), n);
        }
    }

    #[test]
    fn absolute_drive_and_unc_paths_are_rejected() {
        let bad = [
            "/abs.txt",
            "\\abs.txt",
            "C:/x.txt",
            "C:\\x.txt",
            "C:x.txt",
            "\\\\server\\share\\x",
            "//server/share/x",
            "\\\\?\\C:\\x",
            "d:",
        ];
        for (i, n) in bad.iter().enumerate() {
            assert_rejected(&format!("abs{i}"), n);
        }
    }

    #[test]
    fn windows_device_names_are_rejected_with_or_without_extension() {
        let bad = [
            "CON",
            "con",
            "PRN",
            "Aux",
            "NUL",
            "COM1",
            "com9",
            "LPT1",
            "lpt9",
            "NUL.txt",
            "con.tar.gz",
            "d/AUX.log",
            "d/COM3",
            "NUL .txt",
            "CONIN$",
            "CONOUT$",
        ];
        for (i, n) in bad.iter().enumerate() {
            assert_rejected(&format!("dev{i}"), n);
        }
        // Similar-looking but legal names still extract.
        let es = [
            E::stored("CONSOLE.txt", b"1"),
            E::stored("COM10", b"2"),
            E::stored("LPT", b"3"),
            E::stored("nul_x.txt", b"4"),
        ];
        let (r, t) = run("devok", &build(&es));
        assert_eq!(r.unwrap().files, 4);
        assert_eq!(std::fs::read(t.p("sandbox/out/COM10")).unwrap(), b"2");
    }

    #[test]
    fn alternate_data_streams_and_colons_are_rejected() {
        for (i, n) in ["a.txt:evil", "a.txt::$DATA", "d/a:b", ":x", "x:"].iter().enumerate() {
            assert_rejected(&format!("ads{i}"), n);
        }
    }

    #[test]
    fn trailing_dots_and_spaces_are_rejected() {
        for (i, n) in ["a.", "a ", "dir./x", "dir /x", "x/y...", "ok/ "].iter().enumerate() {
            assert_rejected(&format!("trail{i}"), n);
        }
    }

    #[test]
    fn control_characters_and_invalid_windows_characters_are_rejected() {
        let bad = ["a\u{1}b", "a\nb", "a\tb", "a\u{7f}b", "a<b", "a>b", "a\"b", "a|b", "a?b", "a*b", "a\0b"];
        for (i, n) in bad.iter().enumerate() {
            assert_rejected(&format!("ctl{i}"), n);
        }
    }

    #[test]
    fn symlink_entries_are_never_created() {
        let mut link = E::stored("link", b"C:\\Windows");
        link.made_by = (3 << 8) | 20; // Unix host
        link.attr = 0o120777 << 16;
        let (r, t) = run("symlink", &build(&[E::stored("real.txt", b"r"), link]));
        let rep = r.unwrap();
        assert_eq!(rep.files, 1);
        assert_eq!(leftovers(&t.p("sandbox/out")), ["real.txt"]);
    }

    #[test]
    fn a_pre_existing_junction_inside_the_destination_is_not_followed() {
        let t = Tmp::new("junction");
        t.write("elsewhere/keep.txt", b"keep");
        std::fs::create_dir_all(t.p("out")).unwrap();
        let made = junction(&t.p("out/link"), &t.p("elsewhere"));
        if !made {
            return;
        }
        let zip = t.write("in.zip", &build(&[E::stored("link/pwned.txt", b"x")]));
        let r = unzip_to(&zip, &t.p("out"));
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
        assert_eq!(leftovers(&t.p("elsewhere")), ["keep.txt"]);
        std::fs::remove_dir(t.p("out/link")).unwrap(); // removes the junction only, never its target
        assert_eq!(leftovers(&t.p("elsewhere")), ["keep.txt"]);
    }

    #[test]
    fn a_file_where_a_folder_is_needed_is_a_conflict_not_an_overwrite() {
        let (r, _t) = run("conflict", &build(&[E::stored("a", b"file"), E::stored("a/b", b"x")]));
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
    }

    // ------------------------------------------------------------ unzip_to: formats and limits

    #[test]
    fn encrypted_zip64_and_unknown_methods_are_unsupported() {
        let mut enc = E::stored("a", b"x");
        enc.flags |= 1;
        let (r, _t) = run("enc", &build(&[enc]));
        assert!(matches!(r, Err(Error::Unsupported(ref m)) if m.contains("encrypted")), "{r:?}");

        let mut bz = E::stored("a", b"x");
        bz.method = 12;
        let (r, _t) = run("bzip", &build(&[bz]));
        assert!(matches!(r, Err(Error::Unsupported(ref m)) if m.contains("method 12")), "{r:?}");

        let mut z64 = E::stored("a", b"x");
        z64.size = u32::MAX;
        z64.comp = u32::MAX;
        let (r, _t) = run("zip64", &build(&[z64]));
        assert!(matches!(r, Err(Error::Unsupported(ref m)) if m.contains("zip64")), "{r:?}");
    }

    #[test]
    fn more_than_20000_entries_are_refused_before_extracting() {
        let es: Vec<E> = (0..20_001).map(|i| E::stored(&format!("f{i}"), b"")).collect();
        let (r, t) = run("many", &build(&es));
        assert!(matches!(r, Err(Error::Unsupported(ref m)) if m.contains("20000")), "{r:?}");
        assert!(!t.p("sandbox/out").exists());
        let es: Vec<E> = (0..20_000).map(|i| E::stored(&format!("f{i}"), b"")).collect();
        let (r, _t) = run("many-ok", &build(&es));
        assert_eq!(r.unwrap().files, 20_000);
    }

    #[test]
    fn total_uncompressed_size_is_capped_by_real_bytes() {
        let lim = UnzipLimits { max_total: 1000, ..UnzipLimits::default() };
        let es = [E::stored("a", &[1u8; 600]), E::stored("b", &[2u8; 600])];
        let (r, t) = run_with("total", &build(&es), &lim);
        assert!(matches!(r, Err(Error::Unsupported(ref m)) if m.contains("uncompressed")), "{r:?}");
        assert!(!t.p("sandbox/out").exists(), "partial output removed");
        let (r, _t) = run_with("total-ok", &build(&es[..1]), &lim);
        assert_eq!(r.unwrap().bytes, 600);
    }

    #[test]
    fn decompression_bombs_abort_without_filling_the_disk() {
        let lim = UnzipLimits { bomb_min: 1 << 20, ..UnzipLimits::default() };
        let (r, t) = run_with("bomb", &build(&[E::deflated("zeros.bin", &vec![0u8; 8 << 20])]), &lim);
        assert!(matches!(r, Err(Error::Unsupported(ref m)) if m.contains("ratio")), "{r:?}");
        assert!(!t.p("sandbox/out").exists());
        // The same ratio below the size floor is fine (default floor is 100 MiB).
        let (r, _t) = run("small-ratio", &build(&[E::deflated("zeros.bin", &vec![0u8; 8 << 20])]));
        assert_eq!(r.unwrap().bytes, 8 << 20);
        assert_eq!(UnzipLimits::default().bomb_min, 100 << 20);
        assert_eq!(UnzipLimits::default().bomb_ratio, 1000);
        assert_eq!(UnzipLimits::default().max_total, 8 << 30);
    }

    #[test]
    fn declared_sizes_are_not_trusted() {
        let mut small = E::stored("a", &[7u8; 100]);
        small.size = 10; // declares less than there really is
        let (r, t) = run("lie-small", &build(&[small]));
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
        assert!(!t.p("sandbox/out").exists());

        let mut big = E::deflated("b", &[7u8; 100]);
        big.size = 1000; // declares more than the stream holds
        let (r, _t) = run("lie-big", &build(&[big]));
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");

        let mut huge = E::deflated("b", &[7u8; 100_000]);
        huge.size = 100; // stream inflates past the declaration: stop early
        let (r, t) = run("lie-huge", &build(&[huge]));
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
        assert!(!t.p("sandbox/out").exists());
    }

    #[test]
    fn overlapping_entries_that_share_compressed_bytes_are_corrupt() {
        // Four central entries pointing at the same data: the sum of compressed sizes exceeds the file.
        let mut z = build(&[E::stored("a", &[1u8; 1000])]);
        let eocd = z.len() - 22;
        let cd_start = u32::from_le_bytes(z[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        let entry = z[cd_start..eocd].to_vec();
        z.truncate(eocd);
        for _ in 0..3 {
            z.extend_from_slice(&entry);
        }
        let cd_len = (z.len() - cd_start) as u32;
        z.extend_from_slice(&[0x50, 0x4b, 5, 6, 0, 0, 0, 0, 4, 0, 4, 0]);
        z.extend_from_slice(&cd_len.to_le_bytes());
        z.extend_from_slice(&(cd_start as u32).to_le_bytes());
        z.extend_from_slice(&[0, 0]);
        let (r, _t) = run("overlap", &z);
        assert!(matches!(r, Err(Error::Corrupt(ref m)) if m.contains("overlap")), "{r:?}");
    }

    #[test]
    fn a_wrong_crc_is_corrupt_and_removes_everything_extracted_so_far() {
        let mut bad = E::stored("second.txt", b"payload");
        bad.crc ^= 1;
        let (r, t) = run("crc", &build(&[E::stored("first.txt", b"fine"), bad]));
        assert!(matches!(r, Err(Error::Corrupt(ref m)) if m.contains("checksum")), "{r:?}");
        assert!(!t.p("sandbox/out").exists());
    }

    #[test]
    fn a_failure_in_an_existing_destination_removes_only_what_was_extracted() {
        let t = Tmp::new("preexisting");
        t.write("out/mine.txt", b"mine");
        let mut bad = E::stored("second.txt", b"payload");
        bad.crc ^= 1;
        let zip = t.write("in.zip", &build(&[E::stored("first.txt", b"fine"), E::stored("d/x.txt", b"x"), bad]));
        assert!(unzip_to(&zip, &t.p("out")).is_err());
        assert_eq!(leftovers(&t.p("out")), ["mine.txt"]);
        assert_eq!(std::fs::read(t.p("out/mine.txt")).unwrap(), b"mine");
    }

    #[test]
    fn truncated_garbage_and_empty_archives_are_corrupt_and_leave_nothing() {
        let good =
            build(&[E::deflated("a.txt", &b"hello world ".repeat(500)), E::stored("b.bin", &random_bytes(500, 1))]);
        // Every prefix: never a panic, never partial output, and a typed error.
        let t = Tmp::new("prefix");
        for cut in (0..good.len()).step_by(7).chain([good.len() - 1]) {
            let zip = t.write("in.zip", &good[..cut]);
            let out = t.p("o");
            let r = unzip_to(&zip, &out);
            assert!(matches!(r, Err(Error::Corrupt(_))), "cut {cut}: {r:?}");
            assert!(!out.exists(), "cut {cut}");
        }
        // Central directory intact but the data truncated away.
        let mut cut_data = good.clone();
        let eocd = cut_data.len() - 22;
        let cd_start = u32::from_le_bytes(cut_data[eocd + 16..eocd + 20].try_into().unwrap()) as usize;
        cut_data.drain(40..cd_start - 10);
        let (r, _t) = run("datacut", &cut_data);
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
        let (r, _t) = run("garbage", &random_bytes(5000, 9));
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
        let (r, _t) = run("empty", b"");
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
        // A corrupt deflate stream.
        let mut e = E::deflated("x", &b"abcdefgh".repeat(100));
        for b in e.body.iter_mut().take(8) {
            *b = 0xFF;
        }
        let (r, _t) = run("badinflate", &build(&[e]));
        assert!(matches!(r, Err(Error::Corrupt(_))), "{r:?}");
    }

    #[test]
    fn archives_made_by_the_in_memory_writer_extract_too() {
        let mut w = ZipWriter::new();
        w.add("x/y.txt", b"hello hello hello hello", true);
        w.add("ñ.bin", &[1, 2, 3], false);
        let (r, t) = run("inmem", &w.finish());
        assert_eq!(r.unwrap().files, 2);
        assert_eq!(std::fs::read(t.p("sandbox/out/x/y.txt")).unwrap(), b"hello hello hello hello");
        assert_eq!(std::fs::read(t.p("sandbox/out/ñ.bin")).unwrap(), [1, 2, 3]);
    }

    // ------------------------------------------------------------ properties

    fn get<'a>(p: &'a [(&'static str, String)], k: &str) -> Option<&'a str> {
        p.iter().find(|(key, _)| *key == k).map(|(_, v)| v.as_str())
    }
    fn keys(p: &[(&'static str, String)]) -> Vec<&'static str> {
        p.iter().map(|(k, _)| *k).collect()
    }
    fn is_stamp(s: &str) -> bool {
        let b = s.as_bytes();
        b.len() == 16
            && b[4] == b'-'
            && b[7] == b'-'
            && b[10] == b' '
            && b[13] == b':'
            && b.iter().enumerate().all(|(i, c)| [4, 7, 10, 13].contains(&i) || c.is_ascii_digit())
    }
    fn stamp(t: std::time::SystemTime) -> String {
        let tm = local_time(t).unwrap();
        format!("{:04}-{:02}-{:02} {:02}:{:02}", tm.year, tm.month, tm.day, tm.hour, tm.minute)
    }

    #[test]
    fn plain_file_properties_are_in_display_order() {
        let t = Tmp::new("props-text");
        let p = t.write("notes ñ.txt", b"hello");
        let when = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        std::fs::File::options().write(true).open(&p).unwrap().set_modified(when).unwrap();
        let props = properties(&p).unwrap();
        assert_eq!(keys(&props), ["name", "type", "size", "created", "modified"]);
        assert_eq!(get(&props, "name"), Some("notes ñ.txt"));
        assert_eq!(get(&props, "type"), Some("Text file"));
        assert_eq!(get(&props, "size"), Some("5 bytes"));
        assert!(is_stamp(get(&props, "created").unwrap()));
        assert_eq!(get(&props, "modified"), Some(stamp(when).as_str()));
        assert!(props.iter().all(|(_, v)| !v.is_empty() && !v.to_lowercase().contains("unknown")));
    }

    #[test]
    fn size_is_human_and_exact_with_thousands_separators() {
        let t = Tmp::new("props-size");
        let p = t.p("big.bin");
        std::fs::File::create(&p).unwrap().set_len(1_153_434).unwrap();
        let props = properties(&p).unwrap();
        assert_eq!(get(&props, "size"), Some("1.1 MB (1,153,434 bytes)"));
        assert_eq!(get(&props, "type"), Some("BIN file"));
        std::fs::File::create(&p).unwrap().set_len(1).unwrap();
        assert_eq!(get(&properties(&p).unwrap(), "size"), Some("1 byte"));
        std::fs::File::create(&p).unwrap().set_len(2048).unwrap();
        assert_eq!(get(&properties(&p).unwrap(), "size"), Some("2.0 KB (2,048 bytes)"));
        let none = t.write("noext", b"x");
        assert_eq!(get(&properties(&none).unwrap(), "type"), Some("File"));
    }

    #[test]
    fn type_comes_from_the_content_not_the_name() {
        let t = Tmp::new("props-type");
        let png = t.write("photo.jpg", b"\x89PNG\r\n\x1a\n and some more bytes");
        assert_eq!(get(&properties(&png).unwrap(), "type"), Some("PNG image"));
        let pdf = t.write("x.dat", b"%PDF-1.4\n%%EOF");
        assert_eq!(get(&properties(&pdf).unwrap(), "type"), Some("PDF document"));
        let zip = t.write("a.zip", &build(&[E::stored("a", b"1")]));
        assert_eq!(get(&properties(&zip).unwrap(), "type"), Some("ZIP archive"));
        let ogg = t.write("song.ogg", b"OggS....");
        assert_eq!(get(&properties(&ogg).unwrap(), "type"), Some("OGG file"));
    }

    #[test]
    fn folders_and_missing_paths() {
        let t = Tmp::new("props-dir");
        let props = properties(&t.0).unwrap();
        assert_eq!(get(&props, "type"), Some("Folder"));
        assert!(get(&props, "size").is_none());
        assert!(matches!(properties(&t.p("none")), Err(Error::Io(_))));
    }

    fn ooxml(core: Option<&str>, app: Option<&str>, extra: &[&str]) -> Vec<u8> {
        let mut w = ZipWriter::new();
        w.add("[Content_Types].xml", b"<Types/>", true);
        if let Some(c) = core {
            w.add("docProps/core.xml", c.as_bytes(), true);
        }
        if let Some(a) = app {
            w.add("docProps/app.xml", a.as_bytes(), true);
        }
        for e in extra {
            w.add(e, b"<x/>", true);
        }
        w.finish()
    }

    #[test]
    fn word_documents_expose_their_document_properties() {
        let t = Tmp::new("props-docx");
        let core = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<cp:coreProperties xmlns:cp="x" xmlns:dc="y" xmlns:dcterms="z" xmlns:xsi="w">
<dc:title>Informe &amp; plan &#241; &#x41;</dc:title><dc:creator>Ana Pérez</dc:creator>
<cp:lastModifiedBy>Luis</cp:lastModifiedBy>
<dcterms:created xsi:type="dcterms:W3CDTF">2023-11-14T22:13:20Z</dcterms:created>
<dcterms:modified xsi:type="dcterms:W3CDTF">2024-02-29T00:30:00Z</dcterms:modified>
<dc:subject/></cp:coreProperties>"#;
        let app = r#"<Properties><Company>ACME &lt;SA&gt;</Company><Pages>12</Pages></Properties>"#;
        let p = t.write("informe.docx", &ooxml(Some(core), Some(app), &["word/document.xml"]));
        let props = properties(&p).unwrap();
        assert_eq!(
            keys(&props),
            [
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
                "pages"
            ]
        );
        assert_eq!(get(&props, "type"), Some("Word document"));
        assert_eq!(get(&props, "title"), Some("Informe & plan ñ A"));
        assert_eq!(get(&props, "author"), Some("Ana Pérez"));
        assert_eq!(get(&props, "last-modified-by"), Some("Luis"));
        assert_eq!(get(&props, "company"), Some("ACME <SA>"));
        assert_eq!(get(&props, "pages"), Some("12"));
        assert!(is_stamp(get(&props, "doc-modified").unwrap()));
        let utc = std::time::UNIX_EPOCH + std::time::Duration::from_secs(1_700_000_000);
        assert_eq!(
            get(&props, "doc-created"),
            Some(stamp(utc).as_str()),
            "OOXML dates are UTC and shown in local time"
        );
    }

    #[test]
    fn spreadsheets_and_presentations_report_sheets_and_slides() {
        let t = Tmp::new("props-xlsx");
        let core = "<cp:coreProperties><dc:creator>Eva</dc:creator></cp:coreProperties>";
        let sheets =
            ["xl/worksheets/sheet1.xml", "xl/worksheets/sheet2.xml", "xl/worksheets/sheet3.xml", "xl/workbook.xml"];
        let x = t.write("book.xlsx", &ooxml(Some(core), None, &sheets));
        let props = properties(&x).unwrap();
        assert_eq!(get(&props, "type"), Some("Excel workbook"));
        assert_eq!(get(&props, "author"), Some("Eva"));
        assert_eq!(get(&props, "sheets"), Some("3"));
        assert!(get(&props, "title").is_none() && get(&props, "company").is_none());
        let p = t.write("deck.pptx", &ooxml(None, Some("<Properties><Slides>7</Slides></Properties>"), &[]));
        let props = properties(&p).unwrap();
        assert_eq!(get(&props, "type"), Some("PowerPoint presentation"));
        assert_eq!(get(&props, "slides"), Some("7"));
    }

    #[test]
    fn pdf_info_strings_are_decoded_and_outline_titles_ignored() {
        let t = Tmp::new("props-pdf");
        let pdf = [
            "%PDF-1.4\n",
            "1 0 obj\n<< /Type /Catalog /Pages 2 0 R >>\nendobj\n",
            "6 0 obj\n<< /Title (Chapter 1 decoy) /Parent 1 0 R >>\nendobj\n",
            "5 0 obj\n<< /Title (My \\(great\\) doc\\n) /Author <FEFF0048006900E9> /Creator (Writer) /Producer (\\376\\377\\000P\\000D\\000F) /TitleX (no) >>\nendobj\n",
            "trailer\n<< /Size 7 /Root 1 0 R /Info 5 0 R >>\nstartxref\n0\n%%EOF\n",
        ]
        .concat();
        let p = t.write("a.pdf", pdf.as_bytes());
        let props = properties(&p).unwrap();
        assert_eq!(get(&props, "type"), Some("PDF document"));
        assert_eq!(get(&props, "title"), Some("My (great) doc"));
        assert_eq!(get(&props, "author"), Some("Hié"));
        assert_eq!(get(&props, "creator"), Some("Writer"));
        assert_eq!(get(&props, "producer"), Some("PDF"));
        // Without an Info dictionary nothing is invented.
        let bare =
            t.write("b.pdf", b"%PDF-1.4\n1 0 obj\n<< /Title (Chapter) >>\nendobj\ntrailer\n<< /Root 1 0 R >>\n%%EOF");
        let props = properties(&bare).unwrap();
        assert!(get(&props, "title").is_none() && get(&props, "author").is_none());
    }

    #[test]
    fn pdf_properties_on_a_real_pdf_and_a_big_one() {
        let t = Tmp::new("props-pdf-real");
        let fixture = Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/tests/fixtures/hello.pdf"));
        let props = properties(fixture).unwrap();
        assert_eq!(get(&props, "type"), Some("PDF document"));
        assert_eq!(get(&props, "pages"), Some("1"));
        // A multi-megabyte PDF: the Info dictionary lives in the tail and is still found.
        let mut big = b"%PDF-1.4\n".to_vec();
        big.extend(std::iter::repeat(b'x').take(3 * 1024 * 1024));
        big.extend_from_slice(b"\n9 0 obj\n<< /Producer (Tail Producer) >>\nendobj\ntrailer\n<< /Info 9 0 R >>\n%%EOF");
        let p = t.write("big.pdf", &big);
        assert_eq!(get(&properties(&p).unwrap(), "producer"), Some("Tail Producer"));
    }

    #[test]
    fn garbage_never_panics_and_adds_no_document_fields() {
        let t = Tmp::new("props-garbage");
        let noise = random_bytes(20_000, 5);
        for name in ["a.docx", "a.xlsx", "a.pptx", "a.pdf", "a.zip", "a.png", "a.txt"] {
            let p = t.write(name, &noise);
            let props = properties(&p).unwrap();
            assert_eq!(keys(&props)[..2], ["name", "type"], "{name}");
            for k in ["title", "author", "pages", "sheets", "company", "producer"] {
                assert!(get(&props, k).is_none(), "{name} {k}");
            }
        }
        let mut pdf = b"%PDF-1.7 /Info ".to_vec();
        pdf.extend_from_slice(&noise);
        pdf.extend_from_slice(b" /Info 99999999999999999999 0 R /Title (unterminated \\");
        let p = t.write("trunc.pdf", &pdf);
        let _ = properties(&p).unwrap();
        // Truncated OOXML at every few offsets.
        let docx = ooxml(Some("<dc:title>T</dc:title>"), Some("<Pages>3</Pages>"), &[]);
        for cut in (0..docx.len()).step_by(5) {
            let p = t.write("cut.docx", &docx[..cut]);
            let _ = properties(&p).unwrap();
        }
        let empty = t.write("empty.docx", b"");
        assert_eq!(get(&properties(&empty).unwrap(), "size"), Some("0 bytes"));
    }

    // ------------------------------------------------------------ throughput
    // Run with: cargo test --release -- --ignored --nocapture perf_report

    #[test]
    #[ignore]
    fn perf_report() {
        use std::time::Instant;
        let t = Tmp::new("perf");
        // About 200 MB of mixed data: text, noise and already-compressed media.
        std::fs::create_dir_all(t.p("mix")).unwrap();
        let mut seed = 42u64;
        let words = ["alpha ", "beta ", "gamma ", "delta ", "radial ", "archivo ", "datos ", "zip\n"];
        let mut total = 0u64;
        for i in 0..8 {
            let mut f = std::io::BufWriter::new(std::fs::File::create(t.p(&format!("mix/doc{i}.txt"))).unwrap());
            let mut written = 0usize;
            while written < 8 * 1024 * 1024 {
                let w = words[(lcg(&mut seed) % 8) as usize];
                f.write_all(w.as_bytes()).unwrap();
                written += w.len();
            }
            total += written as u64;
        }
        for i in 0..6 {
            let d = random_bytes(16 * 1024 * 1024, i);
            std::fs::write(t.p(&format!("mix/clip{i}.mp4")), &d).unwrap();
            total += d.len() as u64;
        }
        for i in 0..3 {
            let d = random_bytes(8 * 1024 * 1024, 100 + i);
            std::fs::write(t.p(&format!("mix/blob{i}.bin")), &d).unwrap();
            total += d.len() as u64;
        }
        let mb = total as f64 / 1e6;
        let s = Instant::now();
        let rep = zip_paths(&[t.p("mix")], &t.p("mix.zip")).unwrap();
        let dt = s.elapsed().as_secs_f64();
        println!(
            "PERF zip_paths   : {mb:.0} MB in {dt:.2}s = {:.0} MB/s (archive {} MB)",
            mb / dt,
            rep.bytes_out / 1_000_000
        );
        let s = Instant::now();
        let u = unzip_to(&t.p("mix.zip"), &t.p("out")).unwrap();
        let dt = s.elapsed().as_secs_f64();
        println!(
            "PERF unzip_to    : {:.0} MB in {dt:.2}s = {:.0} MB/s",
            u.bytes as f64 / 1e6,
            u.bytes as f64 / 1e6 / dt
        );
        let big = t.p("big.bin");
        {
            let block = random_bytes(1 << 20, 77);
            let mut f = std::io::BufWriter::new(std::fs::File::create(&big).unwrap());
            for _ in 0..500 {
                f.write_all(&block).unwrap();
            }
        }
        let s = Instant::now();
        let h = sha256_file(&big).unwrap();
        let dt = s.elapsed().as_secs_f64();
        println!("PERF sha256_file : 524 MB in {dt:.2}s = {:.0} MB/s ({h})", 524.288 / dt);
        let buf = random_bytes(64 << 20, 5);
        let s = Instant::now();
        let _ = sha256_hex(&buf);
        let dt = s.elapsed().as_secs_f64();
        println!("PERF sha256 (RAM): 67 MB in {dt:.3}s = {:.0} MB/s", 67.108864 / dt);
    }
}
