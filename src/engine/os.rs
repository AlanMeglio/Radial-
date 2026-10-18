//! Windows-provided codecs, reached through WinRT so no codec DLL is shipped with the app.
//!
//! * PDF rendering: `Windows.Data.Pdf` (built into Windows 10/11).
//! * HEIC/AVIF decode and HEIC encode: the Windows Imaging Component codecs that the user may have
//!   installed from the Microsoft Store ("HEIF Image Extensions", "AV1 Video Extension").
//!
//! Everything blocks and runs on a short-lived worker thread that owns its own COM apartment, so
//! callers on a UI thread (STA) can use it freely.

use super::error::{Error, Result};
use crate::convert::Fmt;
use image::RgbaImage;
use std::sync::OnceLock;
use windows::core::{Interface, GUID};
use windows::Data::Pdf::{PdfDocument, PdfPageRenderOptions};
use windows::Graphics::Imaging::{BitmapAlphaMode, BitmapDecoder, BitmapEncoder, BitmapPixelFormat, SoftwareBitmap};
use windows::Storage::Streams::{
    Buffer, DataReader, DataWriter, IBuffer, IRandomAccessStream, InMemoryRandomAccessStream,
};
use windows::Win32::System::WinRT::{RoInitialize, RO_INIT_MULTITHREADED};

fn os_err(what: &str, e: windows::core::Error) -> Error {
    Error::Corrupt(format!("{what}: {}", e.message()))
}

/// Runs `f` on a fresh thread inside a multithreaded WinRT apartment.
fn on_mta<T: Send + 'static>(f: impl FnOnce() -> Result<T> + Send + 'static) -> Result<T> {
    std::thread::Builder::new()
        .name("radial-os-codec".into())
        .spawn(move || {
            // SAFETY: plain COM initialisation for this thread; a failure (apartment already set)
            // is harmless because WinRT calls below report their own errors.
            unsafe {
                let _ = RoInitialize(RO_INIT_MULTITHREADED);
            }
            f()
        })
        .map_err(|e| Error::Io(e.to_string()))?
        .join()
        .unwrap_or_else(|_| Err(Error::Corrupt("OS codec thread crashed".into())))
}

fn stream_from_bytes(bytes: &[u8]) -> windows::core::Result<InMemoryRandomAccessStream> {
    let stream = InMemoryRandomAccessStream::new()?;
    let writer = DataWriter::CreateDataWriter(&stream)?;
    writer.WriteBytes(bytes)?;
    writer.StoreAsync()?.join()?;
    writer.FlushAsync()?.join()?;
    writer.DetachStream()?;
    stream.Seek(0)?;
    Ok(stream)
}

fn stream_to_bytes(stream: &InMemoryRandomAccessStream) -> windows::core::Result<Vec<u8>> {
    let len = stream.Size()? as u32;
    stream.Seek(0)?;
    let reader = DataReader::CreateDataReader(&stream.GetInputStreamAt(0)?)?;
    reader.LoadAsync(len)?.join()?;
    let mut out = vec![0u8; len as usize];
    reader.ReadBytes(&mut out)?;
    Ok(out)
}

fn buffer_to_bytes(buf: &IBuffer) -> windows::core::Result<Vec<u8>> {
    let reader = DataReader::FromBuffer(buf)?;
    let mut out = vec![0u8; buf.Length()? as usize];
    reader.ReadBytes(&mut out)?;
    Ok(out)
}

fn bytes_to_buffer(bytes: &[u8]) -> windows::core::Result<IBuffer> {
    let buf = Buffer::Create(bytes.len() as u32)?;
    buf.SetLength(bytes.len() as u32)?;
    let writer = DataWriter::new()?;
    writer.WriteBytes(bytes)?;
    writer.DetachBuffer()
}

// ---------------------------------------------------------------- PDF

/// Page count of a PDF, or an error for damaged or password-protected files.
pub fn pdf_page_count(pdf: &[u8]) -> Result<u32> {
    let pdf = pdf.to_vec();
    on_mta(move || {
        let doc = load_pdf(&pdf)?;
        doc.PageCount().map_err(|e| os_err("PDF", e))
    })
}

fn load_pdf(pdf: &[u8]) -> Result<PdfDocument> {
    let stream = stream_from_bytes(pdf).map_err(|e| os_err("PDF", e))?;
    let stream: IRandomAccessStream = stream.cast().map_err(|e| os_err("PDF", e))?;
    PdfDocument::LoadFromStreamAsync(&stream)
        .and_then(|op| op.join())
        .map_err(|_| Error::Corrupt("PDF is damaged or password-protected".into()))
}

