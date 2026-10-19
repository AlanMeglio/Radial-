//! RESIZE: shrink one or many pictures with a click. Big preset buttons (each shows its result for the first
//! picture), a custom size, a size estimate for the selected preset, and a worker thread that saves the copies
//! next to the originals.

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use tiny_skia::{Pixmap, PixmapPaint, Transform};

use super::base::{file_name, Base, Phase};
use super::ui::{scaled_pixmap, Field, Kind, Pen, Style, R};
use super::{Dialog, Ev, Host, JobResult};
use crate::config::{tr, Lang};
use crate::convert::{human_size, Fmt};
use crate::engine::resize::{self, Options, Spec};
use crate::jobs::{fill, notice, percent_change, place};
use crate::settings::{RADIUS, RADIUS_CARD};

const W: f32 = 520.0;
const H: f32 = 420.0;
const MARGIN: f32 = 24.0;
const TILE_W: f32 = 152.0;
const TILE_H: f32 = 52.0;
const GAP: f32 = 8.0;

const ID_TILE: u32 = 100;
const ID_CUSTOM: u32 = 110;
const ID_UNIT: u32 = 120;
const ID_FIELD: u32 = 130;
const ID_JPG: u32 = 140;
const ID_GO: u32 = 150;
const ID_CLOSE: u32 = 151;

/// Wait before the estimate starts, so clicking through the presets does not encode every one of them.
const DEBOUNCE: Duration = Duration::from_millis(140);

/// The presets, in grid order: label key and what they do.
const PRESETS: [(&str, Spec); 6] = [
    ("rsz-email", Spec::MaxSide(1600)),
    ("rsz-web", Spec::MaxSide(1280)),
    ("rsz-phone", Spec::MaxSide(1080)),
    ("rsz-small", Spec::MaxSide(640)),
    ("rsz-half", Spec::Percent(50)),
    ("rsz-quarter", Spec::Percent(25)),
];

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Sel {
    Preset(usize),
    Custom,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Unit {
    Side,
    Percent,
}

impl Unit {
    fn default_value(self) -> u32 {
        match self {
            Unit::Side => 1200,
            Unit::Percent => 50,
        }
    }
}

// What the dialog remembers until the process ends: the last choice.
static LAST_SEL: AtomicU32 = AtomicU32::new(0);
static LAST_UNIT: AtomicU32 = AtomicU32::new(0);
static LAST_VALUE: AtomicU32 = AtomicU32::new(1200);

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Estimate {
    Bytes(u64),
    /// The first picture is already at or below that size.
    Small,
    Failed,
}

/// What the save step did, for the notice.
#[derive(Default, Debug)]
struct Outcome {
    ok: Vec<PathBuf>,
    before: u64,
    after: u64,
    skipped: usize,
    failed: Vec<(String, String)>,
}

/// State worker threads write and the dialog reads when their `JobResult` arrives.
#[derive(Default)]
struct Shared {
    /// Newest estimate request; older workers see the mismatch after the debounce and stop.
    latest: AtomicU32,
    estimate: Mutex<Option<(u32, Estimate)>>,
    outcome: Mutex<Option<Outcome>>,
}

fn locked<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|e| e.into_inner())
}

pub struct ResizeDialog {
    base: Base,
    paths: Vec<PathBuf>,
    sel: Sel,
    unit: Unit,
    field: Field,
    to_jpg: bool,
    gen: u32,
    estimate: Option<Estimate>,
    message: Option<String>,
    /// The text of the last notice posted (what the tests look at).
    report: Option<(String, bool)>,
    shared: Arc<Shared>,
    thumb: Option<(Pixmap, u32, u32)>,
}

fn is_lossless_source(p: &Path) -> bool {
    let ext = p.extension().and_then(|e| e.to_str()).map(str::to_ascii_lowercase);
    matches!(ext.as_deref(), Some("png" | "bmp" | "tif" | "tiff"))
}

fn options_for(spec: Spec, to_jpg: bool, p: &Path) -> Options {
    let mut o = Options::new(spec);
    if to_jpg && is_lossless_source(p) {
        o.format = Some(Fmt::Jpg);
    }
    o
}

/// "1600 px - 1600x1067", or that the picture is already that small.
fn tile_sub(spec: Spec, dims: Option<(u32, u32)>, lang: Lang) -> String {
    let head = match spec {
        Spec::MaxSide(n) => format!("{n} px"),
        Spec::Percent(p) => format!("{p}%"),
        Spec::Exact { w, h } => format!("{w}x{h}"),
    };
    match dims {
        Some((w, h)) => {
            let t = resize::target_size(w, h, spec, false);
            if t == (w, h) {
                tr(lang, "rsz-same").to_string()
            } else {
                format!("{head} - {}x{}", t.0, t.1)
            }
        }
        None => head,
    }
}

