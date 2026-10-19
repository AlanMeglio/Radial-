//! The action dialogs (CROP, ADD BG, EDIT, ANNOTATE, REDACT): native Win32 windows, custom drawn with tiny-skia
//! in the settings window's visual language. A dialog is created when an action is dropped on, owns every buffer
//! it needs, runs heavy work on a worker thread and frees everything when it closes.
//!
//! `Shell` is the window plumbing (DPI, dark mode, buffers, input, jobs); each dialog is a [`Dialog`]
//! implementation that draws itself with [`ui::Pen`] and reacts to [`Ev`] events.

mod annotate;
mod base;
mod bg;
mod crop;
mod edit;
pub mod geom;
mod redact;
mod rename;
mod resize;
pub mod ui;

use std::cell::RefCell;
use std::collections::HashMap;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::path::PathBuf;
use std::ptr::{null, null_mut};
use std::time::Instant;

use tiny_skia::{Color, Pixmap};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForMonitor;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::{tr, Lang};
use crate::render::Face;
use crate::ring::ActionKind;
use crate::settings::{palette, system_is_dark};
use crate::theme::{FontKind, Rgba};
use ui::{Hit, Kind, Pen};

const WM_JOB: u32 = WM_APP + 40;
const WM_MOUSELEAVE: u32 = 0x02A3;
const TIMER_BUSY: usize = 1;

/// Pointer shape a dialog asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Cursor {
    Arrow,
    Cross,
    Hand,
    IBeam,
    SizeAll,
    SizeNwse,
    SizeNesw,
    SizeNs,
    SizeWe,
}

/// Input a dialog reacts to. Coordinates are logical pixels in the client area.
#[derive(Clone, Copy, Debug)]
pub enum Ev {
    /// Pressed on a `Kind::Drag` hit (sliders, the canvas); captured until `Up`.
    Down {
        x: f32,
        y: f32,
        id: u32,
    },
    Move {
        x: f32,
        y: f32,
        down: bool,
    },
    Up,
    /// A button, chip, swatch or check box was pressed and released (or activated from the keyboard).
    Click(u32),
    Key {
        vk: u32,
        ctrl: bool,
    },
    Char(char),
}

/// What a worker thread hands back to the dialog that started it.
pub enum JobResult {
    Loaded(Result<base::Loaded, String>),
    /// A freshly rendered preview; `gen` lets the dialog drop answers to stale requests.
    Preview {
        gen: u32,
        img: image::RgbaImage,
    },
    /// A picture chosen as background, scaled down for the preview, with its file name.
    Picture(Result<(image::RgbaImage, String), String>),
    Saved(Result<PathBuf, String>),
}

type Job = Box<dyn FnOnce() -> JobResult + Send>;

/// What the shell lets a dialog see and ask for while it handles an event.
pub struct Host {
    pub hwnd: HWND,
    pub lang: Lang,
    /// Device pixels per logical pixel.
    pub s: f32,
    pub dark: bool,
    pub cursor: Cursor,
    pub close: bool,
    dirty: bool,
    jobs: Vec<Job>,
    hits: Vec<Hit>,
    order: Vec<u32>,
    hot: Option<u32>,
    pressed: Option<u32>,
    pub focus: Option<u32>,
    drag: Option<u32>,
    time: f32,
}

impl Host {
    pub fn redraw(&mut self) {
        self.dirty = true;
    }

    /// Logical rectangle a widget registered during the last draw.
    pub fn rect(&self, id: u32) -> Option<ui::R> {
        self.hits.iter().rev().find(|h| h.id == id).map(|h| h.r)
    }

    /// Runs `f` on a worker thread; its result comes back through `Dialog::job`.
    pub fn spawn(&mut self, f: impl FnOnce() -> JobResult + Send + 'static) {
        self.jobs.push(Box::new(f));
    }

    pub fn set_focus(&mut self, id: Option<u32>) {
        self.focus = id;
        self.dirty = true;
    }

