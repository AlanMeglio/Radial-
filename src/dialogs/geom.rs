//! Pure geometry for the dialogs: the image-to-canvas mapping and the crop rectangle with its handles.
//! Everything here is integer image pixels or plain floats, so it is unit tested without a window.

use crate::engine::crop::{constrain, Aspect, Keep, Rect};

/// Smallest side the crop rectangle can be dragged down to, in image pixels.
pub const MIN_SIDE: i64 = 8;

/// Where an image sits inside its canvas: uniform scale plus the top-left offset, all in canvas units.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Fit {
    pub x: f32,
    pub y: f32,
    pub scale: f32,
}

impl Fit {
    /// Fits an `iw` x `ih` image into `canvas` (`[x, y, w, h]`) with `pad` around it, never enlarging past 1:1
    /// unless `upscale` is set.
    pub fn new(canvas: [f32; 4], iw: u32, ih: u32, pad: f32, upscale: bool) -> Fit {
        let (aw, ah) = ((canvas[2] - 2.0 * pad).max(1.0), (canvas[3] - 2.0 * pad).max(1.0));
        let (iw, ih) = (iw.max(1) as f32, ih.max(1) as f32);
        let mut scale = (aw / iw).min(ah / ih);
        if !upscale {
            scale = scale.min(1.0);
        }
        Fit { x: canvas[0] + (canvas[2] - iw * scale) / 2.0, y: canvas[1] + (canvas[3] - ih * scale) / 2.0, scale }
    }

    pub fn to_screen(self, ix: f32, iy: f32) -> (f32, f32) {
        (self.x + ix * self.scale, self.y + iy * self.scale)
    }

    pub fn to_image(self, sx: f32, sy: f32) -> (f32, f32) {
        ((sx - self.x) / self.scale, (sy - self.y) / self.scale)
    }
}

/// Which part of the crop rectangle the pointer grabbed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Handle {
    Nw,
    N,
    Ne,
    E,
    Se,
    S,
    Sw,
    W,
    Move,
}

/// What is under the pointer for a rectangle given in screen coordinates as `[left, top, right, bottom]`.
/// Corners win over edges, edges over the inside; `tol` is the grab radius.
pub fn hit_handle(r: [f32; 4], px: f32, py: f32, tol: f32) -> Option<Handle> {
    let [l, t, rr, b] = r;
    let near = |a: f32, b: f32| (a - b).abs() <= tol;
    let (on_l, on_r, on_t, on_b) = (near(px, l), near(px, rr), near(py, t), near(py, b));
    let within_x = px >= l - tol && px <= rr + tol;
    let within_y = py >= t - tol && py <= b + tol;
    match (on_l, on_r, on_t, on_b) {
        (true, _, true, _) if within_x && within_y => Some(Handle::Nw),
        (_, true, true, _) if within_x && within_y => Some(Handle::Ne),
        (true, _, _, true) if within_x && within_y => Some(Handle::Sw),
        (_, true, _, true) if within_x && within_y => Some(Handle::Se),
        _ if on_t && within_x => Some(Handle::N),
        _ if on_b && within_x => Some(Handle::S),
        _ if on_l && within_y => Some(Handle::W),
        _ if on_r && within_y => Some(Handle::E),
        _ if px > l && px < rr && py > t && py < b => Some(Handle::Move),
        _ => None,
    }
}

