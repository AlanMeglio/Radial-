//! Small Windows shell helpers behind the universal actions: the clipboard and Explorer.

use std::path::Path;

use windows_sys::Win32::Foundation::{GlobalFree, HANDLE};
use windows_sys::Win32::System::DataExchange::{CloseClipboard, EmptyClipboard, OpenClipboard, SetClipboardData};
use windows_sys::Win32::System::Memory::{GlobalAlloc, GlobalLock, GlobalUnlock, GMEM_MOVEABLE};
use windows_sys::Win32::UI::Shell::ShellExecuteW;
use windows_sys::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

/// Clipboard format for UTF-16 text.
const CF_UNICODETEXT: u32 = 13;

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

/// Puts `text` on the clipboard as Unicode text. Another program may hold the clipboard for a moment, so opening it
/// is retried briefly.
pub fn copy_text(text: &str) -> Result<(), String> {
    let data = wide(text);
    let bytes = data.len() * 2;
    // SAFETY: standard Win32 clipboard sequence. The block is owned by the clipboard once SetClipboardData
    // succeeds and is freed here only when it did not; every opened clipboard is closed.
    unsafe {
        let mut opened = false;
        for _ in 0..10 {
            if OpenClipboard(std::ptr::null_mut()) != 0 {
                opened = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(25));
        }
        if !opened {
            return Err("the clipboard is in use by another program".into());
        }
        let result = (|| {
            let mem = GlobalAlloc(GMEM_MOVEABLE, bytes);
            if mem.is_null() {
                return Err("out of memory".to_string());
            }
            let dst = GlobalLock(mem) as *mut u16;
            if dst.is_null() {
                GlobalFree(mem);
                return Err("could not lock clipboard memory".to_string());
            }
            std::ptr::copy_nonoverlapping(data.as_ptr(), dst, data.len());
            GlobalUnlock(mem);
            EmptyClipboard();
            if SetClipboardData(CF_UNICODETEXT, mem as HANDLE).is_null() {
                GlobalFree(mem);
                return Err("could not set the clipboard".to_string());
            }
            Ok(())
        })();
        CloseClipboard();
        result
    }
}

/// Opens an Explorer window with `path` selected (a folder is selected in its parent).
pub fn reveal(path: &Path) -> Result<(), String> {
    let arg = format!("/select,\"{}\"", path.display());
    let (verb, exe, params) = (wide("open"), wide("explorer.exe"), wide(&arg));
    // SAFETY: all strings are NUL terminated and outlive the call; ShellExecuteW returns a value above 32 on success.
    let code = unsafe {
        ShellExecuteW(
            std::ptr::null_mut(),
            verb.as_ptr(),
            exe.as_ptr(),
            params.as_ptr(),
            std::ptr::null(),
            SW_SHOWNORMAL,
        ) as isize
    };
    if code > 32 {
        Ok(())
    } else {
        Err(format!("Explorer could not be opened (code {code})"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[ignore = "replaces whatever is on the clipboard; run with --ignored"]
    fn text_round_trips_through_the_clipboard_including_unicode() {
        let text = "C:\\Users\\Alanm\\Música\\canción - ñandú ✓.txt\r\nsegunda línea";
        copy_text(text).unwrap();
        let back = read_text().expect("clipboard text");
        assert_eq!(back, text);
    }

    /// Reads the Unicode text on the clipboard (tests only).
    fn read_text() -> Option<String> {
        use windows_sys::Win32::System::DataExchange::GetClipboardData;
        // SAFETY: the clipboard is opened for the read and closed again; the locked block is only read up to its NUL.
        unsafe {
            if OpenClipboard(std::ptr::null_mut()) == 0 {
                return None;
            }
            let h = GetClipboardData(CF_UNICODETEXT);
            let out = if h.is_null() {
                None
            } else {
                let p = GlobalLock(h) as *const u16;
                if p.is_null() {
                    None
                } else {
                    let mut n = 0;
                    while *p.add(n) != 0 {
                        n += 1;
                    }
                    let s = String::from_utf16_lossy(std::slice::from_raw_parts(p, n));
                    GlobalUnlock(h);
                    Some(s)
                }
            };
            CloseClipboard();
            out
        }
    }
}