    pub fn t(&self, key: &str) -> &'static str {
        tr(self.lang, key)
    }
}

pub trait Dialog {
    fn title(&self, lang: Lang) -> String;
    /// Client size in logical pixels.
    fn size(&self) -> (f32, f32);
    /// Called once the window exists: start loading.
    fn start(&mut self, host: &mut Host);
    fn draw(&mut self, pen: &mut Pen);
    fn event(&mut self, ev: Ev, host: &mut Host);
    fn job(&mut self, result: JobResult, host: &mut Host);
    /// A worker is running: the shell animates and refuses to close while saving.
    fn busy(&self) -> bool;
    fn saving(&self) -> bool;
    /// The dialog is editing text (Esc cancels that instead of closing).
    fn editing(&self) -> bool {
        false
    }
    /// A key the focused widget did not use; Enter applies.
    fn default_action(&mut self, host: &mut Host);
}

struct Shell {
    hwnd: HWND,
    w: i32,
    h: i32,
    pm: Pixmap,
    dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    face: Face,
    dlg: Box<dyn Dialog>,
    host: Host,
    accent: Rgba,
    tracking: bool,
    timer: bool,
    last_tick: Instant,
}

static STANDALONE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

/// In `--open` mode the process quits when the last dialog closes.
pub fn set_standalone() {
    STANDALONE.store(true, std::sync::atomic::Ordering::Relaxed);
}

thread_local! {
    static SHELLS: RefCell<HashMap<isize, Shell>> = RefCell::new(HashMap::new());
}

/// Runs `f` on the shell of `hwnd`. The shell is out of the map meanwhile, so a message sent back to the same
/// window from inside `f` is ignored rather than re-entering.
fn with_shell<R>(hwnd: HWND, f: impl FnOnce(&mut Shell) -> R) -> Option<R> {
    let mut sh = SHELLS.with(|m| m.borrow_mut().remove(&(hwnd as isize)))?;
    let r = f(&mut sh);
    SHELLS.with(|m| m.borrow_mut().insert(hwnd as isize, sh));
    Some(r)
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Opening
// ---------------------------------------------------------------------------------------------------------------

#[link(name = "user32")]
extern "system" {
    fn AdjustWindowRectExForDpi(rect: *mut RECT, style: u32, menu: i32, ex: u32, dpi: u32) -> i32;
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: isize, attr: u32, val: *const c_void, size: u32) -> i32;
}

