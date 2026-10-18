//! Software rendering of the radial menu into a premultiplied RGBA pixmap.
//! Look: dark glass sectors, one luminous mint accent, soft glow on the hovered sector.

#![allow(clippy::field_reassign_with_default)]

use ab_glyph::{Font, FontVec, PxScale, ScaleFont};
use std::f32::consts::PI;
use tiny_skia::{
    Color, FillRule, GradientStop, LineCap, LineJoin, LinearGradient, Paint, Path, PathBuilder, Pixmap, Point,
    RadialGradient, SpreadMode, Stroke, Transform,
};

use crate::geometry::sector_angle;

/// Logical (96 dpi) size of the overlay square and its key radii.
pub const SIZE: f32 = 470.0;
pub const CENTER: f32 = SIZE / 2.0;
pub const DEAD_R: f32 = 46.0;
pub const RING_IN: f32 = 66.0;
pub const RING_OUT: f32 = 168.0;
pub const HIT_OUT: f32 = 200.0;

type Rgb = (u8, u8, u8);
const ACCENT: Rgb = (112, 255, 214);

pub struct Slot {
    pub label: &'static str,
    pub enabled: bool,
}

pub struct Scene {
    pub slots: Vec<Slot>,
    /// 0..1 open animation (also used, reversed, to fade out).
    pub open: f32,
    /// Per-slot hover amount 0..1, eased by the caller.
    pub hover: Vec<f32>,
    pub main: String,
    pub sub: String,
    /// True when `main` shows the hovered action (drawn in the accent colour).
    pub main_accent: bool,
}

pub struct Fonts {
    face: FontVec,
}

impl Fonts {
    /// Loads Segoe UI Semibold from the Windows fonts folder (no font is bundled).
    pub fn load() -> Option<Fonts> {
        let dir = std::path::PathBuf::from(std::env::var_os("WINDIR")?).join("Fonts");
        ["seguisb.ttf", "segoeui.ttf", "arial.ttf"]
            .iter()
            .find_map(|f| std::fs::read(dir.join(f)).ok())
            .and_then(|b| FontVec::try_from_vec(b).ok())
            .map(|face| Fonts { face })
    }
}

fn ease_out_back(t: f32) -> f32 {
    let (c1, c3) = (1.45, 2.45);
    let u = t - 1.0;
    1.0 + c3 * u * u * u + c1 * u * u
}

fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

fn color(c: Rgb, a: f32) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, (clamp01(a) * 255.0).round() as u8)
}

fn solid(c: Rgb, a: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(color(c, a));
    p.anti_alias = true;
    p
}

fn polar(r: f32, a: f32) -> Point {
    Point::from_xy(CENTER + r * a.sin(), CENTER - r * a.cos())
}

/// Annular wedge from angle `a0` to `a1` between radii `r0` and `r1`, outline sampled finely.
fn wedge(a0: f32, a1: f32, r0: f32, r1: f32) -> Option<Path> {
    let steps = (((a1 - a0).abs() * 180.0 / PI) / 2.0).ceil().max(4.0) as usize;
    let mut pb = PathBuilder::new();
    let p = polar(r1, a0);
    pb.move_to(p.x, p.y);
    for i in 1..=steps {
        let p = polar(r1, a0 + (a1 - a0) * i as f32 / steps as f32);
        pb.line_to(p.x, p.y);
    }
    for i in 0..=steps {
        let p = polar(r0, a1 - (a1 - a0) * i as f32 / steps as f32);
        pb.line_to(p.x, p.y);
    }
    pb.close();
    pb.finish()
}

fn stops(list: &[(f32, Rgb, f32)]) -> Vec<GradientStop> {
    list.iter().map(|&(t, c, a)| GradientStop::new(t, color(c, a))).collect()
}

fn linear(from: Point, to: Point, list: &[(f32, Rgb, f32)]) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    if let Some(sh) = LinearGradient::new(from, to, stops(list), SpreadMode::Pad, Transform::identity()) {
        p.shader = sh;
    }
    p
}

fn mix(a: Rgb, b: Rgb, t: f32) -> Rgb {
    let l = |x: u8, y: u8| (x as f32 + (y as f32 - x as f32) * clamp01(t)).round() as u8;
    (l(a.0, b.0), l(a.1, b.1), l(a.2, b.2))
}

