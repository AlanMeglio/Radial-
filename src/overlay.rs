//! The overlay: a topmost, non-activating layered window that is also an OLE drop target.
//! Everything is event driven; the frame timer only runs while something is animating.

use std::cell::{Cell, RefCell};
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::{Path, PathBuf};
use std::ptr::null_mut;
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};
use std::time::Instant;

use tiny_skia::Pixmap;
use windows_sys::core::GUID;
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::{FORMATETC, STGMEDIUM};
use windows_sys::Win32::System::Ole::ReleaseStgMedium;
use windows_sys::Win32::UI::HiDpi::GetDpiForMonitor;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{mouse_event, MOUSEEVENTF_MOVE};
use windows_sys::Win32::UI::Shell::{DragQueryFileW, HDROP};
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::{plan_destination, tr, Config, Dest};
use crate::convert::{self, Fmt, ALL};
use crate::geometry::{clamp_origin, sector_at};
use crate::render::{Backdrop, Face, Look, Renderer, Scene, Slot};

pub const WM_SHOW: u32 = WM_APP + 10; // lparam: x | y << 16 (signed 16 bit each)
pub const WM_HIDE: u32 = WM_APP + 11;
pub const WM_HIDE_SOON: u32 = WM_APP + 12;
pub const WM_DONE: u32 = WM_APP + 13; // lparam: Box<String> with the result text

const TIMER_FRAME: usize = 1;
const TIMER_HIDE: usize = 2;
const TIMER_DELAY: usize = 3;
const FRAME_MS: u32 = 8;
const OPEN_SECS: f32 = 0.20;
const CLOSE_SECS: f32 = 0.16;
/// How long the confirmation pulse plays after a drop.
const CONFIRM_SECS: f32 = 0.46;
const COPY: u32 = 1;
const NONE: u32 = 0;

static HWND_MAIN: AtomicUsize = AtomicUsize::new(0);

pub fn set_hwnd(h: HWND) {
    HWND_MAIN.store(h as usize, Relaxed);
}

fn hwnd() -> HWND {
    HWND_MAIN.load(Relaxed) as HWND
}

#[derive(PartialEq, Clone, Copy)]
enum Phase {
    /// Window is up and waiting for a drag to enter it.
    Pending,
    Open,
    Closing,
}

/// Critically-damped-ish spring integrated with fixed sub-steps so large frame gaps stay stable.
#[derive(Clone, Copy, Default)]
pub struct Spring {
    pub x: f32,
    pub v: f32,
}

impl Spring {
    pub fn step(&mut self, target: f32, stiffness: f32, damping_ratio: f32, dt: f32) {
        let c = 2.0 * damping_ratio * stiffness.sqrt();
        let mut left = dt;
        while left > 0.0 {
            let h = left.min(1.0 / 240.0);
            let a = stiffness * (target - self.x) - c * self.v;
            self.v += a * h;
            self.x += self.v * h;
            left -= h;
        }
    }

    pub fn settled(&self, target: f32) -> bool {
        (self.x - target).abs() < 0.002 && self.v.abs() < 0.02
    }
}

struct Ov {
    dpi: f32,
    side: i32,
    origin: (i32, i32),
    dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    pm: Pixmap,
    renderer: Renderer,
    look: Look,
    backdrop: Option<Backdrop>,
    speed: f32,
    scene: Scene,
    appear: Vec<Spring>,
    hover_sp: Vec<Spring>,
    phase: Phase,
    /// Seconds since the menu started opening.
    age: f32,
    target_open: f32,
    hover: Option<usize>,
    files: Vec<PathBuf>,
    last: Instant,
    /// Seconds into the drop confirmation, if one is playing.
    confirm_t: Option<f32>,
}

thread_local! {
    static OV: RefCell<Option<Ov>> = const { RefCell::new(None) };
    static PENDING_AT: Cell<Option<(i32, i32)>> = const { Cell::new(None) };
}

