//! Annotation data model and renderer for the ANNOTATE action (the editing UI comes later).

use super::background::{premultiplied, straight};
use super::error::{Error, Result};
use ab_glyph::{point, Font, FontArc, PxScale, ScaleFont};
use image::RgbaImage;
use tiny_skia::{LineCap, LineJoin, Paint, Path, PathBuilder, Pixmap, Rect as SkRect, Stroke, Transform};

pub type Color = [u8; 4];
/// A point in image pixels (fractions allowed, may lie outside the image).
pub type Pt = (f32, f32);

#[derive(Clone, Debug, PartialEq)]
pub enum Annotation {
    /// Freehand polyline with round caps and joins.
    Stroke {
        points: Vec<Pt>,
        color: Color,
        width: f32,
    },
    Line {
        from: Pt,
        to: Pt,
        color: Color,
        width: f32,
    },
    /// Line with a head at `to`.
    Arrow {
        from: Pt,
        to: Pt,
        color: Color,
        width: f32,
    },
    /// Axis-aligned box between two opposite corners.
    Rect {
        from: Pt,
        to: Pt,
        color: Color,
        width: f32,
        fill: bool,
    },
    Ellipse {
        from: Pt,
        to: Pt,
        color: Color,
        width: f32,
        fill: bool,
    },
    /// Translucent marker box (use a colour with alpha < 255).
    Highlight {
        from: Pt,
        to: Pt,
        color: Color,
    },
    /// Text with its top-left corner at `pos`; `size` is the font height in px.
    Text {
        pos: Pt,
        text: String,
        size: f32,
        color: Color,
    },
}

impl Annotation {
    /// The same annotation for an image `k` times as large: every coordinate, width and font size is multiplied.
    /// The UI draws on a downscaled preview and saves on the full image.
    pub fn scaled(&self, k: f32) -> Annotation {
        let p = |q: Pt| (q.0 * k, q.1 * k);
        match self {
            Annotation::Stroke { points, color, width } => {
                Annotation::Stroke { points: points.iter().map(|&q| p(q)).collect(), color: *color, width: width * k }
            }
            Annotation::Line { from, to, color, width } => {
                Annotation::Line { from: p(*from), to: p(*to), color: *color, width: width * k }
            }
            Annotation::Arrow { from, to, color, width } => {
                Annotation::Arrow { from: p(*from), to: p(*to), color: *color, width: width * k }
            }
            Annotation::Rect { from, to, color, width, fill } => {
                Annotation::Rect { from: p(*from), to: p(*to), color: *color, width: width * k, fill: *fill }
            }
            Annotation::Ellipse { from, to, color, width, fill } => {
                Annotation::Ellipse { from: p(*from), to: p(*to), color: *color, width: width * k, fill: *fill }
            }
            Annotation::Highlight { from, to, color } => {
                Annotation::Highlight { from: p(*from), to: p(*to), color: *color }
            }
            Annotation::Text { pos, text, size, color } => {
                Annotation::Text { pos: p(*pos), text: text.clone(), size: size * k, color: *color }
            }
        }
    }
}

const MAX_COORD: f32 = 100_000.0;
const MAX_WIDTH: f32 = 20_000.0;
const MAX_POINTS: usize = 1_000_000;
const MAX_TEXT: usize = 10_000;

/// A UI font from the Windows font folder, if one can be read.
pub fn load_system_font() -> Option<FontArc> {
    let dir =
        std::path::PathBuf::from(std::env::var_os("WINDIR").unwrap_or_else(|| "C:\\Windows".into())).join("Fonts");
    ["segoeui.ttf", "arial.ttf", "tahoma.ttf", "calibri.ttf", "verdana.ttf"]
        .iter()
        .find_map(|f| std::fs::read(dir.join(f)).ok().and_then(|b| FontArc::try_from_vec(b).ok()))
}

