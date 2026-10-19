//! Software rendering of the radial menu into a premultiplied RGBA pixmap.
//!
//! Everything is data driven by a [`Theme`]: frosted glass sectors (a blurred snapshot of what is behind the
//! window, tinted and lit), layered soft shadows, rim light, vector icons, spring-driven motion values from the
//! caller and a hub that shows what is under the cursor. No bitmaps are bundled; the only external input is the
//! system font, read from the Windows fonts folder.

use ab_glyph::{point, Font, FontVec, PxScale, ScaleFont, VariableFont};
use std::collections::HashMap;
use std::f32::consts::{PI, TAU};
use std::sync::OnceLock;
use tiny_skia::{
    BlendMode, Color, FillRule, FilterQuality, GradientStop, LineCap, LineJoin, LinearGradient, Paint, Path,
    PathBuilder, Pattern, Pixmap, PixmapPaint, Point, RadialGradient, SpreadMode, Stroke, Transform,
};

use crate::config::Config;
use crate::geometry::{clamp_corner, corner_center_angle, sector_angle, Metrics};
use crate::theme::{self, FontKind, Rgba, Theme};

// ---------------------------------------------------------------------------------------------------------------
// Public model
// ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Icon {
    Image,
    Photo,
    Doc,
    Text,
    Pdf,
    Globe,
    Film,
    Layers,
    Grid,
    Sparkle,
    Live,
    Compress,
    Info,
    Pencil,
    Note,
    Frame,
    Crop,
    Redact,
    Folder,
}

impl Icon {
    /// Picks the icon for a sector label (case-insensitive); unknown labels get a folder.
    pub fn for_label(label: &str) -> Icon {
        match label.to_ascii_uppercase().as_str() {
            "PNG" => Icon::Image,
            "JPG" | "JPEG" => Icon::Photo,
            "WEBP" => Icon::Globe,
            "GIF" => Icon::Film,
            "TIFF" | "TIF" => Icon::Layers,
            "BMP" => Icon::Grid,
            "AVIF" => Icon::Sparkle,
            "HEIC" => Icon::Live,
            "PDF" => Icon::Pdf,
            "DOCX" | "DOC" => Icon::Doc,
            "TXT" => Icon::Text,
            "COMPRESS" => Icon::Compress,
            "METADATA" => Icon::Info,
            "EDIT" => Icon::Pencil,
            "ANNOTATE" => Icon::Note,
            "ADD BG" => Icon::Frame,
            "CROP" => Icon::Crop,
            "REDACT" => Icon::Redact,
            _ => Icon::Folder,
        }
    }
}

pub struct Slot {
    pub label: &'static str,
    pub enabled: bool,
    pub icon: Icon,
}

pub struct Scene {
    pub slots: Vec<Slot>,
    /// Global 0..1 fade and zoom (also used, reversed, to fade out).
    pub open: f32,
    /// Per-slot entrance spring, 0..~1.1 (overshoots).
    pub appear: Vec<f32>,
    /// Per-slot hover spring 0..~1.1.
    pub hover: Vec<f32>,
    pub main: String,
    pub sub: String,
    /// True when `main` shows the hovered action (drawn in the accent colour).
    pub main_accent: bool,
    /// Drop confirmation: 0 = idle, 0..1 expanding ring, centred on the hub.
    pub pulse: f32,
    pub confirm: Option<usize>,
    /// Text of the pill under the ring: the key hint, or why the hovered sector is greyed out.
    pub caption: String,
    /// `(current, total)` rings, drawn as dots in the pill; `None` when the file has a single ring.
    pub pager: Option<(usize, usize)>,
    /// Progress 0..1 of resting on the hub to flip rings, drawn as an arc around the hub.
    pub dwell: f32,
}

impl Scene {
    pub fn new(slots: Vec<Slot>, main: String, sub: String) -> Scene {
        let n = slots.len();
        Scene {
            slots,
            open: 0.0,
            appear: vec![0.0; n],
            hover: vec![0.0; n],
            main,
            sub,
            main_accent: false,
            pulse: 0.0,
            confirm: None,
            caption: String::new(),
            pager: None,
            dwell: 0.0,
        }
    }

    /// A fully opened scene, as used by the previews.
    pub fn settled(mut self) -> Scene {
        self.open = 1.0;
        self.appear.iter_mut().for_each(|a| *a = 1.0);
        self
    }
}

/// Everything the renderer needs to know about the chosen look.
#[derive(Clone)]
pub struct Look {
    pub theme: Theme,
    pub m: Metrics,
    /// The user's size setting; text scales with it.
    pub scale: f32,
    pub glass: f32,
}

impl Look {
    pub fn new(cfg: &Config, user_themes: &[Theme]) -> Look {
        let base = theme::find(&cfg.theme, user_themes);
        let theme = cfg.resolve(&base);
        Look { m: Metrics::new(cfg.size, theme.ring), theme, scale: cfg.size, glass: cfg.glass }
    }

    /// Window side in device pixels.
    pub fn side_px(&self, dpi: f32) -> u32 {
        (self.m.size * dpi).ceil() as u32
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Small helpers
// ---------------------------------------------------------------------------------------------------------------

pub fn clamp01(v: f32) -> f32 {
    v.clamp(0.0, 1.0)
}

pub fn col(c: Rgba, k: f32) -> Color {
    Color::from_rgba8(c.0, c.1, c.2, (clamp01(c.3 as f32 / 255.0 * k) * 255.0).round() as u8)
}

pub fn solid(c: Rgba, k: f32) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(col(c, k));
    p.anti_alias = true;
    p
}

fn stops(list: &[(f32, Rgba, f32)]) -> Vec<GradientStop> {
    list.iter().map(|&(t, c, k)| GradientStop::new(t, col(c, k))).collect()
}

pub fn linear(a: (f32, f32), b: (f32, f32), list: &[(f32, Rgba, f32)]) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    if let Some(sh) = LinearGradient::new(
        Point::from_xy(a.0, a.1),
        Point::from_xy(b.0, b.1),
        stops(list),
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        p.shader = sh;
    }
    p
}

pub fn radial(c: (f32, f32), focus: (f32, f32), r: f32, list: &[(f32, Rgba, f32)]) -> Paint<'static> {
    let mut p = Paint::default();
    p.anti_alias = true;
    if let Some(sh) = RadialGradient::new(
        Point::from_xy(focus.0, focus.1),
        Point::from_xy(c.0, c.1),
        r.max(0.5),
        stops(list),
        SpreadMode::Pad,
        Transform::identity(),
    ) {
        p.shader = sh;
    }
    p
}

/// One-pixel line: handled by tiny-skia's hairline renderer, which is much cheaper than a general stroke.
const HAIR: Stroke =
    Stroke { width: 1.0, miter_limit: 4.0, line_cap: LineCap::Butt, line_join: LineJoin::Miter, dash: None };

pub fn stroke(width: f32) -> Stroke {
    Stroke { width, line_cap: LineCap::Round, line_join: LineJoin::Round, ..Stroke::default() }
}

pub fn ease_out_cubic(t: f32) -> f32 {
    1.0 - (1.0 - clamp01(t)).powi(3)
}

// ---------------------------------------------------------------------------------------------------------------
// Shapes
// ---------------------------------------------------------------------------------------------------------------

/// Logical -> device mapping: centre in device pixels and device pixels per logical pixel.
#[derive(Clone, Copy)]
struct Geo {
    cx: f32,
    cy: f32,
    u: f32,
}

impl Geo {
    fn polar(&self, r: f32, a: f32) -> (f32, f32) {
        (self.cx + r * self.u * a.sin(), self.cy - r * self.u * a.cos())
    }
}

/// Cubic Bezier approximation of the arc `p(t) = (cx + r sin t, cy + r cos t)` from `t0` to `t1` in sector-local
/// coordinates (x clockwise-perpendicular, y outwards), mapped through `f`. The current point must be `p(t0)`.
fn arc(pb: &mut PathBuilder, f: &impl Fn(f32, f32) -> (f32, f32), c: (f32, f32), r: f32, t0: f32, t1: f32) {
    let n = ((t1 - t0).abs() / (PI / 2.0)).ceil().max(1.0) as usize;
    let d = (t1 - t0) / n as f32;
    let k = 4.0 / 3.0 * (d / 4.0).tan();
    let p = |t: f32| (c.0 + r * t.sin(), c.1 + r * t.cos());
    let dp = |t: f32| (r * t.cos(), -r * t.sin());
    for i in 0..n {
        let (ta, tb) = (t0 + d * i as f32, t0 + d * (i + 1) as f32);
        let (a, b) = (p(ta), p(tb));
        let (da, db) = (dp(ta), dp(tb));
        let c1 = f(a.0 + k * da.0, a.1 + k * da.1);
        let c2 = f(b.0 - k * db.0, b.1 - k * db.1);
        let e = f(b.0, b.1);
        pb.cubic_to(c1.0, c1.1, c2.0, c2.1, e.0, e.1);
    }
}