/// Runs `f` on the overlay state; does nothing when it is not shown or already borrowed.
fn with<R>(f: impl FnOnce(&mut Ov) -> R) -> Option<R> {
    OV.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

// ---------- Show / hide ----------

/// Copies the screen area under the window into the DIB, for the frosted-glass backdrop.
unsafe fn capture_backdrop(
    dc: HDC,
    bits: *const u8,
    origin: (i32, i32),
    side: i32,
    look: &Look,
    dpi: f32,
) -> Option<Backdrop> {
    let screen = GetDC(null_mut());
    let ok = BitBlt(dc, 0, 0, side, side, screen, origin.0, origin.1, SRCCOPY) != 0;
    ReleaseDC(null_mut(), screen);
    if !ok {
        return None;
    }
    let src = std::slice::from_raw_parts(bits, (side * side * 4) as usize);
    let blur = look.theme.blur * look.glass * look.scale * dpi;
    Backdrop::new(src, side as u32, side as u32, true, blur, look.theme.saturation)
}

pub unsafe fn show(x: i32, y: i32) {
    hide_now();
    let look = crate::current_look();
    let speed = crate::current_config().speed;
    let pt = POINT { x, y };
    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let (mut dx, mut dy) = (96u32, 96u32);
    GetDpiForMonitor(mon, 0, &mut dx, &mut dy);
    let dpi = dx as f32 / 96.0;
    let side = look.side_px(dpi) as i32;
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(mon, &mut mi);
    let r = mi.rcMonitor;
    // Keep the cursor at the menu centre when possible, otherwise slide the window onto the screen.
    let origin = (clamp_origin(x, side, r.left, r.right), clamp_origin(y, side, r.top, r.bottom));

    let screen = GetDC(null_mut());
    let dc = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = side;
    bi.bmiHeader.biHeight = -side;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    let mut bits: *mut c_void = null_mut();
    let dib = CreateDIBSection(screen, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    ReleaseDC(null_mut(), screen);
    let Some(pm) = Pixmap::new(side as u32, side as u32) else { return };
    if dib.is_null() || bits.is_null() {
        DeleteDC(dc);
        return;
    }
    let old = SelectObject(dc, dib);
    let backdrop = capture_backdrop(dc, bits as *const u8, origin, side, &look, dpi);
    let face = Face::load(look.theme.font, look.theme.weight);
    let ov = Ov {
        dpi,
        side,
        origin,
        dc,
        dib,
        old,
        bits: bits as *mut u8,
        pm,
        renderer: Renderer::new(face),
        look,
        backdrop,
        speed,
        scene: Scene::new(vec![], String::new(), String::new()),
        appear: vec![],
        hover_sp: vec![],
        phase: Phase::Pending,
        age: 0.0,
        target_open: 0.0,
        hover: None,
        files: vec![],
        last: Instant::now(),
        confirm_t: None,
    };
    OV.with(|c| *c.borrow_mut() = Some(ov));
    let h = hwnd();
    SetWindowPos(h, HWND_TOPMOST, origin.0, origin.1, side, side, SWP_NOACTIVATE | SWP_SHOWWINDOW);
    with(|o| present(o));
    // Wake the drag loop of the source so it notices the new window without waiting for a real move.
    mouse_event(MOUSEEVENTF_MOVE, 0, 0, 0, 0);
}

unsafe fn hide_now() {
    let h = hwnd();
    KillTimer(h, TIMER_FRAME);
    KillTimer(h, TIMER_HIDE);
    let ov = OV.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.take()));
    if let Some(o) = ov {
        ShowWindow(h, SW_HIDE);
        SelectObject(o.dc, o.old);
        DeleteObject(o.dib);
        DeleteDC(o.dc);
    }
}

