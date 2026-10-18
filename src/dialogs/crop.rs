//! CROP: aspect ratio chips, a draggable rectangle with eight handles, W and H in pixels, Reset and Apply.

use std::path::PathBuf;

use super::base::{Base, Phase};
use super::geom::{self, Handle};
use super::ui::{Canvas, Field, Kind, Pen, Style, R};
use super::{Cursor, Dialog, Ev, Host, JobResult};
use crate::config::{tr, Lang};
use crate::engine::actions;
use crate::engine::crop::{Aspect, Rect};
use crate::theme::Rgba;

const W: f32 = 900.0;
const H: f32 = 620.0;
const CANVAS: R = [24.0, 84.0, 584.0, 472.0];
const PX: f32 = 632.0; // right panel
const PW: f32 = 244.0;

const ID_CANVAS: u32 = 10;
const ID_W: u32 = 20;
const ID_H: u32 = 21;
const ID_RESET: u32 = 30;
const ID_APPLY: u32 = 31;
const ID_CLOSE: u32 = 32;
const ID_ASPECT: u32 = 100;

/// Grab radius of the handles, in logical pixels.
const GRAB: f32 = 9.0;

struct Drag {
    /// `None`: dragging out a new rectangle from `anchor`.
    handle: Option<Handle>,
    start: Rect,
    /// Where the pointer went down, in image pixels.
    anchor: (i64, i64),
}

pub struct CropDialog {
    base: Base,
    canvas: Canvas,
    aspect: Aspect,
    rect: Rect,
    w: Field,
    h: Field,
    drag: Option<Drag>,
}

impl CropDialog {
    pub fn new(src: PathBuf) -> CropDialog {
        CropDialog {
            base: Base::new(src),
            canvas: Canvas::new(CANVAS),
            aspect: Aspect::Free,
            rect: Rect::new(0, 0, 1, 1),
            w: Field::number(0),
            h: Field::number(0),
            drag: None,
        }
    }

    fn sync_fields(&mut self) {
        self.w.set(&self.rect.w.to_string());
        self.h.set(&self.rect.h.to_string());
    }

    fn reset(&mut self) {
        let (w, h) = self.base.dims();
        self.aspect = Aspect::Free;
        self.rect = Rect::new(0, 0, w, h);
        self.sync_fields();
    }

    fn screen_rect(&self) -> [f32; 4] {
        let (l, t) = self.canvas.to_screen(self.rect.x as f32, self.rect.y as f32);
        let (r, b) = self.canvas.to_screen((self.rect.x + self.rect.w) as f32, (self.rect.y + self.rect.h) as f32);
        [l, t, r, b]
    }

    fn image_point(&self, x: f32, y: f32) -> (i64, i64) {
        let (ix, iy) = self.canvas.to_image(x, y);
        (ix.round() as i64, iy.round() as i64)
    }

    fn apply_typed(&mut self, which_w: bool) {
        let (bw, bh) = self.base.dims();
        let field = if which_w { &self.w } else { &self.h };
        let Some(v) = field.value().filter(|&v| v > 0) else { return };
        self.rect = if which_w {
            geom::with_width(self.rect, v, self.aspect, bw, bh)
        } else {
            geom::with_height(self.rect, v, self.aspect, bw, bh)
        };
        // The field being typed in keeps its text; the other one follows.
        if which_w {
            self.h.set(&self.rect.h.to_string());
        } else {
            self.w.set(&self.rect.w.to_string());
        }
    }

    fn cursor_for(&self, x: f32, y: f32) -> Cursor {
        if !self.base.ready() {
            return Cursor::Arrow;
        }
        match geom::hit_handle(self.screen_rect(), x, y, GRAB) {
            Some(Handle::Nw | Handle::Se) => Cursor::SizeNwse,
            Some(Handle::Ne | Handle::Sw) => Cursor::SizeNesw,
            Some(Handle::N | Handle::S) => Cursor::SizeNs,
            Some(Handle::E | Handle::W) => Cursor::SizeWe,
            Some(Handle::Move) => Cursor::SizeAll,
            None if crate::dialogs::ui::contains(self.canvas.img_rect(), x, y) => Cursor::Cross,
            None => Cursor::Arrow,
        }
    }

