#![windows_subsystem = "windows"]
//! Radial: drag a file in Explorer, hold Shift, drop it on a sector of the radial menu to convert it.
//! Tray app. Idle cost is zero: only low-level hooks and the message loop run until a drag starts.

mod convert;
mod geometry;
mod overlay;
mod render;
mod trigger;

use std::cell::RefCell;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering::Relaxed};

use trigger::{Act, Pt, Tracker};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::System::Ole::{OleInitialize, RegisterDragDrop, RevokeDragDrop};
use windows_sys::Win32::System::Threading::CreateMutexW;
use windows_sys::Win32::UI::HiDpi::{SetProcessDpiAwarenessContext, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2};
use windows_sys::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_LSHIFT, VK_RSHIFT, VK_SHIFT};
use windows_sys::Win32::UI::Shell::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

const WM_TRAY: u32 = WM_APP + 1;
const ID_QUIT: usize = 1;
/// Test knob: `RADIAL_ANY_SURFACE=1` accepts drags that start anywhere (used by the end-to-end script).
static ANY_SURFACE: AtomicBool = AtomicBool::new(false);
static TASKBAR_CREATED: AtomicU32 = AtomicU32::new(0);

thread_local! {
    static TRACKER: RefCell<Tracker> = RefCell::new(Tracker::default());
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
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
    if code == HC_ACTION as i32 {
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
                WM_MOUSEMOVE => t.moved(p, GetAsyncKeyState(VK_SHIFT as i32) < 0),
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
    if code == HC_ACTION as i32 {
        let k = &*(lp as *const KBDLLHOOKSTRUCT);
        if matches!(k.vkCode, c if c == VK_SHIFT as u32 || c == VK_LSHIFT as u32 || c == VK_RSHIFT as u32) {
            let down = matches!(wp as u32, WM_KEYDOWN | WM_SYSKEYDOWN);
            let act = TRACKER.with(|t| t.borrow_mut().shift_changed(down));
            dispatch(act, false);
        }
    }
    CallNextHookEx(null_mut(), code, wp, lp)
}

// ---------- Tray ----------

unsafe fn tray_icon() -> HICON {
    const N: u32 = 32;
    let Some(pm) = render::icon_pixmap(N) else { return null_mut() };
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

unsafe fn add_tray(hwnd: HWND) {
    let mut n = nid(hwnd);
    n.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
    n.uCallbackMessage = WM_TRAY;
    n.hIcon = tray_icon();
    put(&mut n.szTip, "Radial: hold Shift while dragging a file");
    Shell_NotifyIconW(NIM_ADD, &n);
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
    let menu = CreatePopupMenu();
    AppendMenuW(menu, MF_STRING, ID_QUIT, wide("Quit").as_ptr());
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    SetForegroundWindow(hwnd); // required so the menu closes when clicking elsewhere
    TrackPopupMenu(menu, TPM_RIGHTBUTTON | TPM_BOTTOMALIGN, pt.x, pt.y, 0, hwnd, null());
    PostMessageW(hwnd, WM_NULL, 0, 0);
    DestroyMenu(menu);
}

unsafe extern "system" fn wnd_proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    if overlay::handle(msg, wp, lp) {
        return 0;
    }
    match msg {
        WM_TRAY if lp as u32 & 0xFFFF == WM_RBUTTONUP => tray_menu(hwnd),
        overlay::WM_DONE => {
            let text = Box::from_raw(lp as *mut String);
            balloon(hwnd, &text);
        }
        WM_COMMAND if wp & 0xFFFF == ID_QUIT => {
            Shell_NotifyIconW(NIM_DELETE, &nid(hwnd));
            PostQuitMessage(0);
        }
        WM_MOUSEACTIVATE => return MA_NOACTIVATE as LRESULT,
        // Explorer restarted: the tray icon must be added again.
        _ if msg == TASKBAR_CREATED.load(Relaxed) => add_tray(hwnd),
        _ => return DefWindowProcW(hwnd, msg, wp, lp),
    }
    0
}

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if let [_, flag, path] = args.as_slice() {
        if flag == "--preview" {
            return render::preview_png(path);
        }
    }
    unsafe {
        CreateMutexW(null(), 0, wide("Local\\RadialMenu").as_ptr());
        if GetLastError() == ERROR_ALREADY_EXISTS {
            return;
        }
        ANY_SURFACE.store(std::env::var_os("RADIAL_ANY_SURFACE").is_some(), Relaxed);
        SetProcessDpiAwarenessContext(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2);
        OleInitialize(null());

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
        let mut msg: MSG = zeroed();
        while GetMessageW(&mut msg, null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
        UnhookWindowsHookEx(mh);
        UnhookWindowsHookEx(kh);
        RevokeDragDrop(hwnd);
    }
}