unsafe fn begin_close() {
    let pending = with(|o| {
        if o.phase == Phase::Pending {
            return true;
        }
        if o.phase != Phase::Closing {
            o.phase = Phase::Closing;
            o.target_open = 0.0;
            o.hover = None;
            o.last = Instant::now();
        }
        false
    });
    match pending {
        Some(false) => start_timer(),
        _ => hide_now(),
    }
}

unsafe fn start_timer() {
    SetTimer(hwnd(), TIMER_FRAME, FRAME_MS, None);
}

unsafe fn present(o: &mut Ov) {
    o.renderer.render(&mut o.pm, &o.scene, &o.look, o.dpi);
    // tiny-skia is premultiplied RGBA; a 32-bit DIB is premultiplied BGRA.
    let src = o.pm.data();
    let dst = std::slice::from_raw_parts_mut(o.bits, src.len());
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = s[3];
    }
    let dst_pt = POINT { x: o.origin.0, y: o.origin.1 };
    let size = SIZE_ { cx: o.side, cy: o.side };
    let src_pt = POINT { x: 0, y: 0 };
    let blend = BLENDFUNCTION {
        BlendOp: AC_SRC_OVER as u8,
        BlendFlags: 0,
        SourceConstantAlpha: 255,
        AlphaFormat: AC_SRC_ALPHA as u8,
    };
    UpdateLayeredWindow(hwnd(), null_mut(), &dst_pt, &size, o.dc, &src_pt, 0, &blend, ULW_ALPHA);
}

type SIZE_ = windows_sys::Win32::Foundation::SIZE;

/// Advances every animated value by `dt` seconds; returns true when everything has come to rest.
fn advance(o: &mut Ov, dt: f32) -> bool {
    let th = &o.look.theme;
    let dt = dt * o.speed;
    o.age += dt;
    let n = o.scene.slots.len();
    let mut settled = true;

    if let Some(t) = o.confirm_t.as_mut() {
        *t += dt;
        let p = (*t / CONFIRM_SECS).min(1.0);
        o.scene.pulse = p;
        // Stay fully open for the first part of the pulse, then fade away.
        o.scene.open = if p < 0.6 { 1.0 } else { 1.0 - ((p - 0.6) / 0.4).min(1.0) };
        settled = p >= 1.0;
    } else {
        let secs = if o.target_open > o.scene.open { OPEN_SECS } else { CLOSE_SECS };
        let step = dt / secs;
        o.scene.open = if o.target_open > o.scene.open {
            (o.scene.open + step).min(o.target_open)
        } else {
            (o.scene.open - step).max(o.target_open)
        };
        settled &= o.scene.open == o.target_open;
    }

    for i in 0..n {
        // Staggered entrance: sector i starts moving after i * stagger seconds; nothing moves while closing.
        let go = o.phase == Phase::Open && o.age >= th.stagger * i as f32;
        let target = if o.phase == Phase::Closing { o.appear[i].x.clamp(0.0, 1.0).round() } else { go as u8 as f32 };
        o.appear[i].step(target, th.stiffness, th.damping, dt);
        settled &= o.appear[i].settled(target) && (o.phase != Phase::Open || go);
        o.scene.appear[i] = o.appear[i].x;

        let ht = if Some(i) == o.hover || (o.confirm_t.is_some() && o.scene.confirm == Some(i)) { 1.0 } else { 0.0 };
        o.hover_sp[i].step(ht, th.stiffness * 1.4, (th.damping + 0.25).min(1.0), dt);
        settled &= o.hover_sp[i].settled(ht);
        o.scene.hover[i] = o.hover_sp[i].x.clamp(0.0, 1.15);
    }
    settled
}

