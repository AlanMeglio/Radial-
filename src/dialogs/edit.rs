//! EDIT: rotate, flip, resize, grayscale, brightness and contrast with a live preview.

use std::path::PathBuf;
use std::sync::Arc;

use image::{DynamicImage, RgbaImage};

use super::base::{Base, Phase};
use super::ui::{track_t, Canvas, Field, Kind, Pen, Style, R};
use super::{Dialog, Ev, Host, JobResult};
use crate::config::{tr, Lang};
use crate::engine::actions;
use crate::engine::edit::{self, EditOp};

const W: f32 = 900.0;
const H: f32 = 660.0;
const CANVAS: R = [24.0, 84.0, 584.0, 512.0];
const PX: f32 = 632.0;
const PW: f32 = 244.0;

const ID_CANVAS: u32 = 10;
const ID_W: u32 = 20;
const ID_H: u32 = 21;
const ID_RESET: u32 = 30;
const ID_APPLY: u32 = 31;
const ID_CLOSE: u32 = 32;
const ID_ROT_L: u32 = 60;
const ID_ROT_R: u32 = 61;
const ID_FLIP_H: u32 = 62;
const ID_FLIP_V: u32 = 63;
const ID_GRAY: u32 = 64;
const ID_LOCK: u32 = 65;
const ID_BRI: u32 = 70;
const ID_CON: u32 = 71;

/// Everything the user changed; turned into engine operations for the preview and for the saved file.
#[derive(Clone, Debug, PartialEq)]
pub struct EditState {
    /// Clockwise quarter turns, 0..4.
    pub turns: u8,
    pub flip_h: bool,
    pub flip_v: bool,
    pub gray: bool,
    /// -100..=100
    pub brightness: i32,
    /// -100..=100
    pub contrast: i32,
    /// Target size when the user typed one that differs from the natural size.
    pub size: Option<(u32, u32)>,
    pub lock: bool,
}

impl Default for EditState {
    fn default() -> EditState {
        EditState {
            turns: 0,
            flip_h: false,
            flip_v: false,
            gray: false,
            brightness: 0,
            contrast: 0,
            size: None,
            lock: true,
        }
    }
}

impl EditState {
    pub fn rotate(&mut self, clockwise: bool) {
        self.turns = (self.turns + if clockwise { 1 } else { 3 }) % 4;
        // A typed size referred to the old orientation.
        self.size = None;
    }

    /// Size after the rotation, before any resize.
    pub fn natural(&self, w: u32, h: u32) -> (u32, u32) {
        if self.turns % 2 == 1 {
            (h, w)
        } else {
            (w, h)
        }
    }

    /// Operations for an image of `w` x `h`. `with_resize` is false for the preview, which stays small.
    pub fn ops(&self, w: u32, h: u32, with_resize: bool) -> Vec<EditOp> {
        let mut ops = Vec::new();
        match self.turns % 4 {
            1 => ops.push(EditOp::Rotate90),
            2 => ops.push(EditOp::Rotate180),
            3 => ops.push(EditOp::Rotate270),
            _ => {}
        }
        if self.flip_h {
            ops.push(EditOp::FlipHorizontal);
        }
        if self.flip_v {
            ops.push(EditOp::FlipVertical);
        }
        if let (true, Some((rw, rh))) = (with_resize, self.size) {
            if (rw, rh) != self.natural(w, h) && rw > 0 && rh > 0 {
                ops.push(EditOp::Resize { w: rw, h: rh });
            }
        }
        if self.gray {
            ops.push(EditOp::Grayscale);
        }
        if self.brightness != 0 {
            ops.push(EditOp::Brightness((self.brightness as f32 * 2.55).round() as i32));
        }
        if self.contrast != 0 {
            ops.push(EditOp::Contrast(self.contrast as f32));
        }
        ops
    }

    /// The height that keeps the natural aspect for a typed width (and the reverse).
    pub fn linked(&self, natural: (u32, u32), typed: u32, is_width: bool) -> u32 {
        let (nw, nh) = (natural.0.max(1) as f64, natural.1.max(1) as f64);
        let v = if is_width { typed as f64 * nh / nw } else { typed as f64 * nw / nh };
        (v.round() as u32).max(1)
    }
}