/// The notice text for a finished run, and whether it should look like a failure.
fn report(o: &Outcome, lang: Lang) -> (String, bool) {
    let mut parts = Vec::new();
    match o.ok.as_slice() {
        [] => {}
        [one] => parts.push(fill(tr(lang, "saved"), &[&file_name(one)])),
        many => parts.push(fill(
            tr(lang, "rsz-done-many"),
            &[&many.len().to_string(), &human_size(o.before), &human_size(o.after), &percent_change(o.before, o.after)],
        )),
    }
    if o.skipped > 0 {
        parts.push(fill(tr(lang, "rsz-skipped"), &[&o.skipped.to_string()]));
    }
    if !o.failed.is_empty() {
        let mut names: Vec<String> = o.failed.iter().take(3).map(|(n, e)| format!("{n} ({e})")).collect();
        if o.failed.len() > 3 {
            names.push("...".into());
        }
        parts.push(fill(tr(lang, "rsz-failed-some"), &[&o.failed.len().to_string(), &names.join(", ")]));
    }
    (parts.join(". "), !o.failed.is_empty())
}

impl ResizeDialog {
    pub fn new(paths: Vec<PathBuf>) -> ResizeDialog {
        let sel = match LAST_SEL.load(Ordering::Relaxed) as usize {
            i if i < PRESETS.len() => Sel::Preset(i),
            _ => Sel::Custom,
        };
        let unit = if LAST_UNIT.load(Ordering::Relaxed) == 1 { Unit::Percent } else { Unit::Side };
        ResizeDialog::build(paths, sel, unit, LAST_VALUE.load(Ordering::Relaxed))
    }

    fn build(paths: Vec<PathBuf>, sel: Sel, unit: Unit, value: u32) -> ResizeDialog {
        let src = paths.first().cloned().unwrap_or_default();
        ResizeDialog {
            base: Base::new(src),
            paths,
            sel,
            unit,
            field: Field::number(value),
            to_jpg: false,
            gen: 0,
            estimate: None,
            message: None,
            report: None,
            shared: Arc::new(Shared::default()),
            thumb: None,
        }
    }

    /// What the current choice asks for; `None` while the custom field is empty or zero.
    fn spec(&self) -> Option<Spec> {
        match self.sel {
            Sel::Preset(i) => Some(PRESETS[i].1),
            Sel::Custom => {
                let v = self.field.value().filter(|&v| v > 0)?;
                Some(match self.unit {
                    Unit::Side => Spec::MaxSide(v),
                    Unit::Percent => Spec::Percent(v.min(100)),
                })
            }
        }
    }

    /// A preset that would not change the first picture is no use as the starting point: move to the first one
    /// that does (the user can still pick any tile).
    fn skip_useless_preset(&mut self) {
        let (Sel::Preset(i), Some(l)) = (self.sel, &self.base.loaded) else { return };
        let shrinks = |s: Spec| resize::target_size(l.w, l.h, s, false) != (l.w, l.h);
        if !shrinks(PRESETS[i].1) {
            if let Some(j) = PRESETS.iter().position(|p| shrinks(p.1)) {
                self.sel = Sel::Preset(j);
            }
        }
    }

    fn offers_jpg(&self) -> bool {
        self.paths.iter().any(|p| is_lossless_source(p))
    }

    fn choose(&mut self, sel: Sel, host: &mut Host) {
        self.sel = sel;
        self.message = None;
        self.request_estimate(host);
    }

    /// Starts a (debounced) in-memory encode of the first picture at the selected size.
    fn request_estimate(&mut self, host: &mut Host) {
        self.estimate = None;
        let (Some(spec), true) = (self.spec(), self.base.ready()) else { return };
        self.gen += 1;
        let gen = self.gen;
        self.shared.latest.store(gen, Ordering::SeqCst);
        let shared = self.shared.clone();
        let first = self.base.src.clone();
        let opts = options_for(spec, self.to_jpg, &first);
        host.spawn(move || {
            std::thread::sleep(DEBOUNCE);
            if shared.latest.load(Ordering::SeqCst) == gen {
                let est = match crate::engine::io::read_limited(&first)
                    .and_then(|bytes| resize::resize_bytes(&bytes, &opts))
                {
                    Ok((out, _)) => Estimate::Bytes(out.len() as u64),
                    Err(e) if resize::is_already_small(&e) => Estimate::Small,
                    Err(_) => Estimate::Failed,
                };
                *locked(&shared.estimate) = Some((gen, est));
            }
            JobResult::Preview { gen, img: image::RgbaImage::new(1, 1) }
        });
    }

