//! "Add BG": places an image on a gradient, solid colour or picture, with padding, rounded corners,
//! a soft drop shadow and an optional canvas aspect ratio.

use super::error::{Error, Result};
use super::raster::{MAX_PIXELS, MAX_SIDE};
use image::{imageops::FilterType, RgbaImage};
use tiny_skia::{
    FillRule, FilterQuality, GradientStop, LinearGradient, Paint, Path, PathBuilder, Pattern, Pixmap, PixmapPaint,
    Point, Rect as SkRect, SpreadMode, Transform,
};

pub type Color = [u8; 4];

#[derive(Clone, Debug)]
pub enum Background {
    Solid(Color),
    /// CSS-style angle: 0 = bottom to top, 90 = left to right, 180 = top to bottom.
    Linear {
        from: Color,
        to: Color,
        angle_deg: f32,
    },
    /// Scaled to cover the canvas and center-cropped.
    Image(RgbaImage),
}

/// Canvas ratio choices of the dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ratio {
    Auto,
    Square,
    /// 16:9
    Wide,
    /// 4:5
    Portrait,
}

impl Ratio {
    pub const ALL: [Ratio; 4] = [Ratio::Auto, Ratio::Square, Ratio::Wide, Ratio::Portrait];

    pub fn label(self) -> &'static str {
        match self {
            Ratio::Auto => "Auto",
            Ratio::Square => "1:1",
            Ratio::Wide => "16:9",
            Ratio::Portrait => "4:5",
        }
    }

    fn terms(self) -> Option<(u64, u64)> {
        match self {
            Ratio::Auto => None,
            Ratio::Square => Some((1, 1)),
            Ratio::Wide => Some((16, 9)),
            Ratio::Portrait => Some((4, 5)),
        }
    }

    /// Width / height, `None` for auto.
    pub fn value(self) -> Option<f64> {
        self.terms().map(|(w, h)| w as f64 / h as f64)
    }
}

#[derive(Clone, Debug)]
pub struct BgOptions {
    pub background: Background,
    /// Space around the image, in px.
    pub padding: u32,
    /// Corner radius of the image, in px.
    pub corners: u32,
    /// Shadow blur in px; 0 disables it.
    pub shadow: u32,
    pub ratio: Ratio,
}

impl Default for BgOptions {
    fn default() -> Self {
        BgOptions { background: presets()[0].1.clone(), padding: 64, corners: 24, shadow: 40, ratio: Ratio::Auto }
    }
}

fn rgb(hex: u32) -> Color {
    [(hex >> 16) as u8, (hex >> 8) as u8, hex as u8, 255]
}

fn grad(from: u32, to: u32) -> Background {
    Background::Linear { from: rgb(from), to: rgb(to), angle_deg: 135.0 }
}

/// Ready-made backgrounds for the picker: (name, background).
pub fn presets() -> &'static [(&'static str, Background)] {
    static PRESETS: std::sync::OnceLock<Vec<(&'static str, Background)>> = std::sync::OnceLock::new();
    PRESETS.get_or_init(|| {
        vec![
            ("Midnight", grad(0x0F2027, 0x2C5364)),
            ("Sunset", grad(0xFF512F, 0xDD2476)),
            ("Ocean", grad(0x2193B0, 0x6DD5ED)),
            ("Aurora", grad(0x7F7FD5, 0x91EAE4)),
            ("Peach", grad(0xFFB88C, 0xDE6262)),
            ("Mint", grad(0x11998E, 0x38EF7D)),
            ("Graphite", grad(0x232526, 0x414345)),
            ("Snow", grad(0xE6E9F0, 0xEEF1F5)),
            ("White", Background::Solid(rgb(0xFFFFFF))),
            ("Black", Background::Solid(rgb(0x0B0B0B))),
        ]
    })
}

/// Canvas size for an image of `w` x `h` with `padding` and `ratio`.
pub fn canvas_size(w: u32, h: u32, padding: u32, ratio: Ratio) -> (u64, u64) {
    let bw = w as u64 + 2 * padding as u64;
    let bh = h as u64 + 2 * padding as u64;
    match ratio.terms() {
        None => (bw, bh),
        Some((rw, rh)) if bw * rh < bh * rw => ((bh * rw).div_ceil(rh), bh),
        Some((rw, rh)) => (bw, (bw * rh).div_ceil(rw)),
    }
}

pub(super) fn premultiplied(img: &RgbaImage) -> Vec<u8> {
    let mut out = img.as_raw().clone();
    for p in out.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * a + 127) / 255) as u8;
            }
        }
    }
    out
}