unsafe fn tick() {
    let done = with(|o| {
        let now = Instant::now();
        let dt = (now - o.last).as_secs_f32().min(0.05);
        o.last = now;
        let settled = advance(o, dt);
        present(o);
        if o.phase == Phase::Closing && (o.scene.open <= 0.0 && (o.confirm_t.is_none() || settled)) {
            Some(true)
        } else {
            Some(settled)
        }
    })
    .flatten();
    match done {
        Some(true) => {
            let closing = with(|o| o.phase == Phase::Closing).unwrap_or(false);
            if closing {
                hide_now();
            } else {
                KillTimer(hwnd(), TIMER_FRAME);
            }
        }
        None => {
            KillTimer(hwnd(), TIMER_FRAME);
        }
        _ => {}
    }
}

// ---------- Window messages ----------

/// Handles overlay messages; returns true when consumed.
pub unsafe fn handle(msg: u32, wp: WPARAM, lp: LPARAM) -> bool {
    match msg {
        WM_SHOW => {
            let (x, y) = (lp as i16 as i32, (lp >> 16) as i16 as i32);
            let delay = crate::current_config().delay_ms;
            if delay == 0 {
                show(x, y);
            } else {
                PENDING_AT.with(|c| c.set(Some((x, y))));
                SetTimer(hwnd(), TIMER_DELAY, delay, None);
            }
        }
        WM_HIDE => {
            cancel_delay();
            begin_close();
        }
        WM_HIDE_SOON => {
            cancel_delay();
            SetTimer(hwnd(), TIMER_HIDE, 400, None);
        }
        WM_TIMER if wp == TIMER_FRAME => tick(),
        WM_TIMER if wp == TIMER_DELAY => {
            KillTimer(hwnd(), TIMER_DELAY);
            if let Some((x, y)) = PENDING_AT.with(|c| c.take()) {
                show(x, y);
            }
        }
        WM_TIMER if wp == TIMER_HIDE => {
            KillTimer(hwnd(), TIMER_HIDE);
            begin_close();
        }
        _ => return false,
    }
    true
}

unsafe fn cancel_delay() {
    KillTimer(hwnd(), TIMER_DELAY);
    PENDING_AT.with(|c| c.set(None));
}

// ---------- Menu model ----------

fn size_text(files: &[PathBuf]) -> String {
    match files {
        [one] => std::fs::metadata(one).map(|m| convert::human_size(m.len())).unwrap_or_default(),
        many => format!("{} files", many.len()),
    }
}

fn idle_texts(src: Fmt, files: &[PathBuf]) -> (String, String) {
    (src.label().into(), size_text(files))
}

fn build_scene(src: Fmt, files: &[PathBuf]) -> Scene {
    let slots: Vec<Slot> = ALL.iter().map(|f| Slot { label: f.label(), enabled: f.encodable() && *f != src }).collect();
    let (main, sub) = idle_texts(src, files);
    Scene::new(slots, main, sub)
}

/// Sector under the screen point, relative to the current menu.
fn sector_for(o: &Ov, pt: POINTL) -> Option<usize> {
    let m = o.look.m;
    let cx = o.origin.0 as f32 + m.center * o.dpi;
    let cy = o.origin.1 as f32 + m.center * o.dpi;
    sector_at((pt.x as f32 - cx) / o.dpi, (pt.y as f32 - cy) / o.dpi, o.scene.slots.len(), m.hub, m.hit_out)
}

/// Updates hover state for a drag position; returns the drop effect to show.
fn hover_at(o: &mut Ov, pt: POINTL) -> (u32, bool) {
    if o.phase != Phase::Open {
        return (NONE, false);
    }
    let hit = sector_for(o, pt);
    let hit = hit.filter(|&i| o.scene.slots[i].enabled);
    let changed = hit != o.hover;
    o.hover = hit;
    match hit {
        Some(i) => {
            o.scene.main = o.scene.slots[i].label.into();
            o.scene.sub = "Convert to".into();
            o.scene.main_accent = true;
        }
        None => {
            if let Some(src) = o.files.first().and_then(|f| convert::source_format(f)) {
                let (main, sub) = idle_texts(src, &o.files);
                o.scene.main = main;
                o.scene.sub = sub;
                o.scene.main_accent = false;
            }
        }
    }
    (if hit.is_some() { COPY } else { NONE }, changed)
}

