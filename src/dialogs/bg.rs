//! ADD BG: a picture on a gradient, colour or picture with padding, rounded corners, a soft shadow and a canvas ratio.
//! The preview is rendered on a worker thread from the downscaled picture; "Save with background" renders the
//! full-size image on a worker thread too.

use std::path::PathBuf;
use std::sync::Arc;

use image::RgbaImage;

use super::base::{Base, Phase};
use super::ui::{track_t, Canvas, Kind, Pen, Style, R};
use super::{Dialog, Ev, Host, JobResult};
use crate::config::{tr, Lang};
use crate::engine::actions;
use crate::engine::background::{self, add_background, Background, BgOptions, Ratio};
use crate::theme::Rgba;

const W: f32 = 900.0;
const H: f32 = 640.0;
const CANVAS: R = [24.0, 84.0, 584.0, 492.0];
const PX: f32 = 632.0;
const PW: f32 = 244.0;

const ID_CANVAS: u32 = 10;
const ID_IMAGE: u32 = 40;
const ID_COLOR: u32 = 41;
const ID_RESET: u32 = 30;
const ID_SAVE: u32 = 31;
const ID_CLOSE: u32 = 32;
const ID_PAD: u32 = 50;
const ID_COR: u32 = 51;
const ID_SHA: u32 = 52;
const ID_HUE: u32 = 53;
const ID_SHADE: u32 = 54;
const ID_RATIO: u32 = 200;
const ID_PRESET: u32 = 300;

const MAX_PAD: u32 = 200;
const MAX_COR: u32 = 80;
const MAX_SHA: u32 = 100;
/// Longest side of a picture used as background; bigger ones are scaled down when loaded.
const PICTURE_MAX: u32 = 2000;
/// Longest side of the picture copy that drives the live preview.
const PICTURE_PREVIEW: u32 = 700;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Choice {
    Preset(usize),
    Color,
    Picture,
}

/// The state of the ADD BG controls; everything the saved image depends on.
#[derive(Clone, Debug, PartialEq)]
pub struct Settings {
    pub choice: Choice,
    pub hue: f32,
    pub shade: f32,
    pub padding: u32,
    pub corners: u32,
    pub shadow: u32,
    pub ratio: Ratio,
}

impl Default for Settings {
    fn default() -> Settings {
        let d = BgOptions::default();
        Settings {
            choice: Choice::Preset(0),
            hue: 0.58,
            shade: 0.55,
            padding: d.padding,
            corners: d.corners,
            shadow: d.shadow,
            ratio: d.ratio,
        }
    }
}

impl Settings {
    /// The custom colour: hue 0..1 at the given shade (0 dark .. 1 light).
    pub fn color(&self) -> [u8; 4] {
        let c = Rgba::from_hsv(self.hue * 360.0, 0.55 * (1.0 - (self.shade - 0.5).abs()), 0.25 + 0.75 * self.shade);
        [c.0, c.1, c.2, 255]
    }

    /// Engine options for an image of `scale` times the original size (1.0 for the real save).
    pub fn options(&self, picture: Option<&RgbaImage>, scale: f32) -> BgOptions {
        let px = |v: u32| (v as f32 * scale).round() as u32;
        let background = match self.choice {
            Choice::Preset(i) => background::presets()[i.min(background::presets().len() - 1)].1.clone(),
            Choice::Color => Background::Solid(self.color()),
            Choice::Picture => match picture {
                Some(p) => Background::Image(p.clone()),
                None => background::presets()[0].1.clone(),
            },
        };
        BgOptions {
            background,
            padding: px(self.padding),
            corners: px(self.corners),
            shadow: px(self.shadow),
            ratio: self.ratio,
        }
    }
}

pub struct BgDialog {
    base: Base,
    canvas: Canvas,
    set: Settings,
    thumb: Arc<RgbaImage>,
    picture: Option<(Arc<RgbaImage>, Arc<RgbaImage>, String)>,
    preview: Option<RgbaImage>,
    gen: u32,
    inflight: bool,
    again: bool,
    drag: Option<u32>,
}

impl BgDialog {
    pub fn new(src: PathBuf) -> BgDialog {
        BgDialog {
            base: Base::new(src),
            canvas: Canvas::new(CANVAS),
            set: Settings::default(),
            thumb: Arc::new(RgbaImage::new(1, 1)),
            picture: None,
            preview: None,
            gen: 0,
            inflight: false,
            again: false,
            drag: None,
        }
    }

    fn scale(&self) -> f32 {
        let (w, _) = self.base.dims();
        (self.thumb.width() as f32 / w.max(1) as f32).min(1.0)
    }

