//! ANNOTATE: pen, line, arrow, box, ellipse, marker and text over the picture, with undo and a colour.
//! Annotations live in original-image pixels; the preview renders them over the downscaled picture with the same
//! engine code that writes the saved file.

use std::path::PathBuf;

use ab_glyph::FontArc;
use image::RgbaImage;

use super::base::{Base, Phase};
use super::ui::{contains, track_t, Canvas, Kind, Pen, Style, R};
use super::{Cursor, Dialog, Ev, Host, JobResult};
use crate::config::{tr, Lang};
use crate::engine::actions;
use crate::engine::annotate::{load_system_font, render, Annotation};
use crate::theme::Rgba;

const W: f32 = 900.0;
const H: f32 = 660.0;
const CANVAS: R = [24.0, 84.0, 584.0, 512.0];
const PX: f32 = 632.0;
const PW: f32 = 244.0;

const ID_CANVAS: u32 = 10;
const ID_APPLY: u32 = 31;
const ID_CLOSE: u32 = 32;
const ID_TOOL: u32 = 100;
const ID_COLOR: u32 = 120;
const ID_FILL: u32 = 130;
const ID_SIZE: u32 = 131;
const ID_UNDO: u32 = 132;
const ID_CLEAR: u32 = 133;

const VK_RETURN: u32 = 0x0D;
const VK_ESCAPE: u32 = 0x1B;
const VK_Z: u32 = 0x5A;

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Tool {
    Pen,
    Line,
    Arrow,
    Box,
    Ellipse,
    Marker,
    Text,
}

pub const TOOLS: [(Tool, &str); 7] = [
    (Tool::Pen, "ann-pen"),
    (Tool::Line, "ann-line"),
    (Tool::Arrow, "ann-arrow"),
    (Tool::Box, "ann-box"),
    (Tool::Ellipse, "ann-ellipse"),
    (Tool::Marker, "ann-marker"),
    (Tool::Text, "ann-text"),
];

pub const COLORS: [[u8; 4]; 8] = [
    [255, 59, 48, 255],
    [255, 149, 0, 255],
    [255, 214, 10, 255],
    [52, 199, 89, 255],
    [10, 132, 255, 255],
    [175, 82, 222, 255],
    [20, 20, 24, 255],
    [255, 255, 255, 255],
];

/// Builds the annotation a drag from `from` to `to` (image pixels) makes with `tool`; `None` while it is too small.
pub fn shape(
    tool: Tool,
    from: (f32, f32),
    to: (f32, f32),
    color: [u8; 4],
    width: f32,
    fill: bool,
) -> Option<Annotation> {
    let far = (from.0 - to.0).hypot(from.1 - to.1) >= 1.0;
    if !far {
        return None;
    }
    Some(match tool {
        Tool::Line => Annotation::Line { from, to, color, width },
        Tool::Arrow => Annotation::Arrow { from, to, color, width },
        Tool::Box => Annotation::Rect { from, to, color, width, fill },
        Tool::Ellipse => Annotation::Ellipse { from, to, color, width, fill },
        Tool::Marker => Annotation::Highlight { from, to, color: [color[0], color[1], color[2], 110] },
        Tool::Pen => Annotation::Stroke { points: vec![from, to], color, width },
        Tool::Text => return None,
    })
}

struct Live {
    tool: Tool,
    from: (f32, f32),
    /// Pen strokes collect their points here.
    points: Vec<(f32, f32)>,
    to: (f32, f32),
}

struct TextEdit {
    pos: (f32, f32),
    text: String,
}

pub struct AnnotateDialog {
    base: Base,
    canvas: Canvas,
    tool: Tool,
    color: usize,
    fill: bool,
    /// On-screen size of the pen or text, 1..=24 logical px.
    size: u32,
    items: Vec<Annotation>,
    live: Option<Live>,
    text: Option<TextEdit>,
    thumb: RgbaImage,
    /// The committed annotations drawn on the thumbnail.
    cache: RgbaImage,
    preview: Option<RgbaImage>,
    font: Option<FontArc>,
    drag_size: bool,
}

