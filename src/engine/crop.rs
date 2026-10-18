//! Crop geometry (aspect ratios, constrained rectangles) and the crop itself.

use super::error::{Error, Result};
use image::DynamicImage;

/// A pixel rectangle; `x`/`y` is the top-left corner.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rect {
    pub x: u32,
    pub y: u32,
    pub w: u32,
    pub h: u32,
}

impl Rect {
    pub fn new(x: u32, y: u32, w: u32, h: u32) -> Rect {
        Rect { x, y, w, h }
    }

    pub fn is_empty(&self) -> bool {
        self.w == 0 || self.h == 0
    }

    /// The part of the rectangle inside a `bw` x `bh` image, or `None` when nothing is left.
    pub fn clamp_to(&self, bw: u32, bh: u32) -> Option<Rect> {
        if self.is_empty() || self.x >= bw || self.y >= bh {
            return None;
        }
        Some(Rect { x: self.x, y: self.y, w: self.w.min(bw - self.x), h: self.h.min(bh - self.y) })
    }
}

/// The aspect choices of the crop dialog.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Aspect {
    Free,
    /// Width : height.
    Ratio(u32, u32),
}

impl Aspect {
    pub const ALL: [Aspect; 6] = [
        Aspect::Free,
        Aspect::Ratio(1, 1),
        Aspect::Ratio(16, 9),
        Aspect::Ratio(9, 16),
        Aspect::Ratio(4, 3),
        Aspect::Ratio(3, 4),
    ];

    pub fn label(self) -> String {
        match self {
            Aspect::Free => "Free".into(),
            Aspect::Ratio(w, h) => format!("{w}:{h}"),
        }
    }

    pub fn from_label(s: &str) -> Option<Aspect> {
        Aspect::ALL.into_iter().find(|a| a.label().eq_ignore_ascii_case(s.trim()))
    }

    /// Width divided by height; `None` for free.
    pub fn value(self) -> Option<f64> {
        match self {
            Aspect::Free => None,
            Aspect::Ratio(w, h) => Some(w as f64 / h as f64),
        }
    }
}

/// Largest centered rectangle of the given aspect inside the image (what the dialog selects first).
pub fn largest_centered(bw: u32, bh: u32, aspect: Aspect) -> Rect {
    let Aspect::Ratio(rw, rh) = aspect else { return Rect::new(0, 0, bw, bh) };
    if bw == 0 || bh == 0 {
        return Rect::new(0, 0, 0, 0);
    }
    let (rw, rh) = (rw as u64, rh as u64);
    let (w, h) = if bw as u64 * rh >= bh as u64 * rw {
        ((bh as u64 * rw / rh) as u32, bh)
    } else {
        (bw, (bw as u64 * rh / rw) as u32)
    };
    Rect::new((bw - w) / 2, (bh - h) / 2, w, h)
}

/// Which side the user is dragging, i.e. which one is kept when the other is derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Keep {
    Width,
    Height,
}

/// Forces `rect` to `aspect` (deriving one side from the other) and shrinks it, keeping the ratio, until it
/// fits inside the image. The top-left corner stays put unless it lies outside the image.
pub fn constrain(rect: Rect, aspect: Aspect, keep: Keep, bw: u32, bh: u32) -> Rect {
    if bw == 0 || bh == 0 {
        return Rect::new(0, 0, 0, 0);
    }
    let x = rect.x.min(bw - 1);
    let y = rect.y.min(bh - 1);
    let (max_w, max_h) = (bw - x, bh - y);
    let (mut w, mut h) = (rect.w.max(1), rect.h.max(1));
    match aspect {
        Aspect::Free => {
            w = w.min(max_w);
            h = h.min(max_h);
        }
        Aspect::Ratio(rw, rh) => {
            let (rw, rh) = (rw as u64, rh as u64);
            match keep {
                Keep::Width => h = ((w as u64 * rh / rw) as u32).max(1),
                Keep::Height => w = ((h as u64 * rw / rh) as u32).max(1),
            }
            if w > max_w || h > max_h {
                if max_w as u64 * rh <= max_h as u64 * rw {
                    w = max_w;
                    h = ((max_w as u64 * rh / rw) as u32).max(1);
                } else {
                    h = max_h;
                    w = ((max_h as u64 * rw / rh) as u32).max(1);
                }
            }
        }
    }
    Rect::new(x, y, w, h)
}

/// Cuts `rect` out of the image. A rectangle that only partly overlaps is clipped; one with no overlap is an error.
pub fn crop(img: &DynamicImage, rect: Rect) -> Result<DynamicImage> {
    let r = rect.clamp_to(img.width(), img.height()).ok_or_else(|| {
        Error::InvalidArgument(format!(
            "crop rectangle {}x{} at {},{} is outside the image",
            rect.w, rect.h, rect.x, rect.y
        ))
    })?;
    Ok(img.crop_imm(r.x, r.y, r.w, r.h))
}

#[cfg(test)]
mod tests {
    use super::*;
    use image::{GenericImageView, Rgb, RgbImage};

