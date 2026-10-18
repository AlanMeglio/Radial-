//! What every image dialog shares: loading the picture off the UI thread, the title strip, the busy state and the
//! save step that places the result according to the user's output settings.

use std::path::{Path, PathBuf};

use super::ui::{Pen, R};
use super::{Host, JobResult};
use crate::convert::human_size;
use crate::jobs::{fill, notice, place};

/// Longest side of the picture kept for the preview. The full image is only ever decoded on a worker thread.
pub const PREVIEW_MAX: u32 = 1400;

pub struct Loaded {
    /// Pixel size of the original (after the EXIF orientation).
    pub w: u32,
    pub h: u32,
    pub thumb: image::RgbaImage,
    /// Size of the file on disk.
    pub bytes: u64,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Phase {
    Loading,
    Ready,
    Saving,
    /// The picture could not be opened.
    Failed,
}

pub struct Base {
    pub src: PathBuf,
    pub name: String,
    pub loaded: Option<Loaded>,
    pub phase: Phase,
    /// Last problem to show in red (a save that failed).
    pub error: Option<String>,
}

pub fn file_name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

/// Decodes `src` and keeps a preview of at most `PREVIEW_MAX` px.
pub fn load(src: &Path) -> Result<Loaded, String> {
    let bytes = crate::engine::io::read_limited(src).map_err(|e| e.to_string())?;
    let img = crate::engine::raster::decode(&bytes).map_err(|e| e.to_string())?;
    let (w, h) = (img.width(), img.height());
    let thumb = if w.max(h) > PREVIEW_MAX { img.thumbnail(PREVIEW_MAX, PREVIEW_MAX) } else { img }.to_rgba8();
    Ok(Loaded { w, h, thumb, bytes: bytes.len() as u64 })
}

impl Base {
    pub fn new(src: PathBuf) -> Base {
        Base { name: file_name(&src), src, loaded: None, phase: Phase::Loading, error: None }
    }

    pub fn start(&mut self, host: &mut Host) {
        let src = self.src.clone();
        host.spawn(move || JobResult::Loaded(load(&src)));
    }

    /// Stores the loaded picture; false when it failed to open.
    pub fn on_loaded(&mut self, r: Result<Loaded, String>) -> bool {
        match r {
            Ok(l) => {
                self.loaded = Some(l);
                self.phase = Phase::Ready;
                true
            }
            Err(e) => {
                self.error = Some(e);
                self.phase = Phase::Failed;
                false
            }
        }
    }

    pub fn ready(&self) -> bool {
        self.phase == Phase::Ready && self.loaded.is_some()
    }

    pub fn busy(&self) -> bool {
        matches!(self.phase, Phase::Loading | Phase::Saving)
    }

    pub fn dims(&self) -> (u32, u32) {
        self.loaded.as_ref().map_or((1, 1), |l| (l.w, l.h))
    }

    /// Title, file name and size of the picture across the top of the window.
    pub fn header(&self, pen: &mut Pen, title: &str, width: f32) {
        pen.text(title, 24.0, 34.0, 19.0, pen.pal.text);
        let name = pen.fit(&self.name, 12.0, width - 300.0);
        let sub = match &self.loaded {
            Some(l) => format!("{}x{} px  -  {}", l.w, l.h, human_size(l.bytes)),
            None => String::new(),
        };
        pen.text_r(&sub, width - 24.0, 36.0, 12.0, pen.pal.dim);
        pen.text(&name, 24.0, 56.0, 12.0, pen.pal.dim);
    }

    /// The veil with a spinner while loading or saving, or the reason the picture could not be opened.
    pub fn status(&self, pen: &mut Pen, area: R) {
        match self.phase {
            Phase::Loading => pen.busy(area, crate::config::tr(pen.lang, "loading")),
            Phase::Saving => pen.busy(area, crate::config::tr(pen.lang, "saving")),
            Phase::Failed => {
                let why = self.error.clone().unwrap_or_default();
                let msg = fill(crate::config::tr(pen.lang, "load-failed"), &[&why]);
                let shown = pen.fit(&msg, 13.0, area[2] - 40.0);
                pen.text_c(&shown, area[0] + area[2] / 2.0, area[1] + area[3] / 2.0, 13.0, super::ui::RED);
            }
            Phase::Ready => {}
        }
    }

    /// A failed save, in red under the controls.
    pub fn error_line(&self, pen: &mut Pen, x: f32, y: f32, w: f32) {
        if let (Some(e), Phase::Ready) = (&self.error, self.phase) {
            let msg = fill(crate::config::tr(pen.lang, "failed"), &[e]);
            let shown = pen.fit(&msg, 11.5, w);
            pen.text(&shown, x, y, 11.5, super::ui::RED);
        }
    }

    /// Runs `work` (which writes next to the source) on a worker thread, then applies the output folder and
    /// collision settings. The dialog closes and a notice is posted when it succeeds.
    pub fn save(
        &mut self,
        host: &mut Host,
        work: impl FnOnce() -> Result<PathBuf, crate::engine::Error> + Send + 'static,
    ) {
        if self.phase != Phase::Ready {
            return;
        }
        self.phase = Phase::Saving;
        self.error = None;
        let cfg = crate::current_config();
        let src = self.src.clone();
        host.spawn(move || {
            let placed = work().map_err(|e| e.to_string()).and_then(|p| place(&src, p, &cfg));
            JobResult::Saved(placed.map(|o| o.unwrap_or(src)))
        });
    }

    pub fn on_saved(&mut self, r: Result<PathBuf, String>, host: &mut Host) {
        match r {
            Ok(path) => {
                notice(fill(host.t("saved"), &[&file_name(&path)]), false);
                host.close = true;
                self.phase = Phase::Ready;
            }
            Err(e) => {
                self.error = Some(e);
                self.phase = Phase::Ready;
            }
        }
    }
}
