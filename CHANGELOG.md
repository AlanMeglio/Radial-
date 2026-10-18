# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
the project uses [Semantic Versioning](https://semver.org/).

## [0.1.0] - 2026-10-18

First public release.

### Added

- Drag a file in Explorer or on the desktop, hold Shift (or Ctrl, Alt or nothing) and a radial menu opens under the
  cursor; drop the file on a sector to act on it.
- Menus built from the file's content: an eight sector image ring (PNG, WEBP, HEIC, TIFF, AVIF, BMP, PDF, DOCX, the
  source's own format replaced by JPG), a four sector PDF ring (DOCX, JPG, PNG, TXT), and rings for DOCX and text
  files. The hub shows the format or the file size.
- Greyed-out sectors with the reason in a caption under the ring when this machine cannot do something (missing
  Windows codec, a format an action does not handle, several files for a dialog action).
- An actions ring for images: COMPRESS and METADATA run on drop and report the result (before and after sizes, what
  was removed); EDIT, ANNOTATE, ADD BG, CROP and REDACT open dialogs. Flip between the rings with Tab or by resting
  on the hub (configurable, as is the ring an image opens on).
- Native action dialogs, custom drawn in the app's own look, dark and light aware, DPI aware and keyboard
  accessible: Crop Image (aspect ratios, draggable rectangle with handles, W and H fields), Add BG (gradients,
  colours or a picture, Padding, Corners, Shadow, ratio), Edit (rotate, flip, resize, grayscale, brightness,
  contrast), Annotate (pen, line, arrow, box, ellipse, marker, text, undo) and Redact (pixel replacement).
- Conversion engine without external tools: images (PNG, JPG, WEBP, GIF, TIFF, BMP in pure Rust; HEIC and AVIF through
  the Windows codecs), PDF to images, text and DOCX (with Windows OCR for scanned pages), images and text to PDF,
  DOCX to PDF and TXT. Nothing is ever overwritten.
- Frosted-glass renderer with spring animation, seven themes plus user themes, a configurator with live preview and a
  three step onboarding, English and Spanish.
- Output folder, name collision policy, notification and sound settings, start with Windows, tray menu with pause.
- Command line: `--formats`, `--open`, `--preview`, `--sheet`, `--settings`, `--dialog`, `--tray-icon`, `--bench`,
  `--autostart`, `--help`, `--version`.
- Application manifest (per-monitor DPI awareness, long paths), icon and version information embedded in the exe.
- Optional `avif-encode` cargo feature for a pure-Rust AVIF encoder.

### Known limits

- Drags are recognised only from Explorer windows and the desktop; drops from elevated windows are probably blocked
  by UIPI (not tested).
- The glass is a snapshot taken when the menu opens, not a live blur.
- AVIF encoding is off by default; password-protected PDFs are refused; PDFs without a Unicode font map give no text.
- Action dialogs take one image at a time; animated GIFs become still pictures.

[0.1.0]: https://github.com/OWNER/radial/releases/tag/v0.1.0