/// The rectangle after dragging `handle` by `(dx, dy)` image pixels from where the drag started, kept inside the
/// `bw` x `bh` image and (for a ratio) locked to `aspect` with the opposite corner or edge staying put.
pub fn drag(start: Rect, handle: Handle, dx: i64, dy: i64, aspect: Aspect, bw: u32, bh: u32) -> Rect {
    let (bw, bh) = (bw as i64, bh as i64);
    let (l, t) = (start.x as i64, start.y as i64);
    let (r, b) = (l + start.w as i64, t + start.h as i64);
    let made =
        |x: i64, y: i64, w: i64, h: i64| Rect::new(x.max(0) as u32, y.max(0) as u32, w.max(1) as u32, h.max(1) as u32);
    if handle == Handle::Move {
        let x = (l + dx).clamp(0, (bw - start.w as i64).max(0));
        let y = (t + dy).clamp(0, (bh - start.h as i64).max(0));
        return made(x, y, start.w as i64, start.h as i64);
    }
    let left = matches!(handle, Handle::Nw | Handle::W | Handle::Sw);
    let right = matches!(handle, Handle::Ne | Handle::E | Handle::Se);
    let top = matches!(handle, Handle::Nw | Handle::N | Handle::Ne);
    let bottom = matches!(handle, Handle::Sw | Handle::S | Handle::Se);
    let corner = (left || right) && (top || bottom);
    let Aspect::Ratio(rw, rh) = aspect else {
        let (mut nl, mut nt, mut nr, mut nb) = (l, t, r, b);
        if left {
            nl = (l + dx).clamp(0, (r - MIN_SIDE).max(0));
        }
        if right {
            nr = (r + dx).clamp((l + MIN_SIDE).min(bw), bw);
        }
        if top {
            nt = (t + dy).clamp(0, (b - MIN_SIDE).max(0));
        }
        if bottom {
            nb = (b + dy).clamp((t + MIN_SIDE).min(bh), bh);
        }
        return made(nl, nt, nr - nl, nb - nt);
    };
    let (rw, rh) = (rw.max(1) as i64, rh.max(1) as i64);
    if corner {
        let (ax, ay) = (if left { r } else { l }, if top { b } else { t });
        let (cx, cy) = (if left { l } else { r } + dx, if top { t } else { b } + dy);
        let (sx, sy) = (if left { -1 } else { 1 }, if top { -1 } else { 1 });
        let (max_w, max_h) = (if sx > 0 { bw - ax } else { ax }, if sy > 0 { bh - ay } else { ay });
        let wmax = (sx * (cx - ax)).max(MIN_SIDE).min(max_w.max(1));
        let hmax = (sy * (cy - ay)).max(MIN_SIDE).min(max_h.max(1));
        let (w, h) = fit_ratio(wmax, hmax, rw, rh);
        return made(if sx > 0 { ax } else { ax - w }, if sy > 0 { ay } else { ay - h }, w, h);
    }
    if left || right {
        let w = if right {
            (r + dx - l).clamp(MIN_SIDE, (bw - l).max(MIN_SIDE))
        } else {
            (r - (l + dx)).clamp(MIN_SIDE, r.max(MIN_SIDE))
        };
        let (w, h) = if w * rh / rw > bh { (bh * rw / rh, bh) } else { (w, (w * rh / rw).max(1)) };
        let y = ((t + b) / 2 - h / 2).clamp(0, (bh - h).max(0));
        made(if right { l } else { r - w }, y, w, h)
    } else {
        let h = if bottom {
            (b + dy - t).clamp(MIN_SIDE, (bh - t).max(MIN_SIDE))
        } else {
            (b - (t + dy)).clamp(MIN_SIDE, b.max(MIN_SIDE))
        };
        let (w, h) = if h * rw / rh > bw { (bw, bw * rh / rw) } else { ((h * rw / rh).max(1), h) };
        let x = ((l + r) / 2 - w / 2).clamp(0, (bw - w).max(0));
        made(x, if bottom { t } else { b - h }, w, h)
    }
}

/// A new rectangle dragged out from corner `a` to the pointer `p`: free, or locked to the ratio with the anchor
/// corner fixed and the box shrunk to fit inside the image. `None` while it is still a click.
pub fn ratio_from_anchor(a: (i64, i64), p: (i64, i64), aspect: Aspect, bw: u32, bh: u32) -> Option<Rect> {
    let Aspect::Ratio(rw, rh) = aspect else { return from_points(a, p, bw, bh) };
    let (sx, sy) = (if p.0 >= a.0 { 1 } else { -1 }, if p.1 >= a.1 { 1 } else { -1 });
    let (max_w, max_h) = (if sx > 0 { bw as i64 - a.0 } else { a.0 }, if sy > 0 { bh as i64 - a.1 } else { a.1 });
    let (wmax, hmax) = ((p.0 - a.0).abs().min(max_w), (p.1 - a.1).abs().min(max_h));
    if wmax < 1 || hmax < 1 {
        return None;
    }
    let (w, h) = fit_ratio(wmax, hmax, rw as i64, rh as i64);
    let (x, y) = (if sx > 0 { a.0 } else { a.0 - w }, if sy > 0 { a.1 } else { a.1 - h });
    Some(Rect::new(x.max(0) as u32, y.max(0) as u32, w as u32, h as u32))
}