unsafe fn make_buffers(hwnd: HWND, w: i32, h: i32) -> Option<(Pixmap, HDC, HBITMAP, HGDIOBJ, *mut u8)> {
    let screen = GetDC(hwnd);
    let dc = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    let mut bits: *mut c_void = null_mut();
    let dib = CreateDIBSection(screen, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    ReleaseDC(hwnd, screen);
    if dib.is_null() || bits.is_null() {
        DeleteDC(dc);
        return None;
    }
    let old = SelectObject(dc, dib);
    Some((Pixmap::new(w as u32, h as u32)?, dc, dib, old, bits as *mut u8))
}

unsafe fn free_buffers(sh: &Shell) {
    SelectObject(sh.dc, sh.old);
    DeleteObject(sh.dib);
    DeleteDC(sh.dc);
}

unsafe fn cursor_dpi() -> u32 {
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let (mut dx, mut dy) = (96u32, 96u32);
    GetDpiForMonitor(mon, 0, &mut dx, &mut dy);
    dx
}

/// Opens the dialog for `action` on `paths` (the picture tools use the first one; resize and rename take them all).
/// Actions without a dialog are ignored.
pub unsafe fn open(action: ActionKind, paths: Vec<PathBuf>) {
    let lang = crate::current_config().lang;
    let Some(path) = paths.first().cloned() else { return };
    let dlg: Box<dyn Dialog> = match action {
        ActionKind::Crop => Box::new(crop::CropDialog::new(path)),
        ActionKind::AddBg => Box::new(bg::BgDialog::new(path)),
        ActionKind::Edit => Box::new(edit::EditDialog::new(path)),
        ActionKind::Annotate => Box::new(annotate::AnnotateDialog::new(path)),
        ActionKind::Redact => Box::new(redact::RedactDialog::new(path)),
        _ => return,
    };
    open_dialog(dlg, lang, cursor_dpi());
}

/// Creates the window for `dlg` and shows it. `dpi` is the monitor DPI to size it for.
unsafe fn open_dialog(dlg: Box<dyn Dialog>, lang: Lang, dpi: u32) {
    let hinst = GetModuleHandleW(null());
    let cls = wide("RadialDialog");
    let mut wc: WNDCLASSW = zeroed();
    wc.lpfnWndProc = Some(proc);
    wc.hInstance = hinst;
    wc.lpszClassName = cls.as_ptr();
    wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
    RegisterClassW(&wc); // fails harmlessly when it already exists

    let s = dpi as f32 / 96.0;
    let (lw, lh) = dlg.size();
    let (cw, ch) = ((lw * s).round() as i32, (lh * s).round() as i32);
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let mut r = RECT { left: 0, top: 0, right: cw, bottom: ch };
    AdjustWindowRectExForDpi(&mut r, style, 0, 0, dpi);
    let (ww, wh) = (r.right - r.left, r.bottom - r.top);
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    let mut mi: MONITORINFO = zeroed();
    mi.cbSize = size_of::<MONITORINFO>() as u32;
    GetMonitorInfoW(MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST), &mut mi);
    let a = mi.rcWork;
    let x = (a.left + (a.right - a.left - ww) / 2).max(a.left);
    let y = (a.top + (a.bottom - a.top - wh) / 3).max(a.top);
    let title = wide(&dlg.title(lang));
    let hwnd = CreateWindowExW(
        WS_EX_APPWINDOW,
        cls.as_ptr(),
        title.as_ptr(),
        style,
        x,
        y,
        ww,
        wh,
        null_mut(),
        null_mut(),
        hinst,
        null(),
    );
    if hwnd.is_null() {
        return;
    }
    let dark = system_is_dark();
    let d: i32 = dark as i32;
    DwmSetWindowAttribute(hwnd as isize, 20, &d as *const i32 as *const c_void, 4);
    let Some((pm, dc, dib, old, bits)) = make_buffers(hwnd, cw, ch) else {
        DestroyWindow(hwnd);
        return;
    };
    let Some(face) = Face::load(FontKind::Variable, 520.0) else {
        DestroyWindow(hwnd);
        return;
    };
    let host = Host {
        hwnd,
        lang,
        s,
        dark,
        cursor: Cursor::Arrow,
        close: false,
        dirty: true,
        jobs: vec![],
        hits: vec![],
        order: vec![],
        hot: None,
        pressed: None,
        focus: None,
        drag: None,
        time: 0.0,
    };
    let mut sh = Shell {
        hwnd,
        w: cw,
        h: ch,
        pm,
        dc,
        dib,
        old,
        bits,
        face,
        dlg,
        host,
        accent: crate::settings::system_accent(),
        tracking: false,
        timer: false,
        last_tick: Instant::now(),
    };
    sh.dlg.start(&mut sh.host);
    render(&mut sh);
    after(&mut sh);
    SHELLS.with(|m| m.borrow_mut().insert(hwnd as isize, sh));
    ShowWindow(hwnd, SW_SHOW);
    bring_to_front(hwnd);
}