    /// Renders the live preview on a worker thread; at most one runs at a time, the latest settings always win.
    fn request_preview(&mut self, host: &mut Host) {
        if !self.base.ready() {
            return;
        }
        if self.inflight {
            self.again = true;
            return;
        }
        self.inflight = true;
        self.gen += 1;
        let gen = self.gen;
        let thumb = Arc::clone(&self.thumb);
        let opts = self.set.options(self.picture.as_ref().map(|p| &*p.1), self.scale());
        host.spawn(move || {
            let img = add_background(&thumb, &opts).unwrap_or_else(|_| (*thumb).clone());
            JobResult::Preview { gen, img }
        });
    }

    fn slider_value(&mut self, id: u32, t: f32) {
        let v = |max: u32| (t * max as f32).round() as u32;
        match id {
            ID_PAD => self.set.padding = v(MAX_PAD),
            ID_COR => self.set.corners = v(MAX_COR),
            ID_SHA => self.set.shadow = v(MAX_SHA),
            ID_HUE => self.set.hue = t,
            ID_SHADE => self.set.shade = t,
            _ => {}
        }
    }

    fn save(&mut self, host: &mut Host) {
        if !self.base.ready() {
            return;
        }
        let opts = self.set.options(self.picture.as_ref().map(|p| &*p.0), 1.0);
        let src = self.base.src.clone();
        self.base.save(host, move || actions::background_file(&src, &opts, None));
    }

    fn pick_picture(&mut self, host: &mut Host) {
        let Some(path) = (unsafe { pick_file(host.hwnd, tr(host.lang, "bg-pick")) }) else { return };
        let name = super::base::file_name(&path);
        host.spawn(move || JobResult::Picture(load_picture(&path).map(|i| (i, name))));
    }
}

/// Opens an image to use as the background, scaled down to `PICTURE_MAX`.
fn load_picture(path: &std::path::Path) -> Result<RgbaImage, String> {
    let bytes = crate::engine::io::read_limited(path).map_err(|e| e.to_string())?;
    let img = crate::engine::raster::decode(&bytes).map_err(|e| e.to_string())?;
    let img = if img.width().max(img.height()) > PICTURE_MAX { img.thumbnail(PICTURE_MAX, PICTURE_MAX) } else { img };
    Ok(img.to_rgba8())
}

/// The standard "open file" dialog, filtered to pictures.
unsafe fn pick_file(owner: windows_sys::Win32::Foundation::HWND, title: &str) -> Option<PathBuf> {
    use windows_sys::Win32::UI::Controls::Dialogs::*;
    if owner.is_null() {
        return None;
    }
    let wide = |s: &str| -> Vec<u16> { s.encode_utf16().chain(Some(0)).collect() };
    let mut file = [0u16; 1024];
    let mut filter: Vec<u16> = Vec::new();
    for part in ["Images", "*.png;*.jpg;*.jpeg;*.webp;*.bmp;*.gif;*.tif;*.tiff", "All files", "*.*"] {
        filter.extend(part.encode_utf16());
        filter.push(0);
    }
    filter.push(0);
    let title = wide(title);
    let mut ofn: OPENFILENAMEW = std::mem::zeroed();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.lpstrFile = file.as_mut_ptr();
    ofn.nMaxFile = file.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_EXPLORER;
    if GetOpenFileNameW(&mut ofn) == 0 {
        return None;
    }
    let len = file.iter().position(|&c| c == 0).unwrap_or(0);
    Some(PathBuf::from(String::from_utf16_lossy(&file[..len])))
}

impl Dialog for BgDialog {
    fn title(&self, lang: Lang) -> String {
        tr(lang, "dlg-bg").to_string()
    }

    fn size(&self) -> (f32, f32) {
        (W, H)
    }

    fn start(&mut self, host: &mut Host) {
        self.base.start(host);
    }

    fn busy(&self) -> bool {
        self.base.busy() || self.inflight
    }

    fn saving(&self) -> bool {
        self.base.phase == Phase::Saving
    }

    fn default_action(&mut self, host: &mut Host) {
        self.save(host);
    }