/// Largest `rw:rh` box inside `wmax` x `hmax` (both at least 1).
fn fit_ratio(wmax: i64, hmax: i64, rw: i64, rh: i64) -> (i64, i64) {
    let (rw, rh) = (rw.max(1), rh.max(1));
    if wmax * rh <= hmax * rw {
        (wmax, (wmax * rh / rw).max(1))
    } else {
        ((hmax * rw / rh).max(1), hmax)
    }
}

/// Largest rectangle of `aspect` that fits inside `rect`, sharing its centre. `Free` leaves it unchanged.
pub fn fit_aspect_inside(rect: Rect, aspect: Aspect) -> Rect {
    let Aspect::Ratio(rw, rh) = aspect else { return rect };
    let (rw, rh) = (rw.max(1) as u64, rh.max(1) as u64);
    let (bw, bh) = (rect.w as u64, rect.h as u64);
    let (w, h) = if bw * rh <= bh * rw { (bw, (bw * rh / rw).max(1)) } else { ((bh * rw / rh).max(1), bh) };
    Rect::new(rect.x + ((bw - w.min(bw)) / 2) as u32, rect.y + ((bh - h.min(bh)) / 2) as u32, w as u32, h as u32)
}

/// `rect` with its width set to `w` (the height follows a ratio), clamped to the image.
pub fn with_width(rect: Rect, w: u32, aspect: Aspect, bw: u32, bh: u32) -> Rect {
    constrain(Rect { w: w.max(1), ..rect }, aspect, Keep::Width, bw, bh)
}

/// `rect` with its height set to `h` (the width follows a ratio), clamped to the image.
pub fn with_height(rect: Rect, h: u32, aspect: Aspect, bw: u32, bh: u32) -> Rect {
    constrain(Rect { h: h.max(1), ..rect }, aspect, Keep::Height, bw, bh)
}

/// Rectangle spanned by two image points (any order), clamped to the image; `None` when it is a click.
pub fn from_points(a: (i64, i64), b: (i64, i64), bw: u32, bh: u32) -> Option<Rect> {
    let (l, r) = (a.0.min(b.0).clamp(0, bw as i64), a.0.max(b.0).clamp(0, bw as i64));
    let (t, bt) = (a.1.min(b.1).clamp(0, bh as i64), a.1.max(b.1).clamp(0, bh as i64));
    (r > l && bt > t).then(|| Rect::new(l as u32, t as u32, (r - l) as u32, (bt - t) as u32))
}

#[cfg(test)]
mod tests {
    use super::*;

    const FREE: Aspect = Aspect::Free;
    const SQ: Aspect = Aspect::Ratio(1, 1);
    const WIDE: Aspect = Aspect::Ratio(16, 9);

    fn r(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect::new(x, y, w, h)
    }

    #[test]
    fn fit_centres_the_image_and_scales_down_only() {
        let f = Fit::new([0.0, 0.0, 400.0, 200.0], 800, 200, 0.0, false);
        assert_eq!((f.scale, f.x, f.y), (0.5, 0.0, 50.0));
        let small = Fit::new([10.0, 20.0, 400.0, 200.0], 100, 50, 0.0, false);
        assert_eq!(small.scale, 1.0, "never enlarged");
        assert_eq!((small.x, small.y), (160.0, 95.0));
        let up = Fit::new([0.0, 0.0, 400.0, 200.0], 100, 50, 0.0, true);
        assert_eq!(up.scale, 4.0);
        let padded = Fit::new([0.0, 0.0, 400.0, 200.0], 400, 200, 20.0, false);
        assert!((padded.scale - 0.8).abs() < 1e-6);
    }

    #[test]
    fn screen_and_image_points_round_trip() {
        let f = Fit::new([30.0, 40.0, 300.0, 300.0], 1000, 500, 0.0, false);
        let (sx, sy) = f.to_screen(250.0, 125.0);
        let (ix, iy) = f.to_image(sx, sy);
        assert!((ix - 250.0).abs() < 1e-3 && (iy - 125.0).abs() < 1e-3);
        assert_eq!(f.to_image(f.x, f.y), (0.0, 0.0));
    }

