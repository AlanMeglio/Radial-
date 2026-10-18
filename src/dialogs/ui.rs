//! Widgets for the action dialogs: one small immediate-mode toolkit drawn with tiny-skia in the same visual language
//! as the settings window (same palette, radii and type), plus the text field and the image canvas.

use tiny_skia::{FillRule, FilterQuality, Paint, PathBuilder, Pattern, Pixmap, PixmapPaint, SpreadMode, Transform};

use super::geom::Fit;
use crate::config::Lang;
use crate::render::{clamp01, linear, solid, stroke, Face};
use crate::settings::{rrect, Pal};
use crate::theme::Rgba;

/// A rectangle as `[x, y, w, h]` in logical pixels.
pub type R = [f32; 4];

/// Position `0..=1` of the pointer `x` along a slider, given the hit rectangle the slider registered.
pub fn track_t(hit: R, x: f32) -> f32 {
    clamp01((x - (hit[0] + 8.0)) / (hit[2] - 16.0).max(1.0))
}

pub fn contains(r: R, x: f32, y: f32) -> bool {
    x >= r[0] && x < r[0] + r[2] && y >= r[1] && y < r[1] + r[3]
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    /// Reports a click when pressed and released on it.
    Button,
    /// Receives press, move and release (sliders, the canvas).
    Drag,
    /// A text field: pressing focuses it.
    Field,
}

#[derive(Clone, Copy, Debug)]
pub struct Hit {
    pub r: R,
    pub id: u32,
    pub kind: Kind,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Style {
    Primary,
    Secondary,
    Danger,
}

/// Everything a dialog needs to draw itself and to register what can be clicked.
pub struct Pen<'a> {
    pub pm: &'a mut Pixmap,
    pub face: &'a mut Face,
    pub pal: &'a Pal,
    /// Device pixels per logical pixel.
    pub s: f32,
    pub hits: &'a mut Vec<Hit>,
    /// Focus order (every button and field, in draw order).
    pub order: &'a mut Vec<u32>,
    pub hot: Option<u32>,
    pub pressed: Option<u32>,
    pub focus: Option<u32>,
    pub lang: Lang,
    pub dark: bool,
    /// Spinner phase in seconds.
    pub time: f32,
}

pub const RED: Rgba = Rgba(0xff, 0x5c, 0x6c, 255);