    fn job(&mut self, result: JobResult, host: &mut Host) {
        match result {
            JobResult::Loaded(r) => {
                if self.base.on_loaded(r) {
                    if let Some(l) = self.base.loaded.as_mut() {
                        self.thumb = Arc::new(std::mem::replace(&mut l.thumb, RgbaImage::new(1, 1)));
                    }
                    self.request_preview(host);
                }
            }
            JobResult::Preview { gen, img } => {
                self.inflight = false;
                if gen == self.gen {
                    self.canvas.set_dims(img.width(), img.height());
                    self.preview = Some(img);
                    self.canvas.invalidate();
                }
                if std::mem::take(&mut self.again) {
                    self.request_preview(host);
                }
            }
            JobResult::Picture(Ok((img, name))) => {
                let small = if img.width().max(img.height()) > PICTURE_PREVIEW {
                    image::DynamicImage::ImageRgba8(img.clone()).thumbnail(PICTURE_PREVIEW, PICTURE_PREVIEW).to_rgba8()
                } else {
                    img.clone()
                };
                self.picture = Some((Arc::new(img), Arc::new(small), name));
                self.set.choice = Choice::Picture;
                self.request_preview(host);
            }
            JobResult::Picture(Err(e)) => self.base.error = Some(e),
            JobResult::Saved(r) => self.base.on_saved(r, host),
        }
    }

    fn event(&mut self, ev: Ev, host: &mut Host) {
        match ev {
            Ev::Down { id, x, .. } if (ID_PAD..=ID_SHADE).contains(&id) => {
                self.drag = Some(id);
                if let Some(r) = host.rect(id) {
                    self.slider_value(id, track_t(r, x));
                }
                self.request_preview(host);
            }
            Ev::Move { x, down: true, .. } => {
                if let Some(id) = self.drag {
                    if let Some(r) = host.rect(id) {
                        self.slider_value(id, track_t(r, x));
                        self.request_preview(host);
                    }
                }
            }
            Ev::Up => {
                if self.drag.take().is_some() {
                    self.request_preview(host);
                }
            }
            Ev::Click(id) => {
                match id {
                    ID_RESET => {
                        self.set = Settings::default();
                        self.request_preview(host);
                    }
                    ID_SAVE => self.save(host),
                    ID_CLOSE => host.close = true,
                    ID_IMAGE => {
                        if self.picture.is_some() {
                            self.set.choice = Choice::Picture;
                            self.request_preview(host);
                        } else {
                            self.pick_picture(host);
                        }
                    }
                    ID_COLOR => {
                        self.set.choice = Choice::Color;
                        self.request_preview(host);
                    }
                    i if (ID_RATIO..ID_RATIO + 4).contains(&i) => {
                        self.set.ratio = Ratio::ALL[(i - ID_RATIO) as usize];
                        self.request_preview(host);
                    }
                    i if (ID_PRESET..ID_PRESET + 32).contains(&i) => {
                        self.set.choice = Choice::Preset((i - ID_PRESET) as usize);
                        self.request_preview(host);
                    }
                    _ => {}
                }
                host.redraw();
            }
            _ => {}
        }
    }