pub(super) fn straight(pix: &Pixmap) -> RgbaImage {
    let mut raw = pix.data().to_vec();
    for p in raw.chunks_exact_mut(4) {
        let a = p[3] as u32;
        if a != 0 && a != 255 {
            for c in &mut p[..3] {
                *c = ((*c as u32 * 255 + a / 2) / a).min(255) as u8;
            }
        }
    }
    RgbaImage::from_raw(pix.width(), pix.height(), raw).expect("pixmap buffer matches its size")
}

fn sk_color(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c[0], c[1], c[2], c[3])
}

fn rounded_rect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    const K: f32 = 0.552_284_8;
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let c = r * K;
    let mut p = PathBuilder::new();
    p.move_to(x + r, y);
    p.line_to(x + w - r, y);
    p.cubic_to(x + w - r + c, y, x + w, y + r - c, x + w, y + r);
    p.line_to(x + w, y + h - r);
    p.cubic_to(x + w, y + h - r + c, x + w - r + c, y + h, x + w - r, y + h);
    p.line_to(x + r, y + h);
    p.cubic_to(x + r - c, y + h, x, y + h - r + c, x, y + h - r);
    p.line_to(x, y + r);
    p.cubic_to(x, y + r - c, x + r - c, y, x + r, y);
    p.close();
    p.finish()
}

fn paint_background(canvas: &mut Pixmap, bg: &Background) {
    let (w, h) = (canvas.width(), canvas.height());
    match bg {
        Background::Solid(c) => canvas.fill(sk_color(*c)),
        Background::Linear { from, to, angle_deg } => {
            let t = angle_deg.to_radians();
            let (dx, dy) = (t.sin(), -t.cos());
            let len = (w as f32 * dx).abs() + (h as f32 * dy).abs();
            let (cx, cy) = (w as f32 / 2.0, h as f32 / 2.0);
            let shader = LinearGradient::new(
                Point::from_xy(cx - dx * len / 2.0, cy - dy * len / 2.0),
                Point::from_xy(cx + dx * len / 2.0, cy + dy * len / 2.0),
                vec![GradientStop::new(0.0, sk_color(*from)), GradientStop::new(1.0, sk_color(*to))],
                SpreadMode::Pad,
                Transform::identity(),
            );
            let mut paint = Paint::default();
            match shader {
                Some(s) => paint.shader = s,
                None => paint.set_color(sk_color(*from)), // degenerate gradient: a single colour
            }
            if let Some(rect) = SkRect::from_xywh(0.0, 0.0, w as f32, h as f32) {
                canvas.fill_rect(rect, &paint, Transform::identity(), None);
            }
        }
        Background::Image(img) => {
            let (bw, bh) = img.dimensions();
            if bw == 0 || bh == 0 {
                return;
            }
            let k = (w as f64 / bw as f64).max(h as f64 / bh as f64);
            let (rw, rh) = (((bw as f64 * k).ceil() as u32).max(w), ((bh as f64 * k).ceil() as u32).max(h));
            let scaled = image::imageops::resize(img, rw, rh, FilterType::Triangle);
            let (ox, oy) = ((rw - w) / 2, (rh - h) / 2);
            let view = image::imageops::crop_imm(&scaled, ox, oy, w, h).to_image();
            canvas.data_mut().copy_from_slice(&premultiplied(&view));
        }
    }
}