    fn apply(&mut self, host: &mut Host) {
        if !self.base.ready() {
            return;
        }
        let (bw, bh) = self.base.dims();
        let Some(rect) = self.rect.clamp_to(bw, bh) else { return };
        let src = self.base.src.clone();
        self.base.save(host, move || actions::crop_file(&src, rect));
    }
}

impl Dialog for CropDialog {
    fn title(&self, lang: Lang) -> String {
        tr(lang, "dlg-crop").to_string()
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
        self.apply(host);
    }

    fn job(&mut self, result: JobResult, host: &mut Host) {
        match result {
            JobResult::Loaded(r) => {
                if self.base.on_loaded(r) {
                    let (w, h) = self.base.dims();
                    self.canvas.set_dims(w, h);
                    self.reset();
                }
            }
            JobResult::Saved(r) => self.base.on_saved(r, host),
            _ => {}
        }
    }

    fn event(&mut self, ev: Ev, host: &mut Host) {
        let (bw, bh) = self.base.dims();
        match ev {
            Ev::Down { x, y, id } if id == ID_CANVAS && self.base.ready() => {
                let anchor = self.image_point(x, y);
                self.drag = match geom::hit_handle(self.screen_rect(), x, y, GRAB) {
                    Some(h) => Some(Drag { handle: Some(h), start: self.rect, anchor }),
                    None if crate::dialogs::ui::contains(self.canvas.img_rect(), x, y) => {
                        Some(Drag { handle: None, start: self.rect, anchor })
                    }
                    None => None,
                };
                host.set_focus(None);
                self.sync_fields();
            }
            Ev::Move { x, y, down } => {
                if down {
                    if let Some(d) = &self.drag {
                        let p = self.image_point(x, y);
                        let next = match d.handle {
                            Some(h) => geom::drag(d.start, h, p.0 - d.anchor.0, p.1 - d.anchor.1, self.aspect, bw, bh),
                            None => geom::ratio_from_anchor(d.anchor, p, self.aspect, bw, bh).unwrap_or(self.rect),
                        };
                        if next != self.rect {
                            self.rect = next;
                            self.sync_fields();
                            host.redraw();
                        }
                    }
                } else {
                    let c = self.cursor_for(x, y);
                    if c != host.cursor {
                        host.cursor = c;
                    }
                }
            }
            Ev::Up => {
                self.drag = None;
                self.sync_fields();
            }
            Ev::Click(id) => {
                match id {
                    ID_W => self.w.focus(),
                    ID_H => self.h.focus(),
                    ID_RESET => self.reset(),
                    ID_APPLY => {
                        self.sync_fields();
                        self.apply(host);
                    }
                    ID_CLOSE => host.close = true,
                    i if (ID_ASPECT..ID_ASPECT + 6).contains(&i) => {
                        self.aspect = Aspect::ALL[(i - ID_ASPECT) as usize];
                        self.rect = geom::fit_aspect_inside(self.rect, self.aspect);
                        self.sync_fields();
                    }
                    _ => {}
                }
                if id != ID_W && id != ID_H {
                    self.sync_fields();
                }
            }
            Ev::Char(c) => match host.focus {
                Some(ID_W) if self.w.on_char(c) => self.apply_typed(true),
                Some(ID_H) if self.h.on_char(c) => self.apply_typed(false),
                _ => {}
            },
            Ev::Key { vk, .. } => match host.focus {
                Some(ID_W) if self.w.on_key(vk) => self.apply_typed(true),
                Some(ID_H) if self.h.on_key(vk) => self.apply_typed(false),
                _ => {}
            },
            _ => {}
        }
    }