// ---------- OLE drop target ----------

#[repr(C)]
struct Vtbl {
    query: unsafe extern "system" fn(*mut c_void, *const GUID, *mut *mut c_void) -> i32,
    add_ref: unsafe extern "system" fn(*mut c_void) -> u32,
    release: unsafe extern "system" fn(*mut c_void) -> u32,
    drag_enter: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> i32,
    drag_over: unsafe extern "system" fn(*mut c_void, u32, POINTL, *mut u32) -> i32,
    drag_leave: unsafe extern "system" fn(*mut c_void) -> i32,
    drop: unsafe extern "system" fn(*mut c_void, *mut c_void, u32, POINTL, *mut u32) -> i32,
}

#[repr(C)]
pub struct DropTarget {
    vtbl: *const Vtbl,
}

unsafe impl Sync for DropTarget {}

static VTBL: Vtbl = Vtbl { query, add_ref, release, drag_enter, drag_over, drag_leave, drop: on_drop };
pub static TARGET: DropTarget = DropTarget { vtbl: &VTBL };

const IID_UNKNOWN: GUID = GUID::from_u128(0x00000000_0000_0000_c000_000000000046);
const IID_DROP_TARGET: GUID = GUID::from_u128(0x00000122_0000_0000_c000_000000000046);

fn same(a: &GUID, b: &GUID) -> bool {
    a.data1 == b.data1 && a.data2 == b.data2 && a.data3 == b.data3 && a.data4 == b.data4
}

unsafe extern "system" fn query(this: *mut c_void, iid: *const GUID, out: *mut *mut c_void) -> i32 {
    let id = *iid;
    if same(&id, &IID_UNKNOWN) || same(&id, &IID_DROP_TARGET) {
        *out = this;
        0
    } else {
        *out = null_mut();
        0x8000_4002u32 as i32 // E_NOINTERFACE
    }
}

// The target is a static: reference counting is a formality.
unsafe extern "system" fn add_ref(_: *mut c_void) -> u32 {
    1
}

unsafe extern "system" fn release(_: *mut c_void) -> u32 {
    1
}

/// Reads the CF_HDROP file list out of an IDataObject.
unsafe fn read_files(data: *mut c_void) -> Vec<PathBuf> {
    type GetData = unsafe extern "system" fn(*mut c_void, *const FORMATETC, *mut STGMEDIUM) -> i32;
    let mut out = Vec::new();
    if data.is_null() {
        return out;
    }
    let vt = *(data as *const *const usize);
    let get_data: GetData = std::mem::transmute(*vt.add(3));
    let fmt = FORMATETC { cfFormat: 15, ptd: null_mut(), dwAspect: 1, lindex: -1, tymed: 1 }; // CF_HDROP, HGLOBAL
    let mut med: STGMEDIUM = zeroed();
    if get_data(data, &fmt, &mut med) != 0 {
        return out;
    }
    let drop = med.u.hGlobal as HDROP;
    let n = DragQueryFileW(drop, u32::MAX, null_mut(), 0);
    for i in 0..n {
        let len = DragQueryFileW(drop, i, null_mut(), 0) as usize;
        let mut buf = vec![0u16; len + 1];
        DragQueryFileW(drop, i, buf.as_mut_ptr(), buf.len() as u32);
        out.push(PathBuf::from(String::from_utf16_lossy(&buf[..len])));
    }
    ReleaseStgMedium(&mut med);
    out
}