impl Pen<'_> {
    pub fn fill(&mut self, r: R, rad: f32, c: Rgba) {
        let s = self.s;
        if let Some(p) = rrect(r[0] * s, r[1] * s, r[2] * s, r[3] * s, rad * s) {
            self.pm.fill_path(&p, &solid(c, 1.0), FillRule::Winding, Transform::identity(), None);
        }
    }

    pub fn outline(&mut self, r: R, rad: f32, c: Rgba) {
        let s = self.s;
        if let Some(p) = rrect(r[0] * s + 0.5, r[1] * s + 0.5, r[2] * s - 1.0, r[3] * s - 1.0, rad * s) {
            self.pm.stroke_path(&p, &solid(c, 1.0), &stroke(1.0), Transform::identity(), None);
        }
    }

    pub fn ring(&mut self, r: R, rad: f32, c: Rgba, w: f32) {
        let s = self.s;
        if let Some(p) = rrect(r[0] * s, r[1] * s, r[2] * s, r[3] * s, rad * s) {
            self.pm.stroke_path(&p, &solid(c, 1.0), &stroke(w * s), Transform::identity(), None);
        }
    }

    pub fn line(&mut self, a: (f32, f32), b: (f32, f32), c: Rgba, w: f32) {
        let s = self.s;
        let mut pb = PathBuilder::new();
        pb.move_to(a.0 * s, a.1 * s);
        pb.line_to(b.0 * s, b.1 * s);
        if let Some(p) = pb.finish() {
            self.pm.stroke_path(&p, &solid(c, 1.0), &stroke(w * s), Transform::identity(), None);
        }
    }

    pub fn text(&mut self, t: &str, x: f32, cy: f32, px: f32, c: Rgba) {
        let s = self.s;
        self.face.draw_left(self.pm, t, x * s, cy * s, px * s, c, 1.0, 0.0);
    }

    pub fn text_c(&mut self, t: &str, cx: f32, cy: f32, px: f32, c: Rgba) {
        let s = self.s;
        self.face.draw(self.pm, t, cx * s, cy * s, px * s, c, 1.0, 0.0);
    }

    pub fn text_r(&mut self, t: &str, right: f32, cy: f32, px: f32, c: Rgba) {
        let w = self.width(t, px);
        self.text(t, right - w, cy, px, c);
    }

    /// Width of `t` in logical pixels.
    pub fn width(&self, t: &str, px: f32) -> f32 {
        self.face.measure(t, px * self.s, 0.0) / self.s
    }

    /// `t` shortened with an ellipsis so it is at most `max` logical pixels wide.
    pub fn fit(&self, t: &str, px: f32, max: f32) -> String {
        if self.width(t, px) <= max {
            return t.to_string();
        }
        let mut out: String = t.to_string();
        while out.chars().count() > 1 && self.width(&format!("{out}\u{2026}"), px) > max {
            out.pop();
        }
        format!("{out}\u{2026}")
    }

    pub fn hit(&mut self, r: R, id: u32, kind: Kind) {
        self.hits.push(Hit { r, id, kind });
        if matches!(kind, Kind::Button | Kind::Field) {
            self.order.push(id);
        }
    }

    pub fn hovered(&self, id: u32) -> bool {
        self.hot == Some(id)
    }

    fn focus_ring(&mut self, r: R, rad: f32, id: u32) {
        if self.focus == Some(id) {
            let g = [r[0] - 3.0, r[1] - 3.0, r[2] + 6.0, r[3] + 6.0];
            self.ring(g, rad + 3.0, self.pal.accent, 1.6);
        }
    }

    /// Small uppercase caption above a group of controls.
    pub fn label(&mut self, t: &str, x: f32, y: f32) {
        let c = self.pal.dim;
        let up = t.to_uppercase();
        let s = self.s;
        self.face.draw_left(self.pm, &up, x * s, (y + 7.0) * s, 10.5 * s, c, 1.0, 0.12 * 10.5 * s);
    }

    pub fn button(&mut self, r: R, label: &str, id: u32, style: Style, enabled: bool) {
        let hov = enabled && self.hovered(id);
        let (bg, fg) = match style {
            Style::Primary => (
                if hov { self.pal.accent.mix(Rgba(255, 255, 255, 255), 0.15) } else { self.pal.accent },
                self.pal.on_accent,
            ),
            Style::Danger => {
                (if hov { RED.mix(Rgba(255, 255, 255, 255), 0.15) } else { RED }, Rgba(255, 255, 255, 255))
            }
            Style::Secondary => (if hov { self.pal.card_hi } else { self.pal.card }, self.pal.text),
        };
        let (bg, fg) = if enabled { (bg, fg) } else { (self.pal.card_hi, self.pal.dim.fade(0.6)) };
        self.fill(r, 10.0, bg);
        if style == Style::Secondary || !enabled {
            self.outline(r, 10.0, self.pal.border);
        }
        let shown = self.fit(label, 13.0, r[2] - 12.0);
        self.text_c(&shown, r[0] + r[2] / 2.0, r[1] + r[3] / 2.0 + 0.5, 13.0, fg);
        if enabled {
            self.hit(r, id, Kind::Button);
            self.focus_ring(r, 10.0, id);
        }
    }

    /// A pill that is either selected or not (aspect ratios, tools).
    pub fn chip(&mut self, r: R, label: &str, sel: bool, id: u32) {
        let hov = self.hovered(id);
        let (bg, fg) = if sel {
            (self.pal.accent, self.pal.on_accent)
        } else if hov {
            (self.pal.card_hi, self.pal.text)
        } else {
            (self.pal.card, self.pal.dim)
        };
        self.fill(r, 9.0, bg);
        if !sel {
            self.outline(r, 9.0, self.pal.border);
        }
        self.text_c(label, r[0] + r[2] / 2.0, r[1] + r[3] / 2.0 + 0.5, 12.0, fg);
        self.hit(r, id, Kind::Button);
        self.focus_ring(r, 9.0, id);
    }

    /// Segmented control; segment `i` has the id `base + i`.
    pub fn seg(&mut self, r: R, labels: &[&str], sel: usize, base: u32) {
        self.fill(r, 9.0, self.pal.card);
        self.outline(r, 9.0, self.pal.border);
        let w = (r[2] - 6.0) / labels.len().max(1) as f32;
        for (i, l) in labels.iter().enumerate() {
            let cell = [r[0] + 3.0 + w * i as f32, r[1] + 3.0, w, r[3] - 6.0];
            let id = base + i as u32;
            let (fill, txt) = if i == sel {
                (Some(self.pal.accent), self.pal.on_accent)
            } else if self.hovered(id) {
                (Some(self.pal.card_hi), self.pal.text)
            } else {
                (None, self.pal.dim)
            };
            if let Some(f) = fill {
                self.fill(cell, 7.0, f);
            }
            self.text_c(l, cell[0] + w / 2.0, cell[1] + cell[3] / 2.0 + 0.5, 12.0, txt);
            self.hit(cell, id, Kind::Button);
            self.focus_ring(cell, 7.0, id);
        }
    }

    /// Horizontal slider with its track in `r`; `t` is 0..1. Returns nothing: the dialog maps the pointer itself.
    pub fn slider(&mut self, r: R, t: f32, id: u32) {
        let mid = r[1] + r[3] / 2.0;
        self.fill([r[0], mid - 2.5, r[2], 5.0], 2.5, self.pal.card_hi);
        let kx = r[0] + r[2] * clamp01(t);
        self.fill([r[0], mid - 2.5, kx - r[0], 5.0], 2.5, self.pal.accent);
        let big = self.hovered(id) || self.pressed == Some(id);
        let rad = if big { 7.5 } else { 6.5 };
        self.fill([kx - rad - 1.0, mid - rad - 1.0, rad * 2.0 + 2.0, rad * 2.0 + 2.0], rad + 1.0, self.pal.bg);
        self.fill([kx - rad, mid - rad, rad * 2.0, rad * 2.0], rad, self.pal.accent);
        self.hit([r[0] - 8.0, r[1] - 4.0, r[2] + 16.0, r[3] + 8.0], id, Kind::Drag);
    }

    /// A labelled slider: caption left and value right on one line, the track under them. Rows are 40 px tall.
    #[allow(clippy::too_many_arguments)]
    pub fn slider_row(&mut self, x: f32, y: f32, w: f32, label: &str, t: f32, value: &str, id: u32) {
        self.text(label, x, y + 8.0, 12.0, self.pal.text);
        self.text_r(value, x + w, y + 8.0, 12.0, self.pal.dim);
        self.slider([x, y + 20.0, w, 14.0], t, id);
    }

    pub fn check(&mut self, r: R, label: &str, on: bool, id: u32) {
        let box_ = [r[0], r[1] + (r[3] - 18.0) / 2.0, 18.0, 18.0];
        self.fill(box_, 5.0, if on { self.pal.accent } else { self.pal.card_hi });
        if !on {
            self.outline(box_, 5.0, self.pal.border);
        } else {
            let (x, y) = (box_[0], box_[1]);
            self.line((x + 4.5, y + 9.5), (x + 7.8, y + 12.8), self.pal.on_accent, 1.8);
            self.line((x + 7.8, y + 12.8), (x + 13.5, y + 5.5), self.pal.on_accent, 1.8);
        }
        self.text(label, r[0] + 26.0, r[1] + r[3] / 2.0 + 0.5, 12.5, self.pal.text);
        self.hit(r, id, Kind::Button);
        self.focus_ring(box_, 5.0, id);
    }

    pub fn swatch(&mut self, r: R, a: Rgba, b: Option<Rgba>, sel: bool, id: u32) {
        let s = self.s;
        if let Some(path) = rrect(r[0] * s, r[1] * s, r[2] * s, r[3] * s, 9.0 * s) {
            let paint = match b {
                Some(b) => linear(
                    (r[0] * s, r[1] * s),
                    ((r[0] + r[2]) * s, (r[1] + r[3]) * s),
                    &[(0.0, a, 1.0), (1.0, b, 1.0)],
                ),
                None => solid(a, 1.0),
            };
            self.pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
        self.outline(r, 9.0, self.pal.border);
        if sel {
            self.ring([r[0] - 2.5, r[1] - 2.5, r[2] + 5.0, r[3] + 5.0], 11.0, self.pal.text, 1.8);
        } else if self.hovered(id) {
            self.ring([r[0] - 1.5, r[1] - 1.5, r[2] + 3.0, r[3] + 3.0], 10.5, self.pal.dim, 1.2);
        }
        self.hit(r, id, Kind::Button);
        self.focus_ring(r, 9.0, id);
    }

    /// A text field; `suffix` is dim text on the right (`px`).
    pub fn field(&mut self, r: R, f: &Field, id: u32, suffix: &str) {
        let focused = self.focus == Some(id);
        self.fill(r, 8.0, self.pal.card);
        self.outline(r, 8.0, if focused { self.pal.accent } else { self.pal.border });
        if focused {
            self.ring([r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0], 9.0, self.pal.accent.fade(0.45), 1.5);
        }
        let right = r[0] + r[2] - 8.0;
        if !suffix.is_empty() {
            self.text_r(suffix, right, r[1] + r[3] / 2.0 + 0.5, 11.0, self.pal.dim);
        }
        let room = r[2] - 16.0 - if suffix.is_empty() { 0.0 } else { self.width(suffix, 11.0) + 4.0 };
        let shown = self.fit(&f.text, 12.5, room);
        let cy = r[1] + r[3] / 2.0 + 0.5;
        let tw = self.width(&shown, 12.5);
        if focused && f.all && !f.text.is_empty() {
            let sel = [r[0] + 6.0, r[1] + 5.0, tw + 4.0, r[3] - 10.0];
            self.fill(sel, 4.0, self.pal.accent.fade(0.35));
        }
        self.text(&shown, r[0] + 8.0, cy, 12.5, self.pal.text);
        if focused && !f.all {
            let upto: String = shown.chars().take(f.caret).collect();
            let cx = r[0] + 8.0 + self.width(&upto, 12.5);
            self.line((cx, r[1] + 6.0), (cx, r[1] + r[3] - 6.0), self.pal.text, 1.2);
        }
        self.hit(r, id, Kind::Field);
    }

    /// An indeterminate spinner: a bright arc travelling around a faint ring.
    pub fn spinner(&mut self, cx: f32, cy: f32, rad: f32) {
        let s = self.s;
        let faint = self.pal.dim.fade(0.25);
        if let Some(ring) = PathBuilder::from_circle(cx * s, cy * s, rad * s) {
            self.pm.stroke_path(&ring, &solid(faint, 1.0), &stroke(3.0 * s), Transform::identity(), None);
        }
        let a0 = self.time * 4.2;
        let mut pb = PathBuilder::new();
        let steps = 18;
        for i in 0..=steps {
            let a = a0 + 1.9 * i as f32 / steps as f32;
            let (x, y) = (cx + rad * a.cos(), cy + rad * a.sin());
            if i == 0 {
                pb.move_to(x * s, y * s);
            } else {
                pb.line_to(x * s, y * s);
            }
        }
        if let Some(p) = pb.finish() {
            self.pm.stroke_path(&p, &solid(self.pal.accent, 1.0), &stroke(3.0 * s), Transform::identity(), None);
        }
    }

    /// Dims `r` and shows a spinner with `msg`: used while a worker thread is busy.
    pub fn busy(&mut self, r: R, msg: &str) {
        let veil = Rgba(self.pal.bg.0, self.pal.bg.1, self.pal.bg.2, 200);
        let s = self.s;
        if let Some(p) = rrect(r[0] * s, r[1] * s, r[2] * s, r[3] * s, 16.0 * s) {
            self.pm.fill_path(&p, &solid(veil, 1.0), FillRule::Winding, Transform::identity(), None);
        }
        let (cx, cy) = (r[0] + r[2] / 2.0, r[1] + r[3] / 2.0);
        self.spinner(cx, cy - 14.0, 14.0);
        self.text_c(msg, cx, cy + 26.0, 12.5, self.pal.dim);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Text field state
// ---------------------------------------------------------------------------------------------------------------

/// Editable single-line text with a caret. Pasting is not supported; digits-only fields reject everything else.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub text: String,
    /// Caret position in characters.
    pub caret: usize,
    /// Everything is selected: the next character replaces the text.
    pub all: bool,
    pub digits: bool,
    pub max: usize,
}

pub const VK_BACK: u32 = 0x08;
pub const VK_DELETE: u32 = 0x2E;
pub const VK_LEFT: u32 = 0x25;
pub const VK_RIGHT: u32 = 0x27;
pub const VK_HOME: u32 = 0x24;
pub const VK_END: u32 = 0x23;

impl Field {
    pub fn number(v: u32) -> Field {
        let text = v.to_string();
        Field { caret: text.len(), text, all: false, digits: true, max: 6 }
    }

    #[cfg(test)]
    pub fn text(max: usize) -> Field {
        Field { text: String::new(), caret: 0, all: false, digits: false, max }
    }

    pub fn set(&mut self, s: &str) {
        self.text = s.to_string();
        self.caret = self.text.chars().count();
        self.all = false;
    }

    /// Called when the field gets the focus: everything is selected, like a native edit box.
    pub fn focus(&mut self) {
        self.all = true;
        self.caret = self.text.chars().count();
    }

    pub fn value(&self) -> Option<u32> {
        self.text.trim().parse().ok()
    }

    fn byte_at(&self, chars: usize) -> usize {
        self.text.char_indices().nth(chars).map_or(self.text.len(), |(i, _)| i)
    }

    /// Returns true when the text changed.
    pub fn on_char(&mut self, c: char) -> bool {
        if c.is_control() || (self.digits && !c.is_ascii_digit()) {
            return false;
        }
        if self.all {
            self.text.clear();
            self.caret = 0;
            self.all = false;
        }
        if self.text.chars().count() >= self.max {
            return false;
        }
        let at = self.byte_at(self.caret);
        self.text.insert(at, c);
        self.caret += 1;
        true
    }

    /// Editing keys; returns true when the text changed.
    pub fn on_key(&mut self, vk: u32) -> bool {
        let len = self.text.chars().count();
        match vk {
            VK_BACK if self.all => {
                self.text.clear();
                self.caret = 0;
                self.all = false;
                true
            }
            VK_BACK if self.caret > 0 => {
                let (a, b) = (self.byte_at(self.caret - 1), self.byte_at(self.caret));
                self.text.replace_range(a..b, "");
                self.caret -= 1;
                true
            }
            VK_DELETE if self.all => {
                self.text.clear();
                self.caret = 0;
                self.all = false;
                true
            }
            VK_DELETE if self.caret < len => {
                let (a, b) = (self.byte_at(self.caret), self.byte_at(self.caret + 1));
                self.text.replace_range(a..b, "");
                true
            }
            VK_LEFT => {
                self.all = false;
                self.caret = self.caret.saturating_sub(1);
                false
            }
            VK_RIGHT => {
                self.all = false;
                self.caret = (self.caret + 1).min(len);
                false
            }
            VK_HOME => {
                self.all = false;
                self.caret = 0;
                false
            }
            VK_END => {
                self.all = false;
                self.caret = len;
                false
            }
            _ => false,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Image canvas
// ---------------------------------------------------------------------------------------------------------------

/// The picture area of a dialog: the image shown at its fitted size over a checkerboard.
pub struct Canvas {
    /// Logical rectangle of the whole area.
    pub r: R,
    /// Dimensions the coordinates of the dialog refer to (the original image, or the displayed one).
    pub iw: u32,
    pub ih: u32,
    pub fit: Fit,
    view: Option<(Pixmap, u32, u32)>,
    checker: Option<Pixmap>,
}

impl Canvas {
    pub fn new(r: R) -> Canvas {
        Canvas { r, iw: 1, ih: 1, fit: Fit::new(r, 1, 1, 0.0, false), view: None, checker: None }
    }

    /// Declares the image dimensions the coordinates refer to and refits.
    pub fn set_dims(&mut self, iw: u32, ih: u32) {
        self.iw = iw.max(1);
        self.ih = ih.max(1);
        self.fit = Fit::new(self.r, self.iw, self.ih, 14.0, true);
        self.view = None;
    }

    /// Logical rectangle the image occupies.
    pub fn img_rect(&self) -> R {
        [self.fit.x, self.fit.y, self.iw as f32 * self.fit.scale, self.ih as f32 * self.fit.scale]
    }

    pub fn to_image(&self, x: f32, y: f32) -> (f32, f32) {
        self.fit.to_image(x, y)
    }

    pub fn to_screen(&self, ix: f32, iy: f32) -> (f32, f32) {
        self.fit.to_screen(ix, iy)
    }

    /// Forgets the scaled copy (the source picture changed).
    pub fn invalidate(&mut self) {
        self.view = None;
    }

    /// Draws the card, the checkerboard and `source` (any size, stretched to the fitted rectangle).
    pub fn draw(&mut self, pen: &mut Pen, source: Option<&image::RgbaImage>) {
        let s = pen.s;
        pen.fill(self.r, 16.0, pen.pal.card_hi.mix(pen.pal.bg, 0.5));
        pen.outline(self.r, 16.0, pen.pal.border);
        let Some(src) = source else { return };
        let ir = self.img_rect();
        let (dw, dh) = ((ir[2] * s).round().max(1.0) as u32, (ir[3] * s).round().max(1.0) as u32);
        if !matches!(&self.view, Some((_, w, h)) if (*w, *h) == (dw, dh)) {
            self.view = scaled_pixmap(src, dw, dh).map(|p| (p, dw, dh));
        }
        let (x, y) = ((ir[0] * s).round() as i32, (ir[1] * s).round() as i32);
        // Transparent images show a checkerboard.
        let tile = checker(&mut self.checker, pen.dark);
        if let (Some(t), Some(rect)) = (tile, tiny_skia::Rect::from_xywh(x as f32, y as f32, dw as f32, dh as f32)) {
            let mut paint = Paint::default();
            paint.shader = Pattern::new(
                t.as_ref(),
                SpreadMode::Repeat,
                FilterQuality::Nearest,
                1.0,
                Transform::from_translate(x as f32, y as f32),
            );
            pen.pm.fill_rect(rect, &paint, Transform::identity(), None);
        }
        if let Some((p, _, _)) = &self.view {
            pen.pm.draw_pixmap(x, y, p.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
        }
    }
}

fn checker(slot: &mut Option<Pixmap>, dark: bool) -> Option<&Pixmap> {
    if slot.is_none() {
        let mut pm = Pixmap::new(16, 16)?;
        let (a, b) =
            if dark { ([0x2a, 0x2d, 0x38], [0x34, 0x38, 0x45]) } else { ([0xe3, 0xe6, 0xee], [0xf4, 0xf5, 0xf9]) };
        for (i, px) in pm.pixels_mut().iter_mut().enumerate() {
            let (x, y) = (i % 16, i / 16);
            let c = if (x / 8 + y / 8) % 2 == 0 { a } else { b };
            *px = tiny_skia::ColorU8::from_rgba(c[0], c[1], c[2], 255).premultiply();
        }
        *slot = Some(pm);
    }
    slot.as_ref()
}

/// `img` resized to exactly `w` x `h` as a premultiplied pixmap.
pub fn scaled_pixmap(img: &image::RgbaImage, w: u32, h: u32) -> Option<Pixmap> {
    let resized;
    let src = if (img.width(), img.height()) == (w, h) {
        img
    } else {
        resized = image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle);
        &resized
    };
    let mut pm = Pixmap::new(w, h)?;
    for (px, s) in pm.pixels_mut().iter_mut().zip(src.pixels()) {
        *px = tiny_skia::ColorU8::from_rgba(s[0], s[1], s[2], s[3]).premultiply();
    }
    Some(pm)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_digits_field_only_takes_digits_and_honours_its_length() {
        let mut f = Field::number(0);
        f.focus();
        assert!(f.on_char('1'));
        assert!(!f.on_char('x'));
        assert!(f.on_char('2'));
        assert_eq!(f.text, "12");
        for c in "34567".chars() {
            f.on_char(c);
        }
        assert_eq!(f.text.len(), 6, "capped at max");
        assert_eq!(f.value(), Some(123456));
    }

    #[test]
    fn typing_over_a_selected_field_replaces_it() {
        let mut f = Field::number(1920);
        f.focus();
        assert!(f.all);
        f.on_char('8');
        assert_eq!((f.text.as_str(), f.all), ("8", false));
    }

    #[test]
    fn backspace_delete_and_caret_movement_edit_in_place() {
        let mut f = Field::text(20);
        for c in "héllo".chars() {
            f.on_char(c);
        }
        assert!(f.on_key(VK_BACK));
        assert_eq!(f.text, "héll");
        f.on_key(VK_HOME);
        assert!(f.on_key(VK_DELETE));
        assert_eq!(f.text, "éll");
        f.on_key(VK_END);
        f.on_key(VK_LEFT);
        f.on_char('Z');
        assert_eq!(f.text, "élZl");
        let mut g = Field::text(5);
        assert!(!g.on_key(VK_BACK), "nothing to delete");
        assert!(!g.on_char('\u{8}'), "control characters are ignored");
    }

    #[test]
    fn backspace_on_a_selection_clears_it() {
        let mut f = Field::number(300);
        f.focus();
        assert!(f.on_key(VK_BACK));
        assert_eq!(f.text, "");
        assert_eq!(f.value(), None);
    }

    #[test]
    fn slider_positions_map_the_pointer_onto_the_track() {
        let hit = [92.0, 96.0, 132.0, 22.0]; // a 116 px track registered with 8 px of slack on each side
        assert_eq!(track_t(hit, 100.0), 0.0);
        assert_eq!(track_t(hit, 158.0), 0.5);
        assert_eq!(track_t(hit, 216.0), 1.0);
        assert_eq!(track_t(hit, -50.0), 0.0);
        assert_eq!(track_t(hit, 999.0), 1.0);
    }

    #[test]
    fn rect_contains_is_half_open() {
        assert!(contains([0.0, 0.0, 10.0, 10.0], 0.0, 0.0));
        assert!(!contains([0.0, 0.0, 10.0, 10.0], 10.0, 5.0));
    }
}