/// Annular sector with rounded corners and parallel gaps, centred on bisector angle `a` (clockwise from up).
/// Radii, gap and corner are logical; `half` is the half angle of the sector.
fn sector_path(g: Geo, a: f32, half: f32, half_gap: f32, r_in: f32, r_out: f32, corner: f32) -> Option<Path> {
    let c = clamp_corner(corner, half_gap, r_in, r_out, half).max(0.4);
    let (sa, ca) = a.sin_cos();
    let f = |x: f32, y: f32| (g.cx + g.u * (y * sa + x * ca), g.cy + g.u * (-y * ca + x * sa));
    let local = |rho: f32, t: f32| (rho * t.sin(), rho * t.cos());
    let (nl, nr) = ((half.cos(), half.sin()), (-half.cos(), half.sin()));

    let t_lo = corner_center_angle(r_out, c, half_gap, half, true);
    let t_li = corner_center_angle(r_in, c, half_gap, half, false);
    let (t_ro, t_ri) = (-t_lo, -t_li);
    let (c_lo, c_li) = (local(r_out - c, t_lo), local(r_in + c, t_li));
    let (c_ro, c_ri) = (local(r_out - c, t_ro), local(r_in + c, t_ri));
    let psi_l = (-half.cos()).atan2(-half.sin()); // edge touch point, left corners
    let psi_r = half.cos().atan2(-half.sin()); // right corners
    let edge = |cc: (f32, f32), n: (f32, f32)| (cc.0 - c * n.0, cc.1 - c * n.1);

    let mut pb = PathBuilder::new();
    let s = edge(c_li, nl);
    let s = f(s.0, s.1);
    pb.move_to(s.0, s.1);
    let e = edge(c_lo, nl);
    let e = f(e.0, e.1);
    pb.line_to(e.0, e.1);
    arc(&mut pb, &f, c_lo, c, psi_l, t_lo);
    arc(&mut pb, &f, (0.0, 0.0), r_out, t_lo, t_ro);
    arc(&mut pb, &f, c_ro, c, t_ro, psi_r);
    let e = edge(c_ri, nr);
    let e = f(e.0, e.1);
    pb.line_to(e.0, e.1);
    arc(&mut pb, &f, c_ri, c, psi_r, t_ri + PI);
    arc(&mut pb, &f, (0.0, 0.0), r_in, t_ri, t_li);
    arc(&mut pb, &f, c_li, c, t_li + PI, psi_l + TAU);
    pb.close();
    pb.finish()
}