    fn remember(&self) {
        let (sel, unit) = (self.sel, self.unit);
        LAST_SEL.store(
            match sel {
                Sel::Preset(i) => i as u32,
                Sel::Custom => PRESETS.len() as u32,
            },
            Ordering::Relaxed,
        );
        LAST_UNIT.store((unit == Unit::Percent) as u32, Ordering::Relaxed);
        if let Some(v) = self.field.value().filter(|&v| v > 0) {
            LAST_VALUE.store(v, Ordering::Relaxed);
        }
    }

    fn go(&mut self, host: &mut Host) {
        let (Some(spec), true) = (self.spec(), self.base.ready()) else { return };
        self.remember();
        self.base.phase = Phase::Saving;
        self.message = None;
        let (files, to_jpg) = (self.paths.clone(), self.to_jpg);
        let shared = self.shared.clone();
        let cfg = crate::current_config();
        host.spawn(move || {
            let results = resize::resize_many(&files, &move |p| options_for(spec, to_jpg, p));
            let mut o = Outcome::default();
            for (src, res) in files.iter().zip(results) {
                let size = |p: &Path| std::fs::metadata(p).map_or(0, |m| m.len());
                match res {
                    Ok(out) => match place(src, out, &cfg) {
                        Ok(placed) => {
                            let placed = placed.unwrap_or_else(|| src.clone());
                            o.before += size(src);
                            o.after += size(&placed);
                            o.ok.push(placed);
                        }
                        Err(e) => o.failed.push((file_name(src), e)),
                    },
                    Err(e) if resize::is_already_small(&e) => o.skipped += 1,
                    Err(e) => o.failed.push((file_name(src), e.to_string())),
                }
            }
            let last = o.ok.last().cloned();
            *locked(&shared.outcome) = Some(o);
            JobResult::Saved(last.ok_or_else(String::new))
        });
    }

    fn finished(&mut self, r: Result<PathBuf, String>, host: &mut Host) {
        self.base.phase = Phase::Ready;
        let Some(o) = locked(&self.shared.outcome).take() else {
            self.message = r.err();
            return;
        };
        let (text, failed) = report(&o, host.lang);
        if o.ok.is_empty() {
            self.message = Some(if o.failed.is_empty() { host.t("rsz-already-run").to_string() } else { text });
            return;
        }
        self.report = Some((text.clone(), failed));
        notice(text, failed);
        host.close = true;
    }
}

impl Dialog for ResizeDialog {
    fn title(&self, lang: Lang) -> String {
        tr(lang, "rsz-title").to_string()
    }

    fn size(&self) -> (f32, f32) {
        (W, H)
    }

    fn start(&mut self, host: &mut Host) {
        self.base.start(host);
    }

    fn busy(&self) -> bool {
        self.base.busy()
    }

    fn saving(&self) -> bool {
        self.base.phase == Phase::Saving
    }

    fn default_action(&mut self, host: &mut Host) {
        self.go(host);
    }

    fn job(&mut self, result: JobResult, host: &mut Host) {
        match result {
            JobResult::Loaded(r) => {
                if self.base.on_loaded(r) {
                    self.skip_useless_preset();
                    self.request_estimate(host);
                }
            }
            JobResult::Preview { gen, .. } if gen == self.gen => {
                let mut slot = locked(&self.shared.estimate);
                if let Some((g, e)) = slot.take() {
                    if g == gen {
                        self.estimate = Some(e);
                    }
                }
            }
            JobResult::Saved(r) => self.finished(r, host),
            _ => {}
        }
    }

