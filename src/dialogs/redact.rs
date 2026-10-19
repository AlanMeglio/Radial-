//! REDACT: drag rectangles over the picture to black them out in a saved copy. The pixels under the boxes are
//! replaced, not covered, so the information cannot be recovered from the copy; the original is never changed.

use std::path::PathBuf;

use super::base::{Base, Phase};
use super::geom::from_points;
use super::ui::{contains, Canvas, Kind, Pen, Style, R};
use super::{Cursor, Dialog, Ev, Host, JobResult};
use crate::config::{tr, Lang};
use crate::engine::actions;
use crate::engine::crop::Rect;
use crate::jobs::fill;
use crate::theme::Rgba;

const W: f32 = 900.0;
const H: f32 = 640.0;
const CANVAS: R = [24.0, 84.0, 584.0, 492.0];
const PX: f32 = 632.0;
const PW: f32 = 244.0;

const ID_CANVAS: u32 = 10;
const ID_APPLY: u32 = 31;
const ID_CLOSE: u32 = 32;
const ID_UNDO: u32 = 132;
const ID_CLEAR: u32 = 133;
const VK_Z: u32 = 0x5A;
const VK_DELETE: u32 = 0x2E;

pub struct RedactDialog {
    base: Base,
    canvas: Canvas,
    rects: Vec<Rect>,
    /// Corner where the current drag started, and the rectangle it spans so far.
    anchor: Option<(i64, i64)>,
    live: Option<Rect>,
}

impl RedactDialog {
    pub fn new(src: PathBuf) -> RedactDialog {
        RedactDialog { base: Base::new(src), canvas: Canvas::new(CANVAS), rects: vec![], anchor: None, live: None }
    }

    fn point(&self, x: f32, y: f32) -> (i64, i64) {
        let (ix, iy) = self.canvas.to_image(x, y);
        (ix.round() as i64, iy.round() as i64)
    }

    fn screen(&self, r: Rect) -> R {
        let (l, t) = self.canvas.to_screen(r.x as f32, r.y as f32);
        let (rr, b) = self.canvas.to_screen((r.x + r.w) as f32, (r.y + r.h) as f32);
        [l, t, rr - l, b - t]
    }

    fn apply(&mut self, host: &mut Host) {
        if !self.base.ready() || self.rects.is_empty() {
            return;
        }
        let src = self.base.src.clone();
        let rects = self.rects.clone();
        self.base.save(host, move || actions::redact_file(&src, &rects, &[], 150));
    }
}