/// Pill shape (device pixels).
fn capsule(x: f32, y: f32, w: f32, h: f32) -> Option<Path> {
    let r = (h / 2.0).min(w / 2.0);
    let k = r * 0.5523;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.cubic_to(x + w, y + r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + r + k, x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

fn circle_path(g: Geo, r: f32) -> Option<Path> {
    PathBuilder::from_circle(g.cx, g.cy, r * g.u)
}

/// Arc of a circle around the menu centre, from angle `a0` to `a1` (clockwise from up).
fn ring_arc(g: Geo, r: f32, a0: f32, a1: f32) -> Option<Path> {
    let f = |x: f32, y: f32| (g.cx + g.u * x, g.cy - g.u * y);
    // Reuse the local arc: p(t) = (r sin t, r cos t) with y up, which is exactly "clockwise from up".
    let mut pb = PathBuilder::new();
    let s = f(r * a0.sin(), r * a0.cos());
    pb.move_to(s.0, s.1);
    arc(&mut pb, &f, (0.0, 0.0), r, a0, a1);
    pb.finish()
}

// ---------------------------------------------------------------------------------------------------------------
// Icons: stroked vector paths on a 24x24 grid, written in a tiny path language.
//   M x y | L x y | Q cx cy x y | Z | O cx cy r (circle) | R x y w h r (rounded rectangle)
// ---------------------------------------------------------------------------------------------------------------

const DOC: &str = "M7 3 L14 3 L19 8 L19 19 Q19 21 17 21 L7 21 Q5 21 5 19 L5 5 Q5 3 7 3 Z M14 3 L14 8 L19 8";

fn icon_source(i: Icon) -> String {
    let s = match i {
        Icon::Image => "R 3 4 18 16 3 O 9 9.5 1.7 M3.5 17 L9 12.5 L13 16 L16 13.5 L20.5 17.5",
        Icon::Photo => "R 3 7 18 13 3 M8.5 7 L10 4.5 L14 4.5 L15.5 7 O 12 13.5 3.2",
        Icon::Doc => return format!("{DOC} M9 13 L15 13 M9 17 L13 17"),
        Icon::Text => "M5 6 L19 6 M5 10.5 L19 10.5 M5 15 L19 15 M5 19.5 L13 19.5",
        Icon::Pdf => return format!("{DOC} M8.5 17 Q10 11 11.5 14.5 Q13 18 15.5 13"),
        Icon::Globe => "O 12 12 9 M3 12 L21 12 M12 3 Q6.5 12 12 21 Q17.5 12 12 3",
        Icon::Film => "R 3 5 18 14 3 M10 9.5 L15 12 L10 14.5 Z",
        Icon::Layers => "M12 3 L21 8 L12 13 L3 8 Z M3 12.5 L12 17.5 L21 12.5 M3 16.8 L12 21.8 L21 16.8",
        Icon::Grid => "R 3.5 3.5 17 17 2.5 M3.5 9.2 L20.5 9.2 M3.5 14.8 L20.5 14.8 M9.2 3.5 L9.2 20.5 M14.8 3.5 L14.8 20.5",
        Icon::Sparkle => "M12 3 Q12.8 11.2 21 12 Q12.8 12.8 12 21 Q11.2 12.8 3 12 Q11.2 11.2 12 3 Z",
        Icon::Live => "O 12 12 9 O 12 12 5 O 12 12 0.9",
        Icon::Compress => "M20 4 L14.5 9.5 M14.5 5.5 L14.5 9.5 L18.5 9.5 M4 20 L9.5 14.5 M9.5 18.5 L9.5 14.5 L5.5 14.5",
        Icon::Info => "O 12 12 9 M12 11 L12 16.5 M12 7.6 L12 7.7",
        Icon::Pencil => "M4.5 19.5 L5.3 15.3 L16 4.6 Q17.2 3.5 18.4 4.6 L19.4 5.6 Q20.5 6.8 19.4 8 L8.7 18.7 Z M14 6.7 L17.3 10",
        Icon::Note => "M5 4.5 L19 4.5 Q20 4.5 20 5.5 L20 15 Q20 16 19 16 L10 16 L6.5 19.5 L6.5 16 L5 16 Q4 16 4 15 L4 5.5 Q4 4.5 5 4.5 Z M8 9 L16 9 M8 12 L13 12",
        Icon::Frame => "R 3 3 18 18 5 R 8 8 8 8 2",
        Icon::Crop => "M6 2.5 L6 15.5 Q6 18 8.5 18 L21.5 18 M2.5 6 L15.5 6 Q18 6 18 8.5 L18 21.5",
        Icon::Redact => "M4 6.5 L20 6.5 M4 17.5 L20 17.5 R 3.5 10.2 17 3.6 1.4",
        Icon::Folder => {
            "M3 7 Q3 5.5 4.5 5.5 L9 5.5 L11 7.5 L19.5 7.5 Q21 7.5 21 9 L21 17.5 Q21 19 19.5 19 L4.5 19 Q3 19 3 17.5 Z"
        }
    };
    s.to_string()
}

const ICONS: [Icon; 19] = [
    Icon::Image,
    Icon::Photo,
    Icon::Doc,
    Icon::Text,
    Icon::Pdf,
    Icon::Globe,
    Icon::Film,
    Icon::Layers,
    Icon::Grid,
    Icon::Sparkle,
    Icon::Live,
    Icon::Compress,
    Icon::Info,
    Icon::Pencil,
    Icon::Note,
    Icon::Frame,
    Icon::Crop,
    Icon::Redact,
    Icon::Folder,
];

fn parse_icon(src: &str) -> Option<Path> {
    let mut pb = PathBuilder::new();
    // "M3.5 17" -> "M 3.5 17": command letters are always their own token.
    let spaced: String =
        src.chars().flat_map(|c| if c.is_ascii_alphabetic() { vec![' ', c, ' '] } else { vec![c] }).collect();
    let mut it = spaced.split_whitespace();
    let num = |it: &mut std::str::SplitWhitespace| it.next().and_then(|t| t.parse::<f32>().ok());
    while let Some(cmd) = it.next() {
        match cmd {
            "M" => pb.move_to(num(&mut it)?, num(&mut it)?),
            "L" => pb.line_to(num(&mut it)?, num(&mut it)?),
            "Q" => pb.quad_to(num(&mut it)?, num(&mut it)?, num(&mut it)?, num(&mut it)?),
            "Z" => pb.close(),
            "O" => {
                let (x, y, r) = (num(&mut it)?, num(&mut it)?, num(&mut it)?);
                pb.push_circle(x, y, r);
            }
            "R" => {
                let (x, y, w, h, r) = (num(&mut it)?, num(&mut it)?, num(&mut it)?, num(&mut it)?, num(&mut it)?);
                pb.move_to(x + r, y);
                pb.line_to(x + w - r, y);
                pb.quad_to(x + w, y, x + w, y + r);
                pb.line_to(x + w, y + h - r);
                pb.quad_to(x + w, y + h, x + w - r, y + h);
                pb.line_to(x + r, y + h);
                pb.quad_to(x, y + h, x, y + h - r);
                pb.line_to(x, y + r);
                pb.quad_to(x, y, x + r, y);
                pb.close();
            }
            _ => return None,
        }
    }
    pb.finish()
}

fn icon_path(i: Icon) -> Option<&'static Path> {
    static CACHE: OnceLock<Vec<Option<Path>>> = OnceLock::new();
    let all = CACHE.get_or_init(|| ICONS.iter().map(|&i| parse_icon(&icon_source(i))).collect());
    ICONS.iter().position(|&x| x == i).and_then(|k| all[k].as_ref())
}

// ---------------------------------------------------------------------------------------------------------------
// Text
// ---------------------------------------------------------------------------------------------------------------

struct Glyph {
    ox: i32,
    oy: i32,
    w: usize,
    h: usize,
    cov: Vec<u8>,
}

/// A loaded font plus a cache of rasterised glyphs, so a frame only blits coverage bitmaps.
pub struct Face {
    font: FontVec,
    cache: HashMap<(char, u32), Glyph>,
}

impl Face {
    /// Loads the font for `kind` from the Windows fonts folder (nothing is bundled) and applies the weight axis.
    pub fn load(kind: FontKind, weight: f32) -> Option<Face> {
        let dir = std::path::PathBuf::from(std::env::var_os("WINDIR")?).join("Fonts");
        let bold = weight >= 600.0;
        let candidates: &[&str] = match kind {
            FontKind::Variable => &["SegUIVar.ttf", "seguisb.ttf", "segoeui.ttf"],
            FontKind::Bahnschrift => &["bahnschrift.ttf", "seguisb.ttf"],
            FontKind::Consolas if bold => &["consolab.ttf", "consola.ttf"],
            FontKind::Consolas => &["consola.ttf", "cour.ttf"],
            FontKind::Georgia if bold => &["georgiab.ttf", "georgia.ttf"],
            FontKind::Georgia => &["georgia.ttf", "times.ttf"],
        };
        let mut font = candidates
            .iter()
            .chain(["segoeui.ttf", "arial.ttf"].iter())
            .find_map(|f| std::fs::read(dir.join(f)).ok())
            .and_then(|b| FontVec::try_from_vec(b).ok())?;
        // Variable fonts: weight and optical size. Static fonts simply refuse these calls.
        font.set_variation(b"wght", weight.clamp(100.0, 900.0));
        font.set_variation(b"opsz", 14.0);
        Some(Face { font, cache: HashMap::new() })
    }

    fn advance(&self, a: char, b: Option<char>, px: f32) -> f32 {
        let s = self.font.as_scaled(PxScale::from(px));
        let ia = s.glyph_id(a);
        s.h_advance(ia) + b.map_or(0.0, |b| s.kern(ia, s.glyph_id(b)))
    }

    /// Draws `text` starting at `x` (left aligned).
    #[allow(clippy::too_many_arguments)]
    pub fn draw_left(&mut self, pm: &mut Pixmap, text: &str, x: f32, cy: f32, px: f32, c: Rgba, k: f32, tracking: f32) {
        let w = self.measure(text, px, tracking);
        self.draw(pm, text, x + w / 2.0, cy, px, c, k, tracking);
    }

    /// Width of `text` in pixels with `tracking` extra pixels between letters.
    pub fn measure(&self, text: &str, px: f32, tracking: f32) -> f32 {
        let chars: Vec<char> = text.chars().collect();
        let n = chars.len();
        let adv: f32 = (0..n).map(|i| self.advance(chars[i], chars.get(i + 1).copied(), px)).sum();
        adv + tracking * n.saturating_sub(1) as f32
    }

    fn glyph(&mut self, ch: char, px: f32) -> &Glyph {
        let key = (ch, (px * 4.0).round() as u32);
        let font = &self.font;
        self.cache.entry(key).or_insert_with(|| {
            let g = font.glyph_id(ch).with_scale_and_position(PxScale::from(px), point(0.0, 0.0));
            match font.outline_glyph(g) {
                Some(o) => {
                    let b = o.px_bounds();
                    let (w, h) = (b.width().ceil() as usize + 1, b.height().ceil() as usize + 1);
                    let mut cov = vec![0u8; w * h];
                    o.draw(|x, y, c| {
                        let i = y as usize * w + x as usize;
                        if i < cov.len() {
                            cov[i] = (c * 255.0).round() as u8;
                        }
                    });
                    Glyph { ox: b.min.x.floor() as i32, oy: b.min.y.floor() as i32, w, h, cov }
                }
                None => Glyph { ox: 0, oy: 0, w: 0, h: 0, cov: vec![] },
            }
        })
    }

    /// Draws `text` horizontally centred on `cx` with its x-height middle at `cy`.
    #[allow(clippy::too_many_arguments)]
    pub fn draw(&mut self, pm: &mut Pixmap, text: &str, cx: f32, cy: f32, px: f32, c: Rgba, k: f32, tracking: f32) {
        if k <= 0.003 || text.is_empty() {
            return;
        }
        let width = self.measure(text, px, tracking);
        let scaled = self.font.as_scaled(PxScale::from(px));
        let base = (cy + (scaled.ascent() + scaled.descent()) / 2.0).round();
        let mut x = cx - width / 2.0;
        let chars: Vec<char> = text.chars().collect();
        let (pw, ph) = (pm.width() as i32, pm.height() as i32);
        let a_all = clamp01(c.3 as f32 / 255.0 * k);
        for (i, &ch) in chars.iter().enumerate() {
            let adv = self.advance(ch, chars.get(i + 1).copied(), px);
            let (gx, gy) = (x.round() as i32, base as i32);
            let g = self.glyph(ch, px);
            let data = pm.data_mut();
            for row in 0..g.h {
                let py = gy + g.oy + row as i32;
                if py < 0 || py >= ph {
                    continue;
                }
                for colm in 0..g.w {
                    let cov = g.cov[row * g.w + colm];
                    if cov == 0 {
                        continue;
                    }
                    let pxx = gx + g.ox + colm as i32;
                    if pxx < 0 || pxx >= pw {
                        continue;
                    }
                    let a = cov as f32 / 255.0 * a_all;
                    let inv = 1.0 - a;
                    let o = ((py * pw + pxx) * 4) as usize;
                    data[o] = (c.0 as f32 * a + data[o] as f32 * inv) as u8;
                    data[o + 1] = (c.1 as f32 * a + data[o + 1] as f32 * inv) as u8;
                    data[o + 2] = (c.2 as f32 * a + data[o + 2] as f32 * inv) as u8;
                    data[o + 3] = (255.0 * a + data[o + 3] as f32 * inv) as u8;
                }
            }
            x += adv + tracking;
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Backdrop (frosted glass) and blur helpers
// ---------------------------------------------------------------------------------------------------------------

/// Downscale factor of the blurred backdrop and the shadow layers.
const LOW: u32 = 4;

/// Separable box blur, edge-clamped, `ch` interleaved channels.
fn box_blur(buf: &mut [u8], w: usize, h: usize, ch: usize, radius: usize) {
    if radius == 0 || w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0u8; buf.len()];
    let win = (2 * radius + 1) as u32;
    // Horizontal then vertical, via a transposing pass to reuse one routine.
    let pass = |src: &[u8], dst: &mut [u8], len: usize, lines: usize, stride_len: usize, stride_line: usize| {
        for l in 0..lines {
            for c in 0..ch {
                let at = |i: isize| -> u32 {
                    let i = i.clamp(0, len as isize - 1) as usize;
                    src[l * stride_line + i * stride_len + c] as u32
                };
                let mut sum: u32 = (0..=2 * radius as isize).map(|k| at(k - radius as isize)).sum();
                for i in 0..len {
                    dst[l * stride_line + i * stride_len + c] = ((sum + win / 2) / win) as u8;
                    sum = sum + at((i + radius + 1) as isize) - at(i as isize - radius as isize);
                }
            }
        }
    };
    pass(buf, &mut tmp, w, h, ch, w * ch);
    pass(&tmp, buf, h, w, w * ch, ch);
}

fn blur3(buf: &mut [u8], w: usize, h: usize, ch: usize, radius: f32) {
    // Three box passes approximate a Gaussian whose sigma is about `radius / 2`.
    let b = (radius * 0.55).round().max(0.0) as usize;
    for _ in 0..3 {
        box_blur(buf, w, h, ch, b);
    }
}

/// A blurred, saturated snapshot of what is behind the menu, at 1/[`LOW`] resolution.
pub struct Backdrop {
    pm: Pixmap,
}

impl Backdrop {
    /// Builds a backdrop from `w`x`h` device pixels. `bgra` selects the channel order of `src`.
    pub fn new(src: &[u8], w: u32, h: u32, bgra: bool, blur_dev: f32, saturation: f32) -> Option<Backdrop> {
        if src.len() < (w * h * 4) as usize || w == 0 || h == 0 {
            return None;
        }
        let (lw, lh) = (w.div_ceil(LOW), h.div_ceil(LOW));
        let mut pm = Pixmap::new(lw, lh)?;
        {
            let out = pm.data_mut();
            for ly in 0..lh {
                for lx in 0..lw {
                    let (mut r, mut g, mut b, mut n) = (0u32, 0u32, 0u32, 0u32);
                    for y in ly * LOW..((ly + 1) * LOW).min(h) {
                        for x in lx * LOW..((lx + 1) * LOW).min(w) {
                            let i = ((y * w + x) * 4) as usize;
                            let (c0, c2) = if bgra { (src[i + 2], src[i]) } else { (src[i], src[i + 2]) };
                            r += c0 as u32;
                            g += src[i + 1] as u32;
                            b += c2 as u32;
                            n += 1;
                        }
                    }
                    let o = ((ly * lw + lx) * 4) as usize;
                    out[o] = (r / n) as u8;
                    out[o + 1] = (g / n) as u8;
                    out[o + 2] = (b / n) as u8;
                    out[o + 3] = 255;
                }
            }
            blur3(out, lw as usize, lh as usize, 4, blur_dev / LOW as f32);
            if (saturation - 1.0).abs() > 0.01 {
                for px in out.chunks_exact_mut(4) {
                    let l = 0.2126 * px[0] as f32 + 0.7152 * px[1] as f32 + 0.0722 * px[2] as f32;
                    for c in &mut px[..3] {
                        *c = (l + (*c as f32 - l) * saturation).clamp(0.0, 255.0) as u8;
                    }
                }
            }
            for px in out.chunks_exact_mut(4) {
                px[3] = 255;
            }
        }
        Some(Backdrop { pm })
    }
}

/// Bilinear upscale of a premultiplied RGBA pixmap, edge clamped. `LOW` device pixels per source pixel.
fn upscale(low: &Pixmap, w: u32, h: u32) -> Option<Pixmap> {
    let mut out = Pixmap::new(w, h)?;
    let (lw, lh) = (low.width() as usize, low.height() as usize);
    let (w, h) = (w as usize, h as usize);
    let src = low.data();
    let pos = |d: usize, max: usize| -> (usize, usize, u32) {
        let u = ((d as f32 + 0.5) / LOW as f32 - 0.5).clamp(0.0, (max - 1) as f32);
        let i0 = u.floor() as usize;
        (i0, (i0 + 1).min(max - 1), ((u - i0 as f32) * 256.0) as u32)
    };
    let xs: Vec<(usize, usize, u32)> = (0..w).map(|x| pos(x, lw)).collect();
    // Horizontal pass: every source row stretched to full width.
    let mut rows = vec![0u8; lh * w * 4];
    for ly in 0..lh {
        let srow = &src[ly * lw * 4..(ly + 1) * lw * 4];
        let drow = &mut rows[ly * w * 4..(ly + 1) * w * 4];
        for (x, &(a, b, t)) in xs.iter().enumerate() {
            for c in 0..4 {
                drow[x * 4 + c] = ((srow[a * 4 + c] as u32 * (256 - t) + srow[b * 4 + c] as u32 * t) >> 8) as u8;
            }
        }
    }
    // Vertical pass.
    let dst = out.data_mut();
    for y in 0..h {
        let (a, b, t) = pos(y, lh);
        let (ra, rb) = (&rows[a * w * 4..(a + 1) * w * 4], &rows[b * w * 4..(b + 1) * w * 4]);
        for ((d, &p), &q) in dst[y * w * 4..(y + 1) * w * 4].iter_mut().zip(ra).zip(rb) {
            *d = ((p as u32 * (256 - t) + q as u32 * t) >> 8) as u8;
        }
    }
    Some(out)
}

/// Layers that only change with the look, size or backdrop, rendered once and reused by every frame.
struct Baked {
    side: u32,
    /// Frosted backdrop with tint, sheen and light baked in (device pixels).
    glass: Pixmap,
    /// Soft shadows of the resting menu (device pixels).
    shadow: Pixmap,
}

/// Rest-pose geometry of sector `i` of `n`.
fn rest_sector(g: Geo, look: &Look, i: usize, n: usize) -> Option<Path> {
    let th = &look.theme;
    let half = PI / n as f32;
    sector_path(
        g,
        sector_angle(i, n),
        half,
        th.gap / 2.0 * look.scale,
        look.m.r_in,
        look.m.r_out,
        th.corner * look.scale,
    )
}

fn bake(look: &Look, dpi: f32, side: u32, bd: Option<&Backdrop>, n: usize) -> Option<Baked> {
    let th = &look.theme;
    let m = look.m;
    let (lw, lh) = (side.div_ceil(LOW), side.div_ceil(LOW));
    let lowk = dpi / LOW as f32;
    let mid = side as f32 / 2.0 / LOW as f32;
    let glow = Geo { cx: mid, cy: mid, u: lowk };

    // ---- Glass: backdrop, radial tint, sheen, light direction, hub tint.
    let mut gl = Pixmap::new(lw, lh)?;
    let frost = if bd.is_some() { clamp01(look.glass) } else { 0.0 };
    if let Some(bd) = bd.filter(|_| frost > 0.01) {
        let p = PixmapPaint { opacity: frost, blend_mode: BlendMode::Source, quality: FilterQuality::Nearest };
        gl.draw_pixmap(0, 0, bd.pm.as_ref(), &p, Transform::identity(), None);
    }
    let boost = 1.0 + (1.0 - frost) * 0.7;
    let r_big = m.r_out * 1.02;
    let (t_hub, t_in, t_out) = ((m.hub * 1.2) / r_big, m.r_in / r_big, m.r_out / r_big);
    let c = (mid, mid);
    let tint = radial(
        c,
        c,
        r_big * lowk,
        &[
            (0.0, th.glass_bottom, 0.0),
            (t_hub, th.glass_bottom, 0.0),
            (t_in, th.glass_bottom, boost),
            (t_out, th.glass_top, boost),
        ],
    );
    let full = tiny_skia::Rect::from_xywh(0.0, 0.0, lw as f32, lh as f32)?;
    gl.fill_rect(full, &tint, Transform::identity(), None);
    let white = Rgba(255, 255, 255, 255);
    if th.sheen > 0.0 {
        let t_mid = t_out - 0.45 * (t_out - t_in);
        let sheen = radial(
            c,
            c,
            r_big * lowk,
            &[(0.0, white, 0.0), (t_in, white, 0.0), (t_mid, white, 0.03 * th.sheen), (t_out, white, 0.20 * th.sheen)],
        );
        gl.fill_rect(full, &sheen, Transform::identity(), None);
        let (sw, sh) = (lw as f32, lh as f32);
        let glint = linear(
            (sw * 0.15, sh * 0.02),
            (sw * 0.85, sh * 0.98),
            &[(0.0, white, 0.16 * th.sheen), (0.45, white, 0.0), (1.0, Rgba(0, 0, 0, 255), 0.14)],
        );
        gl.fill_rect(full, &glint, Transform::identity(), None);
    }
    if let Some(hub) = PathBuilder::from_circle(mid, mid, m.hub * 1.16 * lowk) {
        let p = linear(
            (mid, mid - m.hub * lowk),
            (mid, mid + m.hub * lowk),
            &[(0.0, th.hub.mix(white, 0.10), boost), (1.0, th.hub, boost)],
        );
        gl.fill_path(&hub, &p, FillRule::Winding, Transform::identity(), None);
    }
    let mut glass = upscale(&gl, side, side)?;
    if th.scan > 0.0 {
        // Scanlines: darken every few device rows.
        let period = ((3.0 * dpi).round() as usize).max(2);
        let keep = (1.0 - 0.34 * th.scan) * 256.0;
        let row = side as usize * 4;
        for (y, line) in glass.data_mut().chunks_exact_mut(row).enumerate() {
            if y % period == 0 {
                for px in line.chunks_exact_mut(4) {
                    for c in &mut px[..3] {
                        *c = ((*c as f32 * keep) as u32 >> 8) as u8;
                    }
                }
            }
        }
    }

    // ---- Shadows: blurred silhouettes of the resting menu, ambient plus key light.
    let mut acc = Pixmap::new(lw, lh)?;
    let mut sil = Pixmap::new(lw, lh)?;
    let mut paths: Vec<Path> = (0..n).filter_map(|i| rest_sector(glow, look, i, n)).collect();
    paths.extend(PathBuilder::from_circle(mid, mid, m.hub * lowk));
    let spread = th.shadow_spread * look.scale;
    for (color, blur, dy) in
        [(th.shadow.fade(0.55), spread * 1.5, th.shadow_dy * 1.5), (th.shadow, spread * 0.55, th.shadow_dy * 0.6)]
    {
        sil.fill(Color::TRANSPARENT);
        let mut p = Paint::default();
        p.set_color(Color::BLACK);
        p.anti_alias = true;
        let tf = Transform::from_translate(0.0, dy * look.scale * lowk);
        for path in &paths {
            sil.fill_path(path, &p, FillRule::Winding, tf, None);
        }
        let mut alpha: Vec<u8> = sil.data().chunks_exact(4).map(|px| px[3]).collect();
        blur3(&mut alpha, lw as usize, lh as usize, 1, blur * lowk);
        let mut layer = Pixmap::new(lw, lh)?;
        let (r, g, b, ca) = (color.0 as u32, color.1 as u32, color.2 as u32, color.3 as u32);
        for (d, &a) in layer.data_mut().chunks_exact_mut(4).zip(&alpha) {
            let a = a as u32 * ca / 255;
            d.copy_from_slice(&[(r * a / 255) as u8, (g * a / 255) as u8, (b * a / 255) as u8, a as u8]);
        }
        acc.draw_pixmap(0, 0, layer.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }
    let shadow = upscale(&acc, side, side)?;
    Some(Baked { side, glass, shadow })
}

// ---------------------------------------------------------------------------------------------------------------
// The renderer
// ---------------------------------------------------------------------------------------------------------------

pub struct Renderer {
    pub face: Option<Face>,
    baked: Option<Baked>,
}

/// Source-blend paint that copies the baked glass through a path, with `alpha` opacity.
fn glass_paint(glass: &Pixmap, alpha: f32) -> Paint<'_> {
    let mut p = Paint::default();
    p.anti_alias = true;
    p.blend_mode = BlendMode::Source;
    p.shader =
        Pattern::new(glass.as_ref(), SpreadMode::Pad, FilterQuality::Nearest, clamp01(alpha), Transform::identity());
    p
}

impl Renderer {
    pub fn new(face: Option<Face>) -> Renderer {
        Renderer { face, baked: None }
    }

    /// Bakes the frosted glass and shadow layers. Call whenever the look, size or backdrop changed and before the
    /// first frame; `n` is the number of sectors of the menu that will be drawn.
    pub fn prepare(&mut self, look: &Look, dpi: f32, side: u32, bd: Option<&Backdrop>, n: usize) {
        self.baked = bake(look, dpi, side, bd, n);
    }

    pub fn render(&mut self, pm: &mut Pixmap, scene: &Scene, look: &Look, dpi: f32) {
        let side = pm.width() as f32;
        let n = scene.slots.len();
        let open = clamp01(scene.open);
        let m = look.m;
        let k = dpi;
        if n < 3 || open <= 0.0 {
            pm.fill(Color::TRANSPARENT);
            // Pending: an almost invisible disc so the window is still a drop target before the menu opens.
            let hit = Geo { cx: side / 2.0, cy: side / 2.0, u: k };
            if let Some(path) = circle_path(hit, m.hit_out) {
                pm.fill_path(&path, &solid(Rgba(0, 0, 0, 255), 0.012), FillRule::Winding, Transform::identity(), None);
            }
            return;
        }
        if !matches!(&self.baked, Some(b) if b.side == pm.width()) {
            self.baked = bake(look, dpi, pm.width(), None, n);
        }
        let Some(baked) = self.baked.as_ref() else { return };
        let zoom = 0.9 + 0.1 * ease_out_cubic(open);
        let g = Geo { cx: side / 2.0, cy: side / 2.0, u: k * zoom };
        let fade = open.powf(0.7);
        let th = &look.theme;
        let half = PI / n as f32;

        // Per-sector geometry for this frame.
        struct Sec {
            path: Path,
            /// The outline pulled in by 1.5 px, stroked as the inner highlight.
            glow: Option<Path>,
            a: f32,
            r_mid: f32,
            alpha: f32,
            h: f32,
        }
        let mut secs: Vec<Sec> = Vec::with_capacity(n);
        let mut appeared = 0.0;
        for i in 0..n {
            let e = scene.appear.get(i).copied().unwrap_or(1.0);
            appeared += clamp01(e);
            let h = clamp01(scene.hover.get(i).copied().unwrap_or(0.0));
            let pop = if scene.confirm == Some(i) { 1.0 - clamp01(scene.pulse) } else { 0.0 };
            let a = sector_angle(i, n) + (1.0 - e).clamp(-0.3, 1.0) * 0.32;
            let slide = (e - 1.0).min(0.12) * 34.0 - (1.0 - e).max(0.0) * 6.0;
            let lift = th.hover_lift * h + 6.0 * pop;
            let r_in = m.r_in + slide + lift;
            let r_out = m.r_out + slide + lift + th.hover_grow * h + 5.0 * pop;
            let hg = (th.gap / 2.0 - 0.6 * h).max(0.0) * look.scale;
            let corner = th.corner * look.scale;
            let alpha = fade * clamp01(e * 1.7);
            let Some(path) = sector_path(g, a, half, hg, r_in, r_out, corner) else { continue };
            let inset = |d: f32| sector_path(g, a, half, hg + d, r_in + d, r_out - d, (corner - d).max(0.4));
            let glow = inset(th.inset);
            secs.push(Sec { path, glow, a, r_mid: (r_in + r_out) / 2.0, alpha, h });
        }
        let hub_r = m.hub * (1.0 + 0.03 * scene.hover.iter().cloned().fold(0.0, f32::max));
        let hub_path = circle_path(g, hub_r);

        // 1. Shadows: the baked layer, faded in with the sectors.
        let sk = (fade * appeared / n as f32 * 256.0) as u32;
        for (d, &s) in pm.data_mut().iter_mut().zip(baked.shadow.data()) {
            *d = ((s as u32 * sk) >> 8) as u8;
        }
        // Hover glow under the sectors: a few wide translucent strokes.
        if th.glow > 0.0 {
            for s in secs.iter().filter(|s| s.h > 0.02) {
                for (w, ga) in [(22.0, 0.05), (13.0, 0.09), (6.0, 0.16)] {
                    let p = solid(th.accent, ga * th.glow * s.h * s.alpha * 1.4);
                    pm.stroke_path(&s.path, &p, &stroke(w * k * look.scale), Transform::identity(), None);
                }
            }
        }

        // 2. Orbit ring with ticks and hover arcs.
        if th.orbit > 0.0 {
            let r = m.r_out + 18.0 * look.scale;
            if let Some(p) = circle_path(g, r) {
                pm.stroke_path(&p, &solid(th.text, 0.16 * th.orbit * fade), &stroke(k), Transform::identity(), None);
            }
            if th.ticks {
                let mut pb = PathBuilder::new();
                for t in 0..72 {
                    let a = TAU * t as f32 / 72.0;
                    let len = if t % 6 == 0 { 6.0 } else { 3.0 };
                    let (p0, p1) = (g.polar(r + 2.0, a), g.polar(r + 2.0 + len * look.scale, a));
                    pb.move_to(p0.0, p0.1);
                    pb.line_to(p1.0, p1.1);
                }
                if let Some(p) = pb.finish() {
                    pm.stroke_path(
                        &p,
                        &solid(th.text, 0.28 * th.orbit * fade),
                        &stroke(k * 0.9),
                        Transform::identity(),
                        None,
                    );
                }
            }
            for s in secs.iter().filter(|s| s.h > 0.02) {
                if let Some(p) = ring_arc(g, r, s.a - half * 0.78, s.a + half * 0.78) {
                    let w = 3.2 * k * (0.5 + 0.5 * s.h);
                    pm.stroke_path(&p, &solid(th.accent, s.h.min(1.0) * fade), &stroke(w), Transform::identity(), None);
                }
            }
        }

        // 3. Sector bodies.
        let light_from = (side * 0.2, side * 0.05);
        let light_to = (side * 0.8, side * 0.95);
        let white = Rgba(255, 255, 255, 255);
        for (i, s) in secs.iter().enumerate() {
            let slot = &scene.slots[i];
            let h = s.h;
            let a_body = s.alpha * if slot.enabled { 1.0 } else { 0.72 };
            // Copies the baked glass through the sector shape, replacing the shadow underneath it.
            pm.fill_path(&s.path, &glass_paint(&baked.glass, a_body), FillRule::Winding, Transform::identity(), None);
            let outer = g.polar(s.r_mid + 60.0 * look.scale, s.a);
            let inner = g.polar(s.r_mid - 60.0 * look.scale, s.a);
            // Hover: accent body, bright from the outside.
            if h > 0.0 {
                let hf = th.hover_fill;
                let p = linear(
                    outer,
                    inner,
                    &[(0.0, hf, h * a_body * 0.95), (1.0, hf.mix(th.accent2, 0.55), h * a_body * 0.6)],
                );
                pm.fill_path(&s.path, &p, FillRule::Winding, Transform::identity(), None);
            }
            // Rim light and an inner highlight, both brighter towards the light.
            let rim_c = th.rim.mix(th.accent, h);
            let tinted = |c: Rgba| Rgba(c.0, c.1, c.2, th.rim.3.max(70));
            let (r0, r1) = (rim_c.mix(tinted(th.accent), th.rim_tint), rim_c.mix(tinted(th.accent2), th.rim_tint));
            let rim = linear(
                light_from,
                light_to,
                &[(0.0, r0.mix(white, 0.35 - 0.35 * h - 0.2 * th.rim_tint), 1.6 * a_body), (1.0, r1, 0.5 * a_body)],
            );
            // Width-1 strokes take tiny-skia's hairline fast path (the general stroker is several times slower).
            pm.stroke_path(&s.path, &rim, &HAIR, Transform::identity(), None);
            if let Some(band) = &s.glow {
                let hl = linear(
                    light_from,
                    light_to,
                    &[(0.0, th.highlight, 0.75 * a_body), (0.5, th.highlight, 0.12 * a_body), (1.0, th.highlight, 0.0)],
                );
                pm.stroke_path(band, &hl, &HAIR, Transform::identity(), None);
            }

            // Icon and label.
            let r_mid_pt = g.polar(s.r_mid, s.a);
            let txt = th.text.mix(th.hover_text, h);
            let ta = a_body * if slot.enabled { 1.0 } else { 0.38 };
            let (ix, iy) = (r_mid_pt.0, r_mid_pt.1 - 11.0 * k * look.scale);
            let isz = (22.0 + 3.0 * h) * look.scale * k;
            if let Some(path) = icon_path(slot.icon) {
                let sc = isz / 24.0;
                let tf = Transform::from_row(sc, 0.0, 0.0, sc, ix - isz / 2.0, iy - isz / 2.0);
                pm.stroke_path(path, &solid(txt, ta), &stroke(1.6), tf, None);
            }
            if let Some(face) = self.face.as_mut() {
                let label = if th.caps { slot.label.to_uppercase() } else { slot.label.to_string() };
                let px = 11.0 * look.scale * k;
                let ly = r_mid_pt.1 + 15.5 * k * look.scale;
                face.draw(pm, &label, r_mid_pt.0, ly, px, txt, ta, th.tracking * px);
            }
        }

        // 4. Hub.
        if let Some(hp) = hub_path {
            pm.fill_path(&hp, &glass_paint(&baked.glass, fade), FillRule::Winding, Transform::identity(), None);
            let rim =
                linear(light_from, light_to, &[(0.0, th.rim.mix(white, 0.4), 1.7 * fade), (1.0, th.rim, 0.5 * fade)]);
            pm.stroke_path(&hp, &rim, &stroke(k), Transform::identity(), None);
            // Dial ring with accent arcs pointing at the hovered sectors.
            let dial = hub_r - 7.0 * look.scale;
            if let Some(p) = circle_path(g, dial) {
                pm.stroke_path(&p, &solid(th.text, 0.10 * fade), &stroke(k * 0.8), Transform::identity(), None);
            }
            if th.ticks {
                let mut pb = PathBuilder::new();
                for t in 0..48 {
                    let a = TAU * t as f32 / 48.0;
                    let (p0, p1) = (g.polar(dial - 1.0, a), g.polar(dial - 3.5, a));
                    pb.move_to(p0.0, p0.1);
                    pb.line_to(p1.0, p1.1);
                }
                if let Some(p) = pb.finish() {
                    pm.stroke_path(&p, &solid(th.text, 0.22 * fade), &HAIR, Transform::identity(), None);
                }
            }
            for s in secs.iter().filter(|s| s.h > 0.02) {
                if let Some(p) = ring_arc(g, dial, s.a - 0.5, s.a + 0.5) {
                    pm.stroke_path(
                        &p,
                        &solid(th.accent, s.h.min(1.0) * fade),
                        &stroke(k * 1.8),
                        Transform::identity(),
                        None,
                    );
                }
            }
            if scene.dwell > 0.01 {
                let end = (scene.dwell.min(1.0) * TAU).clamp(0.05, TAU - 0.001);
                if let Some(p) = ring_arc(g, dial, 0.0, end) {
                    pm.stroke_path(&p, &solid(th.accent, fade), &stroke(k * 2.6), Transform::identity(), None);
                }
            }
            if let Some(face) = self.face.as_mut() {
                let maxw = hub_r * 2.0 * 0.74 * k;
                let mut px = 19.0 * look.scale * k;
                let main = if th.caps { scene.main.to_uppercase() } else { scene.main.clone() };
                let tr = th.tracking * 0.6;
                let w = face.measure(&main, px, tr * px);
                if w > maxw {
                    px *= maxw / w;
                }
                let mc = if scene.main_accent && th.name != crate::theme::WINDOWS {
                    th.accent.mix(th.text, 0.15)
                } else {
                    th.text
                };
                face.draw(pm, &main, g.cx, g.cy - 6.5 * k * look.scale, px, mc, fade, tr * px);
                let spx = 9.5 * look.scale * k;
                face.draw(pm, &scene.sub, g.cx, g.cy + 12.0 * k * look.scale, spx, th.text_dim, fade, 0.06 * spx);
            }
        }

        // 4b. The pill under the ring: key hint and ring dots, or the reason a sector is greyed out.
        if !scene.caption.is_empty() {
            if let Some(face) = self.face.as_mut() {
                let u = k * look.scale;
                let px = 10.5 * u;
                let dots = scene.pager.map_or(0, |(_, n)| n);
                let dots_w = if dots > 0 { (dots as f32 * 10.0 + 4.0) * u } else { 0.0 };
                let maxw = side * 0.92 - 28.0 * u - dots_w;
                let mut text = scene.caption.clone();
                if face.measure(&text, px, 0.0) > maxw {
                    while text.chars().count() > 4 && face.measure(&text, px, 0.0) > maxw {
                        text.pop();
                    }
                    text.pop();
                    text.push('\u{2026}');
                }
                let tw = face.measure(&text, px, 0.0);
                let (pw, ph) = (tw + dots_w + 24.0 * u, 22.0 * u);
                let cy = g.cy + (m.r_out + 52.0 * look.scale) * k * zoom;
                let (x0, y0) = (g.cx - pw / 2.0, cy - ph / 2.0);
                if let Some(cap) = capsule(x0, y0, pw, ph) {
                    let glass = glass_paint(&baked.glass, fade * 0.96);
                    pm.fill_path(&cap, &glass, FillRule::Winding, Transform::identity(), None);
                    pm.fill_path(
                        &cap,
                        &solid(th.glass_bottom, 0.35 * fade),
                        FillRule::Winding,
                        Transform::identity(),
                        None,
                    );
                    pm.stroke_path(&cap, &solid(th.rim, 0.8 * fade), &HAIR, Transform::identity(), None);
                }
                if let Some((cur, n)) = scene.pager {
                    for d in 0..n {
                        let dx = x0 + 12.0 * u + d as f32 * 10.0 * u;
                        if let Some(dot) = PathBuilder::from_circle(dx, cy, if d == cur { 3.0 * u } else { 2.2 * u }) {
                            let c = if d == cur { th.accent } else { th.text_dim };
                            pm.fill_path(&dot, &solid(c, fade), FillRule::Winding, Transform::identity(), None);
                        }
                    }
                }
                let tx = x0 + 12.0 * u + dots_w + tw / 2.0;
                face.draw(pm, &text, tx, cy, px, th.text, 0.92 * fade, 0.0);
            }
        }

        // 5. Drop confirmation pulse.
        if scene.pulse > 0.0 && scene.pulse < 1.0 {
            let p = ease_out_cubic(scene.pulse);
            let r = m.hub + (m.r_out + 46.0 * look.scale - m.hub) * p;
            if let Some(ring) = circle_path(g, r) {
                let a = (1.0 - scene.pulse).powf(1.4);
                let wide = stroke(k * (14.0 * (1.0 - p) + 4.0));
                pm.stroke_path(&ring, &solid(th.accent, 0.18 * a), &wide, Transform::identity(), None);
                let bright = th.accent.mix(white, 0.3);
                let thin = stroke(k * (3.0 * (1.0 - p) + 0.8));
                pm.stroke_path(&ring, &solid(bright, 0.95 * a), &thin, Transform::identity(), None);
            }
        }

        // Hit testing: Windows lets mouse input fall through the fully transparent pixels of a layered window, and
        // a drag that falls through leaves the drop target. Whatever the sectors are doing (blooming in again after
        // a ring switch, hidden by the entrance animation), the whole interactive disc stays faintly opaque.
        let reach = Geo { cx: side / 2.0, cy: side / 2.0, u: k };
        if let Some(path) = circle_path(reach, m.hit_out) {
            let mut paint = solid(Rgba(0, 0, 0, 255), 0.012);
            paint.blend_mode = BlendMode::DestinationOver;
            pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Tray icon
// ---------------------------------------------------------------------------------------------------------------

/// Tray icon: four rounded glass petals around a hole, the top one lit with `accent`.
pub fn icon_pixmap(side: u32, accent: Rgba) -> Option<Pixmap> {
    let mut pm = Pixmap::new(side, side)?;
    let s = side as f32;
    let g = Geo { cx: s / 2.0, cy: s / 2.0, u: s / 60.0 };
    for i in 0..4 {
        let a = sector_angle(i, 4);
        let Some(path) = sector_path(g, a, PI / 4.0, 1.3, 13.0, 28.5, 6.0) else { continue };
        let p = if i == 0 {
            linear(
                g.polar(28.0, a),
                g.polar(13.0, a),
                &[(0.0, accent.mix(Rgba(255, 255, 255, 255), 0.3), 1.0), (1.0, accent, 1.0)],
            )
        } else {
            linear(
                g.polar(28.0, a),
                g.polar(13.0, a),
                &[(0.0, Rgba(214, 224, 242, 255), 0.96), (1.0, Rgba(150, 164, 192, 255), 0.92)],
            )
        };
        pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
    }
    Some(pm)
}

// ---------------------------------------------------------------------------------------------------------------
// Preview helpers (used by the configurator and by `--preview`)
// ---------------------------------------------------------------------------------------------------------------

/// A synthetic wallpaper with soft colour fields and crisp shapes, so frosting is visible in previews.
pub fn wallpaper(pm: &mut Pixmap, th: &Theme) {
    let (w, h) = (pm.width() as f32, pm.height() as f32);
    let base = linear((0.0, 0.0), (w, h), &[(0.0, th.preview_a, 1.0), (1.0, th.preview_b, 1.0)]);
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, w, h) {
        pm.fill_rect(r, &base, Transform::identity(), None);
    }
    let blobs = [
        (0.18, 0.22, 0.34, th.accent, 0.38),
        (0.82, 0.30, 0.28, th.accent2, 0.30),
        (0.55, 0.88, 0.40, th.preview_b.mix(Rgba(255, 255, 255, 255), 0.35), 0.35),
    ];
    for (x, y, r, c, a) in blobs {
        let p = radial((x * w, y * h), (x * w, y * h), r * w, &[(0.0, c, a), (1.0, c, 0.0)]);
        if let Some(path) = PathBuilder::from_circle(x * w, y * h, r * w) {
            pm.fill_path(&path, &p, FillRule::Winding, Transform::identity(), None);
        }
    }
    // Crisp "windows" and dots that pass behind the glass.
    let crisp = th.preview_b.mix(Rgba(255, 255, 255, 255), 0.55);
    let dark = th.preview_a.mix(Rgba(0, 0, 0, 255), 0.5);
    let bars = [
        (0.10, 0.62, 0.30, 0.05, crisp),
        (0.62, 0.12, 0.28, 0.05, dark),
        (0.70, 0.74, 0.22, 0.07, crisp),
        (0.08, 0.30, 0.18, 0.04, dark),
    ];
    for (x, y, bw, bh, c) in bars {
        if let Some(r) = tiny_skia::Rect::from_xywh(x * w, y * h, bw * w, bh * h) {
            pm.fill_rect(r, &solid(c, 0.55), Transform::identity(), None);
        }
    }
    for (x, y, r, c) in
        [(0.30, 0.46, 0.035, crisp), (0.74, 0.50, 0.05, dark), (0.46, 0.10, 0.03, crisp), (0.90, 0.60, 0.04, dark)]
    {
        if let Some(path) = PathBuilder::from_circle(x * w, y * h, r * w) {
            pm.fill_path(&path, &solid(c, 0.7), FillRule::Winding, Transform::identity(), None);
        }
    }
}

/// The three demo menus of the product (PNG image: 8 sectors, PDF: 4, image actions: 7), built through the same
/// code as the live overlay. AVIF is shown unavailable so previews also show a greyed-out sector.
pub fn demo_scene(kind: usize, hover: Option<usize>, lang: crate::config::Lang) -> Scene {
    use crate::convert::Fmt;
    use crate::engine::{Availability, SourceKind, Target};
    use crate::ring::{build, Env, RingKind, Switch};
    let env = Env {
        target: &|t| match t {
            Target::Image(Fmt::Avif) => {
                Availability::Unavailable("AVIF encoding needs an AV1 encoder, which Windows does not include".into())
            }
            _ => Availability::Available,
        },
        source: &|_| Availability::Available,
    };
    let (src, ring) = match kind {
        4 => (SourceKind::Pdf, RingKind::Documents),
        7 => (SourceKind::Image(Fmt::Png), RingKind::Actions),
        _ => (SourceKind::Image(Fmt::Png), RingKind::Images),
    };
    let menu = build(src, ring, 1, &env);
    let mut sc = crate::menus::scene(src, &menu, "1.1 MB", Switch::Both, lang).settled();
    if let Some(i) = hover.filter(|&i| i < menu.items.len()) {
        if menu.items[i].enabled {
            sc.hover[i] = 1.0;
        }
        crate::menus::apply(&mut sc, crate::menus::hover(&menu, i, Switch::Both, lang));
    }
    sc
}

/// Renders the demo menu over the theme's sample wallpaper. Returns an opaque pixmap.
pub fn compose_preview(
    cfg: &Config,
    user: &[Theme],
    kind: usize,
    hover: Option<usize>,
    dpi: f32,
    face: Option<Face>,
) -> Option<Pixmap> {
    let look = Look::new(cfg, user);
    let side = look.side_px(dpi);
    let mut wall = Pixmap::new(side, side)?;
    wallpaper(&mut wall, &look.theme);
    let blur = look.theme.blur * look.glass * look.scale * dpi;
    let bd = Backdrop::new(wall.data(), side, side, false, blur, look.theme.saturation);
    let mut menu = Pixmap::new(side, side)?;
    let mut r = Renderer::new(face);
    let scene = demo_scene(kind, hover, cfg.lang);
    r.prepare(&look, dpi, side, bd.as_ref(), scene.slots.len());
    r.render(&mut menu, &scene, &look, dpi);
    wall.draw_pixmap(0, 0, menu.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    Some(wall)
}

/// `--bench`: average and worst frame time of the full pipeline (render only, no window), as text on stdout.
fn bench(cfg: &Config, user: &[Theme], dpi: f32, face: Option<Face>) {
    let look = Look::new(cfg, user);
    let side = look.side_px(dpi);
    let Some(mut wall) = Pixmap::new(side, side) else { return };
    wallpaper(&mut wall, &look.theme);
    let t0 = std::time::Instant::now();
    let bd = Backdrop::new(wall.data(), side, side, false, look.theme.blur * dpi, look.theme.saturation);
    let backdrop_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let Some(mut pm) = Pixmap::new(side, side) else { return };
    let mut r = Renderer::new(face);
    let mut sc = demo_scene(8, None, crate::config::Lang::En);
    let t0 = std::time::Instant::now();
    r.prepare(&look, dpi, side, bd.as_ref(), 8);
    let prepare_ms = t0.elapsed().as_secs_f64() * 1000.0;
    let (mut sum, mut worst, frames) = (0.0f64, 0.0f64, 240);
    for f in 0..frames {
        sc.hover.iter_mut().enumerate().for_each(|(i, h)| *h = if i == (f / 30) % 8 { 1.0 } else { 0.0 });
        let t = std::time::Instant::now();
        r.render(&mut pm, &sc, &look, dpi);
        let ms = t.elapsed().as_secs_f64() * 1000.0;
        sum += ms;
        worst = worst.max(ms);
    }
    println!(
        "canvas {side}x{side} backdrop {backdrop_ms:.2} ms prepare {prepare_ms:.2} ms frame avg {:.2} ms worst {worst:.2} ms",
        sum / frames as f64
    );
}

fn save_png(pm: &Pixmap, path: &str) {
    // Opaque by construction, so premultiplied equals straight.
    let img = image::RgbaImage::from_raw(pm.width(), pm.height(), pm.data().to_vec()).expect("pixmap size");
    img.save(path).expect("save png");
}

/// Handles `--preview out.png [--theme name] [--hover i] [--menu 4|7|8] [--dpi f]` and `--sheet out.png`.
/// Returns true when it consumed the command line.
pub fn cli(args: &[String]) -> bool {
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let user = crate::config::user_themes();
    let dpi: f32 = get("--dpi").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let hover = get("--hover").and_then(|v| v.parse::<usize>().ok());
    let kind: usize = get("--menu").and_then(|v| v.parse().ok()).unwrap_or(8);
    let mut cfg = Config::default();
    if let Some(name) = get("--theme") {
        let t = theme::find(&name, &user);
        cfg.apply_theme(&t);
    }
    if let Some(v) = get("--accent").and_then(|v| Rgba::parse(&v).ok()) {
        cfg.accent = Some(v);
    }
    let face = |c: &Config| {
        let t = c.resolve(&theme::find(&c.theme, &user));
        Face::load(t.font, t.weight)
    };
    if let Some(out) = get("--preview") {
        if let Some(pm) = compose_preview(&cfg, &user, kind, hover.or(Some(1)), dpi, face(&cfg)) {
            save_png(&pm, &out);
        }
        return true;
    }
    if let Some(out) = get("--tray-icon") {
        let size: u32 = get("--size").and_then(|v| v.parse().ok()).unwrap_or(256).clamp(16, 1024);
        if let Some(pm) = icon_pixmap(size, cfg.resolve(&theme::find(&cfg.theme, &user)).accent) {
            // The icon has transparency: write straight (not premultiplied) pixels.
            let mut raw = Vec::with_capacity(pm.data().len());
            for px in pm.pixels() {
                let c = px.demultiply();
                raw.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
            }
            if let Some(img) = image::RgbaImage::from_raw(size, size, raw) {
                img.save(out).expect("save icon png");
            }
        }
        return true;
    }
    if args.iter().any(|a| a == "--bench") {
        bench(&cfg, &user, dpi, face(&cfg));
        return true;
    }
    if let Some(out) = get("--sheet") {
        let themes: Vec<Theme> = theme::builtin().into_iter().chain(user.iter().cloned()).collect();
        let cols = 4usize;
        // Rings differ per theme, so every canvas is scaled to the smallest one to get a regular grid.
        let sides: Vec<u32> = themes
            .iter()
            .map(|t| {
                let mut c = Config::default();
                c.apply_theme(t);
                Look::new(&c, &user).side_px(dpi)
            })
            .collect();
        let cell = sides.iter().copied().min().unwrap_or(400) as usize;
        let label_h = (44.0 * dpi) as usize;
        let slots = themes.len() + 1; // one extra tile invites contributions
        let rows = slots.div_ceil(cols);
        let (w, h) = (cols * cell, rows * (cell + label_h));
        let Some(mut sheet) = Pixmap::new(w as u32, h as u32) else { return true };
        sheet.fill(Color::from_rgba8(14, 15, 20, 255));
        let mut caption = Face::load(FontKind::Variable, 600.0);
        let mut put_caption = |sheet: &mut Pixmap, text: &str, x: usize, y: usize| {
            if let Some(f) = caption.as_mut() {
                let px = 15.0 * dpi;
                let (cx, cy) = ((x + cell / 2) as f32, (y + cell) as f32 + label_h as f32 / 2.0);
                f.draw(sheet, &text.to_uppercase(), cx, cy, px, Rgba(235, 238, 246, 255), 1.0, px * 0.16);
            }
        };
        for (i, t) in themes.iter().enumerate() {
            let mut c = Config::default();
            c.apply_theme(t);
            let kind = [8, 4, 7][i % 3];
            let hv = Some([1, 2, 3][i % 3]);
            let Some(pm) = compose_preview(&c, &user, kind, hv, dpi, face(&c)) else { continue };
            let (x, y) = ((i % cols) * cell, (i / cols) * (cell + label_h));
            let k = cell as f32 / pm.width() as f32;
            let paint = PixmapPaint { quality: FilterQuality::Bicubic, ..PixmapPaint::default() };
            sheet.draw_pixmap(0, 0, pm.as_ref(), &paint, Transform::from_row(k, 0.0, 0.0, k, x as f32, y as f32), None);
            put_caption(&mut sheet, &t.label, x, y);
        }
        // The last tile: how to add a theme.
        let (x, y) = ((themes.len() % cols) * cell, (themes.len() / cols) * (cell + label_h));
        put_caption(&mut sheet, "Add yours", x, y);
        if let Some(f) = caption.as_mut() {
            let px = 17.0 * dpi;
            let lines = ["Drop a .theme file into", "%APPDATA%\\radial\\themes", "or add one to assets/themes"];
            for (i, line) in lines.iter().enumerate() {
                let c = Rgba(150, 158, 178, 255);
                f.draw(
                    &mut sheet,
                    line,
                    (x + cell / 2) as f32,
                    (y + cell / 2) as f32 + (i as f32 - 1.0) * 30.0 * dpi,
                    px,
                    c,
                    1.0,
                    0.0,
                );
            }
        }
        save_png(&sheet, &out);
        return true;
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_icon_parses_into_a_path() {
        for i in ICONS {
            assert!(icon_path(i).is_some(), "{i:?}");
        }
    }

    #[test]
    fn icon_path_language_rejects_garbage() {
        assert!(parse_icon("M 1 2 L 3").is_none());
        assert!(parse_icon("X 1 2").is_none());
        assert!(parse_icon("M1 2 L3 4").is_some(), "letters need no spaces after them");
    }

    #[test]
    fn menu_labels_get_specific_icons() {
        for l in ["PNG", "JPG", "WEBP", "GIF", "TIFF", "BMP", "AVIF", "HEIC", "PDF", "DOCX", "TXT"] {
            assert_ne!(Icon::for_label(l), Icon::Folder, "{l}");
        }
        for l in ["COMPRESS", "METADATA", "EDIT", "ANNOTATE", "ADD BG", "CROP", "REDACT"] {
            assert_ne!(Icon::for_label(l), Icon::Folder, "{l}");
        }
        assert_eq!(Icon::for_label("jpeg"), Icon::Photo);
        assert_eq!(Icon::for_label("???"), Icon::Folder);
    }

    #[test]
    fn sector_paths_build_for_every_menu_size_and_corner() {
        let g = Geo { cx: 250.0, cy: 250.0, u: 1.0 };
        for n in [3usize, 4, 7, 8, 12] {
            for corner in [0.0, 14.0, 40.0] {
                for gap in [0.0, 5.0, 24.0] {
                    let p = sector_path(g, 0.3, PI / n as f32, gap / 2.0, 70.0, 170.0, corner);
                    let b = p.expect("path").bounds();
                    assert!(b.width() > 10.0 && b.height() > 10.0, "n={n} corner={corner} gap={gap}");
                    assert!(b.left() >= 0.0 && b.right() <= 500.0, "stays on the canvas");
                }
            }
        }
    }

    #[test]
    fn upscale_keeps_flat_colour_and_size() {
        let mut low = Pixmap::new(5, 5).unwrap();
        low.fill(Color::from_rgba8(200, 100, 50, 255));
        let big = upscale(&low, 18, 17).unwrap();
        assert_eq!((big.width(), big.height()), (18, 17));
        assert!(big.data().chunks_exact(4).all(|p| p == [200, 100, 50, 255]));
    }

    #[test]
    fn blur_keeps_flat_areas_and_spreads_edges() {
        let mut flat = vec![80u8; 20 * 20];
        blur3(&mut flat, 20, 20, 1, 6.0);
        assert!(flat.iter().all(|&v| v == 80));
        let mut dot = vec![0u8; 21 * 21];
        dot[10 * 21 + 10] = 255;
        blur3(&mut dot, 21, 21, 1, 6.0);
        assert!(dot[10 * 21 + 10] < 255 && dot[10 * 21 + 13] > 0, "energy spreads to neighbours");
    }

    #[test]
    fn backdrop_rejects_short_buffers() {
        assert!(Backdrop::new(&[0; 16], 8, 8, true, 4.0, 1.0).is_none());
        assert!(Backdrop::new(&vec![0u8; 8 * 8 * 4], 8, 8, true, 4.0, 1.3).is_some());
    }

    #[test]
    fn text_is_measured_and_tracked() {
        if let Some(f) = Face::load(FontKind::Variable, 600.0) {
            let (a, b) = (f.measure("PDF", 20.0, 0.0), f.measure("PDF", 20.0, 3.0));
            assert!(a > 10.0 && (b - a - 6.0).abs() < 0.01, "tracking adds (n-1) gaps");
            assert_eq!(f.measure("", 20.0, 3.0), 0.0);
        }
    }

    #[test]
    fn a_frame_renders_visible_pixels_for_every_theme() {
        for t in theme::builtin() {
            let mut cfg = Config::default();
            cfg.apply_theme(&t);
            let pm = compose_preview(&cfg, &[], 8, Some(1), 1.0, None).expect("preview");
            let look = Look::new(&cfg, &[]);
            assert_eq!(pm.width(), look.side_px(1.0), "{}", t.name);
            // The centre of the hub is glass, so it differs from the wallpaper under it.
            let mut wall = Pixmap::new(pm.width(), pm.height()).unwrap();
            wallpaper(&mut wall, &look.theme);
            let c = pm.width() / 2;
            assert_ne!(pm.pixel(c, c), wall.pixel(c, c), "{}", t.name);
        }
    }
}
