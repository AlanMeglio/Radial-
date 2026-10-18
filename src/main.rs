#![windows_subsystem = "windows"]
// tiny-skia paints are configured field by field throughout the renderer.
#![allow(clippy::field_reassign_with_default)]
//! Radial: drag a file in Explorer, hold Shift, drop it on a sector of the radial menu to convert it.
//! Tray app. Idle cost is zero: only low-level hooks and the message loop run until a drag starts.

mod config;
mod convert;
mod dialogs;
mod engine;
mod geometry;
mod jobs;
mod menus;
mod overlay;
mod render;
mod ring;
mod settings;
mod theme;
mod trigger;

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use config::{tr, Config, Lang, TriggerKey};
use render::Look;
use trigger::{Act, Pt, Tracker};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Ole::{OleInitialize, RegisterDragDrop, RevokeDragDrop};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const ID_QUIT: usize = 1;
const ID_SETTINGS: usize = 2;
const ID_PAUSE: usize = 3;
const ID_AUTOSTART: usize = 4;
const ID_ABOUT: usize = 5;
/// Test knob: `RADIAL_ANY_SURFACE=1` accepts drags that start anywhere (used by the end-to-end script).
static ANY_SURFACE: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);
static PAUSED: AtomicBool = AtomicBool::new(false);
/// Tab is held down (so key repeat does not flip the ring over and over).
static TAB_DOWN: AtomicBool = AtomicBool::new(false);
/// 0 shift, 1 ctrl, 2 alt, 3 none; read by the hook procedures, which must not touch the app state.
static TRIGGER: AtomicU32 = AtomicU32::new(0);

struct App {
    cfg: Config,
    user_themes: Vec<theme::Theme>,
    look: Look,
}

thread_local! {
    static TRACKER: RefCell<Tracker> = RefCell::new(Tracker::default());
    static APP: RefCell<Option<App>> = const { RefCell::new(None) };
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------- Shared settings state ----------

pub fn current_config() -> Config {
    APP.with(|a| a.borrow().as_ref().map(|a| a.cfg.clone()).unwrap_or_default())
}

pub fn current_look() -> Look {
    APP.with(|a| a.borrow().as_ref().map(|a| a.look.clone()).unwrap_or_else(|| Look::new(&Config::default(), &[])))
}

pub fn user_themes() -> Vec<theme::Theme> {
    APP.with(|a| a.borrow().as_ref().map(|a| a.user_themes.clone()).unwrap_or_default())
}

fn trigger_code(t: TriggerKey) -> u32 {
    match t {
        TriggerKey::Shift => 0,
        TriggerKey::Ctrl => 1,
        TriggerKey::Alt => 2,
        TriggerKey::None => 3,
    }
}

/// Virtual-key codes (generic, left, right) of the trigger key, or none.
fn trigger_keys() -> Option<[u32; 3]> {
    match TRIGGER.load(Relaxed) {
        0 => Some([VK_SHIFT as u32, VK_LSHIFT as u32, VK_RSHIFT as u32]),
        1 => Some([VK_CONTROL as u32, VK_LCONTROL as u32, VK_RCONTROL as u32]),
        2 => Some([VK_MENU as u32, VK_LMENU as u32, VK_RMENU as u32]),
        _ => None,
    }
}

pub fn key_name(t: TriggerKey, lang: Lang) -> &'static str {
    match t {
        TriggerKey::Shift => "Shift",
        TriggerKey::Ctrl => "Ctrl",
        TriggerKey::Alt => "Alt",
        TriggerKey::None => tr(lang, "key-any"),
    }
}

/// Applies and persists a new configuration: look, trigger key, autostart, tray tooltip and icon.
pub fn set_config(cfg: Config) {
    let user = user_themes_cached();
    let look = Look::new(&cfg, &user);
    TRIGGER.store(trigger_code(cfg.trigger), Relaxed);
    let autostart_changed = APP.with(|a| !matches!(&*a.borrow(), Some(x) if x.cfg.autostart == cfg.autostart));
    if autostart_changed {
        set_autostart(cfg.autostart);
    }
    let _ = config::save(&cfg);
    APP.with(|a| *a.borrow_mut() = Some(App { cfg, user_themes: user, look }));
    unsafe { refresh_tray(overlay_hwnd()) };
}

fn user_themes_cached() -> Vec<theme::Theme> {
    let have = APP.with(|a| a.borrow().as_ref().map(|a| a.user_themes.clone()));
    have.unwrap_or_else(config::user_themes)
}

