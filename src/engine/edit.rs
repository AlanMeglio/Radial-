//! Basic edits for the EDIT action: rotate, flip, resize and simple tone adjustments.

use super::error::{Error, Result};
use super::raster::{MAX_PIXELS, MAX_SIDE};
use image::{imageops::FilterType, DynamicImage};

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum EditOp {
    Rotate90,
    Rotate180,
    Rotate270,
    FlipHorizontal,
    FlipVertical,
    /// Exact size in pixels (aspect ratio is the caller's business).
    Resize {
        w: u32,
        h: u32,
    },
    Grayscale,
    /// -255..=255
    Brightness(i32),
    /// Percent, -100.0..=100.0
    Contrast(f32),
}

fn validate(op: &EditOp) -> Result<()> {
    match *op {
        EditOp::Resize { w, h } => {
            if w == 0 || h == 0 {
                return Err(Error::InvalidArgument("resize to zero pixels".into()));
            }
            if w > MAX_SIDE || h > MAX_SIDE || w as u64 * h as u64 > MAX_PIXELS {
                return Err(Error::TooLarge { width: w, height: h });
            }
        }
        EditOp::Brightness(v) if !(-255..=255).contains(&v) => {
            return Err(Error::InvalidArgument(format!("brightness {v} is outside -255..=255")));
        }
        EditOp::Contrast(c) if !(-100.0..=100.0).contains(&c) => {
            return Err(Error::InvalidArgument(format!("contrast {c} is outside -100..=100")));
        }
        _ => {}
    }
    Ok(())
}

/// Applies the operations in order. All of them are validated before any pixel work starts.
pub fn apply(img: DynamicImage, ops: &[EditOp]) -> Result<DynamicImage> {
    ops.iter().try_for_each(validate)?;
    Ok(ops.iter().fold(img, |img, op| match *op {
        EditOp::Rotate90 => img.rotate90(),
        EditOp::Rotate180 => img.rotate180(),
        EditOp::Rotate270 => img.rotate270(),
        EditOp::FlipHorizontal => img.fliph(),
        EditOp::FlipVertical => img.flipv(),
        EditOp::Resize { w, h } => img.resize_exact(w, h, FilterType::Lanczos3),
        EditOp::Grayscale => img.grayscale(),
        EditOp::Brightness(v) => img.brighten(v),
        EditOp::Contrast(c) => img.adjust_contrast(c),
    }))
}

#[cfg(test)]
mod tests {
    use super::super::error::Error;
    use super::*;
    use image::{GenericImageView, Rgb, RgbImage};

    fn marked() -> DynamicImage {
        // 3x2 image, top-left pixel red, everything else dark grey.
        let mut i = RgbImage::from_pixel(3, 2, Rgb([50, 50, 50]));
        i.put_pixel(0, 0, Rgb([255, 0, 0]));
        DynamicImage::ImageRgb8(i)
    }

    fn red_at(img: &DynamicImage) -> (u32, u32) {
        let rgb = img.to_rgb8();
        rgb.enumerate_pixels().find(|(_, _, p)| p.0 == [255, 0, 0]).map(|(x, y, _)| (x, y)).unwrap()
    }

    #[test]
    fn rotations_move_the_marker_and_swap_dimensions() {
        let r90 = apply(marked(), &[EditOp::Rotate90]).unwrap();
        assert_eq!((r90.dimensions(), red_at(&r90)), ((2, 3), (1, 0)));
        let r180 = apply(marked(), &[EditOp::Rotate180]).unwrap();
        assert_eq!((r180.dimensions(), red_at(&r180)), ((3, 2), (2, 1)));
        let r270 = apply(marked(), &[EditOp::Rotate270]).unwrap();
        assert_eq!((r270.dimensions(), red_at(&r270)), ((2, 3), (0, 2)));
    }

    #[test]
    fn flips() {
        assert_eq!(red_at(&apply(marked(), &[EditOp::FlipHorizontal]).unwrap()), (2, 0));
        assert_eq!(red_at(&apply(marked(), &[EditOp::FlipVertical]).unwrap()), (0, 1));
    }

    #[test]
    fn four_quarter_turns_are_the_identity() {
        let out = apply(marked(), &[EditOp::Rotate90; 4]).unwrap();
        assert_eq!(out.to_rgb8(), marked().to_rgb8());
    }

    #[test]
    fn resize_is_exact_and_validated() {
        assert_eq!(apply(marked(), &[EditOp::Resize { w: 9, h: 4 }]).unwrap().dimensions(), (9, 4));
        assert!(matches!(apply(marked(), &[EditOp::Resize { w: 0, h: 4 }]), Err(Error::InvalidArgument(_))));
        assert!(matches!(apply(marked(), &[EditOp::Resize { w: 90_000, h: 90_000 }]), Err(Error::TooLarge { .. })));
    }

    #[test]
    fn tone_adjustments() {
        let g = apply(marked(), &[EditOp::Grayscale]).unwrap().to_rgb8();
        let p = g.get_pixel(0, 0).0;
        assert!(p[0] == p[1] && p[1] == p[2]);
        let b = apply(marked(), &[EditOp::Brightness(40)]).unwrap().to_rgb8();
        assert_eq!(b.get_pixel(1, 1).0, [90, 90, 90]);
        let c = apply(marked(), &[EditOp::Contrast(50.0)]).unwrap().to_rgb8();
        assert!(c.get_pixel(1, 1).0[0] != 50);
        assert!(matches!(apply(marked(), &[EditOp::Brightness(5000)]), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn ops_apply_in_order() {
        let out = apply(marked(), &[EditOp::Rotate90, EditOp::Resize { w: 4, h: 4 }]).unwrap();
        assert_eq!(out.dimensions(), (4, 4));
    }
}
