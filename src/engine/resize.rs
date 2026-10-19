//! Resize pictures to presets or exact sizes.

use super::error::{Error, Result};
use super::io;
use super::kind::{detect_head, SourceKind};
use super::raster::{self, EncodeOptions};
use crate::convert::Fmt;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spec {
    /// The longest side becomes this many pixels; the ratio is kept.
    MaxSide(u32),
    /// 1..=100 percent of both sides.
    Percent(u32),
    Exact {
        w: u32,
        h: u32,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Options {
    pub spec: Spec,
    pub allow_upscale: bool,
    pub jpeg_quality: u8,
    pub format: Option<Fmt>,
}

impl Options {
    pub fn new(spec: Spec) -> Options {
        Options { spec, allow_upscale: false, jpeg_quality: 85, format: None }
    }
}

/// The message of the error `resize_*` returns when the picture is already at or below the wanted size.
const ALREADY_SMALL: &str = "already at or below that size";

/// True for the "nothing to do" error: the picture is not bigger than the wanted size (nothing is written).
pub fn is_already_small(e: &Error) -> bool {
    matches!(e, Error::Unsupported(m) if m == ALREADY_SMALL)
}

/// The size `spec` gives a `src_w` x `src_h` picture. Never zero; never bigger than the source unless
/// `allow_upscale`; `MaxSide` lands exactly on the requested longest side.
pub fn target_size(src_w: u32, src_h: u32, spec: Spec, allow_upscale: bool) -> (u32, u32) {
    let (w, h) = (src_w.max(1) as u64, src_h.max(1) as u64);
    let scaled =
        |num: u64, den: u64| (((w * num + den / 2) / den).max(1) as u32, ((h * num + den / 2) / den).max(1) as u32);
    match spec {
        Spec::MaxSide(n) => {
            let n = n.max(1) as u64;
            let longest = w.max(h);
            if n >= longest && !allow_upscale {
                return (w as u32, h as u32);
            }
            let (a, b) = scaled(n, longest);
            // The longest side is exact whatever the rounding of the other one.
            if w >= h {
                (n as u32, b)
            } else {
                (a, n as u32)
            }
        }
        Spec::Percent(p) => {
            let p = p.max(1) as u64;
            if p >= 100 && !allow_upscale {
                return (w as u32, h as u32);
            }
            scaled(p, 100)
        }
        Spec::Exact { w: tw, h: th } => {
            let (tw, th) = (tw.max(1), th.max(1));
            if allow_upscale {
                (tw, th)
            } else {
                (tw.min(w as u32), th.min(h as u32))
            }
        }
    }
}

/// Output format when the caller did not pick one: the source's own, except for formats that make poor
/// resized copies (GIF loses its animation anyway, TIFF/HEIC/AVIF are big or rarely opened): PNG when there is
/// transparency to keep, JPEG otherwise (GIF always PNG).
fn pick_format(src: Fmt, has_alpha: bool, wanted: Option<Fmt>) -> Fmt {
    if let Some(f) = wanted {
        return f;
    }
    match src {
        Fmt::Png | Fmt::Jpg | Fmt::Webp | Fmt::Bmp => src,
        Fmt::Gif => Fmt::Png,
        Fmt::Tiff | Fmt::Heic | Fmt::Avif => {
            if has_alpha {
                Fmt::Png
            } else {
                Fmt::Jpg
            }
        }
    }
}

/// Averages `f` x `f` blocks (edge blocks are smaller). `ch` is 3 (RGB) or 4 (RGBA, colours weighted by alpha so
/// transparent pixels do not darken the edges). Rows of the result are shared out between `threads` workers.
fn box_shrink(src: &[u8], sw: usize, sh: usize, ch: usize, f: usize, threads: usize) -> (Vec<u8>, usize, usize) {
    let (nw, nh) = (sw.div_ceil(f), sh.div_ceil(f));
    let mut out = vec![0u8; nw * nh * ch];
    let rows_per = nh.div_ceil(threads.max(1)).max(1);
    let work = |first_row: usize, chunk: &mut [u8]| {
        for (r, orow) in chunk.chunks_exact_mut(nw * ch).enumerate() {
            let oy = first_row + r;
            let (y0, y1) = (oy * f, ((oy + 1) * f).min(sh));
            for ox in 0..nw {
                let (x0, x1) = (ox * f, ((ox + 1) * f).min(sw));
                let mut acc = [0u32; 4];
                for y in y0..y1 {
                    let row = &src[(y * sw + x0) * ch..(y * sw + x1) * ch];
                    if ch == 3 {
                        for px in row.chunks_exact(3) {
                            acc[0] += px[0] as u32;
                            acc[1] += px[1] as u32;
                            acc[2] += px[2] as u32;
                        }
                    } else {
                        for px in row.chunks_exact(4) {
                            let a = px[3] as u32;
                            acc[0] += px[0] as u32 * a;
                            acc[1] += px[1] as u32 * a;
                            acc[2] += px[2] as u32 * a;
                            acc[3] += a;
                        }
                    }
                }
                let n = ((y1 - y0) * (x1 - x0)) as u32;
                let o = &mut orow[ox * ch..(ox + 1) * ch];
                if ch == 3 {
                    for c in 0..3 {
                        o[c] = ((acc[c] + n / 2) / n) as u8;
                    }
                } else {
                    // Fully transparent blocks stay all zero.
                    let a = acc[3].max(1);
                    for c in 0..3 {
                        o[c] = ((acc[c] + a / 2) / a) as u8;
                    }
                    o[3] = ((acc[3] + n / 2) / n) as u8;
                }
            }
        }
    };
    if threads <= 1 || nh < 2 {
        work(0, &mut out);
    } else {
        std::thread::scope(|sc| {
            for (i, chunk) in out.chunks_mut(rows_per * nw * ch).enumerate() {
                let work = &work;
                sc.spawn(move || work(i * rows_per, chunk));
            }
        });
    }
    (out, nw, nh)
}

/// High quality downscale: an exact box average for the whole-number part of the reduction (fast, and exact
/// for big reductions), then Lanczos3 for the last factor below 2.
fn scale(img: image::DynamicImage, tw: u32, th: u32, threads: usize) -> image::DynamicImage {
    use image::{imageops::FilterType::Lanczos3, DynamicImage as D, ImageBuffer};
    let (sw, sh) = (img.width() as usize, img.height() as usize);
    let alpha = img.color().has_alpha();
    let f = (sw / tw as usize).min(sh / th as usize);
    macro_rules! go {
        ($into:ident, $variant:ident, $ch:expr) => {{
            let buf = img.$into();
            if f >= 2 {
                let (data, nw, nh) = box_shrink(buf.as_raw(), sw, sh, $ch, f, threads);
                drop(buf);
                let small = ImageBuffer::from_raw(nw as u32, nh as u32, data).expect("buffer size");
                if (nw as u32, nh as u32) == (tw, th) {
                    D::$variant(small)
                } else {
                    D::$variant(image::imageops::resize(&small, tw, th, Lanczos3))
                }
            } else {
                D::$variant(image::imageops::resize(&buf, tw, th, Lanczos3))
            }
        }};
    }
    if alpha {
        go!(into_rgba8, ImageRgba8, 4)
    } else {
        go!(into_rgb8, ImageRgb8, 3)
    }
}

fn resize_bytes_with(input: &[u8], opts: &Options, threads: usize) -> Result<(Vec<u8>, Fmt)> {
    let Some(SourceKind::Image(src_fmt)) = detect_head(None, input) else {
        return Err(Error::Unsupported("not a picture".into()));
    };
    let img = raster::decode(input)?;
    let (sw, sh) = (img.width(), img.height());
    let (tw, th) = target_size(sw, sh, opts.spec, opts.allow_upscale);
    if (tw, th) == (sw, sh) {
        return Err(Error::Unsupported(ALREADY_SMALL.into()));
    }
    if tw > raster::MAX_SIDE || th > raster::MAX_SIDE || tw as u64 * th as u64 > raster::MAX_PIXELS {
        return Err(Error::TooLarge { width: tw, height: th });
    }
    let fmt = pick_format(src_fmt, img.color().has_alpha(), opts.format);
    let out = scale(img, tw, th, threads);
    let bytes = raster::encode(&out, fmt, &EncodeOptions { jpeg_quality: opts.jpeg_quality })?;
    Ok((bytes, fmt))
}

fn cores() -> usize {
    std::thread::available_parallelism().map_or(1, |n| n.get())
}

/// Resizes the picture in `input` (any format the engine decodes) and encodes it again.
///
/// The EXIF orientation is applied first and the result carries no metadata at all: EXIF, GPS position, camera
/// and software tags are dropped on purpose, so a resized copy is safe to share. Returns the encoded bytes and
/// their format. A picture that is already at or below the wanted size gives the error
/// [`is_already_small`] recognises.
pub fn resize_bytes(input: &[u8], opts: &Options) -> Result<(Vec<u8>, Fmt)> {
    resize_bytes_with(input, opts, cores().min(8))
}

/// Files above this size are processed one at a time, so a batch of huge photos cannot exhaust memory.
const BIG_FILE: usize = 24 * 1024 * 1024;
static BIG: Mutex<()> = Mutex::new(());

fn resize_file_with(src: &Path, opts: &Options, threads: usize) -> Result<PathBuf> {
    let bytes = io::read_limited(src)?;
    let _one_at_a_time = (bytes.len() > BIG_FILE).then(|| BIG.lock().unwrap_or_else(|e| e.into_inner()));
    let (out, fmt) = resize_bytes_with(&bytes, opts, threads)?;
    drop(bytes);
    let dir = src.parent().filter(|p| !p.as_os_str().is_empty()).unwrap_or(Path::new("."));
    let stem = src.file_stem().and_then(|s| s.to_str()).unwrap_or("image");
    let ext = src
        .extension()
        .and_then(|e| e.to_str())
        .filter(|e| Fmt::from_ext(e) == Some(fmt))
        .map_or_else(|| fmt.ext().to_string(), str::to_ascii_lowercase);
    io::commit_unique(dir, &format!("{stem} (resized)"), &ext, &out)
}

/// Writes `<stem> (resized).<ext>` next to `src` and returns its path. Never overwrites (a taken name gets
/// ` (1)`, ` (2)`...), and nothing is created when the work fails. EXIF orientation is applied and all metadata,
/// GPS included, is left out of the copy (privacy).
pub fn resize_file(src: &Path, opts: &Options) -> Result<PathBuf> {
    resize_file_with(src, opts, cores().min(8))
}

/// Resizes every file with the options `opts_for` picks for it, on a small pool of threads that leaves cores
/// free for the UI. Results come back in the order of `files`.
pub fn resize_many(files: &[PathBuf], opts_for: &(dyn Fn(&Path) -> Options + Sync)) -> Vec<Result<PathBuf>> {
    let workers = cores().saturating_sub(2).clamp(1, 6).min(files.len().max(1));
    if workers <= 1 {
        let threads = if files.len() == 1 { cores().min(8) } else { 1 };
        return files.iter().map(|f| resize_file_with(f, &opts_for(f), threads)).collect();
    }
    let next = AtomicUsize::new(0);
    let mut done: Vec<(usize, Result<PathBuf>)> = std::thread::scope(|sc| {
        let jobs: Vec<_> = (0..workers)
            .map(|_| {
                sc.spawn(|| {
                    let mut mine = Vec::new();
                    loop {
                        let i = next.fetch_add(1, Ordering::Relaxed);
                        let Some(f) = files.get(i) else { break };
                        mine.push((i, resize_file_with(f, &opts_for(f), 1)));
                    }
                    mine
                })
            })
            .collect();
        jobs.into_iter().flat_map(|j| j.join().unwrap_or_default()).collect()
    });
    done.sort_by_key(|(i, _)| *i);
    done.into_iter().map(|(_, r)| r).collect()
}

#[cfg(test)]
mod tests {
    use super::super::raster::{self, testutil::*};
    use super::*;
    use image::{DynamicImage, GenericImageView};

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-rsz-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn dims(bytes: &[u8]) -> (u32, u32) {
        raster::decode(bytes).unwrap().dimensions()
    }

    #[test]
    fn max_side_keeps_the_ratio_and_the_longest_side_is_exact() {
        assert_eq!(target_size(4000, 3000, Spec::MaxSide(1600), false), (1600, 1200));
        assert_eq!(target_size(3000, 4000, Spec::MaxSide(1600), false), (1200, 1600));
        assert_eq!(target_size(3000, 2000, Spec::MaxSide(1600), false), (1600, 1067), "rounds to nearest");
        assert_eq!(target_size(5000, 7, Spec::MaxSide(100), false), (100, 1), "never zero");
    }

    #[test]
    fn nothing_grows_unless_asked() {
        assert_eq!(target_size(800, 600, Spec::MaxSide(1600), false), (800, 600));
        assert_eq!(target_size(800, 600, Spec::MaxSide(1600), true), (1600, 1200));
        assert_eq!(target_size(800, 600, Spec::Exact { w: 2000, h: 100 }, false), (800, 100));
        assert_eq!(target_size(800, 600, Spec::Exact { w: 2000, h: 100 }, true), (2000, 100));
        assert_eq!(target_size(800, 600, Spec::Percent(200), false), (800, 600));
        assert_eq!(target_size(800, 600, Spec::Percent(200), true), (1600, 1200));
    }

    #[test]
    fn percent_scales_both_sides_and_never_reaches_zero() {
        assert_eq!(target_size(1000, 500, Spec::Percent(50), false), (500, 250));
        assert_eq!(target_size(1000, 500, Spec::Percent(25), false), (250, 125));
        assert_eq!(target_size(1001, 3, Spec::Percent(25), false), (250, 1));
        assert_eq!(target_size(1000, 500, Spec::Percent(100), false), (1000, 500));
        assert_eq!(target_size(1000, 500, Spec::Percent(0), false), (10, 5), "0 is read as 1%");
    }

    #[test]
    fn exact_and_zero_inputs_are_sane() {
        assert_eq!(target_size(800, 600, Spec::Exact { w: 100, h: 50 }, false), (100, 50));
        assert_eq!(target_size(800, 600, Spec::Exact { w: 0, h: 0 }, false), (1, 1));
        assert_eq!(target_size(800, 600, Spec::MaxSide(0), false), (1, 1));
    }

    #[test]
    fn output_decodes_at_the_expected_size_in_the_same_format() {
        let img = DynamicImage::ImageRgb8(gradient(400, 300));
        let png_in = png(&img);
        let (out, fmt) = resize_bytes(&png_in, &Options::new(Spec::MaxSide(100))).unwrap();
        assert_eq!((fmt, dims(&out)), (Fmt::Png, (100, 75)));
        let (out, fmt) = resize_bytes(&jpeg(&gradient(400, 300)), &Options::new(Spec::Percent(50))).unwrap();
        assert_eq!((fmt, dims(&out)), (Fmt::Jpg, (200, 150)));
    }

    #[test]
    fn exif_orientation_is_applied_before_sizing() {
        let src = jpeg_with_orientation(&gradient(400, 200), 6);
        let (out, _) = resize_bytes(&src, &Options::new(Spec::MaxSide(100))).unwrap();
        assert_eq!(dims(&out), (50, 100), "stored 400x200, shown 200x400");
    }

    #[test]
    fn exif_and_gps_metadata_are_not_copied() {
        let src = jpeg_with_orientation(&gradient(400, 200), 1);
        assert!(src.windows(6).any(|w| w == b"Exif\0\0"));
        let (out, _) = resize_bytes(&src, &Options::new(Spec::MaxSide(100))).unwrap();
        assert!(!out.windows(6).any(|w| w == b"Exif\0\0"));
    }

    #[test]
    fn a_picture_with_alpha_stays_png_and_keeps_its_transparency() {
        let img = DynamicImage::ImageRgba8(rgba(64, 32));
        let (out, fmt) = resize_bytes(&png(&img), &Options::new(Spec::Percent(50))).unwrap();
        assert_eq!(fmt, Fmt::Png);
        let back = raster::decode(&out).unwrap();
        assert_eq!(back.dimensions(), (32, 16));
        assert!(back.color().has_alpha());
        let px = back.to_rgba8();
        assert_eq!(px.get_pixel(2, 8)[3], 255);
        assert!(px.get_pixel(30, 8)[3] < 200, "the translucent half stays translucent");
    }

    #[test]
    fn the_quality_option_changes_the_jpeg_size() {
        let src = jpeg(&gradient(600, 400));
        let mut hi = Options::new(Spec::MaxSide(300));
        hi.jpeg_quality = 95;
        let mut lo = hi;
        lo.jpeg_quality = 30;
        let (a, _) = resize_bytes(&src, &hi).unwrap();
        let (b, _) = resize_bytes(&src, &lo).unwrap();
        assert!(b.len() < a.len(), "{} !< {}", b.len(), a.len());
    }

    #[test]
    fn a_requested_format_wins_and_other_sources_get_a_sensible_one() {
        let png_in = png(&DynamicImage::ImageRgb8(gradient(200, 100)));
        let mut o = Options::new(Spec::Percent(50));
        o.format = Some(Fmt::Jpg);
        assert_eq!(resize_bytes(&png_in, &o).unwrap().1, Fmt::Jpg);
        let mut gif = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(rgba(40, 40)).write_to(&mut gif, image::ImageFormat::Gif).unwrap();
        assert_eq!(resize_bytes(&gif.into_inner(), &Options::new(Spec::Percent(50))).unwrap().1, Fmt::Png);
        let mut tiff = std::io::Cursor::new(Vec::new());
        DynamicImage::ImageRgb8(gradient(40, 40)).write_to(&mut tiff, image::ImageFormat::Tiff).unwrap();
        assert_eq!(resize_bytes(&tiff.into_inner(), &Options::new(Spec::Percent(50))).unwrap().1, Fmt::Jpg);
    }

    #[test]
    fn a_picture_already_at_or_below_the_size_is_refused_with_a_clear_error() {
        let src = png(&DynamicImage::ImageRgb8(gradient(100, 50)));
        for spec in [Spec::MaxSide(100), Spec::MaxSide(1600), Spec::Percent(100), Spec::Exact { w: 100, h: 50 }] {
            let e = resize_bytes(&src, &Options::new(spec)).unwrap_err();
            assert!(is_already_small(&e), "{spec:?}: {e}");
        }
        assert!(!is_already_small(&Error::Corrupt("x".into())));
    }

    #[test]
    fn upscaling_works_when_asked() {
        let src = png(&DynamicImage::ImageRgb8(gradient(100, 50)));
        let mut o = Options::new(Spec::MaxSide(200));
        o.allow_upscale = true;
        assert_eq!(dims(&resize_bytes(&src, &o).unwrap().0), (200, 100));
    }

    #[test]
    fn the_file_is_saved_next_to_the_source_and_never_overwrites() {
        let d = dir("file");
        let src = d.join("photo.jpg");
        std::fs::write(&src, jpeg(&gradient(400, 300))).unwrap();
        let before = std::fs::read(&src).unwrap();
        let o = Options::new(Spec::MaxSide(100));
        let a = resize_file(&src, &o).unwrap();
        let b = resize_file(&src, &o).unwrap();
        assert_eq!(a, d.join("photo (resized).jpg"));
        assert_eq!(b, d.join("photo (resized) (1).jpg"));
        assert_eq!(dims(&std::fs::read(&a).unwrap()), (100, 75));
        assert_eq!(std::fs::read(&src).unwrap(), before, "the original is untouched");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_jpeg_keeps_its_own_extension_and_other_formats_get_theirs() {
        let d = dir("ext");
        let src = d.join("a.jpeg");
        std::fs::write(&src, jpeg(&gradient(200, 100))).unwrap();
        assert_eq!(resize_file(&src, &Options::new(Spec::Percent(50))).unwrap(), d.join("a (resized).jpeg"));
        let p = d.join("b.png");
        std::fs::write(&p, png(&DynamicImage::ImageRgb8(gradient(200, 100)))).unwrap();
        let mut o = Options::new(Spec::Percent(50));
        o.format = Some(Fmt::Jpg);
        assert_eq!(resize_file(&p, &o).unwrap(), d.join("b (resized).jpg"));
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_tiny_file_writes_nothing() {
        let d = dir("tiny");
        let src = d.join("t.png");
        std::fs::write(&src, png(&DynamicImage::ImageRgb8(gradient(50, 40)))).unwrap();
        let e = resize_file(&src, &Options::new(Spec::MaxSide(1600))).unwrap_err();
        assert!(is_already_small(&e));
        assert_eq!(std::fs::read_dir(&d).unwrap().count(), 1);
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_corrupt_file_errors_and_leaves_no_partial_output() {
        let d = dir("bad");
        let src = d.join("bad.png");
        let good = png(&DynamicImage::ImageRgb8(gradient(64, 64)));
        std::fs::write(&src, &good[..good.len() / 2]).unwrap();
        assert!(resize_file(&src, &Options::new(Spec::Percent(50))).is_err());
        let txt = d.join("n.txt");
        std::fs::write(&txt, b"hello").unwrap();
        assert!(matches!(resize_file(&txt, &Options::new(Spec::Percent(50))), Err(Error::Unsupported(_))));
        assert!(resize_file(&d.join("missing.png"), &Options::new(Spec::Percent(50))).is_err());
        let names: Vec<_> = std::fs::read_dir(&d).unwrap().map(|e| e.unwrap().file_name()).collect();
        assert_eq!(names.len(), 2, "only the two inputs: {names:?}");
        let _ = std::fs::remove_dir_all(&d);
    }

    #[test]
    fn a_big_reduction_averages_instead_of_dropping_pixels() {
        // 1-pixel black/white stripes: a box average lands on mid grey, picking pixels would not.
        let img =
            image::RgbImage::from_fn(
                512,
                512,
                |x, _| if x % 2 == 0 { image::Rgb([0; 3]) } else { image::Rgb([255; 3]) },
            );
        let (out, _) = resize_bytes(&png(&DynamicImage::ImageRgb8(img)), &Options::new(Spec::MaxSide(32))).unwrap();
        let px = raster::decode(&out).unwrap().to_rgb8();
        for p in px.pixels() {
            assert!((p[0] as i32 - 127).abs() <= 6, "{p:?}");
        }
    }

    #[test]
    fn many_files_run_in_parallel_and_report_each_one() {
        let d = dir("many");
        let mut files = Vec::new();
        for i in 0..6 {
            let p = d.join(format!("p{i}.jpg"));
            std::fs::write(&p, jpeg(&gradient(300 + i * 10, 200))).unwrap();
            files.push(p);
        }
        let tiny = d.join("tiny.png");
        std::fs::write(&tiny, png(&DynamicImage::ImageRgb8(gradient(20, 20)))).unwrap();
        files.push(tiny);
        let results = resize_many(&files, &|_| Options::new(Spec::MaxSide(100)));
        assert_eq!(results.len(), 7);
        for (i, r) in results.iter().enumerate().take(6) {
            assert_eq!(r.as_ref().unwrap(), &d.join(format!("p{i} (resized).jpg")));
        }
        assert!(is_already_small(results[6].as_ref().unwrap_err()));
        let _ = std::fs::remove_dir_all(&d);
    }

    /// `cargo test --release resize_timings -- --ignored --nocapture`
    #[test]
    #[ignore = "benchmark: run in release mode with --ignored --nocapture"]
    fn resize_timings() {
        use std::time::Instant;
        let mut seed = 11u32;
        let mut photo = |w: u32, h: u32| {
            image::RgbImage::from_fn(w, h, |x, y| {
                seed = seed.wrapping_mul(1664525).wrapping_add(1013904223);
                let n = (seed >> 29) as u8;
                let t = ((x as f32 / 40.0).sin() * 40.0 + (y as f32 / 55.0).cos() * 40.0) as i32;
                image::Rgb([
                    ((x / 24) as i32 + t) as u8 + n,
                    ((y / 16) as i32 - t) as u8 + n,
                    (((x + y) / 40) as i32 + t) as u8,
                ])
            })
        };
        let big = jpeg(&photo(6000, 4000));
        let o = Options::new(Spec::MaxSide(1600));
        let t = Instant::now();
        let (out, _) = resize_bytes(&big, &o).unwrap();
        println!("24 MP JPEG ({} KB) -> 1600 px ({} KB): {:?}", big.len() / 1024, out.len() / 1024, t.elapsed());

        let d = dir("bench");
        let one = jpeg(&photo(4000, 3000));
        let files: Vec<PathBuf> = (0..20)
            .map(|i| {
                let p = d.join(format!("p{i}.jpg"));
                std::fs::write(&p, &one).unwrap();
                p
            })
            .collect();
        let t = Instant::now();
        let r = resize_many(&files, &|_| Options::new(Spec::MaxSide(1600)));
        println!("20 x 12 MP JPEG -> 1600 px: {:?} ({} ok)", t.elapsed(), r.iter().filter(|r| r.is_ok()).count());
        let _ = std::fs::remove_dir_all(&d);
    }
}