    fn event(&mut self, ev: Ev, host: &mut Host) {
        match ev {
            Ev::Click(id) => match id {
                i if (ID_TILE..ID_TILE + PRESETS.len() as u32).contains(&i) => {
                    self.choose(Sel::Preset((i - ID_TILE) as usize), host)
                }
                ID_CUSTOM => {
                    self.field.focus();
                    host.set_focus(Some(ID_FIELD));
                    self.choose(Sel::Custom, host);
                }
                ID_FIELD => {
                    self.field.focus();
                    self.choose(Sel::Custom, host);
                }
                i if i == ID_UNIT || i == ID_UNIT + 1 => {
                    let unit = if i == ID_UNIT { Unit::Side } else { Unit::Percent };
                    if unit != self.unit {
                        self.unit = unit;
                        self.field.set(&unit.default_value().to_string());
                    }
                    self.choose(Sel::Custom, host);
                }
                ID_JPG => {
                    self.to_jpg = !self.to_jpg;
                    self.request_estimate(host);
                }
                ID_GO => self.go(host),
                ID_CLOSE => host.close = true,
                _ => {}
            },
            Ev::Char(c) if host.focus == Some(ID_FIELD) && self.field.on_char(c) => self.choose(Sel::Custom, host),
            Ev::Key { vk, .. } if host.focus == Some(ID_FIELD) && self.field.on_key(vk) => {
                self.choose(Sel::Custom, host)
            }
            _ => {}
        }
    }

    fn draw(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        self.draw_header(pen);

        pen.label(tr(l, "rsz-size"), MARGIN, 108.0);
        let dims = self.base.loaded.as_ref().map(|x| (x.w, x.h));
        for (i, (key, spec)) in PRESETS.iter().enumerate() {
            let r = [MARGIN + (i % 3) as f32 * (TILE_W + GAP), 130.0 + (i / 3) as f32 * (TILE_H + GAP), TILE_W, TILE_H];
            tile(pen, r, tr(l, key), &tile_sub(*spec, dims, l), self.sel == Sel::Preset(i), ID_TILE + i as u32);
        }

        // Custom row: a chip, the unit switch, the number and the resulting size.
        let y = 262.0;
        pen.chip([MARGIN, y, 84.0, 34.0], tr(l, "rsz-custom"), self.sel == Sel::Custom, ID_CUSTOM);
        let unit = if self.unit == Unit::Side { 0 } else { 1 };
        pen.seg([116.0, y, 208.0, 34.0], &[tr(l, "rsz-longest"), tr(l, "rsz-percent")], unit, ID_UNIT);
        pen.field([332.0, y, 84.0, 34.0], &self.field, ID_FIELD, if self.unit == Unit::Side { "px" } else { "%" });
        if let (Some(spec), Some((w, h))) = (self.spec(), dims) {
            let (tw, th) = resize::target_size(w, h, spec, false);
            if self.sel == Sel::Custom {
                pen.text_r(&format!("{tw}x{th}"), W - MARGIN, y + 17.5, 11.5, pen.pal.dim);
            }
        }

        // One line: the problem if there is one, else the estimate for the selected size.
        let line_y = 322.0;
        if let Some(m) = &self.message {
            let shown = pen.fit(m, 12.5, W - 2.0 * MARGIN);
            pen.text(&shown, MARGIN, line_y, 12.5, super::ui::RED);
        } else if self.base.ready() {
            match self.estimate {
                Some(Estimate::Bytes(b)) => {
                    let was = self.base.loaded.as_ref().map_or(0, |x| x.bytes);
                    let t = fill(tr(l, "rsz-estimate"), &[&human_size(b), &human_size(was)]);
                    pen.text(&t, MARGIN, line_y, 12.5, pen.pal.text);
                }
                Some(Estimate::Small) => pen.text(tr(l, "rsz-already"), MARGIN, line_y, 12.5, pen.pal.dim),
                Some(Estimate::Failed) => {}
                None if self.spec().is_some() => pen.text(tr(l, "rsz-estimating"), MARGIN, line_y, 12.5, pen.pal.dim),
                None => {}
            }
        }
        if self.offers_jpg() {
            pen.check([MARGIN, 336.0, 300.0, 24.0], tr(l, "rsz-jpg"), self.to_jpg, ID_JPG);
        }

        // Footer.
        let by = H - 56.0;
        pen.text(tr(l, "saved-next-to"), MARGIN, by + 18.0, 11.0, pen.pal.dim);
        let ready = self.base.ready() && self.spec().is_some();
        let go = [W - MARGIN - 120.0, by, 120.0, 36.0];
        pen.button([go[0] - 8.0 - 88.0, by, 88.0, 36.0], tr(l, "cancel"), ID_CLOSE, Style::Secondary, !self.saving());
        pen.button(go, tr(l, "rsz-go"), ID_GO, Style::Primary, ready);
        self.base.status(pen, [0.0, 100.0, W, by - 100.0 - 4.0]);
    }
}