    #[test]
    fn labels_match_the_dialog_and_parse_back() {
        let labels: Vec<_> = Aspect::ALL.iter().map(|a| a.label()).collect();
        assert_eq!(labels, ["Free", "1:1", "16:9", "9:16", "4:3", "3:4"]);
        for a in Aspect::ALL {
            assert_eq!(Aspect::from_label(&a.label()), Some(a));
        }
        assert_eq!(Aspect::from_label("2:1"), None);
        assert_eq!(Aspect::from_label("free"), Some(Aspect::Free));
    }

    #[test]
    fn values() {
        assert_eq!(Aspect::Free.value(), None);
        assert_eq!(Aspect::Ratio(16, 9).value(), Some(16.0 / 9.0));
    }

    #[test]
    fn largest_centered_rectangles() {
        assert_eq!(largest_centered(3000, 2000, Aspect::Free), Rect::new(0, 0, 3000, 2000));
        assert_eq!(largest_centered(3000, 2000, Aspect::Ratio(1, 1)), Rect::new(500, 0, 2000, 2000));
        assert_eq!(largest_centered(3000, 2000, Aspect::Ratio(16, 9)), Rect::new(0, 156, 3000, 1687));
        assert_eq!(largest_centered(3000, 2000, Aspect::Ratio(9, 16)), Rect::new(937, 0, 1125, 2000));
        assert_eq!(largest_centered(3000, 2000, Aspect::Ratio(4, 3)), Rect::new(167, 0, 2666, 2000));
        assert_eq!(largest_centered(3000, 2000, Aspect::Ratio(3, 4)), Rect::new(750, 0, 1500, 2000));
        assert_eq!(largest_centered(0, 0, Aspect::Ratio(1, 1)), Rect::new(0, 0, 0, 0));
    }

    #[test]
    fn constrain_derives_the_other_side() {
        let r = Rect::new(10, 10, 400, 300);
        assert_eq!(constrain(r, Aspect::Ratio(1, 1), Keep::Width, 1000, 1000), Rect::new(10, 10, 400, 400));
        assert_eq!(constrain(r, Aspect::Ratio(1, 1), Keep::Height, 1000, 1000), Rect::new(10, 10, 300, 300));
        assert_eq!(constrain(r, Aspect::Ratio(16, 9), Keep::Width, 1000, 1000), Rect::new(10, 10, 400, 225));
        assert_eq!(constrain(r, Aspect::Free, Keep::Width, 1000, 1000), r);
    }

    #[test]
    fn constrain_shrinks_to_fit_inside_the_image_keeping_the_ratio() {
        let r = Rect::new(900, 0, 400, 300);
        assert_eq!(constrain(r, Aspect::Ratio(1, 1), Keep::Width, 1000, 1000), Rect::new(900, 0, 100, 100));
        let r = Rect::new(0, 500, 400, 300);
        assert_eq!(constrain(r, Aspect::Ratio(4, 3), Keep::Width, 1000, 600), Rect::new(0, 500, 133, 100));
        // Free crops are simply clipped.
        assert_eq!(
            constrain(Rect::new(900, 0, 400, 300), Aspect::Free, Keep::Width, 1000, 1000),
            Rect::new(900, 0, 100, 300)
        );
        // A corner outside the image is pulled back in.
        let c = constrain(Rect::new(5000, 5000, 10, 10), Aspect::Free, Keep::Width, 100, 100);
        assert!(c.x < 100 && c.y < 100 && !c.is_empty());
    }

    #[test]
    fn clamp_to() {
        assert_eq!(Rect::new(5, 5, 10, 10).clamp_to(100, 100), Some(Rect::new(5, 5, 10, 10)));
        assert_eq!(Rect::new(95, 95, 10, 10).clamp_to(100, 100), Some(Rect::new(95, 95, 5, 5)));
        assert_eq!(Rect::new(100, 0, 10, 10).clamp_to(100, 100), None);
        assert_eq!(Rect::new(0, 0, 0, 10).clamp_to(100, 100), None);
        assert_eq!(Rect::new(u32::MAX - 1, 0, 10, 10).clamp_to(100, 100), None);
    }

    fn numbered() -> DynamicImage {
        DynamicImage::ImageRgb8(RgbImage::from_fn(10, 8, |x, y| Rgb([x as u8, y as u8, 7])))
    }

    #[test]
    fn crop_keeps_the_exact_pixels() {
        let out = crop(&numbered(), Rect::new(2, 3, 4, 2)).unwrap();
        assert_eq!(out.dimensions(), (4, 2));
        assert_eq!(out.to_rgb8().get_pixel(0, 0).0, [2, 3, 7]);
        assert_eq!(out.to_rgb8().get_pixel(3, 1).0, [5, 4, 7]);
    }

    #[test]
    fn crop_clips_partial_overlap_and_rejects_the_rest() {
        let out = crop(&numbered(), Rect::new(8, 6, 10, 10)).unwrap();
        assert_eq!(out.dimensions(), (2, 2));
        assert!(matches!(crop(&numbered(), Rect::new(10, 0, 5, 5)), Err(Error::InvalidArgument(_))));
        assert!(matches!(crop(&numbered(), Rect::new(0, 0, 0, 5)), Err(Error::InvalidArgument(_))));
    }
}
