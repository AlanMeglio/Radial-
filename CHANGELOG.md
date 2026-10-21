# Changelog

All notable changes are listed here. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and
the project uses [Semantic Versioning](https://semver.org/).

## [Unreleased]

### Added

- **Four rings, and the rings a file has.** Documents and Images convert it, Tools hold what only that kind of file
  has, Files hold what works on any file (same order every time), and "yours" holds your own sectors. A ring with
  nothing for the dragged files is skipped. The mouse wheel walks through them while you drag (down is next); Tab and
  resting on the hub still work. Any file opens a menu now, including folders, ZIPs and files Radial cannot convert.
- **Excel and CSV.** A workbook becomes CSV (one per sheet; dates stay dates), TXT, PDF or pictures; a CSV becomes a
  workbook, PDF or pictures. XLSX is read and written without any dependency.
- **Documents reach pictures** (Word, text, CSV and workbooks through a PDF) and **pictures reach text** (Windows OCR).
- **COPY TEXT** puts the text of a picture or PDF on the clipboard. **RESIZE** shrinks pictures with presets (email,
  web, phone, small, half, quarter, or your own size) in one click, for any number of files.
- **MERGE** turns several PDFs, pictures, Word, text, CSV or workbook files into one PDF in natural order; **SPLIT**
  writes one PDF per page. Both copy the real content byte for byte (text stays text, fonts and vectors stay).
- **RENAME** with a live preview (find and replace, case, spaces, accents, prefix and suffix, numbering, dates,
  extension case), safe swaps and chains, never overwriting.
- **ZIP** (streaming, constant memory), **UNZIP** (refuses unsafe paths and zip bombs), **INFO**, **SHA-256** (verifies
  against a hash on the clipboard), **COPY PATH**, **REVEAL** and **SORT** (a folder into subfolders by type or month)
  work on any file. The tray menu has **Undo last rename or sort**.
- **Your own sectors** (Settings > My sectors): move or copy to a folder, or open with a program, optionally for some
  extensions only. They get their own ring.
- A new default look, **Windows 11**: flat acrylic sectors, the system accent colour, light or dark with the system.
  The dialogs and Settings use the same neutrals, 4 px controls and sentence-case captions.

### Changed

- The actions ring is now the Tools ring (copy text and resize joined the picture tools) and the Files ring.
  `start_ring` accepts `auto`, `documents`, `images`, `tools` and `files` (`convert` reads as `auto`, `actions` as
  `tools`).
- REDACT is gone: ANNOTATE's filled black box replaces pixels in the saved copy, which is the same thing.

### Removed

- Code nothing could reach (the engine had hidden it behind a blanket `allow(dead_code)`).

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
- Frosted-glass renderer with spring animation, eight built-in themes plus user themes, a configurator with live preview and a
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

[0.1.0]: https://github.com/AlanMeglio/Radial-/releases/tag/v0.1.0