fn bad(msg: &str) -> Error {
    Error::InvalidArgument(msg.to_string())
}

fn check_pt(p: Pt) -> Result<()> {
    if p.0.is_finite() && p.1.is_finite() {
        Ok(())
    } else {
        Err(bad("annotation coordinates must be finite numbers"))
    }
}

fn check_width(w: f32) -> Result<()> {
    if w.is_finite() && w > 0.0 {
        Ok(())
    } else {
        Err(bad("annotation line width must be positive"))
    }
}

fn validate(a: &Annotation) -> Result<()> {
    match a {
        Annotation::Stroke { points, width, .. } => {
            if points.len() > MAX_POINTS {
                return Err(bad("too many stroke points"));
            }
            points.iter().try_for_each(|p| check_pt(*p))?;
            check_width(*width)
        }
        Annotation::Line { from, to, width, .. } | Annotation::Arrow { from, to, width, .. } => {
            check_pt(*from)?;
            check_pt(*to)?;
            check_width(*width)
        }
        Annotation::Rect { from, to, width, .. } | Annotation::Ellipse { from, to, width, .. } => {
            check_pt(*from)?;
            check_pt(*to)?;
            check_width(*width)
        }
        Annotation::Highlight { from, to, .. } => {
            check_pt(*from)?;
            check_pt(*to)
        }
        Annotation::Text { pos, text, size, .. } => {
            check_pt(*pos)?;
            if !(size.is_finite() && *size > 0.0) {
                return Err(bad("text size must be positive"));
            }
            if text.chars().count() > MAX_TEXT {
                return Err(bad("text is too long"));
            }
            Ok(())
        }
    }
}

fn paint(c: Color) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color_rgba8(c[0], c[1], c[2], c[3]);
    p.anti_alias = true;
    p
}

fn stroke(width: f32) -> Stroke {
    Stroke { width: width.min(MAX_WIDTH), line_cap: LineCap::Round, line_join: LineJoin::Round, ..Stroke::default() }
}

fn clamp(p: Pt) -> Pt {
    (p.0.clamp(-MAX_COORD, MAX_COORD), p.1.clamp(-MAX_COORD, MAX_COORD))
}

fn bounds(a: Pt, b: Pt) -> Option<SkRect> {
    let (a, b) = (clamp(a), clamp(b));
    SkRect::from_ltrb(a.0.min(b.0), a.1.min(b.1), a.0.max(b.0), a.1.max(b.1))
}

fn polyline(pts: &[Pt]) -> Option<Path> {
    let mut pb = PathBuilder::new();
    let (first, rest) = pts.split_first()?;
    let f = clamp(*first);
    pb.move_to(f.0, f.1);
    for p in rest {
        let p = clamp(*p);
        pb.line_to(p.0, p.1);
    }
    pb.finish()
}

fn draw_line(pix: &mut Pixmap, from: Pt, to: Pt, color: Color, width: f32) {
    if let Some(path) = polyline(&[from, to]) {
        pix.stroke_path(&path, &paint(color), &stroke(width), Transform::identity(), None);
    }
}

fn draw_arrow(pix: &mut Pixmap, from: Pt, to: Pt, color: Color, width: f32) {
    let (from, to) = (clamp(from), clamp(to));
    let (dx, dy) = (to.0 - from.0, to.1 - from.1);
    let len = (dx * dx + dy * dy).sqrt();
    if len < 1e-3 {
        draw_line(pix, from, to, color, width);
        return;
    }
    let (ux, uy) = (dx / len, dy / len);
    let head = (width * 4.0).max(12.0).min(len.max(12.0));
    let rot = |a: f32| (ux * a.cos() - uy * a.sin(), ux * a.sin() + uy * a.cos());
    let (l, r) = (rot(0.5), rot(-0.5));
    // The shaft stops inside the head so a round cap never pokes out of the tip.
    draw_line(pix, from, (to.0 - ux * head * 0.6, to.1 - uy * head * 0.6), color, width);
    let mut pb = PathBuilder::new();
    pb.move_to(to.0, to.1);
    pb.line_to(to.0 - l.0 * head, to.1 - l.1 * head);
    pb.line_to(to.0 - r.0 * head, to.1 - r.1 * head);
    pb.close();
    if let Some(tri) = pb.finish() {
        pix.fill_path(&tri, &paint(color), tiny_skia::FillRule::Winding, Transform::identity(), None);
    }
}