/// In-place 3-pass box blur of a single-channel buffer (approximates a Gaussian).
fn blur_alpha(buf: &mut [u8], w: usize, h: usize, r: usize) {
    if r == 0 || w == 0 || h == 0 {
        return;
    }
    let mut tmp = vec![0u8; buf.len()];
    for _ in 0..3 {
        // Horizontal then vertical, each a running-sum window of 2r+1 samples.
        for y in 0..h {
            run(&buf[y * w..(y + 1) * w], &mut tmp[y * w..(y + 1) * w], r);
        }
        for x in 0..w {
            let col: Vec<u8> = (0..h).map(|y| tmp[y * w + x]).collect();
            let mut out = vec![0u8; h];
            run(&col, &mut out, r);
            for y in 0..h {
                buf[y * w + x] = out[y];
            }
        }
    }
}

fn run(src: &[u8], dst: &mut [u8], r: usize) {
    let n = src.len();
    let win = (2 * r + 1) as u32;
    let mut sum: u32 = src.iter().take(r + 1).map(|&v| v as u32).sum();
    for i in 0..n {
        dst[i] = (sum / win) as u8;
        if i + r + 1 < n {
            sum += src[i + r + 1] as u32;
        }
        if i >= r {
            sum -= src[i - r] as u32;
        }
    }
}

fn draw_shadow(canvas: &mut Pixmap, x: f32, y: f32, w: u32, h: u32, corners: f32, blur: u32) {
    let reach = (blur as usize) * 2 + 2;
    let (rw, rh) = (w as usize + 2 * reach, h as usize + 2 * reach);
    let Some(mut mask) = Pixmap::new(rw as u32, rh as u32) else { return };
    let Some(path) = rounded_rect(reach as f32, reach as f32, w as f32, h as f32, corners) else { return };
    let mut paint = Paint::default();
    paint.set_color_rgba8(0, 0, 0, 255);
    mask.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    let mut alpha: Vec<u8> = mask.data().chunks_exact(4).map(|p| p[3]).collect();
    blur_alpha(&mut alpha, rw, rh, (blur as usize / 3).max(1));
    let off_y = (blur / 3) as i64;
    let (cw, ch) = (canvas.width() as i64, canvas.height() as i64);
    let (x0, y0) = (x as i64 - reach as i64, y as i64 - reach as i64 + off_y);
    let data = canvas.data_mut();
    for sy in 0..rh as i64 {
        let cy = y0 + sy;
        if !(0..ch).contains(&cy) {
            continue;
        }
        for sx in 0..rw as i64 {
            let cx = x0 + sx;
            if !(0..cw).contains(&cx) {
                continue;
            }
            // Shadow opacity 50%: black over the canvas pixel (premultiplied).
            let a = alpha[sy as usize * rw + sx as usize] as u32 / 2;
            if a == 0 {
                continue;
            }
            let i = ((cy * cw + cx) * 4) as usize;
            for c in &mut data[i..i + 3] {
                *c = (*c as u32 * (255 - a) / 255) as u8;
            }
            data[i + 3] = ((data[i + 3] as u32 * (255 - a) + 255 * a) / 255) as u8;
        }
    }
}

