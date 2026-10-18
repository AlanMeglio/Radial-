//! Pure hit-testing math for the radial menu. No Win32 here, so everything is unit tested.

use std::f32::consts::TAU;

/// Angle (radians, clockwise, 0 = straight up) of the centre of sector `i` out of `n`.
pub fn sector_angle(i: usize, n: usize) -> f32 {
    TAU * i as f32 / n as f32
}

/// Sector under the offset `(dx, dy)` from the menu centre (screen axes, y grows down).
/// `None` inside the dead zone (`r < inner`) or beyond `outer`.
pub fn sector_at(dx: f32, dy: f32, n: usize, inner: f32, outer: f32) -> Option<usize> {
    let r = dx.hypot(dy);
    if n == 0 || r < inner || r > outer {
        return None;
    }
    // atan2(dx, -dy) is 0 for "up" and grows clockwise.
    let a = dx.atan2(-dy).rem_euclid(TAU);
    let step = TAU / n as f32;
    Some((((a + step / 2.0) / step) as usize) % n)
}

/// Top-left window origin so a `size` square centred on `c` stays inside `[lo, hi)`.
pub fn clamp_origin(c: i32, size: i32, lo: i32, hi: i32) -> i32 {
    (c - size / 2).min(hi - size).max(lo)
}

/// Logical-pixel layout of the whole overlay for a given scale and ring thickness.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Metrics {
    /// Side of the square canvas.
    pub size: f32,
    pub center: f32,
    /// Radius of the central hub; also the dead zone for hit-testing.
    pub hub: f32,
    pub r_in: f32,
    pub r_out: f32,
    /// Outer radius that still counts as hovering a sector.
    pub hit_out: f32,
}

impl Metrics {
    /// `scale` is the user's size setting, `ring` the sector thickness at scale 1.
    pub fn new(scale: f32, ring: f32) -> Metrics {
        let hub = 50.0 * scale;
        let r_in = 70.0 * scale;
        let r_out = r_in + ring * scale;
        let hit_out = r_out + 34.0 * scale;
        let center = r_out + 76.0 * scale;
        Metrics { size: center * 2.0, center, hub, r_in, r_out, hit_out }
    }
}

/// Where the centre of a rounded corner sits, as an angle clockwise from the sector bisector.
///
/// The corner circle has radius `c` and touches the sector's left edge (the ray at `-half`, pushed `half_gap`
/// towards the bisector) and the arc at `radius`: from inside for the outer arc, from outside for the inner one.
/// The right corner is the mirror image.
pub fn corner_center_angle(radius: f32, c: f32, half_gap: f32, half: f32, outer: bool) -> f32 {
    let rho = if outer { radius - c } else { radius + c };
    ((half_gap + c) / rho).clamp(-1.0, 1.0).asin() - half
}