/// Draws one frame. `dpi` is the monitor scale (1.0 = 96 dpi); the pixmap must be `SIZE * dpi` square.
pub fn render(pm: &mut Pixmap, scene: &Scene, dpi: f32, fonts: Option<&Fonts>) {
    pm.fill(Color::TRANSPARENT);
    let n = scene.slots.len();
    let open = clamp01(scene.open);
    if n == 0 || open <= 0.0 {
        // Pending: an almost invisible disc so the window is still a drop target before the menu opens.
        if let Some(path) = PathBuilder::from_circle(CENTER * dpi, CENTER * dpi, HIT_OUT * dpi) {
            pm.fill_path(&path, &solid((0, 0, 0), 0.012), FillRule::Winding, Transform::identity(), None);
        }
        return;
    }
    let k = 0.84 + 0.16 * ease_out_back(open).min(1.12);
    let sx = dpi * k;
    let off = CENTER * dpi - CENTER * sx;
    let tf = Transform::from_row(sx, 0.0, 0.0, sx, off, off);
    let g = open.powf(0.8);

    // Faint halo: darkens whatever is behind and keeps the whole disc hit-testable.
    let c = Point::from_xy(CENTER, CENTER);
    let halo = HIT_OUT + 20.0;
    if let Some(sh) = RadialGradient::new(
        c,
        c,
        halo,
        stops(&[(0.0, (4, 6, 14), 0.30), (0.78, (4, 6, 14), 0.16), (1.0, (4, 6, 14), 0.02)]),
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        let mut p = Paint::default();
        p.shader = sh;
        if let Some(path) = PathBuilder::from_circle(CENTER, CENTER, halo) {
            pm.fill_path(&path, &p, FillRule::Winding, tf, None);
        }
    }

    let half = PI / n as f32;
    let gap = (3.2f32).to_radians().min(half * 0.5);
    for (i, slot) in scene.slots.iter().enumerate() {
        let h = clamp01(scene.hover.get(i).copied().unwrap_or(0.0));
        let local = clamp01(open * 1.5 - 0.5 * i as f32 / n as f32);
        let e = ease_out_back(local).min(1.1);
        let a = sector_angle(i, n);
        let lift = h * 9.0 - (1.0 - e) * 30.0;
        let (r0, r1) = (RING_IN + lift, RING_OUT + lift + h * 6.0);
        let alpha = g * clamp01(local * 1.3);
        let Some(path) = wedge(a - half + gap * 0.5, a + half - gap * 0.5, r0, r1) else { continue };

        // Glass body: lighter near the outer rim, deeper inside.
        let body_a = if slot.enabled { 1.0 } else { 0.7 };
        let body = linear(
            polar(r1, a),
            polar(r0, a),
            &[(0.0, (34, 42, 66), 0.80 * alpha * body_a), (1.0, (10, 13, 24), 0.82 * alpha * body_a)],
        );
        pm.fill_path(&path, &body, FillRule::Winding, tf, None);
        if h > 0.0 {
            // Glow: wide soft strokes under a bright rim.
            for (w, ga) in [(14.0, 0.05), (8.0, 0.09), (4.0, 0.16)] {
                let st = Stroke { width: w, line_join: LineJoin::Round, line_cap: LineCap::Round, ..Stroke::default() };
                pm.stroke_path(&path, &solid(ACCENT, ga * h * alpha), &st, tf, None);
            }
            let fill =
                linear(polar(r1, a), polar(r0, a), &[(0.0, ACCENT, 0.34 * h * alpha), (1.0, ACCENT, 0.06 * h * alpha)]);
            pm.fill_path(&path, &fill, FillRule::Winding, tf, None);
        }
        let rim = if h > 0.0 { solid(ACCENT, (0.35 + 0.6 * h) * alpha) } else { solid((255, 255, 255), 0.13 * alpha) };
        let st = Stroke { width: 1.2 + 0.6 * h, line_join: LineJoin::Round, ..Stroke::default() };
        pm.stroke_path(&path, &rim, &st, tf, None);

        if let Some(f) = fonts {
            let pos = polar((r0 + r1) / 2.0, a);
            let ta = alpha * if slot.enabled { 1.0 } else { 0.30 };
            let col = mix((232, 238, 250), ACCENT, h);
            let px = (15.0 + 1.5 * h) * sx;
            let (x, y) = ((pos.x - CENTER) * sx + CENTER * dpi, (pos.y - CENTER) * sx + CENTER * dpi);
            draw_text(pm, f, slot.label, x, y, px, col, ta, 0.9 * sx);
        }
    }

    // Centre disc.
    let cr = DEAD_R + 2.0;
    if let Some(disc) = PathBuilder::from_circle(CENTER, CENTER, cr) {
        let mut p = Paint::default();
        p.anti_alias = true;
        if let Some(sh) = RadialGradient::new(
            Point::from_xy(CENTER - 10.0, CENTER - 14.0),
            c,
            cr + 14.0,
            stops(&[(0.0, (40, 50, 78), 0.86 * g), (1.0, (9, 12, 22), 0.9 * g)]),
            SpreadMode::Pad,
            Transform::identity(),
        ) {
            p.shader = sh;
            pm.fill_path(&disc, &p, FillRule::Winding, tf, None);
        }
        let any = scene.hover.iter().cloned().fold(0.0, f32::max);
        let st = Stroke { width: 1.4, ..Stroke::default() };
        pm.stroke_path(&disc, &solid(mix((255, 255, 255), ACCENT, any), (0.14 + 0.5 * any) * g), &st, tf, None);
    }
    if let Some(f) = fonts {
        let mid = CENTER * dpi;
        let mc = if scene.main_accent { ACCENT } else { (244, 247, 255) };
        draw_text(pm, f, &scene.main, mid, mid - 6.0 * sx, 19.0 * sx, mc, g, 0.6 * sx);
        draw_text(pm, f, &scene.sub, mid, mid + 14.0 * sx, 11.5 * sx, (160, 172, 198), g * 0.9, 0.3 * sx);
    }
}