    #[test]
    fn handles_are_found_corners_first_then_edges_then_inside() {
        let rect = [100.0, 100.0, 300.0, 250.0];
        let tol = 8.0;
        assert_eq!(hit_handle(rect, 102.0, 98.0, tol), Some(Handle::Nw));
        assert_eq!(hit_handle(rect, 300.0, 250.0, tol), Some(Handle::Se));
        assert_eq!(hit_handle(rect, 300.0, 100.0, tol), Some(Handle::Ne));
        assert_eq!(hit_handle(rect, 100.0, 250.0, tol), Some(Handle::Sw));
        assert_eq!(hit_handle(rect, 200.0, 103.0, tol), Some(Handle::N));
        assert_eq!(hit_handle(rect, 200.0, 252.0, tol), Some(Handle::S));
        assert_eq!(hit_handle(rect, 97.0, 180.0, tol), Some(Handle::W));
        assert_eq!(hit_handle(rect, 303.0, 180.0, tol), Some(Handle::E));
        assert_eq!(hit_handle(rect, 200.0, 180.0, tol), Some(Handle::Move));
        assert_eq!(hit_handle(rect, 50.0, 50.0, tol), None);
        assert_eq!(hit_handle(rect, 320.0, 180.0, tol), None);
    }

    #[test]
    fn a_free_corner_drag_moves_only_that_corner() {
        let out = drag(r(100, 100, 200, 150), Handle::Se, 30, -20, FREE, 1000, 800);
        assert_eq!(out, r(100, 100, 230, 130));
        let out = drag(r(100, 100, 200, 150), Handle::Nw, 40, 10, FREE, 1000, 800);
        assert_eq!(out, r(140, 110, 160, 140));
    }

    #[test]
    fn a_free_edge_drag_moves_one_side() {
        assert_eq!(drag(r(100, 100, 200, 150), Handle::E, 50, 99, FREE, 1000, 800), r(100, 100, 250, 150));
        assert_eq!(drag(r(100, 100, 200, 150), Handle::N, 99, -40, FREE, 1000, 800), r(100, 60, 200, 190));
        assert_eq!(drag(r(100, 100, 200, 150), Handle::W, -60, 0, FREE, 1000, 800), r(40, 100, 260, 150));
    }

    #[test]
    fn dragging_never_leaves_the_image() {
        assert_eq!(drag(r(100, 100, 200, 150), Handle::Se, 5000, 5000, FREE, 1000, 800), r(100, 100, 900, 700));
        assert_eq!(drag(r(100, 100, 200, 150), Handle::Nw, -5000, -5000, FREE, 1000, 800), r(0, 0, 300, 250));
        assert_eq!(drag(r(100, 100, 200, 150), Handle::Move, 5000, 5000, FREE, 1000, 800), r(800, 650, 200, 150));
        assert_eq!(drag(r(100, 100, 200, 150), Handle::Move, -5000, -5000, FREE, 1000, 800), r(0, 0, 200, 150));
    }

    #[test]
    fn dragging_past_the_opposite_side_stops_at_the_minimum_size() {
        let out = drag(r(100, 100, 200, 150), Handle::E, -5000, 0, FREE, 1000, 800);
        assert_eq!((out.x, out.w), (100, MIN_SIDE as u32));
        let out = drag(r(100, 100, 200, 150), Handle::Nw, 5000, 5000, FREE, 1000, 800);
        assert_eq!((out.w, out.h), (MIN_SIDE as u32, MIN_SIDE as u32));
        assert_eq!((out.x + out.w, out.y + out.h), (300, 250), "the opposite corner stays put");
    }

    #[test]
    fn a_ratio_corner_drag_keeps_the_ratio_and_the_opposite_corner() {
        let out = drag(r(100, 100, 200, 200), Handle::Se, 100, 30, SQ, 1000, 800);
        assert_eq!((out.x, out.y), (100, 100));
        assert_eq!(out.w, out.h);
        assert_eq!(out.w, 230, "the smaller of the two pointer extents decides");
        let out = drag(r(100, 100, 320, 180), Handle::Nw, 20, 0, WIDE, 1000, 800);
        assert_eq!((out.x + out.w, out.y + out.h), (420, 280));
        assert!(out.w as i64 * 9 - out.h as i64 * 16 <= 16 && out.h as i64 * 16 - out.w as i64 * 9 <= 16);
    }

