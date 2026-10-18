# Radial

Drag a file in Explorer, hold **Shift**, and a radial menu opens under the cursor. Drop the file on a sector to
convert it. The result is written next to the original and never overwrites anything (`photo.png`, `photo (1).png`).

<!-- TODO: replace with a screen recording -->
![Radial demo](docs/demo.gif)

Windows 11, written in Rust, about 1.9 MB, no runtime, zero CPU while idle.

## Usage

1. Run `radial.exe`. It lives in the tray (right click, Quit).
2. Start dragging an image from Explorer or the desktop.
3. Hold Shift, move onto a sector, release.

Images: PNG, JPG, WEBP, GIF, TIFF and BMP convert to each other. AVIF and HEIC are shown disabled because there is no
lean pure-Rust encoder yet. WEBP output is lossless. JPG output flattens transparency onto white.
PDF, DOCX and TXT, plus the crop and background tools, are planned.

## Build

```
cargo build --release
```

The binary is `target/release/radial.exe`. Fonts are read from the Windows fonts folder (Segoe UI), nothing is bundled.

## How it works

- Low-level mouse and keyboard hooks feed a small state machine (`src/trigger.rs`) that spots "left button dragged,
  Shift held". Nothing else runs while idle.
- The menu is a topmost, non-activating layered window drawn with a software rasterizer and an OLE drop target, so
  it receives the drop like any other window. The window buffers only exist while the menu is on screen.
- Sector hit-testing is pure math (`src/geometry.rs`).

## Limits

Drags are only recognised when they start over an Explorer window or the desktop. Windows probably blocks drops from elevated (admin) windows (UIPI); not tested.

## License

MIT