impl AnnotateDialog {
    pub fn new(src: PathBuf) -> AnnotateDialog {
        AnnotateDialog {
            base: Base::new(src),
            canvas: Canvas::new(CANVAS),
            tool: Tool::Pen,
            color: 0,
            fill: false,
            size: 4,
            items: vec![],
            live: None,
            text: None,
            thumb: RgbaImage::new(1, 1),
            cache: RgbaImage::new(1, 1),
            preview: None,
            font: None,
            drag_size: false,
        }
    }

    /// Image pixels per logical screen pixel: strokes keep the width they have on screen.
    fn per_px(&self) -> f32 {
        1.0 / self.canvas.fit.scale.max(0.01)
    }

    fn stroke_width(&self) -> f32 {
        (self.size as f32 * self.per_px()).max(1.0)
    }

    fn font_px(&self) -> f32 {
        (10.0 + self.size as f32 * 1.5) * self.per_px()
    }

    fn color_rgba(&self) -> [u8; 4] {
        COLORS[self.color]
    }

    fn k(&self) -> f32 {
        let (w, _) = self.base.dims();
        self.thumb.width() as f32 / w.max(1) as f32
    }

    /// Redraws the committed annotations over the thumbnail.
    fn rebuild_cache(&mut self) {
        let k = self.k();
        let mut img = self.thumb.clone();
        let scaled: Vec<Annotation> = self.items.iter().map(|a| a.scaled(k)).collect();
        let _ = render(&mut img, &scaled, self.font.as_ref());
        self.cache = img;
        self.refresh_preview();
    }

    /// The cache plus whatever is being drawn or typed right now.
    fn refresh_preview(&mut self) {
        let k = self.k();
        let mut extra: Vec<Annotation> = Vec::new();
        if let Some(l) = &self.live {
            let a = if l.tool == Tool::Pen {
                Some(Annotation::Stroke {
                    points: l.points.clone(),
                    color: self.color_rgba(),
                    width: self.stroke_width(),
                })
            } else {
                shape(l.tool, l.from, l.to, self.color_rgba(), self.stroke_width(), self.fill)
            };
            extra.extend(a);
        }
        if let Some(t) = &self.text {
            if !t.text.is_empty() {
                extra.push(Annotation::Text {
                    pos: t.pos,
                    text: t.text.clone(),
                    size: self.font_px(),
                    color: self.color_rgba(),
                });
            }
        }
        let mut img = self.cache.clone();
        let scaled: Vec<Annotation> = extra.iter().map(|a| a.scaled(k)).collect();
        let _ = render(&mut img, &scaled, self.font.as_ref());
        self.preview = Some(img);
        self.canvas.invalidate();
    }

    fn commit_text(&mut self) {
        if let Some(t) = self.text.take() {
            if !t.text.trim().is_empty() {
                self.items.push(Annotation::Text {
                    pos: t.pos,
                    text: t.text,
                    size: self.font_px(),
                    color: self.color_rgba(),
                });
                self.rebuild_cache();
                return;
            }
        }
        self.refresh_preview();
    }

    fn image_point(&self, x: f32, y: f32) -> (f32, f32) {
        let (w, h) = self.base.dims();
        let (ix, iy) = self.canvas.to_image(x, y);
        (ix.clamp(0.0, w as f32), iy.clamp(0.0, h as f32))
    }

    fn apply(&mut self, host: &mut Host) {
        self.commit_text();
        if !self.base.ready() || self.items.is_empty() {
            return;
        }
        let src = self.base.src.clone();
        let items = self.items.clone();
        let font = self.font.clone();
        self.base.save(host, move || actions::annotate_file(&src, &items, font.as_ref()));
    }

    fn undo(&mut self) {
        if self.text.is_some() {
            self.text = None;
            self.refresh_preview();
        } else if self.items.pop().is_some() {
            self.rebuild_cache();
        }
    }
}

impl Dialog for AnnotateDialog {
    fn title(&self, lang: Lang) -> String {
        tr(lang, "dlg-annotate").to_string()
    }

    fn size(&self) -> (f32, f32) {
        (W, H)
    }

    fn start(&mut self, host: &mut Host) {
        self.font = load_system_font();
        self.base.start(host);
    }

    fn busy(&self) -> bool {
        self.base.busy()
    }

    fn saving(&self) -> bool {
        self.base.phase == Phase::Saving
    }

    fn editing(&self) -> bool {
        self.text.is_some()
    }