impl Dialog for RedactDialog {
    fn title(&self, lang: Lang) -> String {
        tr(lang, "dlg-redact").to_string()
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
                }
            }
            JobResult::Saved(r) => self.base.on_saved(r, host),
            _ => {}
        }
    }

    fn event(&mut self, ev: Ev, host: &mut Host) {
        let (bw, bh) = self.base.dims();
        match ev {
            Ev::Down { id, x, y } if id == ID_CANVAS && self.base.ready() => {
                self.anchor = Some(self.point(x, y));
                self.live = None;
            }
            Ev::Move { x, y, down } => {
                if down {
                    if let Some(a) = self.anchor {
                        self.live = from_points(a, self.point(x, y), bw, bh);
                    }
                } else {
                    host.cursor = if self.base.ready() && contains(self.canvas.img_rect(), x, y) {
                        Cursor::Cross
                    } else {
                        Cursor::Arrow
                    };
                }
            }
            Ev::Up => {
                self.anchor = None;
                if let Some(r) = self.live.take() {
                    if r.w >= 2 && r.h >= 2 {
                        self.rects.push(r);
                    }
                }
            }
            Ev::Click(ID_APPLY) => self.apply(host),
            Ev::Click(ID_CLOSE) => host.close = true,
            Ev::Click(ID_UNDO) => {
                self.rects.pop();
            }
            Ev::Click(ID_CLEAR) => self.rects.clear(),
            Ev::Key { vk, ctrl, .. } if (ctrl && vk == VK_Z) || vk == VK_DELETE => {
                self.rects.pop();
            }
            _ => {}
        }
    }

    fn draw(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        self.base.header(pen, tr(l, "dlg-redact"), W);
        let thumb = self.base.loaded.as_ref().map(|x| &x.thumb);
        self.canvas.draw(pen, thumb);
        pen.hit(CANVAS, ID_CANVAS, Kind::Drag);
        if self.base.ready() {
            let black = Rgba(0, 0, 0, 255);
            for r in &self.rects {
                let s = self.screen(*r);
                pen.fill(s, 0.0, black);
                pen.ring(s, 0.0, Rgba(255, 255, 255, 70), 1.0);
            }
            if let Some(r) = self.live {
                let s = self.screen(r);
                pen.fill(s, 0.0, Rgba(0, 0, 0, 190));
                pen.ring(s, 0.0, Rgba(255, 255, 255, 230), 1.5);
                let label = format!("{} x {}", r.w, r.h);
                let w = pen.width(&label, 11.5) + 16.0;
                let chip = [
                    (s[0]).max(self.canvas.img_rect()[0]),
                    (s[1] - 26.0).max(self.canvas.img_rect()[1] + 2.0),
                    w,
                    20.0,
                ];
                pen.fill(chip, 4.0, Rgba(0, 0, 0, 200));
                pen.text_c(&label, chip[0] + w / 2.0, chip[1] + 10.5, 11.5, Rgba(255, 255, 255, 255));
            }
        }
        self.base.status(pen, CANVAS);

        let mut y = 84.0;
        pen.label(tr(l, "red-areas"), PX, y);
        y += 22.0;
        let count = fill(tr(l, "red-count"), &[&self.rects.len().to_string()]);
        pen.text(&count, PX, y + 8.0, 13.0, pen.pal.text);
        y += 30.0;
        let half = (PW - 6.0) / 2.0;
        let can = self.base.ready() && !self.rects.is_empty();
        pen.button([PX, y, half, 34.0], tr(l, "undo"), ID_UNDO, Style::Secondary, can);
        pen.button([PX + half + 6.0, y, half, 34.0], tr(l, "clear"), ID_CLEAR, Style::Secondary, can);
        y += 52.0;
        // How it works, word wrapped by hand into short lines.
        for line in tr(l, "red-hint").split('|') {
            pen.text(line, PX, y, 11.5, pen.pal.dim);
            y += 18.0;
        }

        let by = H - 56.0;
        let busy = self.base.phase != Phase::Ready;
        pen.button([PX, by, 96.0, 36.0], tr(l, "cancel"), ID_CLOSE, Style::Secondary, self.base.phase != Phase::Saving);
        pen.button(
            [PX + 104.0, by, PW - 104.0, 36.0],
            tr(l, "red-apply"),
            ID_APPLY,
            Style::Danger,
            !busy && !self.rects.is_empty(),
        );
        self.base.error_line(pen, PX, by - 20.0, PW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ready() -> RedactDialog {
        let mut d = RedactDialog::new(PathBuf::from("x.png"));
        d.base.loaded =
            Some(super::super::base::Loaded { w: 200, h: 100, thumb: image::RgbaImage::new(1, 1), bytes: 1 });
        d.base.phase = Phase::Ready;
        d.canvas.set_dims(200, 100);
        d
    }

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

    #[test]
    fn dragging_adds_a_rectangle_and_undo_removes_it() {
        let mut d = ready();
        let mut h = host();
        let (x0, y0) = d.canvas.to_screen(20.0, 10.0);
        let (x1, y1) = d.canvas.to_screen(120.0, 60.0);
        d.event(Ev::Down { id: ID_CANVAS, x: x0, y: y0 }, &mut h);
        d.event(Ev::Move { x: x1, y: y1, down: true }, &mut h);
        d.event(Ev::Up, &mut h);
        assert_eq!(d.rects, vec![Rect::new(20, 10, 100, 50)]);
        d.event(Ev::Click(ID_UNDO), &mut h);
        assert!(d.rects.is_empty());
    }

    #[test]
    fn a_click_without_a_drag_adds_nothing_and_drags_are_clamped_to_the_image() {
        let mut d = ready();
        let mut h = host();
        let (x0, y0) = d.canvas.to_screen(50.0, 50.0);
        d.event(Ev::Down { id: ID_CANVAS, x: x0, y: y0 }, &mut h);
        d.event(Ev::Up, &mut h);
        assert!(d.rects.is_empty());
        d.event(Ev::Down { id: ID_CANVAS, x: x0, y: y0 }, &mut h);
        d.event(Ev::Move { x: x0 + 5000.0, y: y0 + 5000.0, down: true }, &mut h);
        d.event(Ev::Up, &mut h);
        assert_eq!(d.rects, vec![Rect::new(50, 50, 150, 50)]);
    }

    #[test]
    fn clear_and_the_delete_key_remove_rectangles() {
        let mut d = ready();
        let mut h = host();
        d.rects = vec![Rect::new(0, 0, 10, 10), Rect::new(20, 20, 10, 10)];
        d.event(Ev::Key { vk: VK_DELETE, ctrl: false }, &mut h);
        assert_eq!(d.rects.len(), 1);
        d.event(Ev::Click(ID_CLEAR), &mut h);
        assert!(d.rects.is_empty());
    }

    #[test]
    fn saving_needs_at_least_one_rectangle() {
        let mut d = ready();
        let mut h = host();
        d.apply(&mut h);
        assert!(h.jobs.is_empty(), "nothing to redact, nothing saved");
        d.rects.push(Rect::new(0, 0, 5, 5));
        d.apply(&mut h);
        assert_eq!(h.jobs.len(), 1);
        assert_eq!(d.base.phase, Phase::Saving);
    }
}