    fn draw(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        self.base.header(pen, tr(l, "dlg-bg"), W);
        self.canvas.draw(pen, self.preview.as_ref());
        pen.hit(CANVAS, ID_CANVAS, Kind::Button);
        if self.inflight && self.base.ready() {
            pen.spinner(CANVAS[0] + CANVAS[2] - 22.0, CANVAS[1] + 22.0, 8.0);
        }
        self.base.status(pen, CANVAS);

        // Backgrounds: presets, then a picture and a custom colour.
        let mut y = 84.0;
        pen.label(tr(l, "bg-background"), PX, y);
        y += 20.0;
        let presets = background::presets();
        let cw = (PW - 4.0 * 6.0) / 5.0;
        for (i, (_, bg)) in presets.iter().enumerate() {
            let r = [PX + (i % 5) as f32 * (cw + 6.0), y + (i / 5) as f32 * 38.0, cw, 32.0];
            let (a, b) = match bg {
                Background::Solid(c) => (Rgba(c[0], c[1], c[2], 255), None),
                Background::Linear { from, to, .. } => {
                    (Rgba(from[0], from[1], from[2], 255), Some(Rgba(to[0], to[1], to[2], 255)))
                }
                Background::Image(_) => (Rgba(0, 0, 0, 255), None),
            };
            pen.swatch(r, a, b, self.set.choice == Choice::Preset(i), ID_PRESET + i as u32);
        }
        y += 2.0 * 38.0 + 2.0;
        let half = (PW - 6.0) / 2.0;
        let pic_label = match &self.picture {
            Some((_, _, name)) => pen.fit(name, 12.0, half - 14.0),
            None => tr(l, "bg-image").to_string(),
        };
        pen.chip([PX, y, half, 32.0], &pic_label, self.set.choice == Choice::Picture, ID_IMAGE);
        pen.chip([PX + half + 6.0, y, half, 32.0], tr(l, "bg-color"), self.set.choice == Choice::Color, ID_COLOR);
        y += 44.0;
        if self.set.choice == Choice::Color {
            let t = format!("{:.0}%", self.set.hue * 100.0);
            pen.slider_row(PX, y, PW, tr(l, "bg-hue"), self.set.hue, &t, ID_HUE);
            y += 40.0;
            let t = format!("{:.0}%", self.set.shade * 100.0);
            pen.slider_row(PX, y, PW, tr(l, "bg-shade"), self.set.shade, &t, ID_SHADE);
            y += 40.0;
        }
        let px = tr(l, "px");
        let rows = [
            (ID_PAD, "bg-padding", self.set.padding, MAX_PAD),
            (ID_COR, "bg-corners", self.set.corners, MAX_COR),
            (ID_SHA, "bg-shadow", self.set.shadow, MAX_SHA),
        ];
        for (id, key, v, max) in rows {
            pen.slider_row(PX, y, PW, tr(l, key), v as f32 / max as f32, &format!("{v} {px}"), id);
            y += 40.0;
        }
        pen.label(tr(l, "bg-ratio"), PX, y + 2.0);
        let labels: Vec<&str> =
            Ratio::ALL.iter().map(|r| if *r == Ratio::Auto { tr(l, "auto") } else { r.label() }).collect();
        let sel = Ratio::ALL.iter().position(|r| *r == self.set.ratio).unwrap_or(0);
        pen.seg([PX, y + 22.0, PW, 32.0], &labels, sel, ID_RATIO);

        let busy = self.base.phase != Phase::Ready;
        let by = H - 56.0;
        pen.button([PX, by - 46.0, 100.0, 36.0], tr(l, "reset-btn"), ID_RESET, Style::Secondary, !busy);
        pen.button(
            [PX + 108.0, by - 46.0, PW - 108.0, 36.0],
            tr(l, "cancel"),
            ID_CLOSE,
            Style::Secondary,
            self.base.phase != Phase::Saving,
        );
        pen.button([PX, by, PW, 36.0], tr(l, "save-bg"), ID_SAVE, Style::Primary, !busy);
        self.base.error_line(pen, PX, by - 62.0, PW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_settings_match_the_engines_defaults() {
        let s = Settings::default();
        let d = BgOptions::default();
        assert_eq!((s.padding, s.corners, s.shadow, s.ratio), (d.padding, d.corners, d.shadow, d.ratio));
        assert_eq!(s.choice, Choice::Preset(0));
    }

    #[test]
    fn preview_options_scale_the_pixel_values_with_the_preview() {
        let s = Settings { padding: 100, corners: 40, shadow: 20, ..Settings::default() };
        let o = s.options(None, 0.25);
        assert_eq!((o.padding, o.corners, o.shadow), (25, 10, 5));
        let full = s.options(None, 1.0);
        assert_eq!((full.padding, full.corners, full.shadow), (100, 40, 20));
    }

    #[test]
    fn the_background_follows_the_choice() {
        let mut s = Settings { choice: Choice::Preset(8), ..Settings::default() };
        assert!(matches!(s.options(None, 1.0).background, Background::Solid([255, 255, 255, 255])));
        s.choice = Choice::Color;
        assert!(matches!(s.options(None, 1.0).background, Background::Solid(c) if c[3] == 255));
        let pic = RgbaImage::new(4, 4);
        s.choice = Choice::Picture;
        assert!(matches!(s.options(Some(&pic), 1.0).background, Background::Image(_)));
        assert!(
            matches!(s.options(None, 1.0).background, Background::Linear { .. }),
            "no picture yet: the first preset"
        );
        s.choice = Choice::Preset(999);
        let _ = s.options(None, 1.0); // an out-of-range preset never panics
    }

    #[test]
    fn the_custom_colour_runs_from_dark_to_light() {
        let lum = |c: [u8; 4]| c[0] as u32 + c[1] as u32 + c[2] as u32;
        let dark = Settings { shade: 0.0, ..Settings::default() }.color();
        let light = Settings { shade: 1.0, ..Settings::default() }.color();
        assert!(lum(dark) < lum(light));
    }

    #[test]
    fn slider_values_map_onto_their_ranges() {
        let mut d = BgDialog::new(PathBuf::from("x.png"));
        d.slider_value(ID_PAD, 1.0);
        d.slider_value(ID_COR, 0.5);
        d.slider_value(ID_SHA, 0.0);
        assert_eq!((d.set.padding, d.set.corners, d.set.shadow), (MAX_PAD, MAX_COR / 2, 0));
    }
}