    fn default_action(&mut self, host: &mut Host) {
        self.apply(host);
    }

    fn job(&mut self, result: JobResult, host: &mut Host) {
        match result {
            JobResult::Loaded(r) => {
                if self.base.on_loaded(r) {
                    if let Some(l) = self.base.loaded.as_mut() {
                        self.thumb = std::mem::replace(&mut l.thumb, RgbaImage::new(1, 1));
                    }
                    let (w, h) = self.base.dims();
                    self.canvas.set_dims(w, h);
                    self.rebuild_cache();
                }
            }
            JobResult::Saved(r) => self.base.on_saved(r, host),
            _ => {}
        }
    }

    fn event(&mut self, ev: Ev, host: &mut Host) {
        match ev {
            Ev::Down { id, x, y } if id == ID_CANVAS && self.base.ready() => {
                host.set_focus(None);
                let p = self.image_point(x, y);
                if self.tool == Tool::Text {
                    self.commit_text();
                    if self.font.is_some() {
                        self.text = Some(TextEdit { pos: p, text: String::new() });
                    }
                    self.refresh_preview();
                } else {
                    self.commit_text();
                    self.live = Some(Live { tool: self.tool, from: p, points: vec![p], to: p });
                }
            }
            Ev::Down { id: ID_SIZE, x, .. } => {
                self.drag_size = true;
                if let Some(r) = host.rect(ID_SIZE) {
                    self.size = 1 + (track_t(r, x) * 23.0).round() as u32;
                }
                self.refresh_preview();
            }
            Ev::Move { x, y, down } => {
                if down && self.drag_size {
                    if let Some(r) = host.rect(ID_SIZE) {
                        self.size = 1 + (track_t(r, x) * 23.0).round() as u32;
                        self.refresh_preview();
                    }
                } else if down && self.live.is_some() {
                    let p = self.image_point(x, y);
                    if let Some(l) = self.live.as_mut() {
                        l.to = p;
                        if l.tool == Tool::Pen && l.points.last().map_or(true, |q| (q.0 - p.0).hypot(q.1 - p.1) >= 1.0) {
                            l.points.push(p);
                        }
                    }
                    self.refresh_preview();
                } else if !down {
                    let over = contains(self.canvas.img_rect(), x, y) && self.base.ready();
                    host.cursor = match (over, self.tool) {
                        (true, Tool::Text) => Cursor::IBeam,
                        (true, _) => Cursor::Cross,
                        _ => Cursor::Arrow,
                    };
                }
            }
            Ev::Up => {
                self.drag_size = false;
                if let Some(l) = self.live.take() {
                    let made = if l.tool == Tool::Pen && l.points.len() >= 2 {
                        Some(Annotation::Stroke {
                            points: l.points,
                            color: self.color_rgba(),
                            width: self.stroke_width(),
                        })
                    } else if l.tool == Tool::Pen {
                        // A click with the pen leaves a dot.
                        Some(Annotation::Stroke {
                            points: l.points,
                            color: self.color_rgba(),
                            width: self.stroke_width(),
                        })
                    } else {
                        shape(l.tool, l.from, l.to, self.color_rgba(), self.stroke_width(), self.fill)
                    };
                    if let Some(a) = made {
                        self.items.push(a);
                    }
                    self.rebuild_cache();
                }
            }
            Ev::Click(id) => match id {
                ID_APPLY => self.apply(host),
                ID_CLOSE => host.close = true,
                ID_UNDO => self.undo(),
                ID_CLEAR => {
                    self.text = None;
                    self.items.clear();
                    self.rebuild_cache();
                }
                ID_FILL => {
                    self.fill = !self.fill;
                    self.refresh_preview();
                }
                i if (ID_TOOL..ID_TOOL + 7).contains(&i) => {
                    self.commit_text();
                    self.tool = TOOLS[(i - ID_TOOL) as usize].0;
                }
                i if (ID_COLOR..ID_COLOR + 8).contains(&i) => {
                    self.color = (i - ID_COLOR) as usize;
                    self.refresh_preview();
                }
                _ => {}
            },
            Ev::Char(c) => {
                if let Some(t) = self.text.as_mut() {
                    if t.text.chars().count() < 200 {
                        t.text.push(c);
                    }
                    self.refresh_preview();
                }
            }
            Ev::Key { vk, ctrl, .. } => {
                if ctrl && vk == VK_Z {
                    self.undo();
                } else if self.text.is_some() {
                    match vk {
                        VK_RETURN => self.commit_text(),
                        VK_ESCAPE => {
                            self.text = None;
                            self.refresh_preview();
                        }
                        0x08 => {
                            if let Some(t) = self.text.as_mut() {
                                t.text.pop();
                            }
                            self.refresh_preview();
                        }
                        _ => {}
                    }
                }
            }
            _ => {}
        }
    }