unsafe extern "system" fn drag_enter(
    _: *mut c_void,
    data: *mut c_void,
    _keys: u32,
    pt: POINTL,
    effect: *mut u32,
) -> i32 {
    let files = read_files(data);
    let src = files.first().and_then(|f| convert::source_format(f));
    *effect = NONE;
    let Some(src) = src else { return 0 };
    let opened = with(|o| {
        o.scene = build_scene(src, &files);
        let n = o.scene.slots.len();
        o.appear = vec![Spring::default(); n];
        o.hover_sp = vec![Spring::default(); n];
        o.renderer.prepare(&o.look, o.dpi, o.side as u32, o.backdrop.as_ref(), n);
        o.files = files;
        o.phase = Phase::Open;
        o.target_open = 1.0;
        o.age = 0.0;
        o.last = Instant::now();
        *effect = hover_at(o, pt).0;
    });
    if opened.is_some() {
        start_timer();
    }
    0
}

unsafe extern "system" fn drag_over(_: *mut c_void, _keys: u32, pt: POINTL, effect: *mut u32) -> i32 {
    *effect = NONE;
    let r = with(|o| hover_at(o, pt));
    if let Some((e, changed)) = r {
        *effect = e;
        if changed {
            start_timer();
        }
    }
    0
}

unsafe extern "system" fn drag_leave(_: *mut c_void) -> i32 {
    begin_close();
    0
}

unsafe extern "system" fn on_drop(_: *mut c_void, _data: *mut c_void, _keys: u32, pt: POINTL, effect: *mut u32) -> i32 {
    *effect = NONE;
    let job = with(|o| {
        let (e, _) = hover_at(o, pt);
        let idx = o.hover.filter(|_| e == COPY)?;
        // Start the confirmation: the ring pulses out from the hub while the sector stays lit.
        o.phase = Phase::Closing;
        o.confirm_t = Some(0.0);
        o.scene.confirm = Some(idx);
        o.scene.pulse = 0.001;
        o.last = Instant::now();
        Some((ALL[idx], o.files.clone()))
    })
    .flatten();
    if let Some((fmt, files)) = job {
        *effect = COPY;
        start_timer();
        let h = hwnd() as usize;
        let cfg = crate::current_config();
        std::thread::spawn(move || run_job(h, fmt, files, cfg));
    } else {
        begin_close();
    }
    0
}

/// Moves `from` to `to`, falling back to copy + delete across volumes.
fn move_file(from: &Path, to: &Path) -> std::io::Result<()> {
    if let Some(dir) = to.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::rename(from, to).or_else(|_| std::fs::copy(from, to).and_then(|_| std::fs::remove_file(from)))
}

/// Applies the output folder and name-collision settings to a freshly converted file.
fn place(source: &Path, result: PathBuf, cfg: &Config) -> Result<Option<PathBuf>, String> {
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

/// Converts on a worker thread and reports the outcome to the UI thread.
fn run_job(h: usize, fmt: Fmt, files: Vec<PathBuf>, cfg: Config) {
    let mut ok = Vec::new();
    let mut skipped = Vec::new();
    let mut errs = Vec::new();
    for f in &files {
        match convert::convert(f, fmt).and_then(|p| place(f, p, &cfg)) {
            Ok(Some(p)) => ok.push(p),
            Ok(None) => skipped.push(f.clone()),
            Err(e) => errs.push(e),
        }
    }
    let name = |p: &Path| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
    let l = cfg.lang;
    let text = match (ok.as_slice(), errs.first(), skipped.first()) {
        (_, Some(e), _) => tr(l, "failed").replace("{}", e),
        ([one], None, _) => tr(l, "saved").replace("{}", &name(one)),
        ([], None, Some(s)) => tr(l, "skipped").replace("{}", &name(s)),
        (_, None, _) => tr(l, "converted").replacen("{}", &ok.len().to_string(), 1).replacen("{}", fmt.label(), 1),
    };
    let failed = !errs.is_empty();
    let boxed = Box::into_raw(Box::new((text, failed)));
    unsafe {
        if PostMessageW(h as HWND, WM_DONE, 0, boxed as LPARAM) == 0 {
            drop(Box::from_raw(boxed));
        }
    }
}