/// Renders the given 0-based pages at `dpi`, handing each bitmap to `f` as soon as it exists so only one
/// page is in memory at a time. A page whose pixels would exceed `max_pixels` is scaled down.
pub fn render_pdf_each<T: Send + 'static>(
    pdf: &[u8],
    pages: &[u32],
    dpi: u32,
    max_pixels: u64,
    mut f: impl FnMut(u32, RgbaImage) -> Result<T> + Send + 'static,
) -> Result<Vec<T>> {
    let pdf = pdf.to_vec();
    let pages = pages.to_vec();
    on_mta(move || {
        let doc = load_pdf(&pdf)?;
        let count = doc.PageCount().map_err(|e| os_err("PDF", e))?;
        let mut out = Vec::with_capacity(pages.len());
        for &i in &pages {
            if i >= count {
                return Err(Error::InvalidArgument(format!("page {} of {count}", i + 1)));
            }
            let page = doc.GetPage(i).map_err(|e| os_err("PDF page", e))?;
            let size = page.Size().map_err(|e| os_err("PDF page", e))?;
            let scale = dpi as f64 / 96.0;
            let (mut w, mut h) = ((size.Width as f64 * scale).max(1.0), (size.Height as f64 * scale).max(1.0));
            let px = w * h;
            if px > max_pixels as f64 {
                let k = (max_pixels as f64 / px).sqrt();
                w = (w * k).max(1.0);
                h = (h * k).max(1.0);
            }
            let opts = PdfPageRenderOptions::new().map_err(|e| os_err("PDF page", e))?;
            opts.SetDestinationWidth(w as u32).map_err(|e| os_err("PDF page", e))?;
            opts.SetDestinationHeight(h as u32).map_err(|e| os_err("PDF page", e))?;
            opts.SetBackgroundColor(windows::UI::Color { A: 255, R: 255, G: 255, B: 255 })
                .map_err(|e| os_err("PDF page", e))?;
            let target = InMemoryRandomAccessStream::new().map_err(|e| os_err("PDF page", e))?;
            page.RenderWithOptionsToStreamAsync(&target, &opts)
                .and_then(|op| op.join())
                .map_err(|e| os_err("PDF page render", e))?;
            let png = stream_to_bytes(&target).map_err(|e| os_err("PDF page", e))?;
            let bitmap = image::load_from_memory_with_format(&png, image::ImageFormat::Png)?.to_rgba8();
            out.push(f(i, bitmap)?);
        }
        Ok(out)
    })
}

/// Renders the given pages into memory (see `render_pdf_each` for the one-at-a-time variant).
pub fn render_pdf_pages(pdf: &[u8], pages: &[u32], dpi: u32, max_pixels: u64) -> Result<Vec<RgbaImage>> {
    render_pdf_each(pdf, pages, dpi, max_pixels, |_, img| Ok(img))
}

// ---------------------------------------------------------------- OCR

/// Recognises the text of a picture with the OCR engine built into Windows (your profile languages).
/// Must run on an MTA thread, e.g. inside the `render_pdf_each` callback.
fn ocr_bitmap(img: &RgbaImage) -> Result<String> {
    use windows::Media::Ocr::OcrEngine;
    let unavailable = |reason: String| Error::Unavailable { feature: "OCR", reason };
    let engine = OcrEngine::TryCreateFromUserProfileLanguages()
        .map_err(|_| unavailable("Windows has no OCR language pack for your profile languages".into()))?;
    let max = OcrEngine::MaxImageDimension().unwrap_or(10_000);
    let img = if img.width().max(img.height()) > max {
        let k = max as f64 / img.width().max(img.height()) as f64;
        let (w, h) = (((img.width() as f64 * k) as u32).max(1), ((img.height() as f64 * k) as u32).max(1));
        std::borrow::Cow::Owned(image::imageops::resize(img, w, h, image::imageops::FilterType::Triangle))
    } else {
        std::borrow::Cow::Borrowed(img)
    };
    // The engine reads BGRA.
    let mut bgra = img.as_raw().clone();
    bgra.chunks_exact_mut(4).for_each(|p| p.swap(0, 2));
    let buf = bytes_to_buffer(&bgra).map_err(|e| os_err("OCR", e))?;
    let bmp = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
        &buf,
        BitmapPixelFormat::Bgra8,
        img.width() as i32,
        img.height() as i32,
        BitmapAlphaMode::Ignore,
    )
    .map_err(|e| os_err("OCR", e))?;
    let result = engine.RecognizeAsync(&bmp).and_then(|op| op.join()).map_err(|e| os_err("OCR", e))?;
    Ok(result.Text().map_err(|e| os_err("OCR", e))?.to_string())
}