impl ResizeDialog {
    fn draw_header(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        let thumb = [MARGIN, 20.0, 72.0, 72.0];
        self.draw_thumb(pen, thumb);
        let x = thumb[0] + thumb[2] + 16.0;
        let room = W - x - MARGIN;
        let info = match &self.base.loaded {
            Some(i) => format!("{}x{} px  -  {}", i.w, i.h, human_size(i.bytes)),
            None => String::new(),
        };
        if self.paths.len() > 1 {
            pen.text(&fill(tr(l, "rsz-pictures"), &[&self.paths.len().to_string()]), x, 34.0, 14.0, pen.pal.text);
            let name = pen.fit(&self.base.name, 12.0, room);
            pen.text(&name, x, 56.0, 12.0, pen.pal.dim);
            pen.text(&info, x, 76.0, 12.0, pen.pal.dim);
        } else {
            let name = pen.fit(&self.base.name, 14.0, room);
            pen.text(&name, x, 44.0, 14.0, pen.pal.text);
            pen.text(&info, x, 66.0, 12.0, pen.pal.dim);
        }
    }

    /// The first picture, fitted into the square on the left of the header.
    fn draw_thumb(&mut self, pen: &mut Pen, r: R) {
        pen.fill(r, RADIUS_CARD, pen.pal.card_hi.mix(pen.pal.bg, 0.5));
        pen.outline(r, RADIUS_CARD, pen.pal.border);
        let Some(l) = &self.base.loaded else { return };
        let (tw, th) = (l.thumb.width().max(1) as f32, l.thumb.height().max(1) as f32);
        let k = ((r[2] - 8.0) / tw).min((r[3] - 8.0) / th);
        let (w, h) = (tw * k, th * k);
        let s = pen.s;
        let (dw, dh) = ((w * s).round().max(1.0) as u32, (h * s).round().max(1.0) as u32);
        if !matches!(&self.thumb, Some((_, a, b)) if (*a, *b) == (dw, dh)) {
            self.thumb = scaled_pixmap(&l.thumb, dw, dh).map(|p| (p, dw, dh));
        }
        if let Some((p, _, _)) = &self.thumb {
            let (x, y) =
                (((r[0] + (r[2] - w) / 2.0) * s).round() as i32, ((r[1] + (r[3] - h) / 2.0) * s).round() as i32);
            pen.pm.draw_pixmap(x, y, p.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
    }
}

/// A big preset button: name on top, the result for the first picture under it.
fn tile(pen: &mut Pen, r: R, name: &str, sub: &str, sel: bool, id: u32) {
    let hov = pen.hovered(id);
    let (bg, fg, dim) = if sel {
        (pen.pal.accent, pen.pal.on_accent, pen.pal.on_accent.fade(0.8))
    } else if hov {
        (pen.pal.card_hi, pen.pal.text, pen.pal.dim)
    } else {
        (pen.pal.card, pen.pal.text, pen.pal.dim)
    };
    pen.fill(r, RADIUS, bg);
    if !sel {
        pen.outline(r, RADIUS, pen.pal.border);
    }
    let room = r[2] - 24.0;
    let (name, sub) = (pen.fit(name, 13.5, room), pen.fit(sub, 11.5, room));
    pen.text(&name, r[0] + 12.0, r[1] + 19.0, 13.5, fg);
    pen.text(&sub, r[0] + 12.0, r[1] + 37.0, 11.5, dim);
    pen.hit(r, id, Kind::Button);
    if pen.focus == Some(id) {
        pen.ring([r[0] - 2.0, r[1] - 2.0, r[2] + 4.0, r[3] + 4.0], RADIUS + 2.0, pen.pal.text, 2.0);
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Cursor, Host};
    use super::*;
    use crate::engine::raster::{self, testutil::*};
    use image::GenericImageView;

    /// The remembered choice is process-wide, so tests that build or run a dialog take turns.
    static TURN: Mutex<()> = Mutex::new(());

    fn host() -> Host {
        Host {
            hwnd: std::ptr::null_mut(),
            lang: Lang::En,
            s: 1.0,
            dark: true,
            cursor: Cursor::Arrow,
            close: false,
            dirty: false,
            jobs: vec![],
            hits: vec![],
            order: vec![],
            hot: None,
            pressed: None,
            focus: None,
            drag: None,
            time: 0.0,
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-rszdlg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    /// Runs every queued job inline and feeds the results back, like the offscreen preview does.
    fn pump(d: &mut ResizeDialog, h: &mut Host) {
        for _ in 0..8 {
            let jobs = std::mem::take(&mut h.jobs);
            if jobs.is_empty() {
                break;
            }
            for j in jobs {
                let r = j();
                d.job(r, h);
            }
        }
    }

    fn open(paths: Vec<PathBuf>) -> (ResizeDialog, Host) {
        let mut d = ResizeDialog::build(paths, Sel::Preset(0), Unit::Side, 1200);
        let mut h = host();
        d.start(&mut h);
        pump(&mut d, &mut h);
        (d, h)
    }

    fn photo(d: &Path, name: &str, w: u32, h: u32) -> PathBuf {
        let p = d.join(name);
        std::fs::write(&p, jpeg(&gradient(w, h))).unwrap();
        p
    }

    #[test]
    fn the_presets_are_the_documented_sizes() {
        let specs: Vec<Spec> = PRESETS.iter().map(|p| p.1).collect();
        assert_eq!(
            specs,
            [
                Spec::MaxSide(1600),
                Spec::MaxSide(1280),
                Spec::MaxSide(1080),
                Spec::MaxSide(640),
                Spec::Percent(50),
                Spec::Percent(25)
            ]
        );
    }

    #[test]
    fn each_tile_shows_its_result_for_the_first_picture() {
        assert_eq!(tile_sub(Spec::MaxSide(1600), Some((3000, 2000)), Lang::En), "1600 px - 1600x1067");
        assert_eq!(tile_sub(Spec::Percent(50), Some((3000, 2000)), Lang::En), "50% - 1500x1000");
        assert_eq!(tile_sub(Spec::MaxSide(1600), Some((800, 600)), Lang::En), "Already this small");
        assert_eq!(tile_sub(Spec::MaxSide(1600), Some((800, 600)), Lang::Es), "Ya es así de chica");
        assert_eq!(tile_sub(Spec::MaxSide(640), None, Lang::En), "640 px");
    }

    #[test]
    fn clicking_a_preset_selects_it_and_the_custom_row_follows_the_typing() {
        let _t = locked(&TURN);
        let dd = dir("select");
        let (mut d, mut h) = open(vec![photo(&dd, "a.jpg", 1700, 200)]);
        assert_eq!(d.spec(), Some(Spec::MaxSide(1600)));
        d.event(Ev::Click(ID_TILE + 2), &mut h);
        assert_eq!((d.sel, d.spec()), (Sel::Preset(2), Some(Spec::MaxSide(1080))));
        d.event(Ev::Click(ID_TILE + 5), &mut h);
        assert_eq!(d.spec(), Some(Spec::Percent(25)));

        d.event(Ev::Click(ID_CUSTOM), &mut h);
        assert_eq!((d.sel, h.focus), (Sel::Custom, Some(ID_FIELD)));
        for c in "800".chars() {
            d.event(Ev::Char(c), &mut h);
        }
        assert_eq!(d.spec(), Some(Spec::MaxSide(800)));
        d.event(Ev::Click(ID_UNIT + 1), &mut h);
        assert_eq!((d.unit, d.field.text.as_str()), (Unit::Percent, "50"));
        h.focus = Some(ID_FIELD);
        d.event(Ev::Click(ID_FIELD), &mut h);
        for c in "300".chars() {
            d.event(Ev::Char(c), &mut h);
        }
        assert_eq!(d.spec(), Some(Spec::Percent(100)), "percent is capped at 100");
        d.field.set("");
        assert_eq!(d.spec(), None, "an empty field asks for nothing");
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn the_estimate_is_computed_off_the_ui_thread_and_stale_requests_are_dropped() {
        let _t = locked(&TURN);
        let dd = dir("est");
        let src = photo(&dd, "a.jpg", 1700, 1100);
        let (mut d, mut h) = open(vec![src.clone()]);
        assert!(matches!(d.estimate, Some(Estimate::Bytes(b)) if b > 0), "{:?}", d.estimate);
        // Two quick clicks queue two workers; only the newest answers.
        d.event(Ev::Click(ID_TILE + 3), &mut h);
        d.event(Ev::Click(ID_TILE + 4), &mut h);
        assert_eq!((h.jobs.len(), d.estimate), (2, None));
        pump(&mut d, &mut h);
        let Some(Estimate::Bytes(half)) = d.estimate else { panic!("{:?}", d.estimate) };
        let want =
            resize::resize_bytes(&std::fs::read(&src).unwrap(), &Options::new(Spec::Percent(50))).unwrap().0.len();
        assert_eq!(half, want as u64, "the estimate is exactly what saving would write");
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn a_picture_already_that_small_has_no_estimate_and_nothing_to_resize() {
        let _t = locked(&TURN);
        let dd = dir("small");
        let (mut d, mut h) = open(vec![photo(&dd, "a.jpg", 400, 300)]);
        assert_eq!(d.sel, Sel::Preset(4), "the opening choice skips presets that change nothing: Half is the first");
        assert!(matches!(d.estimate, Some(Estimate::Bytes(_))));
        d.event(Ev::Click(ID_TILE), &mut h);
        pump(&mut d, &mut h);
        assert_eq!(d.estimate, Some(Estimate::Small));
        d.event(Ev::Click(ID_GO), &mut h);
        pump(&mut d, &mut h);
        assert!(!h.close, "nothing was resized, so the dialog stays");
        assert_eq!(d.message.as_deref(), Some(tr(Lang::En, "rsz-already-run")));
        assert_eq!(d.base.phase, Phase::Ready);
        assert_eq!(std::fs::read_dir(&dd).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn resizing_one_picture_saves_next_to_it_and_closes_with_its_name() {
        let _t = locked(&TURN);
        let dd = dir("one");
        let src = photo(&dd, "holiday.jpg", 1400, 700);
        let (mut d, mut h) = open(vec![src]);
        d.event(Ev::Click(ID_TILE + 3), &mut h);
        pump(&mut d, &mut h);
        d.event(Ev::Click(ID_GO), &mut h);
        assert_eq!(d.base.phase, Phase::Saving);
        assert!(d.saving() && d.busy());
        pump(&mut d, &mut h);
        let out = dd.join("holiday (resized).jpg");
        assert_eq!(raster::decode(&std::fs::read(&out).unwrap()).unwrap().dimensions(), (640, 320));
        assert!(h.close);
        assert_eq!(d.report, Some(("Saved holiday (resized).jpg".to_string(), false)));
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn a_batch_reports_totals_skips_and_failures_and_closes_when_something_worked() {
        let _t = locked(&TURN);
        let dd = dir("batch");
        let mut files: Vec<PathBuf> = (0..4).map(|i| photo(&dd, &format!("p{i}.jpg"), 800 + i * 8, 600)).collect();
        let tiny = dd.join("tiny.png");
        std::fs::write(&tiny, png(&image::DynamicImage::ImageRgb8(gradient(30, 20)))).unwrap();
        let bad = dd.join("bad.jpg");
        std::fs::write(&bad, b"not a picture at all").unwrap();
        files.extend([tiny, bad]);
        let (mut d, mut h) = open(files);
        d.event(Ev::Click(ID_TILE + 3), &mut h); // Small: 640 px
        pump(&mut d, &mut h);
        d.event(Ev::Key { vk: 0x0D, ctrl: false }, &mut h); // not handled here; Enter goes through default_action
        d.default_action(&mut h);
        pump(&mut d, &mut h);
        assert!(h.close, "four of six worked");
        for i in 0..4 {
            let out = dd.join(format!("p{i} (resized).jpg"));
            assert_eq!(raster::decode(&std::fs::read(&out).unwrap()).unwrap().width(), 640);
        }
        assert!(!dd.join("tiny (resized).png").exists() && !dd.join("bad (resized).jpg").exists());
        let (text, failed) = d.report.clone().unwrap();
        assert!(failed);
        assert!(text.starts_with("Resized 4 pictures: "), "{text}");
        assert!(text.contains(" -> ") && text.contains("(-"), "{text}");
        assert!(text.contains("1 already small enough"), "{text}");
        assert!(text.contains("1 failed: bad.jpg ("), "{text}");
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn when_every_file_fails_the_dialog_stays_open_and_says_why() {
        let _t = locked(&TURN);
        let dd = dir("allbad");
        let bad = dd.join("bad.png");
        std::fs::write(&bad, b"nope").unwrap();
        let mut d = ResizeDialog::build(vec![bad], Sel::Preset(0), Unit::Side, 1200);
        let mut h = host();
        d.base.loaded =
            Some(super::super::base::Loaded { w: 4000, h: 3000, thumb: image::RgbaImage::new(1, 1), bytes: 10 });
        d.base.phase = Phase::Ready;
        d.event(Ev::Click(ID_GO), &mut h);
        pump(&mut d, &mut h);
        assert!(!h.close);
        assert!(d.message.as_deref().unwrap().contains("bad.png"), "{:?}", d.message);
        assert_eq!(d.base.phase, Phase::Ready);
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn the_jpg_option_only_appears_for_lossless_sources_and_only_converts_those() {
        let _t = locked(&TURN);
        let dd = dir("jpg");
        let photo_jpg = photo(&dd, "a.jpg", 1000, 800);
        let (d, _) = open(vec![photo_jpg.clone()]);
        assert!(!d.offers_jpg());

        let shot = dd.join("shot.png");
        std::fs::write(&shot, png(&image::DynamicImage::ImageRgb8(gradient(1000, 800)))).unwrap();
        let (mut d, mut h) = open(vec![shot.clone(), photo_jpg]);
        assert!(d.offers_jpg());
        d.event(Ev::Click(ID_JPG), &mut h);
        assert!(d.to_jpg);
        d.event(Ev::Click(ID_TILE + 3), &mut h);
        pump(&mut d, &mut h);
        d.event(Ev::Click(ID_GO), &mut h);
        pump(&mut d, &mut h);
        assert!(
            dd.join("shot.jpg").exists(),
            "the png became a jpg (the output setting drops the suffix when the extension differs)"
        );
        assert!(!dd.join("shot (resized).png").exists() && dd.join("shot.png").exists());
        assert!(dd.join("a (resized).jpg").exists());
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn without_the_option_a_png_stays_a_png() {
        let _t = locked(&TURN);
        let dd = dir("png");
        let shot = dd.join("shot.png");
        std::fs::write(&shot, png(&image::DynamicImage::ImageRgb8(gradient(1000, 800)))).unwrap();
        let (mut d, mut h) = open(vec![shot]);
        d.event(Ev::Click(ID_TILE + 3), &mut h);
        pump(&mut d, &mut h);
        d.event(Ev::Click(ID_GO), &mut h);
        pump(&mut d, &mut h);
        assert!(dd.join("shot (resized).png").exists());
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn the_last_choice_is_remembered_for_the_session() {
        let _t = locked(&TURN);
        let dd = dir("memory");
        let (mut d, mut h) = open(vec![photo(&dd, "a.jpg", 1000, 700)]);
        d.event(Ev::Click(ID_TILE + 4), &mut h);
        pump(&mut d, &mut h);
        d.event(Ev::Click(ID_GO), &mut h);
        pump(&mut d, &mut h);
        assert_eq!(ResizeDialog::new(vec![]).sel, Sel::Preset(4));

        let (mut d, mut h) = open(vec![photo(&dd, "b.jpg", 1000, 700)]);
        d.event(Ev::Click(ID_UNIT + 1), &mut h);
        d.field.set("40");
        d.event(Ev::Click(ID_GO), &mut h);
        pump(&mut d, &mut h);
        let again = ResizeDialog::new(vec![]);
        assert_eq!((again.sel, again.unit, again.spec()), (Sel::Custom, Unit::Percent, Some(Spec::Percent(40))));
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn go_waits_for_the_picture_and_for_a_size() {
        let _t = locked(&TURN);
        let mut d = ResizeDialog::build(vec![PathBuf::from("x.jpg")], Sel::Preset(0), Unit::Side, 1200);
        let mut h = host();
        d.event(Ev::Click(ID_GO), &mut h);
        assert!(h.jobs.is_empty(), "still loading");
        d.base.phase = Phase::Ready;
        d.base.loaded = Some(super::super::base::Loaded { w: 10, h: 10, thumb: image::RgbaImage::new(1, 1), bytes: 1 });
        d.sel = Sel::Custom;
        d.field.set("");
        d.event(Ev::Click(ID_GO), &mut h);
        assert!(h.jobs.is_empty(), "no size typed");
    }

    #[test]
    fn the_dialog_text_exists_in_both_languages() {
        for key in [
            "rsz-title",
            "rsz-size",
            "rsz-email",
            "rsz-web",
            "rsz-phone",
            "rsz-small",
            "rsz-half",
            "rsz-quarter",
            "rsz-custom",
            "rsz-longest",
            "rsz-percent",
            "rsz-same",
            "rsz-pictures",
            "rsz-estimating",
            "rsz-estimate",
            "rsz-already",
            "rsz-already-run",
            "rsz-jpg",
            "rsz-go",
            "rsz-done-many",
            "rsz-skipped",
            "rsz-failed-some",
            "act-resize",
            "act-resize-hint",
        ] {
            for l in [Lang::En, Lang::Es] {
                assert!(!tr(l, key).is_empty(), "{key}");
            }
        }
        let o = Outcome {
            ok: vec![PathBuf::from("a (resized).jpg"); 3],
            before: 3 << 20,
            after: 1 << 20,
            ..Default::default()
        };
        assert_eq!(report(&o, Lang::En).0, "Resized 3 pictures: 3.0 MB -> 1.0 MB (-67%)");
        assert_eq!(report(&o, Lang::Es).0, "Achicadas 3 imágenes: 3.0 MB -> 1.0 MB (-67%)");
    }
}