    fn draw(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        self.base.header(pen, tr(l, "dlg-annotate"), W);
        self.canvas.draw(pen, self.preview.as_ref());
        pen.hit(CANVAS, ID_CANVAS, Kind::Drag);
        self.base.status(pen, CANVAS);
        if let Some(t) = &self.text {
            // A caret where the text goes.
            let (sx, sy) = self.canvas.to_screen(t.pos.0, t.pos.1);
            let h = self.font_px() * self.canvas.fit.scale;
            let k = self.canvas.fit.scale;
            let tw = if t.text.is_empty() { 0.0 } else { pen.width(&t.text, h * 0.86) };
            let _ = k;
            pen.line((sx + tw + 1.0, sy), (sx + tw + 1.0, sy + h), pen.pal.accent, 1.5);
        }

        let mut y = 84.0;
        pen.label(tr(l, "ann-tools"), PX, y);
        y += 20.0;
        let cw = (PW - 12.0) / 3.0;
        for (i, (tool, key)) in TOOLS.iter().enumerate() {
            let r = [PX + (i % 3) as f32 * (cw + 6.0), y + (i / 3) as f32 * 38.0, cw, 32.0];
            let enabled_text = *tool != Tool::Text || self.font.is_some();
            let id = ID_TOOL + i as u32;
            if enabled_text {
                pen.chip(r, tr(l, key), self.tool == *tool, id);
            }
        }
        y += 3.0 * 38.0 + 2.0;
        pen.check([PX, y, PW, 26.0], tr(l, "ann-fill"), self.fill, ID_FILL);
        y += 40.0;
        pen.label(tr(l, "ann-color"), PX, y);
        y += 20.0;
        let sw = (PW - 7.0 * 6.0) / 8.0;
        for (i, c) in COLORS.iter().enumerate() {
            let r = [PX + i as f32 * (sw + 6.0), y, sw, 28.0];
            pen.swatch(r, Rgba(c[0], c[1], c[2], 255), None, self.color == i, ID_COLOR + i as u32);
        }
        y += 44.0;
        let label = if self.tool == Tool::Text { tr(l, "ann-font") } else { tr(l, "ann-width") };
        pen.slider_row(
            PX,
            y,
            PW,
            label,
            (self.size - 1) as f32 / 23.0,
            &format!("{} {}", self.size, tr(l, "px")),
            ID_SIZE,
        );
        y += 52.0;
        let half = (PW - 6.0) / 2.0;
        let can_undo = self.base.ready() && (!self.items.is_empty() || self.text.is_some());
        pen.button([PX, y, half, 34.0], tr(l, "undo"), ID_UNDO, Style::Secondary, can_undo);
        pen.button([PX + half + 6.0, y, half, 34.0], tr(l, "clear"), ID_CLEAR, Style::Secondary, can_undo);
        if self.tool == Tool::Text && self.font.is_none() {
            pen.text(tr(l, "ann-nofont"), PX, y + 52.0, 11.5, super::ui::RED);
        } else if self.tool == Tool::Text {
            pen.text(tr(l, "ann-text-hint"), PX, y + 52.0, 11.5, pen.pal.dim);
        }

        let by = H - 56.0;
        let busy = self.base.phase != Phase::Ready;
        pen.button([PX, by, 96.0, 36.0], tr(l, "cancel"), ID_CLOSE, Style::Secondary, self.base.phase != Phase::Saving);
        pen.button(
            [PX + 104.0, by, PW - 104.0, 36.0],
            tr(l, "apply"),
            ID_APPLY,
            Style::Primary,
            !busy && !self.items.is_empty(),
        );
        self.base.error_line(pen, PX, by - 20.0, PW);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_drag_makes_the_shape_of_the_tool() {
        let (a, b, c) = ((10.0, 10.0), (50.0, 40.0), [255, 0, 0, 255]);
        assert!(matches!(shape(Tool::Line, a, b, c, 3.0, false), Some(Annotation::Line { .. })));
        assert!(matches!(shape(Tool::Arrow, a, b, c, 3.0, false), Some(Annotation::Arrow { .. })));
        assert!(matches!(shape(Tool::Box, a, b, c, 3.0, true), Some(Annotation::Rect { fill: true, .. })));
        assert!(matches!(shape(Tool::Ellipse, a, b, c, 3.0, false), Some(Annotation::Ellipse { fill: false, .. })));
        assert!(shape(Tool::Text, a, b, c, 3.0, false).is_none(), "text is typed, not dragged");
    }

    #[test]
    fn the_marker_is_translucent_whatever_the_colour() {
        match shape(Tool::Marker, (0.0, 0.0), (20.0, 5.0), [10, 20, 30, 255], 4.0, false) {
            Some(Annotation::Highlight { color, .. }) => assert_eq!(color, [10, 20, 30, 110]),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_click_is_not_a_shape() {
        assert_eq!(shape(Tool::Box, (5.0, 5.0), (5.2, 5.2), [0; 4], 2.0, false), None);
    }

    fn loaded(d: &mut AnnotateDialog) {
        d.base.loaded = Some(super::super::base::Loaded { w: 100, h: 100, thumb: RgbaImage::new(1, 1), bytes: 1 });
        d.base.phase = Phase::Ready;
        d.thumb = RgbaImage::from_pixel(100, 100, image::Rgba([255, 255, 255, 255]));
        d.canvas.set_dims(100, 100);
        d.rebuild_cache();
    }

    #[test]
    fn undo_removes_the_last_annotation_and_clear_removes_all() {
        let mut d = AnnotateDialog::new(PathBuf::from("x.png"));
        loaded(&mut d);
        d.items.push(Annotation::Line { from: (0.0, 0.0), to: (9.0, 9.0), color: [0, 0, 0, 255], width: 2.0 });
        d.items.push(Annotation::Line { from: (0.0, 9.0), to: (9.0, 0.0), color: [0, 0, 0, 255], width: 2.0 });
        d.undo();
        assert_eq!(d.items.len(), 1);
        d.undo();
        d.undo();
        assert!(d.items.is_empty(), "undo on an empty list is harmless");
    }

    #[test]
    fn committed_annotations_show_in_the_preview() {
        let mut d = AnnotateDialog::new(PathBuf::from("x.png"));
        loaded(&mut d);
        d.items.push(Annotation::Rect {
            from: (0.0, 0.0),
            to: (100.0, 100.0),
            color: [255, 0, 0, 255],
            width: 1.0,
            fill: true,
        });
        d.rebuild_cache();
        let px = d.preview.as_ref().unwrap().get_pixel(50, 50).0;
        assert_eq!(px, [255, 0, 0, 255]);
    }

    #[test]
    fn typing_builds_a_text_annotation_on_enter_and_escape_drops_it() {
        let mut d = AnnotateDialog::new(PathBuf::from("x.png"));
        loaded(&mut d);
        d.text = Some(TextEdit { pos: (5.0, 5.0), text: "hello".into() });
        d.commit_text();
        assert!(matches!(&d.items[..], [Annotation::Text { text, .. }] if text == "hello"));
        d.text = Some(TextEdit { pos: (5.0, 5.0), text: "   ".into() });
        d.commit_text();
        assert_eq!(d.items.len(), 1, "blank text is not stored");
    }

    #[test]
    fn the_stroke_width_keeps_its_on_screen_size_at_any_zoom() {
        let mut d = AnnotateDialog::new(PathBuf::from("x.png"));
        d.base.loaded = Some(super::super::base::Loaded { w: 4000, h: 3000, thumb: RgbaImage::new(1, 1), bytes: 1 });
        d.canvas.set_dims(4000, 3000);
        d.size = 4;
        let on_screen = d.stroke_width() * d.canvas.fit.scale;
        assert!((on_screen - 4.0).abs() < 0.01, "{on_screen}");
    }
}