// ---------- Start with Windows (HKCU Run) ----------

#[link(name = "advapi32")]
extern "system" {
    fn RegSetKeyValueW(key: isize, sub: *const u16, name: *const u16, ty: u32, data: *const c_void, cb: u32) -> i32;
    fn RegDeleteKeyValueW(key: isize, sub: *const u16, name: *const u16) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn MessageBeep(kind: u32) -> i32;
}

#[link(name = "kernel32")]
extern "system" {
    fn GetUserDefaultLocaleName(name: *mut u16, cch: i32) -> i32;
}

const HKCU: isize = 0x8000_0001u32 as i32 as isize;
const RUN_KEY: &str = "Software\\Microsoft\\Windows\\CurrentVersion\\Run";

pub fn set_autostart(on: bool) {
    let (sub, name) = (wide(RUN_KEY), wide("Radial"));
    unsafe {
        if on {
            let Ok(exe) = std::env::current_exe() else { return };
            let val = wide(&format!("\"{}\"", exe.display()));
            RegSetKeyValueW(
                HKCU,
                sub.as_ptr(),
                name.as_ptr(),
                1,
                val.as_ptr() as *const c_void,
                (val.len() * 2) as u32,
            );
        } else {
            RegDeleteKeyValueW(HKCU, sub.as_ptr(), name.as_ptr());
        }
    }
}

fn system_language() -> Lang {
    let mut buf = [0u16; 85];
    let n = unsafe { GetUserDefaultLocaleName(buf.as_mut_ptr(), buf.len() as i32) };
    let s = String::from_utf16_lossy(&buf[..(n.max(1) - 1) as usize]);
    if s.to_ascii_lowercase().starts_with("es") {
        Lang::Es
    } else {
        Lang::En
    }
}

// ---------- Hooks ----------

/// Posts what the tracker decided to the overlay window; the hook itself must return fast.
unsafe fn dispatch(act: Act, on_button_up: bool) {
    let h = overlay_hwnd();
    match act {
        Act::Show(p) => {
            PostMessageW(
                h,
                overlay::WM_SHOW,
                0,
                ((p.x as i16 as u16 as isize) | ((p.y as i16 as u16 as isize) << 16)) as LPARAM,
            );
        }
        // After button-up the OLE drop still has to land, so closing is delayed a little.
        Act::Hide => {
            PostMessageW(h, if on_button_up { overlay::WM_HIDE_SOON } else { overlay::WM_HIDE }, 0, 0);
        }
        Act::None => {}
    }
}

fn overlay_hwnd() -> HWND {
    OVERLAY.with(|c| c.get())
}

thread_local! {
    static OVERLAY: std::cell::Cell<HWND> = const { std::cell::Cell::new(null_mut()) };
}

unsafe fn root_class_at(p: POINT) -> String {
    let w = WindowFromPoint(p);
    let root = if w.is_null() { w } else { GetAncestor(w, GA_ROOT) };
    let mut buf = [0u16; 64];
    let n = if root.is_null() { 0 } else { GetClassNameW(root, buf.as_mut_ptr(), buf.len() as i32) };
    String::from_utf16_lossy(&buf[..n.max(0) as usize])
}