fn draw_text(pix: &mut Pixmap, font: &FontArc, pos: Pt, text: &str, size: f32, color: Color) {
    let scale = PxScale::from(size.min(5000.0));
    let scaled = font.as_scaled(scale);
    let pos = clamp(pos);
    let (w, h) = (pix.width() as i32, pix.height() as i32);
    let data = pix.data_mut();
    let mut caret = point(pos.0, pos.1 + scaled.ascent());
    let mut prev = None;
    for ch in text.chars() {
        if ch == '\n' {
            caret = point(pos.0, caret.y + scaled.height() + scaled.line_gap());
            prev = None;
            continue;
        }
        let id = font.glyph_id(ch);
        if let Some(p) = prev {
            caret.x += scaled.kern(p, id);
        }
        let glyph = id.with_scale_and_position(scale, caret);
        caret.x += scaled.h_advance(id);
        prev = Some(id);
        let Some(outline) = font.outline_glyph(glyph) else { continue };
        let b = outline.px_bounds();
        outline.draw(|gx, gy, cov| {
            let (x, y) = (b.min.x as i32 + gx as i32, b.min.y as i32 + gy as i32);
            if x < 0 || y < 0 || x >= w || y >= h || cov <= 0.0 {
                return;
            }
            let sa = (color[3] as f32 / 255.0) * cov.min(1.0);
            let i = ((y * w + x) * 4) as usize;
            for k in 0..3 {
                data[i + k] = (data[i + k] as f32 * (1.0 - sa) + color[k] as f32 * sa).round() as u8;
            }
            data[i + 3] = (data[i + 3] as f32 * (1.0 - sa) + 255.0 * sa).round() as u8;
        });
    }
}