/// Draws `text` centred on (`cx`, `cy`) with the given pixel size, blending into the premultiplied pixmap.
#[allow(clippy::too_many_arguments)]
fn draw_text(pm: &mut Pixmap, fonts: &Fonts, text: &str, cx: f32, cy: f32, px: f32, c: Rgb, alpha: f32, tracking: f32) {
    let scaled = fonts.face.as_scaled(PxScale::from(px));
    let width: f32 = text.chars().map(|ch| scaled.h_advance(scaled.glyph_id(ch)) + tracking).sum::<f32>() - tracking;
    let mut x = cx - width / 2.0;
    let base = cy + (scaled.ascent() + scaled.descent()) / 2.0;
    let (w, h) = (pm.width() as i32, pm.height() as i32);
    let data = pm.data_mut();
    for ch in text.chars() {
        let id = scaled.glyph_id(ch);
        let glyph = id.with_scale_and_position(PxScale::from(px), ab_glyph::point(x, base));
        if let Some(o) = fonts.face.outline_glyph(glyph) {
            let b = o.px_bounds();
            o.draw(|gx, gy, cov| {
                let (px, py) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
                if px < 0 || py < 0 || px >= w || py >= h {
                    return;
                }
                let a = clamp01(cov * alpha);
                let i = ((py * w + px) * 4) as usize;
                let inv = 1.0 - a;
                data[i] = (c.0 as f32 * a + data[i] as f32 * inv) as u8;
                data[i + 1] = (c.1 as f32 * a + data[i + 1] as f32 * inv) as u8;
                data[i + 2] = (c.2 as f32 * a + data[i + 2] as f32 * inv) as u8;
                data[i + 3] = (255.0 * a + data[i + 3] as f32 * inv) as u8;
            });
        }
        x += scaled.h_advance(id) + tracking;
    }
}

/// Tray icon: four glass wedges, the top one lit with the accent colour.
pub fn icon_pixmap(side: u32) -> Option<Pixmap> {
    let mut pm = Pixmap::new(side, side)?;
    let sx = side as f32 / 360.0;
    let t = (180.0 - CENTER) * sx;
    let tf = Transform::from_row(sx, 0.0, 0.0, sx, t, t);
    let half = PI / 4.0;
    for i in 0..4 {
        let a = sector_angle(i, 4);
        let Some(path) = wedge(a - half + 0.09, a + half - 0.09, 58.0, 176.0) else { continue };
        let paint = if i == 0 { solid(ACCENT, 1.0) } else { solid((176, 190, 222), 0.95) };
        pm.fill_path(&path, &paint, FillRule::Winding, tf, None);
    }
    Some(pm)
}

/// Dev helper: `radial --preview out.png` renders a static frame over a gradient backdrop.
pub fn preview_png(path: &str) {
    let dpi = 2.0;
    let side = (SIZE * dpi) as u32;
    let mut pm = Pixmap::new(side, side).expect("pixmap");
    let fonts = Fonts::load();
    let labels = ["PNG", "JPG", "WEBP", "GIF", "AVIF", "TIFF", "BMP", "HEIC"];
    let scene = Scene {
        slots: labels.iter().map(|l| Slot { label: l, enabled: !matches!(*l, "AVIF" | "HEIC") }).collect(),
        open: 1.0,
        hover: (0..8).map(|i| if i == 1 { 1.0 } else { 0.0 }).collect(),
        main: "JPG".into(),
        sub: "1.1 MB".into(),
        main_accent: false,
    };
    render(&mut pm, &scene, dpi, fonts.as_ref());
    let mut img = image::RgbaImage::new(side, side);
    for (x, y, px) in img.enumerate_pixels_mut() {
        let t = (x + y) as f32 / (2.0 * side as f32);
        let bg = [(60.0 + 150.0 * t) as u32, (90.0 + 40.0 * (1.0 - t)) as u32, (170.0 - 60.0 * t) as u32];
        let s = pm.pixel(x, y).expect("in bounds");
        let inv = 255 - s.alpha() as u32;
        *px = image::Rgba([
            (s.red() as u32 + bg[0] * inv / 255) as u8,
            (s.green() as u32 + bg[1] * inv / 255) as u8,
            (s.blue() as u32 + bg[2] * inv / 255) as u8,
            255,
        ]);
    }
    img.save(path).expect("save preview");
}