pub fn add_background(img: &RgbaImage, opts: &BgOptions) -> Result<RgbaImage> {
    let (w, h) = img.dimensions();
    if w == 0 || h == 0 {
        return Err(Error::InvalidArgument("the image has no pixels".into()));
    }
    let (cw, ch) = canvas_size(w, h, opts.padding, opts.ratio);
    if cw > MAX_SIDE as u64 || ch > MAX_SIDE as u64 || cw * ch > MAX_PIXELS {
        return Err(Error::TooLarge { width: cw.min(u32::MAX as u64) as u32, height: ch.min(u32::MAX as u64) as u32 });
    }
    let mut canvas =
        Pixmap::new(cw as u32, ch as u32).ok_or(Error::TooLarge { width: cw as u32, height: ch as u32 })?;
    paint_background(&mut canvas, &opts.background);

    let (x, y) = ((cw as u32 - w) / 2, (ch as u32 - h) / 2);
    let corners = (opts.corners as f32).min(w.min(h) as f32 / 2.0);
    if opts.shadow > 0 {
        draw_shadow(&mut canvas, x as f32, y as f32, w, h, corners, opts.shadow);
    }
    let size = tiny_skia::IntSize::from_wh(w, h).ok_or_else(|| Error::InvalidArgument("empty image".into()))?;
    let src = Pixmap::from_vec(premultiplied(img), size).ok_or_else(|| Error::Corrupt("image buffer".into()))?;
    if corners < 0.5 {
        canvas.draw_pixmap(x as i32, y as i32, src.as_ref(), &PixmapPaint::default(), Transform::identity(), None);
    } else if let Some(path) = rounded_rect(x as f32, y as f32, w as f32, h as f32, corners) {
        let paint = Paint {
            anti_alias: true,
            shader: Pattern::new(
                src.as_ref(),
                SpreadMode::Pad,
                FilterQuality::Nearest,
                1.0,
                Transform::from_translate(x as f32, y as f32),
            ),
            ..Paint::default()
        };
        canvas.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
    Ok(straight(&canvas))
}

#[cfg(test)]
mod tests {
    use super::super::error::Error;
    use super::*;
    use image::{Rgba, RgbaImage};

    fn red(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([255, 0, 0, 255]))
    }

    fn solid(c: Color) -> BgOptions {
        BgOptions { background: Background::Solid(c), padding: 20, corners: 0, shadow: 0, ratio: Ratio::Auto }
    }

    #[test]
    fn ratio_labels_and_values() {
        let l: Vec<_> = Ratio::ALL.iter().map(|r| r.label()).collect();
        assert_eq!(l, ["Auto", "1:1", "16:9", "4:5"]);
        assert_eq!(Ratio::Auto.value(), None);
        assert_eq!(Ratio::Square.value(), Some(1.0));
        assert_eq!(Ratio::Portrait.value(), Some(0.8));
    }

    #[test]
    fn canvas_sizes() {
        assert_eq!(canvas_size(1000, 600, 50, Ratio::Auto), (1100, 700));
        assert_eq!(canvas_size(1000, 600, 50, Ratio::Square), (1100, 1100));
        assert_eq!(canvas_size(1000, 600, 50, Ratio::Wide), (1245, 700));
        assert_eq!(canvas_size(1000, 600, 50, Ratio::Portrait), (1100, 1375));
        assert_eq!(canvas_size(100, 100, 0, Ratio::Auto), (100, 100));
    }

    #[test]
    fn solid_background_with_padding_keeps_the_image_in_the_middle() {
        let out = add_background(&red(40, 30), &solid([0, 0, 255, 255])).unwrap();
        assert_eq!(out.dimensions(), (80, 70));
        assert_eq!(out.get_pixel(0, 0).0, [0, 0, 255, 255]);
        assert_eq!(out.get_pixel(79, 69).0, [0, 0, 255, 255]);
        assert_eq!(out.get_pixel(19, 35).0, [0, 0, 255, 255], "just left of the image");
        assert_eq!(out.get_pixel(20, 20).0, [255, 0, 0, 255], "first image pixel");
        assert_eq!(out.get_pixel(59, 49).0, [255, 0, 0, 255], "last image pixel");
        assert_eq!(out.get_pixel(60, 49).0, [0, 0, 255, 255]);
    }

    #[test]
    fn ratio_extends_the_canvas_and_centers_the_image() {
        let mut o = solid([10, 20, 30, 255]);
        o.ratio = Ratio::Square;
        let out = add_background(&red(40, 20), &o).unwrap();
        assert_eq!(out.dimensions(), (80, 80));
        assert_eq!(out.get_pixel(40, 40).0, [255, 0, 0, 255]);
        assert_eq!(out.get_pixel(40, 5).0, [10, 20, 30, 255]);
    }

    #[test]
    fn gradient_runs_in_the_requested_direction() {
        let g = |angle| BgOptions {
            background: Background::Linear { from: [0, 0, 0, 255], to: [255, 255, 255, 255], angle_deg: angle },
            padding: 50,
            corners: 0,
            shadow: 0,
            ratio: Ratio::Auto,
        };
        let lr = add_background(&red(10, 10), &g(90.0)).unwrap();
        assert!(lr.get_pixel(2, 55)[0] < 40 && lr.get_pixel(107, 55)[0] > 215);
        let tb = add_background(&red(10, 10), &g(180.0)).unwrap();
        assert!(tb.get_pixel(55, 2)[0] < 40 && tb.get_pixel(55, 107)[0] > 215);
        let bt = add_background(&red(10, 10), &g(0.0)).unwrap();
        assert!(bt.get_pixel(55, 107)[0] < 40 && bt.get_pixel(55, 2)[0] > 215);
    }

    #[test]
    fn rounded_corners_cut_the_image_corner_but_not_the_middle() {
        let mut o = solid([0, 0, 255, 255]);
        o.corners = 16;
        let out = add_background(&red(60, 60), &o).unwrap();
        assert_eq!(out.get_pixel(20, 20).0, [0, 0, 255, 255], "corner pixel shows the background");
        assert_eq!(out.get_pixel(50, 50).0, [255, 0, 0, 255]);
        assert_eq!(out.get_pixel(20 + 30, 20 + 1).0, [255, 0, 0, 255], "straight edge stays square");
    }

    #[test]
    fn corner_radius_larger_than_the_image_is_clamped() {
        let mut o = solid([0, 0, 255, 255]);
        o.corners = 10_000;
        let out = add_background(&red(20, 20), &o).unwrap();
        assert_eq!(out.get_pixel(30, 30).0, [255, 0, 0, 255], "a pill/circle keeps its centre");
    }

    #[test]
    fn shadow_darkens_the_background_below_the_image_and_fades_out() {
        let mut o = solid([200, 200, 200, 255]);
        o.padding = 60;
        let plain = add_background(&red(60, 60), &o).unwrap();
        o.shadow = 30;
        let shaded = add_background(&red(60, 60), &o).unwrap();
        let below = (90, 60 + 60 + 4);
        assert_eq!(plain.get_pixel(below.0, below.1).0, [200, 200, 200, 255]);
        assert!(shaded.get_pixel(below.0, below.1)[0] < 170, "{:?}", shaded.get_pixel(below.0, below.1));
        assert_eq!(shaded.get_pixel(1, 1).0, [200, 200, 200, 255], "far corner is untouched");
        assert_eq!(shaded.get_pixel(90, 90).0, [255, 0, 0, 255], "the image stays on top of its shadow");
    }

    #[test]
    fn image_background_is_cover_scaled() {
        let bg = RgbaImage::from_pixel(7, 3, Rgba([0, 255, 0, 255]));
        let o = BgOptions { background: Background::Image(bg), padding: 30, corners: 0, shadow: 0, ratio: Ratio::Wide };
        let out = add_background(&red(20, 20), &o).unwrap();
        assert_eq!(out.dimensions(), (143, 80));
        assert_eq!(out.get_pixel(0, 0).0, [0, 255, 0, 255]);
        assert_eq!(out.get_pixel(142, 79).0, [0, 255, 0, 255]);
    }

    #[test]
    fn transparent_source_shows_the_background_through() {
        let src = RgbaImage::from_pixel(10, 10, Rgba([255, 0, 0, 0]));
        let out = add_background(&src, &solid([0, 0, 255, 255])).unwrap();
        assert_eq!(out.get_pixel(25, 25).0, [0, 0, 255, 255]);
    }

    #[test]
    fn output_is_deterministic_and_presets_are_usable() {
        let mut o = BgOptions { padding: 10, shadow: 8, ..BgOptions::default() };
        let a = add_background(&red(30, 30), &o).unwrap();
        let b = add_background(&red(30, 30), &o).unwrap();
        assert_eq!(a, b);
        assert!(presets().len() >= 6);
        for (name, bg) in presets().iter() {
            assert!(!name.is_empty());
            o.background = bg.clone();
            assert!(add_background(&red(8, 8), &o).is_ok(), "{name}");
        }
    }

    #[test]
    fn absurd_canvases_are_refused() {
        let mut o = solid([0, 0, 0, 255]);
        o.padding = 60_000;
        assert!(matches!(add_background(&red(10, 10), &o), Err(Error::TooLarge { .. })));
        assert!(matches!(add_background(&RgbaImage::new(0, 0), &solid([0; 4])), Err(Error::InvalidArgument(_))));
    }
}