    #[test]
    fn a_ratio_drag_shrinks_to_fit_inside_the_image() {
        let out = drag(r(600, 100, 200, 200), Handle::Se, 900, 900, SQ, 1000, 800);
        assert_eq!((out.x, out.y), (600, 100));
        assert_eq!(out.w, 400, "limited by the 1000 px width");
        assert_eq!(out.w, out.h);
    }

    #[test]
    fn a_ratio_edge_drag_grows_the_other_side_around_the_centre() {
        let out = drag(r(100, 100, 200, 200), Handle::E, 100, 0, SQ, 1000, 800);
        assert_eq!((out.x, out.w, out.h), (100, 300, 300));
        assert_eq!(out.y, 50, "stays centred on its old vertical middle");
        let out = drag(r(100, 100, 200, 200), Handle::S, 0, 100, SQ, 1000, 800);
        assert_eq!((out.y, out.w, out.h), (100, 300, 300));
        assert_eq!(out.x, 50);
    }

    #[test]
    fn an_aspect_chosen_later_fits_inside_the_current_rectangle() {
        assert_eq!(fit_aspect_inside(r(10, 10, 400, 300), FREE), r(10, 10, 400, 300));
        let sq = fit_aspect_inside(r(10, 10, 400, 300), SQ);
        assert_eq!(sq, r(60, 10, 300, 300));
        let wide = fit_aspect_inside(r(0, 0, 300, 400), WIDE);
        assert_eq!((wide.w, wide.h), (300, 168));
        assert_eq!(wide.y, 116, "centred");
        let tall = fit_aspect_inside(r(0, 0, 400, 300), Aspect::Ratio(3, 4));
        assert_eq!((tall.w, tall.h), (225, 300));
    }

    #[test]
    fn typed_sizes_follow_the_aspect_and_stay_inside() {
        assert_eq!(with_width(r(0, 0, 200, 100), 300, FREE, 1000, 800), r(0, 0, 300, 100));
        assert_eq!(with_width(r(0, 0, 200, 100), 320, WIDE, 1000, 800), r(0, 0, 320, 180));
        assert_eq!(with_height(r(0, 0, 200, 100), 180, WIDE, 1000, 800), r(0, 0, 320, 180));
        let big = with_width(r(900, 0, 50, 50), 500, SQ, 1000, 800);
        assert_eq!((big.x, big.w, big.h), (900, 100, 100), "shrunk to what fits");
        assert_eq!(with_width(r(0, 0, 200, 100), 0, FREE, 1000, 800).w, 1, "never zero");
    }

    #[test]
    fn a_new_rectangle_with_a_ratio_grows_from_the_anchor_in_any_direction() {
        let sq = ratio_from_anchor((100, 100), (180, 150), SQ, 1000, 800).unwrap();
        assert_eq!(sq, r(100, 100, 50, 50), "the smaller extent decides");
        let up_left = ratio_from_anchor((300, 300), (200, 100), WIDE, 1000, 800).unwrap();
        assert_eq!((up_left.x + up_left.w, up_left.y + up_left.h), (300, 300), "anchor stays the bottom-right corner");
        assert_eq!((up_left.w, up_left.h), (100, 56));
        let clamped = ratio_from_anchor((900, 700), (5000, 5000), SQ, 1000, 800).unwrap();
        assert_eq!(clamped, r(900, 700, 100, 100));
        assert_eq!(ratio_from_anchor((10, 10), (10, 90), SQ, 100, 100), None);
        assert_eq!(ratio_from_anchor((50, 50), (20, 90), FREE, 100, 100), Some(r(20, 50, 30, 40)));
    }

    #[test]
    fn a_rectangle_from_two_points_is_normalised_and_clamped() {
        assert_eq!(from_points((50, 60), (10, 20), 100, 100), Some(r(10, 20, 40, 40)));
        assert_eq!(from_points((-20, -5), (500, 500), 100, 80), Some(r(0, 0, 100, 80)));
        assert_eq!(from_points((10, 10), (10, 40), 100, 100), None, "a click or a line is not a rectangle");
    }
}