unsafe extern "system" fn mouse_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && !PAUSED.load(Relaxed) {
        let ms = &*(lp as *const MSLLHOOKSTRUCT);
        let p = Pt { x: ms.pt.x, y: ms.pt.y };
        let mut up = false;
        let act = TRACKER.with(|t| {
            let mut t = t.borrow_mut();
            match wp as u32 {
                WM_LBUTTONDOWN => {
                    let eligible = ANY_SURFACE.load(Relaxed) || trigger::is_file_surface(&root_class_at(ms.pt));
                    t.button_down(p, eligible)
                }
                WM_MOUSEMOVE => {
                    let held = match trigger_keys() {
                        None => true,
                        Some(k) => GetAsyncKeyState(k[0] as i32) < 0,
                    };
                    t.moved(p, held)
                }
                WM_LBUTTONUP => {
                    up = true;
                    t.button_up()
                }
                _ => Act::None,
            }
        });
        dispatch(act, up);
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

unsafe extern "system" fn key_proc(code: i32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if code == HC_ACTION as i32 && !PAUSED.load(Relaxed) {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        // While a menu is up Tab flips between the rings. It is swallowed so the drag source never sees it.
        if k.vkCode == VK_TAB as u32 && overlay::tab_wanted() {
            let down = matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            if down && !TAB_DOWN.swap(true, Relaxed) {
                PostMessageW(overlay_hwnd(), overlay::WM_RING_TOGGLE, 0, 0);
            } else if !down {
                TAB_DOWN.store(false, Relaxed);
            }
            return 1;
        }
        if trigger_keys().is_some_and(|keys| keys.contains(&k.vkCode)) {
            let down = matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let act = TRACKER.with(|t| t.borrow_mut().shift_changed(down));
            dispatch(act, false);
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

// ---------- Tray ----------

unsafe fn tray_icon(paused: bool) -> HICON {
    const N: u32 = 32;
    let accent = if paused { theme::Rgba::rgb(120, 126, 140) } else { current_look().theme.accent };
    let Some(pm) = render::icon_pixmap(N, accent) else { return null_mut() };
    // 32-bit colour bitmap with straight alpha (BGRA) plus an all-zero mask.
    let mut color = Vec::with_capacity((N * N * 4) as usize);
    for px in pm.pixels() {
        let c = px.demultiply();
        color.extend_from_slice(&[c.blue(), c.green(), c.red(), c.alpha()]);
    }
    let mask = vec![0u8; (N * N / 8) as usize];
    CreateIcon(GetModuleHandleW(null()), N as i32, N as i32, 1, 32, mask.as_ptr(), color.as_ptr())
}

unsafe fn nid(hwnd: HWND) -> NOTIFYICONDATAW {
    let mut n: NOTIFYICONDATAW = zeroed();
    n.cbSize = size_of::<NOTIFYICONDATAW>() as u32;
    n.hWnd = hwnd;
    n.uID = 1;
    n
}

fn put(dst: &mut [u16], s: &str) {
    let room = dst.len() - 1;
    for (d, c) in dst.iter_mut().zip(s.encode_utf16().take(room)) {
        *d = c;
    }
}

fn tip() -> String {
    let cfg = current_config();
    if PAUSED.load(Relaxed) {
        tr(cfg.lang, "tip-paused").to_string()
    } else {
        tr(cfg.lang, "tip").replace("{}", key_name(cfg.trigger, cfg.lang))
    }
}

unsafe fn fill_tray(n: &mut NOTIFYICONDATAW) -> HICON {
    n.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = tray_icon(PAUSED.load(Relaxed));
    put(&mut n.szTip, &tip());
    n.hIcon
}

unsafe fn add_tray(hwnd: HWND) {
    let mut n = nid(hwnd);
    let icon = fill_tray(&mut n);
    Shell_NotifyIconW(NIM_ADD, &n);
    DestroyIcon(icon);
}

/// Updates icon and tooltip after a setting changed.
unsafe fn refresh_tray(hwnd: HWND) {
    if hwnd.is_null() {
        return;
    }
    let mut n = nid(hwnd);
    let icon = fill_tray(&mut n);
    Shell_NotifyIconW(NIM_MODIFY, &n);
    DestroyIcon(icon);
}

unsafe fn balloon(hwnd: HWND, text: &str) {
    let mut n = nid(hwnd);
    n.uFlags = NIF_INFO;
    n.dwInfoFlags = NIIF_INFO;
    put(&mut n.szInfoTitle, "Radial");
    put(&mut n.szInfo, text);
    Shell_NotifyIconW(NIM_MODIFY, &n);
}

unsafe fn tray_menu(hwnd: HWND) {
    let cfg = current_config();
    let l = cfg.lang;
    let menu = CreatePopupMenu();
    let item = |id: usize, key: &str, checked: bool| {
        let flags = MF_STRING | if checked { MF_CHECKED } else { 0 };
        AppendMenuW(menu, flags, id, wide(tr(l, key)).as_ptr());
    };
    item(ID_SETTINGS, "settings", false);
    let paused = PAUSED.load(Relaxed);
    item(ID_PAUSE, if paused { "resume" } else { "pause" }, false);
    item(ID_AUTOSTART, "startup", cfg.autostart);
    item(ID_ABOUT, "about", false);
    AppendMenuW(menu, MF_SEPARATOR, 0, null());
    item(ID_QUIT, "quit", false);
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd); // required so the menu closes when clicking elsewhere
    TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

unsafe fn about(hwnd: HWND) {
    let text = tr(current_config().lang, "about-text").replace("{}", env!("CARGO_PKG_VERSION"));
    MessageBoxW(hwnd, wide(&text).as_ptr(), wide("Radial").as_ptr(), MB_OK | MB_ICONINFORMATION);
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if overlay::handle(msg, wp, lp) {
        return 0;
    }
    match msg {
        WM_TRAY if lp as u32 & 0xFFFF == WM_RBUTTONUP => tray_menu(hwnd),
        WM_TRAY if lp as u32 & 0xFFFF == WM_LBUTTONDBLCLK => settings::open(false),
        overlay::WM_DONE => {
            let (text, failed) = *Box::from_raw(lp as *mut (String, bool));
            let cfg = current_config();
            if cfg.notify.balloon() || failed {
                balloon(hwnd, &text);
            }
            if cfg.notify.sound() {
                MessageBeep(if failed { MB_ICONHAND } else { MB_ICONASTERISK });
            }
        }
        WM_COMMAND => match wp & 0xFFFF {
            ID_QUIT => {
                Shell_NotifyIconW(NIM_DELETE, &nid(hwnd));
                PostQuitMessage(0);
            }
            ID_SETTINGS => settings::open(false),
            ID_PAUSE => {
                PAUSED.fetch_xor(true, Relaxed);
                TRACKER.with(|t| *t.borrow_mut() = Tracker::default());
                PostMessageW(hwnd, overlay::WM_HIDE, 0, 0);
                refresh_tray(hwnd);
            }
            ID_AUTOSTART => {
                let mut c = current_config();
                c.autostart = !c.autostart;
                set_config(c);
            }
            ID_ABOUT => about(hwnd),
            _ => {}
        },
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        // Explorer restarted: the tray icon must be added again.
        _ if msg == TASKBAR_CREATED.load(Relaxed) => add_tray(hwnd),
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if render::cli(&args) || settings::cli(&args) || dialogs::cli(&args) {
        return;
    }
    if let [_, flag, state] = args.as_slice() {
        if flag == "--autostart" {
            set_autostart(state == "on");
            return;
        }
    }
    unsafe {
        // `RADIAL_INSTANCE=x` lets a second copy run side by side (used by tests).
        let name = format!("Local\\RadialMenu{}", std::env::var("RADIAL_INSTANCE").unwrap_or_default());
        CreateMutexW(null(), 0, wide(&name).as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        ANY_SURFACE.store(std::env::var_os("RADIAL_ANY_SURFACE").is_some(), Relaxed);
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        OleInitialize(null());
        engine::warm_up();

        let mut cfg = config::load();
        let first_run = !cfg.onboarded;
        if first_run {
            cfg.lang = system_language();
        }
        let user = config::user_themes();
        let look = Look::new(&cfg, &user);
        TRIGGER.store(trigger_code(cfg.trigger), Relaxed);
        APP.with(|a| *a.borrow_mut() = Some(App { cfg, user_themes: user, look }));

        let hinst = GetModuleHandleW(null());
        let cls = wide("RadialOverlay");
        let mut wc: WNDCLASSW = zeroed();
        wc.lpfnWndProc = Some(wnd_proc);
        wc.hInstance = hinst;
        wc.lpszClassName = cls.as_ptr();
        RegisterClassW(&wc);
        // Created hidden and empty; buffers are only allocated while a menu is on screen.
        let hwnd = CreateWindowExW(
            WS_EX_LAYERED | WS_EX_TOPMOST | WS_EX_TOOLWINDOW | WS_EX_NOACTIVATE,
            cls.as_ptr(),
            cls.as_ptr(),
            WS_POPUP,
            0,
            0,
            0,
            0,
            null_mut(),
            null_mut(),
            hinst,
            null(),
        );
        OVERLAY.with(|c| c.set(hwnd));
        overlay::set_hwnd(hwnd);
        RegisterDragDrop(hwnd, &overlay::TARGET as *const _ as *mut _);
        add_tray(hwnd);
        TASKBAR_CREATED.store(RegisterWindowMessageW(wide("TaskbarCreated").as_ptr()), Relaxed);

        let mh = SetWindowsHookExW(WH_MOUSE_LL, Some(mouse_proc), hinst, 0);
        let kh = SetWindowsHookExW(WH_KEYBOARD_LL, Some(key_proc), hinst, 0);
        if first_run && std::env::var_os("RADIAL_NO_ONBOARDING").is_none() {
            settings::open(true);
        }
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            // Dialog-style keyboard handling is not needed; the settings window is custom drawn.
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(mh);
        UnhookWindowsHookEx(kh);
        RevokeDragDrop(hwnd);
    }
}
