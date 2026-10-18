//! Command line helpers that print to the console: `--help`, `--version` and `--formats`.
//!
//! The binary is a GUI-subsystem program, so `main` attaches to the parent console when there are arguments; output
//! also works when it is redirected to a file or a pipe.

use std::ffi::OsStr;
use std::os::windows::ffi::OsStrExt;
use std::ptr::null;

use windows_sys::Win32::Foundation::{GENERIC_READ, GENERIC_WRITE, INVALID_HANDLE_VALUE};
use windows_sys::Win32::Storage::FileSystem::{CreateFileW, FILE_SHARE_WRITE, OPEN_EXISTING};
use windows_sys::Win32::System::Console::{
    AttachConsole, GetStdHandle, SetStdHandle, ATTACH_PARENT_PROCESS, STD_ERROR_HANDLE, STD_OUTPUT_HANDLE,
};

use crate::convert::Fmt;
use crate::engine::{self, Availability, SourceKind, Target};

/// Makes `println!` reach the terminal that started the program (a no-op when output is already redirected).
pub fn attach_console() {
    unsafe {
        if AttachConsole(ATTACH_PARENT_PROCESS) == 0 {
            return;
        }
        let name: Vec<u16> = OsStr::new("CONOUT$").encode_wide().chain(Some(0)).collect();
        for std in [STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
            let cur = GetStdHandle(std);
            if cur.is_null() || cur == INVALID_HANDLE_VALUE {
                let h = CreateFileW(
                    name.as_ptr(),
                    GENERIC_READ | GENERIC_WRITE,
                    FILE_SHARE_WRITE,
                    null(),
                    OPEN_EXISTING,
                    0,
                    std::ptr::null_mut(),
                );
                if h != INVALID_HANDLE_VALUE {
                    SetStdHandle(std, h);
                }
            }
        }
    }
}

const HELP: &str = "\
Radial - drag a file, hold Shift, drop it on a radial menu.

Usage:
  radial.exe                          run in the tray (the normal way)
  radial.exe --open ACTION IMAGE      open one action dialog, no tray: crop, bg, edit, annotate, redact
  radial.exe --formats                what this machine can read and write
  radial.exe --autostart on|off       add or remove the start-with-Windows entry
  radial.exe --version | --help

Development and documentation (render to a PNG, no window):
  --preview out.png [--theme T] [--hover N] [--menu 8|4|7] [--accent #rrggbb] [--dpi F]
  --sheet out.png                     every theme on one contact sheet
  --settings out.png [--tab N] [--step N] [--lang es] [--light] [--theme T] [--menu 8|4|7]
  --dialog crop|bg|edit|annotate|redact --out out.png [--image F] [--script S] [--lang es] [--light]
  --tray-icon out.png [--size N]      the tray icon at any size
  --bench [--theme T] [--dpi F]       time to bake and render a frame

Settings live in %APPDATA%\\radial\\config; see the README.
";

/// One row of the formats report.
fn row(name: &str, read: &Availability, write: &Availability) -> String {
    let word = |a: &Availability| match a {
        Availability::Available => "yes".to_string(),
        Availability::Unavailable(why) => format!("no ({why})"),
    };
    format!("{name:<6} read: {:<10} write: {}", word(read), word(write))
}

/// The per-format report printed by `--formats`.
pub fn formats_report() -> String {
    let mut out = String::from("Images\n");
    for f in [Fmt::Png, Fmt::Jpg, Fmt::Webp, Fmt::Gif, Fmt::Tiff, Fmt::Bmp, Fmt::Heic, Fmt::Avif] {
        let read = engine::source_availability(SourceKind::Image(f));
        let write = engine::target_availability(Target::Image(f));
        out.push_str(&format!("  {}\n", row(f.label(), &read, &write)));
    }
    out.push_str("Documents\n");
    for (name, kind, target) in [
        ("PDF", SourceKind::Pdf, Target::Pdf),
        ("DOCX", SourceKind::Docx, Target::Docx),
        ("TXT", SourceKind::Text, Target::Txt),
    ] {
        let read = engine::source_availability(kind);
        let write = engine::target_availability(target);
        out.push_str(&format!("  {}\n", row(name, &read, &write)));
    }
    out
}

/// Handles `--help`, `--version` and `--formats`; true when the command line was one of them.
pub fn run(args: &[String]) -> bool {
    let has = |f: &str| args.iter().any(|a| a == f);
    if has("--help") || has("-h") {
        print!("{HELP}");
    } else if has("--version") || has("-V") {
        println!("radial {}", env!("CARGO_PKG_VERSION"));
    } else if has("--formats") {
        engine::warm_up();
        print!("{}", formats_report());
    } else {
        return false;
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_report_lists_every_image_and_document_format() {
        let r = formats_report();
        for name in ["PNG", "JPG", "WEBP", "GIF", "TIFF", "BMP", "HEIC", "AVIF", "PDF", "DOCX", "TXT"] {
            assert!(r.lines().any(|l| l.trim_start().starts_with(name)), "{name} missing:\n{r}");
        }
        assert!(r.lines().any(|l| l.trim_start().starts_with("PNG") && l.contains("write: yes")));
    }

    #[test]
    fn the_help_text_mentions_every_flag_the_binary_handles() {
        for flag in [
            "--open",
            "--formats",
            "--autostart",
            "--preview",
            "--sheet",
            "--settings",
            "--dialog",
            "--tray-icon",
            "--bench",
        ] {
            assert!(HELP.contains(flag), "{flag}");
        }
    }
}
