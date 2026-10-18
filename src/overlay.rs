//! The overlay: a topmost, non-activating layered window that is also an OLE drop target.
//! Everything is event driven; the frame timer only runs while something is animating.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
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

use crate::convert::{self, Fmt, ALL};
use crate::geometry::{clamp_origin, sector_at};
use crate::render::{self, Fonts, Scene, Slot, CENTER, DEAD_R, HIT_OUT, SIZE};

pub const WM_SHOW: u32 = WM_APP + 10; // lparam: x | y << 16 (signed 16 bit each)
pub const WM_HIDE: u32 = WM_APP + 11;
pub const WM_HIDE_SOON: u32 = WM_APP + 12;
pub const WM_DONE: u32 = WM_APP + 13; // lparam: Box<String> with the result text

const TIMER_FRAME: usize = 1;
const TIMER_HIDE: usize = 2;
const FRAME_MS: u32 = 15;
const OPEN_SECS: f32 = 0.22;
const CLOSE_SECS: f32 = 0.15;
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

struct Ov {
    dpi: f32,
    side: i32,
    origin: (i32, i32),
    dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    pm: Pixmap,
    fonts: Option<Fonts>,
    scene: Scene,
    phase: Phase,
    target_open: f32,
    hover: Option<usize>,
    files: Vec<PathBuf>,
    last: Instant,
}

thread_local! {
    static OV: RefCell<Option<Ov>> = const { RefCell::new(None) };
}

/// Runs `f` on the overlay state; does nothing when it is not shown or already borrowed.
fn with<R>(f: impl FnOnce(&mut Ov) -> R) -> Option<R> {
    OV.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

// ---------- Show / hide ----------

pub unsafe fn show(x: i32, y: i32) {
    hide_now();
    let pt = POINT { x, y };
    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let (mut dx, mut dy) = (96u32, 96u32);
    GetDpiForMonitor(mon, 0, &mut dx, &mut dy);
    let dpi = dx as f32 / 96.0;
    let side = (SIZE * dpi).round() as i32;
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(mon, &mut mi);
    let r = mi.rcMonitor;
    // Keep the cursor at the menu centre when possible, otherwise slide the window onto the screen.
    let c = (CENTER * dpi) as i32;
    let origin =
        (clamp_origin(x - c + side / 2, side, r.left, r.right), clamp_origin(y - c + side / 2, side, r.top, r.bottom));

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
    let ov = Ov {
        dpi,
        side,
        origin,
        dc,
        dib,
        old,
        bits: bits as *mut u8,
        pm,
        fonts: Fonts::load(),
        scene: Scene {
            slots: vec![],
            open: 0.0,
            hover: vec![],
            main: String::new(),
            sub: String::new(),
            main_accent: false,
        },
        phase: Phase::Pending,
        target_open: 0.0,
        hover: None,
        files: vec![],
        last: Instant::now(),
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
        o.phase = Phase::Closing;
        o.target_open = 0.0;
        o.hover = None;
        o.last = Instant::now();
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
    render::render(&mut o.pm, &o.scene, o.dpi, o.fonts.as_ref());
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

unsafe fn tick() {
    let done = with(|o| {
        let now = Instant::now();
        let dt = (now - o.last).as_secs_f32().min(0.05);
        o.last = now;
        let step = dt / if o.target_open > o.scene.open { OPEN_SECS } else { CLOSE_SECS };
        o.scene.open = if o.target_open > o.scene.open {
            (o.scene.open + step).min(o.target_open)
        } else {
            (o.scene.open - step).max(o.target_open)
        };
        let k = 1.0 - (-dt * 24.0).exp();
        let mut settled = o.scene.open == o.target_open;
        for (i, h) in o.scene.hover.iter_mut().enumerate() {
            let t = if Some(i) == o.hover { 1.0 } else { 0.0 };
            *h += (t - *h) * k;
            if (t - *h).abs() < 0.01 {
                *h = t;
            } else {
                settled = false;
            }
        }
        present(o);
        if o.phase == Phase::Closing && o.scene.open <= 0.0 {
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
        WM_SHOW => show(lp as i16 as i32, (lp >> 16) as i16 as i32),
        WM_HIDE => begin_close(),
        WM_HIDE_SOON => {
            SetTimer(hwnd(), TIMER_HIDE, 400, None);
        }
        WM_TIMER if wp == TIMER_FRAME => tick(),
        WM_TIMER if wp == TIMER_HIDE => {
            KillTimer(hwnd(), TIMER_HIDE);
            begin_close();
        }
        _ => return false,
    }
    true
}

// ---------- Menu model ----------

fn build_scene(src: Fmt, files: &[PathBuf]) -> Scene {
    let slots: Vec<Slot> = ALL.iter().map(|f| Slot { label: f.label(), enabled: f.encodable() && *f != src }).collect();
    let sub = match files {
        [one] => std::fs::metadata(one).map(|m| convert::human_size(m.len())).unwrap_or_default(),
        many => format!("{} files", many.len()),
    };
    Scene { hover: vec![0.0; slots.len()], slots, open: 0.0, main: src.label().into(), sub, main_accent: false }
}

/// Sector under the screen point, relative to the current menu.
fn sector_for(o: &Ov, pt: POINTL) -> Option<usize> {
    let cx = o.origin.0 as f32 + CENTER * o.dpi;
    let cy = o.origin.1 as f32 + CENTER * o.dpi;
    sector_at((pt.x as f32 - cx) / o.dpi, (pt.y as f32 - cy) / o.dpi, o.scene.slots.len(), DEAD_R, HIT_OUT)
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
            let src = o.files.first().and_then(|f| convert::source_format(f));
            if let Some(src) = src {
                let fresh = build_scene(src, &o.files);
                o.scene.main = fresh.main;
                o.scene.sub = fresh.sub;
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
        o.files = files;
        o.phase = Phase::Open;
        o.target_open = 1.0;
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
        Some((ALL[idx], o.files.clone()))
    })
    .flatten();
    if let Some((fmt, files)) = job {
        *effect = COPY;
        let h = hwnd() as usize;
        std::thread::spawn(move || run_job(h, fmt, files));
    }
    begin_close();
    0
}

/// Converts on a worker thread and reports the outcome to the UI thread.
fn run_job(h: usize, fmt: Fmt, files: Vec<PathBuf>) {
    let mut ok = Vec::new();
    let mut errs = Vec::new();
    for f in &files {
        match convert::convert(f, fmt) {
            Ok(p) => ok.push(p),
            Err(e) => errs.push(e),
        }
    }
    let text = match (ok.as_slice(), errs.first()) {
        ([one], None) => {
            format!("Saved {}", one.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default())
        }
        (_, None) => format!("Converted {} files to {}", ok.len(), fmt.label()),
        (_, Some(e)) => format!("Failed: {e}"),
    };
    let boxed = Box::into_raw(Box::new(text));
    unsafe {
        if PostMessageW(h as HWND, WM_DONE, 0, boxed as LPARAM) == 0 {
            drop(Box::from_raw(boxed));
        }
    }
}