/// Text of every page of a scanned PDF, by rendering each page at `dpi` and running OCR on it.
pub fn ocr_pdf_pages(pdf: &[u8], dpi: u32, max_pixels: u64) -> Result<Vec<String>> {
    let count = pdf_page_count(pdf)?;
    let pages: Vec<u32> = (0..count).collect();
    render_pdf_each(pdf, &pages, dpi, max_pixels, |_, bitmap| ocr_bitmap(&bitmap))
}

// ---------------------------------------------------------------- HEIC / AVIF

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Availability {
    Available,
    Unavailable(String),
}

impl Availability {
    pub fn is_available(&self) -> bool {
        matches!(self, Availability::Available)
    }
}

fn ext_of(fmt: Fmt) -> &'static str {
    match fmt {
        Fmt::Avif => ".avif",
        _ => ".heic",
    }
}

fn find_codec(fmt: Fmt, encoder: bool) -> Option<GUID> {
    let wanted: &[&str] = if fmt == Fmt::Avif { &[".avif"] } else { &[".heic", ".heif"] };
    let list = if encoder {
        BitmapEncoder::GetEncoderInformationEnumerator().ok()?
    } else {
        BitmapDecoder::GetDecoderInformationEnumerator().ok()?
    };
    for info in list {
        let Ok(exts) = info.FileExtensions() else { continue };
        if exts.into_iter().any(|e| wanted.iter().any(|w| e.to_string().eq_ignore_ascii_case(w))) {
            return info.CodecId().ok();
        }
    }
    None
}

/// Decodes any image the Windows Imaging Component understands (used for HEIC and AVIF).
pub fn decode_with_wic(bytes: &[u8], what: &'static str) -> Result<RgbaImage> {
    let bytes = bytes.to_vec();
    on_mta(move || {
        let stream = stream_from_bytes(&bytes).map_err(|e| os_err(what, e))?;
        let stream: IRandomAccessStream = stream.cast().map_err(|e| os_err(what, e))?;
        let decoder = BitmapDecoder::CreateAsync(&stream).and_then(|op| op.join()).map_err(|_| Error::Unavailable {
            feature: what,
            reason: format!("Windows has no {what} codec installed (Microsoft Store: HEIF / AV1 extensions)"),
        })?;
        let bmp = decoder
            .GetSoftwareBitmapConvertedAsync(BitmapPixelFormat::Rgba8, BitmapAlphaMode::Straight)
            .and_then(|op| op.join())
            .map_err(|e| os_err(what, e))?;
        let (w, h) = (bmp.PixelWidth().unwrap_or(0) as u32, bmp.PixelHeight().unwrap_or(0) as u32);
        if (w as u64) * (h as u64) > super::raster::MAX_PIXELS {
            return Err(Error::TooLarge { width: w, height: h });
        }
        let buf = Buffer::Create(w * h * 4).map_err(|e| os_err(what, e))?;
        bmp.CopyToBuffer(&buf).map_err(|e| os_err(what, e))?;
        let raw = buffer_to_bytes(&buf.cast::<IBuffer>().map_err(|e| os_err(what, e))?).map_err(|e| os_err(what, e))?;
        RgbaImage::from_raw(w, h, raw).ok_or_else(|| Error::Corrupt(format!("{what}: pixel buffer size mismatch")))
    })
}

/// Encodes with the OS codec registered for `fmt` (HEIC). `Unavailable` when Windows lacks one.
pub fn encode_with_wic(img: &RgbaImage, fmt: Fmt) -> Result<Vec<u8>> {
    let (w, h, raw) = (img.width(), img.height(), img.as_raw().clone());
    let label = fmt.label();
    on_mta(move || {
        let unavailable = |why: &str| Error::Unavailable { feature: label, reason: why.to_string() };
        let id = find_codec(fmt, true)
            .ok_or_else(|| unavailable(&format!("Windows has no {label} encoder for {}", ext_of(fmt))))?;
        let out = InMemoryRandomAccessStream::new().map_err(|e| os_err(label, e))?;
        let enc = BitmapEncoder::CreateAsync(id, &out)
            .and_then(|op| op.join())
            .map_err(|_| unavailable(&format!("the {label} encoder could not start")))?;
        let buf = bytes_to_buffer(&raw).map_err(|e| os_err(label, e))?;
        let bmp = SoftwareBitmap::CreateCopyWithAlphaFromBuffer(
            &buf,
            BitmapPixelFormat::Rgba8,
            w as i32,
            h as i32,
            BitmapAlphaMode::Straight,
        )
        .map_err(|e| os_err(label, e))?;
        enc.SetSoftwareBitmap(&bmp).map_err(|e| os_err(label, e))?;
        enc.FlushAsync()
            .and_then(|op| op.join())
            .map_err(|_| unavailable(&format!("the {label} encoder failed (HEVC video extension missing?)")))?;
        stream_to_bytes(&out).map_err(|e| os_err(label, e))
    })
}

