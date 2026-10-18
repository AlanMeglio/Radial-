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
}