pub struct EditDialog {
    base: Base,
    canvas: Canvas,
    st: EditState,
    thumb: Arc<RgbaImage>,
    preview: Option<RgbaImage>,
    gen: u32,
    inflight: bool,
    again: bool,
    w: Field,
    h: Field,
    drag: Option<u32>,
}

impl EditDialog {
    pub fn new(src: PathBuf) -> EditDialog {
        EditDialog {
            base: Base::new(src),
            canvas: Canvas::new(CANVAS),
            st: EditState::default(),
            thumb: Arc::new(RgbaImage::new(1, 1)),
            preview: None,
            gen: 0,
            inflight: false,
            again: false,
            w: Field::number(0),
            h: Field::number(0),
            drag: None,
        }
    }

    fn natural(&self) -> (u32, u32) {
        let (w, h) = self.base.dims();
        self.st.natural(w, h)
    }

    fn current_size(&self) -> (u32, u32) {
        self.st.size.unwrap_or_else(|| self.natural())
    }

    fn sync_fields(&mut self) {
        let (w, h) = self.current_size();
        self.w.set(&w.to_string());
        self.h.set(&h.to_string());
    }

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
        let ops = self.st.ops(thumb.width(), thumb.height(), false);
        host.spawn(move || {
            let img = edit::apply(DynamicImage::ImageRgba8((*thumb).clone()), &ops)
                .map(|i| i.to_rgba8())
                .unwrap_or_else(|_| (*thumb).clone());
            JobResult::Preview { gen, img }
        });
    }

    fn slider(&mut self, id: u32, t: f32) {
        let v = ((t * 200.0).round() as i32 - 100).clamp(-100, 100);
        // A little magnetism at the centre so "no change" is easy to hit.
        let v = if v.abs() <= 2 { 0 } else { v };
        if id == ID_BRI {
            self.st.brightness = v;
        } else {
            self.st.contrast = v;
        }
    }

    fn typed(&mut self, is_width: bool) {
        let natural = self.natural();
        let field = if is_width { &self.w } else { &self.h };
        let Some(v) = field.value().filter(|&v| v > 0) else { return };
        let (w, h) = self.current_size();
        let (nw, nh) = if self.st.lock {
            if is_width {
                (v, self.st.linked(natural, v, true))
            } else {
                (self.st.linked(natural, v, false), v)
            }
        } else if is_width {
            (v, h)
        } else {
            (w, v)
        };
        self.st.size = Some((nw, nh));
        if self.st.lock {
            if is_width {
                self.h.set(&nh.to_string());
            } else {
                self.w.set(&nw.to_string());
            }
        }
    }

    fn apply(&mut self, host: &mut Host) {
        let (w, h) = self.base.dims();
        let ops = self.st.ops(w, h, true);
        if !self.base.ready() || ops.is_empty() {
            return;
        }
        let src = self.base.src.clone();
        self.base.save(host, move || actions::edit_file(&src, &ops));
    }
}

impl Dialog for EditDialog {
    fn title(&self, lang: Lang) -> String {
        tr(lang, "dlg-edit").to_string()
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
        self.apply(host);
    }