/// A background process cannot always take the foreground; the topmost flip puts the window in front regardless.
unsafe fn bring_to_front(hwnd: HWND) {
    SetWindowPos(hwnd, HWND_TOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    SetWindowPos(hwnd, HWND_NOTOPMOST, 0, 0, 0, 0, SWP_NOMOVE | SWP_NOSIZE);
    SetForegroundWindow(hwnd);
}

// ---------------------------------------------------------------------------------------------------------------
// Drawing
// ---------------------------------------------------------------------------------------------------------------

fn render(sh: &mut Shell) {
    let pal = palette(sh.host.dark, sh.accent);
    sh.pm.fill(Color::from_rgba8(pal.bg.0, pal.bg.1, pal.bg.2, 255));
    sh.host.hits.clear();
    sh.host.order.clear();
    let h = &mut sh.host;
    let mut pen = Pen {
        pm: &mut sh.pm,
        face: &mut sh.face,
        pal: &pal,
        s: h.s,
        hits: &mut h.hits,
        order: &mut h.order,
        hot: h.hot,
        pressed: h.pressed,
        focus: h.focus,
        lang: h.lang,
        dark: h.dark,
        time: h.time,
    };
    sh.dlg.draw(&mut pen);
    // 32-bit DIB, premultiplied RGBA -> BGRA; the dialog background is opaque.
    let src = sh.pm.data();
    let dst = unsafe { std::slice::from_raw_parts_mut(sh.bits, src.len()) };
    for (d, s) in dst.chunks_exact_mut(4).zip(src.chunks_exact(4)) {
        d[0] = s[2];
        d[1] = s[1];
        d[2] = s[0];
        d[3] = 255;
    }
    sh.host.dirty = false;
}

/// Runs what an event asked for: repaint, worker threads, the busy timer, closing.
unsafe fn after(sh: &mut Shell) {
    if sh.host.dirty {
        InvalidateRect(sh.hwnd, null(), 0);
    }
    for job in sh.host.jobs.drain(..) {
        let h = sh.hwnd as usize;
        let _ = std::thread::Builder::new().name("radial-job".into()).spawn(move || {
            let boxed = Box::into_raw(Box::new(job()));
            if unsafe { PostMessageW(h as HWND, WM_JOB, 0, boxed as LPARAM) } == 0 {
                drop(unsafe { Box::from_raw(boxed) });
            }
        });
    }
    let busy = sh.dlg.busy();
    if busy && !sh.timer {
        sh.timer = true;
        sh.last_tick = Instant::now();
        SetTimer(sh.hwnd, TIMER_BUSY, 33, None);
    } else if !busy && sh.timer {
        sh.timer = false;
        KillTimer(sh.hwnd, TIMER_BUSY);
        sh.host.dirty = true;
        InvalidateRect(sh.hwnd, null(), 0);
    }
    if sh.host.close {
        sh.host.close = false;
        PostMessageW(sh.hwnd, WM_CLOSE, 1, 0);
    }
}

fn hit_at(host: &Host, x: f32, y: f32) -> Option<Hit> {
    host.hits.iter().rev().find(|h| ui::contains(h.r, x, y)).copied()
}

fn key_mods() -> (bool, bool) {
    unsafe { (GetKeyState(VK_SHIFT as i32) < 0, GetKeyState(VK_CONTROL as i32) < 0) }
}

/// Moves the keyboard focus through the focusable widgets in draw order.
fn cycle_focus(host: &mut Host, back: bool) {
    let n = host.order.len();
    if n == 0 {
        return;
    }
    let cur = host.focus.and_then(|f| host.order.iter().position(|&i| i == f));
    let next = match (cur, back) {
        (None, false) => 0,
        (None, true) => n - 1,
        (Some(i), false) => (i + 1) % n,
        (Some(i), true) => (i + n - 1) % n,
    };
    host.focus = Some(host.order[next]);
    host.dirty = true;
}

fn is_field(host: &Host, id: u32) -> bool {
    host.hits.iter().any(|h| h.id == id && h.kind == Kind::Field)
}

unsafe fn cursor_handle(c: Cursor) -> HCURSOR {
    let id = match c {
        Cursor::Arrow => IDC_ARROW,
        Cursor::Cross => IDC_CROSS,
        Cursor::Hand => IDC_HAND,
        Cursor::IBeam => IDC_IBEAM,
        Cursor::SizeAll => IDC_SIZEALL,
        Cursor::SizeNwse => IDC_SIZENWSE,
        Cursor::SizeNesw => IDC_SIZENESW,
        Cursor::SizeNs => IDC_SIZENS,
        Cursor::SizeWe => IDC_SIZEWE,
    };
    LoadCursorW(null_mut(), id)
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (mx, my) = ((lp & 0xFFFF) as i16 as f32, ((lp >> 16) & 0xFFFF) as i16 as f32);
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            with_shell(hwnd, |sh| {
                if sh.host.dirty {
                    render(sh);
                }
                BitBlt(hdc, 0, 0, sh.w, sh.h, sh.dc, 0, 0, SRCCOPY);
            });
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_SETCURSOR if (lp & 0xFFFF) as u32 == HTCLIENT => {
            let c = with_shell(hwnd, |sh| sh.host.cursor).unwrap_or(Cursor::Arrow);
            SetCursor(cursor_handle(c));
            1
        }
        WM_LBUTTONDOWN => {
            with_shell(hwnd, |sh| {
                let s = sh.host.s;
                let (x, y) = (mx / s, my / s);
                let hit = hit_at(&sh.host, x, y);
                sh.host.pressed = hit.map(|h| h.id);
                match hit {
                    Some(h) if h.kind == Kind::Field => {
                        sh.host.focus = Some(h.id);
                        sh.dlg.event(Ev::Click(h.id), &mut sh.host);
                    }
                    Some(h) if h.kind == Kind::Drag => {
                        sh.host.drag = Some(h.id);
                        SetCapture(hwnd);
                        sh.dlg.event(Ev::Down { x, y, id: h.id }, &mut sh.host);
                    }
                    Some(_) => {}
                    None => sh.host.focus = None,
                }
                sh.host.dirty = true;
                after(sh);
            });
            0
        }
        WM_MOUSEMOVE => {
            with_shell(hwnd, |sh| {
                if !sh.tracking {
                    let mut t = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut t);
                    sh.tracking = true;
                }
                let s = sh.host.s;
                let (x, y) = (mx / s, my / s);
                let hit = hit_at(&sh.host, x, y);
                let hot = hit.map(|h| h.id);
                if hot != sh.host.hot {
                    sh.host.hot = hot;
                    sh.host.dirty = true;
                }
                // Buttons get a hand and text fields an I-beam; a dialog can override this for its canvas.
                sh.host.cursor = match hit.map(|h| h.kind) {
                    Some(Kind::Button) => Cursor::Hand,
                    Some(Kind::Field) => Cursor::IBeam,
                    _ => Cursor::Arrow,
                };
                sh.dlg.event(Ev::Move { x, y, down: sh.host.drag.is_some() }, &mut sh.host);
                after(sh);
            });
            0
        }
        WM_MOUSELEAVE => {
            with_shell(hwnd, |sh| {
                sh.tracking = false;
                sh.host.hot = None;
                sh.host.dirty = true;
                after(sh);
            });
            0
        }
        WM_LBUTTONUP => {
            with_shell(hwnd, |sh| {
                let s = sh.host.s;
                let (x, y) = (mx / s, my / s);
                let pressed = sh.host.pressed.take();
                if sh.host.drag.take().is_some() {
                    ReleaseCapture();
                    sh.dlg.event(Ev::Up, &mut sh.host);
                } else if let (Some(p), Some(h)) = (pressed, hit_at(&sh.host, x, y)) {
                    if p == h.id && h.kind == Kind::Button {
                        sh.host.focus = None;
                        sh.dlg.event(Ev::Click(p), &mut sh.host);
                    }
                }
                sh.host.dirty = true;
                after(sh);
            });
            0
        }
        WM_KEYDOWN => {
            with_shell(hwnd, |sh| {
                let vk = wp as u32;
                let (shift, ctrl) = key_mods();
                let editing = sh.dlg.editing();
                let in_field = sh.host.focus.is_some_and(|f| is_field(&sh.host, f));
                match vk {
                    v if v == VK_ESCAPE as u32 && !editing => sh.host.close = true,
                    v if v == VK_TAB as u32 && !editing => cycle_focus(&mut sh.host, shift),
                    v if v == VK_RETURN as u32 && !editing => match sh.host.focus {
                        Some(f) if !in_field => sh.dlg.event(Ev::Click(f), &mut sh.host),
                        _ => sh.dlg.default_action(&mut sh.host),
                    },
                    v if v == VK_SPACE as u32 && !in_field && !editing && sh.host.focus.is_some() => {
                        if let Some(f) = sh.host.focus {
                            sh.dlg.event(Ev::Click(f), &mut sh.host);
                        }
                    }
                    _ => sh.dlg.event(Ev::Key { vk, ctrl }, &mut sh.host),
                }
                sh.host.dirty = true;
                after(sh);
            });
            0
        }
        WM_CHAR => {
            with_shell(hwnd, |sh| {
                if let Some(c) = char::from_u32(wp as u32).filter(|c| !c.is_control()) {
                    sh.dlg.event(Ev::Char(c), &mut sh.host);
                    sh.host.dirty = true;
                    after(sh);
                }
            });
            0
        }
        WM_TIMER if wp == TIMER_BUSY => {
            with_shell(hwnd, |sh| {
                let now = Instant::now();
                sh.host.time += (now - sh.last_tick).as_secs_f32();
                sh.last_tick = now;
                sh.host.dirty = true;
                after(sh);
            });
            0
        }
        WM_JOB => {
            let result = *Box::from_raw(lp as *mut JobResult);
            with_shell(hwnd, |sh| {
                sh.dlg.job(result, &mut sh.host);
                sh.host.dirty = true;
                after(sh);
            });
            0
        }
        WM_DPICHANGED => {
            let new_dpi = (wp & 0xFFFF) as u32;
            let r = &*(lp as *const RECT);
            SetWindowPos(
                hwnd,
                null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            with_shell(hwnd, |sh| {
                let s = new_dpi as f32 / 96.0;
                let (lw, lh) = sh.dlg.size();
                let (w, h) = ((lw * s).round() as i32, (lh * s).round() as i32);
                free_buffers(sh);
                if let Some((pm, dc, dib, old, bits)) = make_buffers(hwnd, w, h) {
                    (sh.pm, sh.dc, sh.dib, sh.old, sh.bits, sh.w, sh.h, sh.host.s) = (pm, dc, dib, old, bits, w, h, s);
                    sh.host.dirty = true;
                    InvalidateRect(hwnd, null(), 0);
                }
            });
            0
        }
        WM_CLOSE => {
            // `wp == 1`: closing requested by the dialog itself (after a successful save).
            let refuse = wp == 0 && with_shell(hwnd, |sh| sh.dlg.saving()).unwrap_or(false);
            if refuse {
                return 0;
            }
            if let Some(sh) = SHELLS.with(|m| m.borrow_mut().remove(&(hwnd as isize))) {
                if sh.timer {
                    KillTimer(hwnd, TIMER_BUSY);
                }
                free_buffers(&sh);
                drop(sh); // the dialog, its pixmaps and the font go with it
            }
            DestroyWindow(hwnd);
            if STANDALONE.load(std::sync::atomic::Ordering::Relaxed) && SHELLS.with(|m| m.borrow().is_empty()) {
                PostQuitMessage(0);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Offscreen rendering for review: `radial --dialog crop|bg|edit|annotate|redact out.png [--image f] [--lang es]`
// ---------------------------------------------------------------------------------------------------------------

pub fn cli(args: &[String]) -> bool {
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let Some(which) = get("--dialog") else { return false };
    let Some(out) = get("--out") else { return true };
    let lang = if get("--lang").as_deref() == Some("es") { Lang::Es } else { Lang::En };
    let s: f32 = get("--dpi").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let src = get("--image").map(PathBuf::from).unwrap_or_else(sample_image);
    let mut dlg: Box<dyn Dialog> = match which.as_str() {
        "crop" => Box::new(crop::CropDialog::new(src)),
        "bg" => Box::new(bg::BgDialog::new(src)),
        "edit" => Box::new(edit::EditDialog::new(src)),
        "annotate" => Box::new(annotate::AnnotateDialog::new(src)),
        "redact" => Box::new(redact::RedactDialog::new(src)),
        _ => return true,
    };
    let (lw, lh) = dlg.size();
    let (w, h) = ((lw * s).round() as u32, (lh * s).round() as u32);
    let Some(mut pm) = Pixmap::new(w, h) else { return true };
    let Some(mut face) = Face::load(FontKind::Variable, 520.0) else { return true };
    let user = crate::config::user_themes();
    let mut cfg = crate::config::Config::default();
    if let Some(t) = get("--theme") {
        cfg.apply_theme(&crate::theme::find(&t, &user));
    }
    let accent = crate::settings::system_accent();
    let dark = !args.iter().any(|a| a == "--light");
    let mut host = Host {
        hwnd: null_mut(),
        lang,
        s,
        dark,
        cursor: Cursor::Arrow,
        close: false,
        dirty: true,
        jobs: vec![],
        hits: vec![],
        order: vec![],
        hot: None,
        pressed: None,
        focus: None,
        drag: None,
        time: 0.0,
    };
    dlg.start(&mut host);
    // Offscreen there is no message loop: run the jobs inline and feed the results back.
    let mut rounds = 0;
    while !host.jobs.is_empty() && rounds < 8 {
        rounds += 1;
        for job in std::mem::take(&mut host.jobs) {
            let r = job();
            dlg.job(r, &mut host);
        }
    }
    if let Some(script) = get("--script") {
        script_events(&mut *dlg, &mut host, &script, &mut pm, &mut face, accent);
    }
    let pal = palette(dark, accent);
    pm.fill(Color::from_rgba8(pal.bg.0, pal.bg.1, pal.bg.2, 255));
    let h2 = &mut host;
    let mut pen = Pen {
        pm: &mut pm,
        face: &mut face,
        pal: &pal,
        s,
        hits: &mut h2.hits,
        order: &mut h2.order,
        hot: None,
        pressed: None,
        focus: h2.focus,
        lang,
        dark,
        time: 0.0,
    };
    dlg.draw(&mut pen);
    let img = image::RgbaImage::from_raw(w, h, pm.data().to_vec()).expect("pixmap size");
    img.save(out).expect("save dialog png");
    true
}

/// `--script "click:12;drag:x1,y1,x2,y2;key:27"`: drives a dialog offscreen so previews can show a working state.
fn script_events(dlg: &mut dyn Dialog, host: &mut Host, script: &str, pm: &mut Pixmap, face: &mut Face, accent: Rgba) {
    let pal = palette(host.dark, accent);
    let mut draw = |dlg: &mut dyn Dialog, host: &mut Host| {
        host.hits.clear();
        host.order.clear();
        let mut pen = Pen {
            pm,
            face,
            pal: &pal,
            s: host.s,
            hits: &mut host.hits,
            order: &mut host.order,
            hot: None,
            pressed: None,
            focus: host.focus,
            lang: host.lang,
            dark: host.dark,
            time: 0.0,
        };
        dlg.draw(&mut pen);
    };
    draw(dlg, host);
    for step in script.split(';').map(str::trim).filter(|s| !s.is_empty()) {
        let (cmd, arg) = step.split_once(':').unwrap_or((step, ""));
        let nums: Vec<f32> = arg.split(',').filter_map(|v| v.trim().parse().ok()).collect();
        match (cmd, nums.as_slice()) {
            ("click", [id]) => dlg.event(Ev::Click(*id as u32), host),
            ("drag", [x1, y1, x2, y2]) => {
                let id = hit_at(host, *x1, *y1).map_or(0, |h| h.id);
                dlg.event(Ev::Down { x: *x1, y: *y1, id }, host);
                for i in 1..=8 {
                    let t = i as f32 / 8.0;
                    dlg.event(Ev::Move { x: x1 + (x2 - x1) * t, y: y1 + (y2 - y1) * t, down: true }, host);
                }
                dlg.event(Ev::Up, host);
            }
            ("key", [vk]) => dlg.event(Ev::Key { vk: *vk as u32, ctrl: false }, host),
            ("text", _) => arg.chars().for_each(|c| dlg.event(Ev::Char(c), host)),
            ("focus", [id]) => {
                host.focus = Some(*id as u32);
                dlg.event(Ev::Click(*id as u32), host);
            }
            _ => {}
        }
        for job in std::mem::take(&mut host.jobs) {
            let r = job();
            dlg.job(r, host);
        }
        draw(dlg, host);
    }
}

/// A generated picture for previews when no `--image` is given: a soft landscape with a few shapes.
fn sample_image() -> PathBuf {
    let dir = std::env::temp_dir().join("radial-preview");
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("sample.png");
    if !path.exists() {
        let (w, h) = (1600u32, 1000u32);
        let img = image::RgbaImage::from_fn(w, h, |x, y| {
            let (fx, fy) = (x as f32 / w as f32, y as f32 / h as f32);
            let sky = [(70.0 + 120.0 * fy) as u8, (110.0 + 100.0 * fy) as u8, (190.0 + 50.0 * (1.0 - fy)) as u8];
            let hill = (0.62 + 0.08 * (fx * 7.0).sin() + 0.05 * (fx * 17.0).cos()) < fy;
            let sun = ((fx - 0.72).powi(2) * 1.6 + (fy - 0.28).powi(2)).sqrt() < 0.09;
            let c = if sun {
                [255, 214, 120]
            } else if hill {
                [(40.0 + 50.0 * fx) as u8, (110.0 + 40.0 * (1.0 - fy)) as u8, 70]
            } else {
                sky
            };
            image::Rgba([c[0], c[1], c[2], 255])
        });
        let _ = img.save(&path);
    }
    path
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host {
            hwnd: null_mut(),
            lang: Lang::En,
            s: 1.0,
            dark: true,
            cursor: Cursor::Arrow,
            close: false,
            dirty: false,
            jobs: vec![],
            hits: vec![],
            order: vec![10, 11, 12],
            hot: None,
            pressed: None,
            focus: None,
            drag: None,
            time: 0.0,
        }
    }

    #[test]
    fn tab_moves_the_focus_forwards_and_shift_tab_backwards_with_wrap() {
        let mut h = host();
        cycle_focus(&mut h, false);
        assert_eq!(h.focus, Some(10));
        cycle_focus(&mut h, false);
        cycle_focus(&mut h, false);
        assert_eq!(h.focus, Some(12));
        cycle_focus(&mut h, false);
        assert_eq!(h.focus, Some(10), "wraps");
        cycle_focus(&mut h, true);
        assert_eq!(h.focus, Some(12));
        let mut none = host();
        cycle_focus(&mut none, true);
        assert_eq!(none.focus, Some(12), "backwards from nothing starts at the end");
    }

    #[test]
    fn the_topmost_hit_wins() {
        let mut h = host();
        h.hits.push(Hit { r: [0.0, 0.0, 100.0, 100.0], id: 1, kind: Kind::Drag });
        h.hits.push(Hit { r: [10.0, 10.0, 20.0, 20.0], id: 2, kind: Kind::Button });
        assert_eq!(hit_at(&h, 15.0, 15.0).map(|x| x.id), Some(2));
        assert_eq!(hit_at(&h, 80.0, 80.0).map(|x| x.id), Some(1));
        assert!(hit_at(&h, 150.0, 5.0).is_none());
        assert_eq!(h.rect(2), Some([10.0, 10.0, 20.0, 20.0]));
    }
}