static HEIC_ENC: OnceLock<Availability> = OnceLock::new();
static DECODE: OnceLock<(Availability, Availability)> = OnceLock::new();

/// Can this machine read `fmt` right now? (Probed once per process.)
pub fn can_decode(fmt: Fmt) -> Availability {
    let (heic, avif) = DECODE.get_or_init(|| {
        let probe = |f: Fmt| match on_mta(move || Ok(find_codec(f, false).is_some())) {
            Ok(true) => Availability::Available,
            _ => Availability::Unavailable(format!("no {} decoder is installed in Windows", f.label())),
        };
        (probe(Fmt::Heic), probe(Fmt::Avif))
    });
    match fmt {
        Fmt::Heic => heic.clone(),
        Fmt::Avif => avif.clone(),
        _ => Availability::Available,
    }
}

/// Can this machine write `fmt` right now? HEIC is verified with a real 16x16 test encode.
pub fn can_encode(fmt: Fmt) -> Availability {
    match fmt {
        Fmt::Heic => HEIC_ENC
            .get_or_init(|| {
                match encode_with_wic(&RgbaImage::from_pixel(16, 16, image::Rgba([90, 120, 200, 255])), Fmt::Heic) {
                    Ok(_) => Availability::Available,
                    Err(Error::Unavailable { reason, .. }) => Availability::Unavailable(reason),
                    Err(e) => Availability::Unavailable(e.to_string()),
                }
            })
            .clone(),
        Fmt::Avif if cfg!(feature = "avif-encode") => Availability::Available,
        Fmt::Avif => {
            if on_mta(|| Ok(find_codec(Fmt::Avif, true).is_some())).unwrap_or(false) {
                Availability::Available
            } else {
                Availability::Unavailable("Windows ships no AVIF encoder".into())
            }
        }
        _ => Availability::Available,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::pdfwrite::text_to_pdf;

    #[test]
    fn damaged_pdf_is_a_typed_error() {
        assert!(matches!(pdf_page_count(b"%PDF-1.4 garbage"), Err(Error::Corrupt(_))));
        assert!(matches!(pdf_page_count(b""), Err(Error::Corrupt(_))));
    }

    #[test]
    fn page_index_out_of_range_is_rejected() {
        let pdf = text_to_pdf("one page");
        assert!(matches!(render_pdf_pages(&pdf, &[1], 96, 1 << 24), Err(Error::InvalidArgument(_))));
    }

    #[test]
    fn dpi_scales_the_render_and_the_pixel_cap_scales_it_down() {
        let pdf = text_to_pdf("scale");
        let small = render_pdf_pages(&pdf, &[0], 72, 100_000_000).unwrap().remove(0);
        let big = render_pdf_pages(&pdf, &[0], 144, 100_000_000).unwrap().remove(0);
        assert!(big.width() >= small.width() * 2 - 2);
        let capped = render_pdf_pages(&pdf, &[0], 144, 100_000).unwrap().remove(0);
        assert!((capped.width() as u64) * (capped.height() as u64) <= 100_000);
    }

    #[test]
    fn codec_availability_is_reported_honestly() {
        let heic_dec = can_decode(Fmt::Heic);
        let avif_dec = can_decode(Fmt::Avif);
        let heic_enc = can_encode(Fmt::Heic);
        let avif_enc = can_encode(Fmt::Avif);
        eprintln!(
            "HEIC decode {heic_dec:?}\nAVIF decode {avif_dec:?}\nHEIC encode {heic_enc:?}\nAVIF encode {avif_enc:?}"
        );
        assert!(can_decode(Fmt::Png).is_available() && can_encode(Fmt::Jpg).is_available());
        // Consistency: if the probe says HEIC can be written, writing really works and reads back.
        if heic_enc.is_available() {
            let img = RgbaImage::from_pixel(32, 32, image::Rgba([200, 40, 40, 255]));
            let bytes = encode_with_wic(&img, Fmt::Heic).unwrap();
            assert!(super::super::raster::decode(&bytes).is_ok());
        } else {
            assert!(matches!(encode_with_wic(&RgbaImage::new(4, 4), Fmt::Heic), Err(Error::Unavailable { .. })));
        }
    }
}