    fn job(&mut self, result: JobResult, host: &mut Host) {
        match result {
            JobResult::Loaded(r) => {
                if self.base.on_loaded(r) {
                    if let Some(l) = self.base.loaded.as_mut() {
                        self.thumb = Arc::new(std::mem::replace(&mut l.thumb, RgbaImage::new(1, 1)));
                    }
                    self.sync_fields();
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
            JobResult::Saved(r) => self.base.on_saved(r, host),
            _ => {}
        }
    }

    fn event(&mut self, ev: Ev, host: &mut Host) {
        match ev {
            Ev::Down { id, x, .. } if id == ID_BRI || id == ID_CON => {
                self.drag = Some(id);
                if let Some(r) = host.rect(id) {
                    self.slider(id, track_t(r, x));
                }
                self.request_preview(host);
            }
            Ev::Move { x, down: true, .. } => {
                if let Some(id) = self.drag {
                    if let Some(r) = host.rect(id) {
                        self.slider(id, track_t(r, x));
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
                    ID_W => self.w.focus(),
                    ID_H => self.h.focus(),
                    ID_ROT_L | ID_ROT_R => {
                        self.st.rotate(id == ID_ROT_R);
                        self.request_preview(host);
                    }
                    ID_FLIP_H => {
                        self.st.flip_h = !self.st.flip_h;
                        self.request_preview(host);
                    }
                    ID_FLIP_V => {
                        self.st.flip_v = !self.st.flip_v;
                        self.request_preview(host);
                    }
                    ID_GRAY => {
                        self.st.gray = !self.st.gray;
                        self.request_preview(host);
                    }
                    ID_LOCK => self.st.lock = !self.st.lock,
                    ID_RESET => {
                        self.st = EditState::default();
                        self.request_preview(host);
                    }
                    ID_APPLY => self.apply(host),
                    ID_CLOSE => host.close = true,
                    _ => {}
                }
                if id != ID_W && id != ID_H {
                    self.sync_fields();
                }
            }
            Ev::Char(c) => match host.focus {
                Some(ID_W) if self.w.on_char(c) => self.typed(true),
                Some(ID_H) if self.h.on_char(c) => self.typed(false),
                _ => {}
            },
            Ev::Key { vk, .. } => match host.focus {
                Some(ID_W) if self.w.on_key(vk) => self.typed(true),
                Some(ID_H) if self.h.on_key(vk) => self.typed(false),
                _ => {}
            },
            _ => {}
        }
    }

    fn draw(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        self.base.header(pen, tr(l, "dlg-edit"), W);
        self.canvas.draw(pen, self.preview.as_ref());
        pen.hit(CANVAS, ID_CANVAS, Kind::Button);
        if self.inflight && self.base.ready() {
            pen.spinner(CANVAS[0] + CANVAS[2] - 22.0, CANVAS[1] + 22.0, 8.0);
        }
        self.base.status(pen, CANVAS);

        let mut y = 84.0;
        pen.label(tr(l, "edit-orient"), PX, y);
        y += 20.0;
        let half = (PW - 6.0) / 2.0;
        let chips = [
            (ID_ROT_L, "edit-rot-l", false),
            (ID_ROT_R, "edit-rot-r", false),
            (ID_FLIP_H, "edit-flip-h", self.st.flip_h),
            (ID_FLIP_V, "edit-flip-v", self.st.flip_v),
        ];
        for (i, (id, key, on)) in chips.iter().enumerate() {
            let r = [PX + (i % 2) as f32 * (half + 6.0), y + (i / 2) as f32 * 38.0, half, 32.0];
            pen.chip(r, tr(l, key), *on, *id);
        }
        y += 84.0;
        pen.label(tr(l, "edit-tone"), PX, y);
        y += 20.0;
        pen.check([PX, y, PW, 26.0], tr(l, "edit-gray"), self.st.gray, ID_GRAY);
        y += 34.0;
        let signed = |v: i32| if v > 0 { format!("+{v}") } else { v.to_string() };
        pen.slider_row(
            PX,
            y,
            PW,
            tr(l, "edit-bri"),
            (self.st.brightness + 100) as f32 / 200.0,
            &signed(self.st.brightness),
            ID_BRI,
        );
        y += 40.0;
        pen.slider_row(
            PX,
            y,
            PW,
            tr(l, "edit-con"),
            (self.st.contrast + 100) as f32 / 200.0,
            &signed(self.st.contrast),
            ID_CON,
        );
        y += 48.0;
        pen.label(tr(l, "crop-size"), PX, y);
        y += 20.0;
        let fw = (PW - 24.0) / 2.0;
        pen.field([PX, y, fw, 34.0], &self.w, ID_W, "px");
        pen.text_c("x", PX + fw + 12.0, y + 17.5, 13.0, pen.pal.dim);
        pen.field([PX + fw + 24.0, y, fw, 34.0], &self.h, ID_H, "px");
        y += 42.0;
        pen.check([PX, y, PW, 26.0], tr(l, "edit-lock"), self.st.lock, ID_LOCK);

        let busy = self.base.phase != Phase::Ready;
        let (bw, bh) = self.base.dims();
        let changed = !self.st.ops(bw, bh, true).is_empty();
        let by = H - 56.0;
        pen.button([PX, by, 64.0, 36.0], tr(l, "reset-btn"), ID_RESET, Style::Secondary, !busy);
        pen.button(
            [PX + 72.0, by, 72.0, 36.0],
            tr(l, "cancel"),
            ID_CLOSE,
            Style::Secondary,
            self.base.phase != Phase::Saving,
        );
        pen.button([PX + 152.0, by, PW - 152.0, 36.0], tr(l, "apply"), ID_APPLY, Style::Primary, !busy && changed);
        self.base.error_line(pen, PX, by - 20.0, PW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_untouched_state_has_nothing_to_apply() {
        assert!(EditState::default().ops(100, 50, true).is_empty());
    }

    #[test]
    fn quarter_turns_wrap_and_swap_the_natural_size() {
        let mut s = EditState::default();
        s.rotate(true);
        assert_eq!((s.turns, s.natural(100, 50)), (1, (50, 100)));
        s.rotate(true);
        s.rotate(true);
        s.rotate(true);
        assert_eq!(s.turns, 0, "four right turns are a full circle");
        s.rotate(false);
        assert_eq!(s.turns, 3);
        assert_eq!(s.ops(100, 50, true), vec![EditOp::Rotate270]);
    }

    #[test]
    fn operations_come_out_in_a_fixed_order() {
        let s = EditState {
            turns: 2,
            flip_h: true,
            flip_v: true,
            gray: true,
            brightness: 20,
            contrast: -30,
            size: Some((10, 5)),
            lock: true,
        };
        assert_eq!(
            s.ops(100, 50, true),
            vec![
                EditOp::Rotate180,
                EditOp::FlipHorizontal,
                EditOp::FlipVertical,
                EditOp::Resize { w: 10, h: 5 },
                EditOp::Grayscale,
                EditOp::Brightness(51),
                EditOp::Contrast(-30.0),
            ]
        );
        assert!(!s.ops(100, 50, false).iter().any(|o| matches!(o, EditOp::Resize { .. })), "the preview never resizes");
    }

    #[test]
    fn a_size_equal_to_the_natural_one_is_no_resize() {
        let s = EditState { size: Some((100, 50)), ..EditState::default() };
        assert!(s.ops(100, 50, true).is_empty());
    }

    #[test]
    fn rotating_forgets_a_typed_size_because_it_belonged_to_the_old_orientation() {
        let mut s = EditState { size: Some((10, 5)), ..EditState::default() };
        s.rotate(true);
        assert_eq!(s.size, None);
    }

    #[test]
    fn linked_sizes_keep_the_aspect_and_never_reach_zero() {
        let s = EditState::default();
        assert_eq!(s.linked((1600, 1000), 800, true), 500);
        assert_eq!(s.linked((1600, 1000), 500, false), 800);
        assert_eq!(s.linked((1600, 1000), 1, true), 1, "0.625 rounds to 1, never 0");
    }

    #[test]
    fn the_sliders_have_a_dead_zone_at_the_centre_and_reach_both_ends() {
        let mut d = EditDialog::new(PathBuf::from("x.png"));
        d.slider(ID_BRI, 0.5);
        assert_eq!(d.st.brightness, 0);
        d.slider(ID_BRI, 0.505);
        assert_eq!(d.st.brightness, 0);
        d.slider(ID_BRI, 1.0);
        assert_eq!(d.st.brightness, 100);
        d.slider(ID_CON, 0.0);
        assert_eq!(d.st.contrast, -100);
    }

    #[test]
    fn typing_a_width_with_the_lock_on_derives_the_height() {
        let mut d = EditDialog::new(PathBuf::from("x.png"));
        d.base.loaded = Some(super::super::base::Loaded { w: 1600, h: 1000, thumb: RgbaImage::new(1, 1), bytes: 1 });
        d.w.set("800");
        d.typed(true);
        assert_eq!(d.st.size, Some((800, 500)));
        assert_eq!(d.h.text, "500");
        d.st.lock = false;
        d.h.set("300");
        d.typed(false);
        assert_eq!(d.st.size, Some((800, 300)));
    }
}
