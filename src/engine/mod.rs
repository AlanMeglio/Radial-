//! Conversion and action engine: pure, blocking, `Send` library code with no UI.
//!
//! * Menus: `detect` (content first, extension second) and `targets_for` give the sectors of the radial menu.
//! * Conversions: `convert_file` / `convert_bytes` cover image, PDF, DOCX and text pairs.
//! * Actions: `compress`, `meta`, `edit`, `annotate`, `background`, `crop`, `redact`, with file-level
//!   wrappers in `actions`.
//! * Windows codecs (PDF rendering, HEIC/AVIF) go through `os`; check `target_availability` /
//!   `source_availability` before offering a sector, and expect `Error::Unavailable` otherwise.
//!
//! Nothing here overwrites a file, panics on damaged input, or decodes beyond `raster::MAX_PIXELS`.
#![allow(dead_code)] // entry points are wired into the UI incrementally

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
pub mod io;
pub mod jpeg;
pub mod kind;
pub mod meta;
pub mod os;
pub mod pdfread;
pub mod pdfwrite;
pub mod pipeline;
pub mod raster;
pub mod redact;
pub mod zip;

#[allow(unused_imports)] // re-exported for the UI layer
pub use error::{Error, Result};
#[allow(unused_imports)]
pub use kind::{detect, detect_head, targets_for, SourceKind, Target};
#[allow(unused_imports)]
pub use os::Availability;
#[allow(unused_imports)]
pub use pipeline::{convert_bytes, convert_file, source_availability, target_availability, Artifact, ConvertOptions};

/// Probes the Windows codecs (HEIC/AVIF) on a background thread so the first menu does not wait for them.
pub fn warm_up() {
    let _ = std::thread::Builder::new().name("radial-warm-up".into()).spawn(|| {
        use crate::convert::Fmt;
        let _ = os::can_decode(Fmt::Heic);
        let _ = os::can_encode(Fmt::Heic);
        let _ = os::can_encode(Fmt::Avif);
    });
}