    fn draw(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        self.base.header(pen, tr(l, "dlg-crop"), W);
        let thumb = self.base.loaded.as_ref().map(|x| &x.thumb);
        self.canvas.draw(pen, thumb);
        pen.hit(CANVAS, ID_CANVAS, Kind::Drag);
        if self.base.ready() {
            self.draw_overlay(pen);
        }
        self.base.status(pen, CANVAS);

        // Right panel.
        pen.label(tr(l, "crop-aspect"), PX, 86.0);
        let cw = (PW - 12.0) / 3.0;
        for (i, a) in Aspect::ALL.iter().enumerate() {
            let r = [PX + (i % 3) as f32 * (cw + 6.0), 106.0 + (i / 3) as f32 * 38.0, cw, 32.0];
            let label = if *a == Aspect::Free { tr(l, "free").to_string() } else { a.label() };
            pen.chip(r, &label, *a == self.aspect, ID_ASPECT + i as u32);
        }
        pen.label(tr(l, "crop-size"), PX, 198.0);
        let fw = (PW - 24.0) / 2.0;
        pen.field([PX, 218.0, fw, 34.0], &self.w, ID_W, "px");
        pen.text_c("x", PX + fw + 12.0, 235.5, 13.0, pen.pal.dim);
        pen.field([PX + fw + 24.0, 218.0, fw, 34.0], &self.h, ID_H, "px");
        let (iw, ih) = self.base.dims();
        let info = crate::jobs::fill(tr(l, "crop-info"), &[&self.rect.x.to_string(), &self.rect.y.to_string()]);
        pen.text(&info, PX, 276.0, 11.5, pen.pal.dim);
        let orig = crate::jobs::fill(tr(l, "crop-original"), &[&iw.to_string(), &ih.to_string()]);
        pen.text(&orig, PX, 296.0, 11.5, pen.pal.dim);

        let busy = self.base.phase != Phase::Ready;
        let y = H - 56.0;
        pen.button([PX, y, 64.0, 36.0], tr(l, "reset-btn"), ID_RESET, Style::Secondary, !busy);
        pen.button(
            [PX + 72.0, y, 72.0, 36.0],
            tr(l, "cancel"),
            ID_CLOSE,
            Style::Secondary,
            self.base.phase != Phase::Saving,
        );
        pen.button([PX + 152.0, y, PW - 152.0, 36.0], tr(l, "apply"), ID_APPLY, Style::Primary, !busy);
        pen.text(tr(l, "saved-next-to"), PX, y - 22.0, 11.0, pen.pal.dim);
        self.base.error_line(pen, PX, y - 44.0, PW);
    }
}

impl CropDialog {
    /// Dimmed surroundings, the rectangle, thirds, handles and the size chip.
    fn draw_overlay(&self, pen: &mut Pen) {
        let img = self.canvas.img_rect();
        let [l, t, r, b] = self.screen_rect();
        let veil = Rgba(4, 6, 12, 150);
        pen.fill([img[0], img[1], img[2], (t - img[1]).max(0.0)], 0.0, veil);
        pen.fill([img[0], b, img[2], (img[1] + img[3] - b).max(0.0)], 0.0, veil);
        pen.fill([img[0], t, (l - img[0]).max(0.0), b - t], 0.0, veil);
        pen.fill([r, t, (img[0] + img[2] - r).max(0.0), b - t], 0.0, veil);
        let white = Rgba(255, 255, 255, 255);
        let faint = Rgba(255, 255, 255, 90);
        for i in 1..3 {
            let k = i as f32 / 3.0;
            pen.line((l + (r - l) * k, t), (l + (r - l) * k, b), faint, 1.0);
            pen.line((l, t + (b - t) * k), (r, t + (b - t) * k), faint, 1.0);
        }
        pen.ring([l, t, r - l, b - t], 0.0, white, 1.5);
        let accent = pen.pal.accent;
        for (hx, hy) in [
            (l, t),
            ((l + r) / 2.0, t),
            (r, t),
            (r, (t + b) / 2.0),
            (r, b),
            ((l + r) / 2.0, b),
            (l, b),
            (l, (t + b) / 2.0),
        ] {
            let size = 9.0;
            pen.fill([hx - size / 2.0 - 1.0, hy - size / 2.0 - 1.0, size + 2.0, size + 2.0], 3.0, Rgba(0, 0, 0, 140));
            pen.fill([hx - size / 2.0, hy - size / 2.0, size, size], 2.5, white);
            pen.ring([hx - size / 2.0, hy - size / 2.0, size, size], 2.5, accent, 1.2);
        }
        let label = format!("{} x {}", self.rect.w, self.rect.h);
        let w = pen.width(&label, 11.5) + 18.0;
        let chip = [(l + 8.0).min(img[0] + img[2] - w), (t + 8.0).min(img[1] + img[3] - 26.0), w, 22.0];
        pen.fill(chip, 11.0, Rgba(10, 12, 20, 200));
        pen.text_c(&label, chip[0] + w / 2.0, chip[1] + 11.5, 11.5, white);
    }
}