/// Draws the annotations in order. Text needs `font`; without one the call fails with `Unavailable`
/// before anything is drawn.
pub fn render(img: &mut RgbaImage, annotations: &[Annotation], font: Option<&FontArc>) -> Result<()> {
    annotations.iter().try_for_each(validate)?;
    let needs_font = annotations.iter().any(|a| matches!(a, Annotation::Text { text, .. } if !text.is_empty()));
    if needs_font && font.is_none() {
        return Err(Error::Unavailable {
            feature: "Text annotations",
            reason: "no system font could be loaded".into(),
        });
    }
    if annotations.is_empty() || img.width() == 0 || img.height() == 0 {
        return Ok(());
    }
    let size = tiny_skia::IntSize::from_wh(img.width(), img.height()).ok_or_else(|| bad("empty image"))?;
    let mut pix = Pixmap::from_vec(premultiplied(img), size).ok_or_else(|| Error::Corrupt("image buffer".into()))?;
    for a in annotations {
        match a {
            Annotation::Stroke { points, color, width } => match points.as_slice() {
                [] => {}
                [only] => {
                    let c = clamp(*only);
                    let r = (width.min(MAX_WIDTH)) / 2.0;
                    if let Some(circle) =
                        SkRect::from_xywh(c.0 - r, c.1 - r, r * 2.0, r * 2.0).and_then(PathBuilder::from_oval)
                    {
                        pix.fill_path(
                            &circle,
                            &paint(*color),
                            tiny_skia::FillRule::Winding,
                            Transform::identity(),
                            None,
                        );
                    }
                }
                pts => {
                    if let Some(path) = polyline(pts) {
                        pix.stroke_path(&path, &paint(*color), &stroke(*width), Transform::identity(), None);
                    }
                }
            },
            Annotation::Line { from, to, color, width } => draw_line(&mut pix, *from, *to, *color, *width),
            Annotation::Arrow { from, to, color, width } => draw_arrow(&mut pix, *from, *to, *color, *width),
            Annotation::Rect { from, to, color, width, fill } => {
                if let Some(path) = bounds(*from, *to).map(PathBuilder::from_rect) {
                    if *fill {
                        pix.fill_path(&path, &paint(*color), tiny_skia::FillRule::Winding, Transform::identity(), None);
                    } else {
                        pix.stroke_path(&path, &paint(*color), &stroke(*width), Transform::identity(), None);
                    }
                }
            }
            Annotation::Ellipse { from, to, color, width, fill } => {
                if let Some(path) = bounds(*from, *to).and_then(PathBuilder::from_oval) {
                    if *fill {
                        pix.fill_path(&path, &paint(*color), tiny_skia::FillRule::Winding, Transform::identity(), None);
                    } else {
                        pix.stroke_path(&path, &paint(*color), &stroke(*width), Transform::identity(), None);
                    }
                }
            }
            Annotation::Highlight { from, to, color } => {
                if let Some(path) = bounds(*from, *to).map(PathBuilder::from_rect) {
                    pix.fill_path(&path, &paint(*color), tiny_skia::FillRule::Winding, Transform::identity(), None);
                }
            }
            Annotation::Text { pos, text, size, color } => {
                if let (Some(f), false) = (font, text.is_empty()) {
                    draw_text(&mut pix, f, *pos, text, *size, *color);
                }
            }
        }
    }
    *img = straight(&pix);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::super::error::Error;
    use super::*;
    use image::Rgba;

    const RED: Color = [255, 0, 0, 255];

    fn white(w: u32, h: u32) -> RgbaImage {
        RgbaImage::from_pixel(w, h, Rgba([255, 255, 255, 255]))
    }

    fn changed(a: &RgbaImage, b: &RgbaImage) -> Vec<(u32, u32)> {
        a.enumerate_pixels().filter(|(x, y, p)| b.get_pixel(*x, *y) != *p).map(|(x, y, _)| (x, y)).collect()
    }

    #[test]
    fn stroke_paints_a_solid_line_and_nothing_else() {
        let mut img = white(100, 100);
        render(
            &mut img,
            &[Annotation::Stroke { points: vec![(10.0, 50.0), (90.0, 50.0)], color: RED, width: 6.0 }],
            None,
        )
        .unwrap();
        assert_eq!(img.get_pixel(50, 50).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(50, 40).0, [255, 255, 255, 255]);
        assert_eq!(img.get_pixel(50, 60).0, [255, 255, 255, 255]);
        assert_eq!(img.get_pixel(2, 50).0, [255, 255, 255, 255], "round cap stops 3 px beyond the end");
    }

    #[test]
    fn a_single_point_stroke_is_a_dot() {
        let mut img = white(40, 40);
        render(&mut img, &[Annotation::Stroke { points: vec![(20.0, 20.0)], color: RED, width: 10.0 }], None).unwrap();
        assert_eq!(img.get_pixel(20, 20).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(30, 30).0, [255, 255, 255, 255]);
    }

    #[test]
    fn rect_outline_versus_fill() {
        let mut outline = white(60, 60);
        let r = |fill| Annotation::Rect { from: (10.0, 10.0), to: (50.0, 50.0), color: RED, width: 4.0, fill };
        render(&mut outline, &[r(false)], None).unwrap();
        assert_eq!(outline.get_pixel(10, 30).0, [255, 0, 0, 255]);
        assert_eq!(outline.get_pixel(30, 30).0, [255, 255, 255, 255], "inside stays untouched");
        let mut filled = white(60, 60);
        render(&mut filled, &[r(true)], None).unwrap();
        assert_eq!(filled.get_pixel(30, 30).0, [255, 0, 0, 255]);
        let mut swapped = white(60, 60);
        render(
            &mut swapped,
            &[Annotation::Rect { from: (50.0, 50.0), to: (10.0, 10.0), color: RED, width: 4.0, fill: true }],
            None,
        )
        .unwrap();
        assert_eq!(swapped, filled, "corner order does not matter");
    }

    #[test]
    fn ellipse_and_line_and_arrow_draw() {
        let mut img = white(80, 80);
        render(
            &mut img,
            &[Annotation::Ellipse { from: (10.0, 10.0), to: (70.0, 70.0), color: RED, width: 3.0, fill: true }],
            None,
        )
        .unwrap();
        assert_eq!(img.get_pixel(40, 40).0, [255, 0, 0, 255]);
        assert_eq!(img.get_pixel(11, 11).0, [255, 255, 255, 255], "bounding-box corner is outside the ellipse");
        let mut l = white(50, 50);
        render(&mut l, &[Annotation::Line { from: (5.0, 25.0), to: (45.0, 25.0), color: RED, width: 4.0 }], None)
            .unwrap();
        assert_eq!(l.get_pixel(25, 25).0, [255, 0, 0, 255]);
        let mut a = white(50, 50);
        render(&mut a, &[Annotation::Arrow { from: (5.0, 25.0), to: (45.0, 25.0), color: RED, width: 3.0 }], None)
            .unwrap();
        assert_eq!(a.get_pixel(25, 25).0, [255, 0, 0, 255]);
        assert_ne!(a.get_pixel(36, 21).0, [255, 255, 255, 255], "arrow head flares out above the shaft");
        assert_ne!(changed(&a, &l).len(), 0);
    }

    #[test]
    fn highlight_blends_with_what_is_underneath() {
        let mut img = RgbaImage::from_pixel(20, 20, Rgba([0, 0, 255, 255]));
        render(
            &mut img,
            &[Annotation::Highlight { from: (0.0, 0.0), to: (20.0, 20.0), color: [255, 255, 0, 128] }],
            None,
        )
        .unwrap();
        let p = img.get_pixel(10, 10).0;
        assert!((120..136).contains(&p[0]) && (120..136).contains(&p[1]) && (118..134).contains(&p[2]), "{p:?}");
        assert_eq!(p[3], 255);
    }

    #[test]
    fn later_annotations_paint_over_earlier_ones() {
        let mut img = white(30, 30);
        let blue = [0, 0, 255, 255];
        let a = |c| Annotation::Rect { from: (0.0, 0.0), to: (30.0, 30.0), color: c, width: 1.0, fill: true };
        render(&mut img, &[a(RED), a(blue)], None).unwrap();
        assert_eq!(img.get_pixel(15, 15).0, blue);
    }

    #[test]
    fn shapes_outside_the_image_and_huge_widths_do_not_panic() {
        let mut img = white(20, 20);
        let anns = [
            Annotation::Line { from: (-500.0, -500.0), to: (900.0, 900.0), color: RED, width: 3.0 },
            Annotation::Rect { from: (100.0, 100.0), to: (200.0, 200.0), color: RED, width: 2.0, fill: true },
            Annotation::Stroke { points: vec![(5.0, 5.0), (15.0, 15.0)], color: RED, width: 5000.0 },
        ];
        render(&mut img, &anns, None).unwrap();
        assert_eq!(img.get_pixel(0, 0).0, [255, 0, 0, 255]);
    }

    #[test]
    fn invalid_numbers_are_rejected_before_drawing() {
        let mut img = white(10, 10);
        let before = img.clone();
        for bad in [
            Annotation::Line { from: (f32::NAN, 0.0), to: (1.0, 1.0), color: RED, width: 1.0 },
            Annotation::Line { from: (0.0, 0.0), to: (f32::INFINITY, 1.0), color: RED, width: 1.0 },
            Annotation::Line { from: (0.0, 0.0), to: (1.0, 1.0), color: RED, width: 0.0 },
            Annotation::Line { from: (0.0, 0.0), to: (1.0, 1.0), color: RED, width: -2.0 },
            Annotation::Text { pos: (0.0, 0.0), text: "x".into(), size: 0.0, color: RED },
        ] {
            let good = Annotation::Rect { from: (0.0, 0.0), to: (10.0, 10.0), color: RED, width: 1.0, fill: true };
            assert!(matches!(render(&mut img, &[good, bad], None), Err(Error::InvalidArgument(_))));
            assert_eq!(img, before, "nothing is drawn when any annotation is invalid");
        }
    }

    #[test]
    fn text_needs_a_font_and_draws_inside_its_box() {
        let t = Annotation::Text { pos: (10.0, 10.0), text: "Hola Ñ".into(), size: 24.0, color: [0, 0, 0, 255] };
        let mut img = white(160, 60);
        let before = img.clone();
        assert!(matches!(render(&mut img, std::slice::from_ref(&t), None), Err(Error::Unavailable { .. })));
        assert_eq!(img, before);
        let font = load_system_font().expect("Windows ships Segoe UI / Arial");
        render(&mut img, &[t], Some(&font)).unwrap();
        let diff = changed(&img, &before);
        assert!(diff.len() > 100, "text should cover many pixels, got {}", diff.len());
        assert!(diff.iter().all(|&(x, y)| x >= 8 && y >= 8 && x < 150 && y < 52), "text stays near its anchor");
        let mut again = before.clone();
        render(
            &mut again,
            &[Annotation::Text { pos: (10.0, 10.0), text: "Hola Ñ".into(), size: 24.0, color: [0, 0, 0, 255] }],
            Some(&font),
        )
        .unwrap();
        assert_eq!(again, img, "rendering is deterministic");
    }

    #[test]
    fn empty_and_multiline_text() {
        let font = load_system_font().unwrap();
        let mut img = white(100, 100);
        let before = img.clone();
        render(
            &mut img,
            &[Annotation::Text { pos: (5.0, 5.0), text: String::new(), size: 20.0, color: RED }],
            Some(&font),
        )
        .unwrap();
        assert_eq!(img, before);
        render(
            &mut img,
            &[Annotation::Text { pos: (5.0, 5.0), text: "a\nb".into(), size: 20.0, color: RED }],
            Some(&font),
        )
        .unwrap();
        let rows: Vec<u32> = changed(&img, &before).iter().map(|p| p.1).collect();
        assert!(rows.iter().any(|&y| y > 30), "the second line is below the first");
    }

    #[test]
    fn scaling_multiplies_coordinates_widths_and_font_sizes_and_keeps_colours() {
        let a = Annotation::Arrow { from: (10.0, 20.0), to: (30.0, 40.0), color: [1, 2, 3, 4], width: 6.0 };
        assert_eq!(
            a.scaled(0.5),
            Annotation::Arrow { from: (5.0, 10.0), to: (15.0, 20.0), color: [1, 2, 3, 4], width: 3.0 }
        );
        let t = Annotation::Text { pos: (8.0, 8.0), text: "hi".into(), size: 20.0, color: [9, 9, 9, 255] };
        assert_eq!(
            t.scaled(2.0),
            Annotation::Text { pos: (16.0, 16.0), text: "hi".into(), size: 40.0, color: [9, 9, 9, 255] }
        );
        let s = Annotation::Stroke { points: vec![(1.0, 2.0), (3.0, 4.0)], color: [0, 0, 0, 255], width: 2.0 };
        assert_eq!(
            s.scaled(3.0),
            Annotation::Stroke { points: vec![(3.0, 6.0), (9.0, 12.0)], color: [0, 0, 0, 255], width: 6.0 }
        );
    }
}
