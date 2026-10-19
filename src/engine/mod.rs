//! Conversion and action engine: pure, blocking, `Send` library code with no UI.
//!
//! * Menus: `detect` (content first, extension second) and `supports` say which conversions a file has.
//! * Conversions: `convert_file` / `convert_bytes` cover image, PDF, DOCX and text pairs.
//! * Actions: `compress`, `meta`, `edit`, `annotate`, `background`, `crop`, with file-level
//!   wrappers in `actions`.
//! * Windows codecs (PDF rendering, HEIC/AVIF) go through `os`; check `target_availability` /
//!   `source_availability` before offering a sector, and expect `Error::Unavailable` otherwise.
//!
//! Nothing here overwrites a file, panics on damaged input, or decodes beyond `raster::MAX_PIXELS`.
#![allow(dead_code)] // entry points are wired into the UI incrementally
#![allow(clippy::chunks_exact_to_as_chunks)] // `as_chunks` needs Rust 1.88; the declared MSRV is older

pub mod actions;
pub mod annotate;
pub mod background;
#[cfg(test)]
mod bench;
pub mod compress;
pub mod crop;
pub mod docx;
pub mod edit;
pub mod error;
pub mod fileops;
pub mod io;
pub mod jpeg;
pub mod kind;
pub mod meta;
pub mod os;
pub mod pdfread;
pub mod pdftools;
pub mod pdfwrite;
pub mod pipeline;
pub mod raster;
pub mod rename;
pub mod resize;
pub mod sortdir;
pub mod xlsx;
pub mod zip;

#[allow(unused_imports)] // re-exported for the UI layer
pub use error::{Error, Result};
#[allow(unused_imports)]
pub use kind::{detect, detect_any, detect_head, supports, SourceKind, Target};
#[allow(unused_imports)]
pub use os::Availability;
#[allow(unused_imports)]
pub use pipeline::{
    convert_bytes, convert_file, merge_to_pdf, pair_availability, read_text_file, source_availability,
    target_availability, Artifact, ConvertOptions,
};

/// Probes the Windows codecs (HEIC/AVIF) on a background thread so the first menu does not wait for them.
/// Runs once per process; the app calls it when the first menu starts to appear, so an idle tray process never
/// loads the Windows imaging stack.
pub fn warm_up() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        let _ = std::thread::Builder::new().name("radial-warm-up".into()).spawn(|| {
            use crate::convert::Fmt;
            let _ = os::can_decode(Fmt::Heic);
            let _ = os::can_encode(Fmt::Heic);
            let _ = os::can_encode(Fmt::Avif);
        });
    });
}