/// Largest usable corner radius so corners never cross each other or the opposite arc.
pub fn clamp_corner(c: f32, half_gap: f32, r_in: f32, r_out: f32, half: f32) -> f32 {
    let by_depth = (r_out - r_in) / 2.0 - 0.5;
    // Inner corners must not meet: (g + c) <= (r_in + c) sin(h).
    let s = half.sin().min(0.98);
    let by_width = (r_in * s - half_gap) / (1.0 - s);
    c.min(by_depth).min(by_width).max(0.0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn four_sectors_follow_the_compass() {
        assert_eq!(sector_at(0.0, -100.0, 4, 30.0, 200.0), Some(0)); // up
        assert_eq!(sector_at(100.0, 0.0, 4, 30.0, 200.0), Some(1)); // right
        assert_eq!(sector_at(0.0, 100.0, 4, 30.0, 200.0), Some(2)); // down
        assert_eq!(sector_at(-100.0, 0.0, 4, 30.0, 200.0), Some(3)); // left
    }

    #[test]
    fn eight_sectors_are_centred_on_their_angle() {
        // 20 degrees clockwise of up is still sector 0 (it spans +-22.5), 25 is sector 1.
        let at = |deg: f32| {
            let r = deg.to_radians();
            sector_at(100.0 * r.sin(), -100.0 * r.cos(), 8, 30.0, 200.0)
        };
        assert_eq!(at(20.0), Some(0));
        assert_eq!(at(25.0), Some(1));
        assert_eq!(at(-20.0), Some(0));
        assert_eq!(at(-25.0), Some(7));
        assert_eq!(at(180.0), Some(4));
    }

    #[test]
    fn dead_zone_and_outside_are_none() {
        assert_eq!(sector_at(0.0, 0.0, 4, 30.0, 200.0), None);
        assert_eq!(sector_at(10.0, 10.0, 4, 30.0, 200.0), None);
        assert_eq!(sector_at(0.0, -250.0, 4, 30.0, 200.0), None);
        assert_eq!(sector_at(0.0, -30.0, 4, 30.0, 200.0), Some(0)); // edge is inside
    }

    #[test]
    fn zero_sectors_never_hit() {
        assert_eq!(sector_at(0.0, -100.0, 0, 30.0, 200.0), None);
    }

    #[test]
    fn sector_angles_are_evenly_spaced() {
        assert!((sector_angle(0, 4)).abs() < 1e-6);
        assert!((sector_angle(1, 4) - TAU / 4.0).abs() < 1e-6);
        assert!((sector_angle(3, 8) - 3.0 * TAU / 8.0).abs() < 1e-6);
    }

    #[test]
    fn clamp_keeps_window_on_screen() {
        assert_eq!(clamp_origin(500, 100, 0, 1000), 450); // centred
        assert_eq!(clamp_origin(10, 100, 0, 1000), 0); // left edge
        assert_eq!(clamp_origin(995, 100, 0, 1000), 900); // right edge
        assert_eq!(clamp_origin(50, 100, 200, 1000), 200); // second monitor offset
    }

    #[test]
    fn metrics_scale_linearly_and_contain_the_hit_area() {
        let a = Metrics::new(1.0, 100.0);
        let b = Metrics::new(2.0, 100.0);
        assert!((b.r_out - 2.0 * a.r_out).abs() < 1e-3 && (b.size - 2.0 * a.size).abs() < 1e-3);
        assert!(a.hub < a.r_in && a.r_in < a.r_out && a.r_out < a.hit_out);
        assert!(a.center - a.hit_out > 20.0, "room left for the shadow and orbit ring");
        assert_eq!(a.center * 2.0, a.size);
        let thin = Metrics::new(1.0, 60.0);
        assert!((thin.r_out - thin.r_in - 60.0).abs() < 1e-3);
    }

    #[test]
    fn corner_circles_touch_the_edge_and_the_arc() {
        let (half, half_gap, c) = (std::f32::consts::PI / 8.0, 2.5, 10.0);
        for (radius, outer) in [(170.0f32, true), (70.0, false)] {
            let rho = if outer { radius - c } else { radius + c };
            let th = corner_center_angle(radius, c, half_gap, half, outer);
            let (cx, cy) = (rho * th.sin(), rho * th.cos());
            // Left edge: inward normal is (cos h, sin h), the edge line sits at half_gap along it.
            let dist = cx * half.cos() + cy * half.sin() - half_gap;
            assert!((dist - c).abs() < 1e-3, "edge distance {dist}");
        }
    }

    #[test]
    fn corner_is_clamped_to_what_fits() {
        let half = std::f32::consts::PI / 8.0;
        let c = clamp_corner(500.0, 2.5, 70.0, 170.0, half);
        assert!(c <= (170.0 - 70.0) / 2.0);
        assert_eq!(clamp_corner(3.0, 2.5, 70.0, 170.0, half), 3.0);
        assert_eq!(clamp_corner(-4.0, 2.5, 70.0, 170.0, half), 0.0);
        // Many thin sectors leave little room at the inner radius.
        let thin = clamp_corner(40.0, 6.0, 40.0, 140.0, std::f32::consts::PI / 24.0);
        assert!(thin < 40.0);
    }
}
