# Radial

Drag a file in Explorer, hold **Shift**, and a radial menu blooms under the cursor. Drop the file on a sector to
convert it. Written in Rust for Windows 11, about 2 MB, no runtime, zero CPU while idle.

![Radial themes](docs/themes.png)

## Features

- **Drag, hold a key, drop.** The menu opens at the cursor while you drag a file; the sector under the cursor lights
  up and the hub in the middle shows what you are about to do (and the size or format of the file).
- **Real frosted glass.** The area behind the menu is captured when it opens, blurred, saturated and tinted, so the
  sectors really look like glass over whatever is on your screen. Layered soft shadows, rim light, vector icons
  (nothing bundled, no emoji) and spring-driven motion with a staggered entrance, hover lift and a drop-confirmation
  pulse.
- **Seven themes, and yours.** Obsidian Glass, Frosted Paper, Mono Ink, Aurora, Sunset, Terminal Neon and Noir Gold.
  A theme is a plain text file, so adding one is a copy and a few colour edits (see [Themes](#themes)).
- **A configurator with a live preview.** First run starts a three step onboarding (pick a look, pick a key, done);
  afterwards it is one click away in the tray menu. It is drawn by the app itself, follows the system dark or light
  mode, and updates the preview while you drag the sliders.
- **Everything is adjustable.** Accent colour, size, ring thickness, gap, corner roundness, glass strength, animation
  speed, font, trigger key (Shift, Ctrl, Alt or none), open delay, output folder, what to do when the name is taken,
  sound or notification when done, start with Windows, English or Spanish.
- **Tiny and quiet.** One thread, event driven: the only things alive while idle are two low-level hooks and the tray
  icon. The frame timer exists only while something animates. The settings window is created on demand and freed
  when it closes.

Image formats PNG, JPG, WEBP, GIF, TIFF and BMP convert to each other. AVIF and HEIC are shown disabled because
there is no lean pure-Rust encoder yet. WEBP output is lossless. JPG output flattens transparency onto white.
The configurator previews the three menu layouts (images, documents, actions); wiring of the document and action
menus follows the conversion engine and is tracked in the roadmap.

## Install

Download `radial.exe` from the releases page and run it. It lives in the tray (right click for the menu). There is no
installer; delete the file and `%APPDATA%\radial` to remove it.

### Build

```
cargo build --release
```

The binary is `target/release/radial.exe`. Fonts are read from the Windows fonts folder (Segoe UI Variable, Bahnschrift,
Consolas, Georgia), nothing is bundled.

## Using it

1. Run `radial.exe`. On first run the onboarding opens.
2. Start dragging an image from Explorer or the desktop.
3. Hold the trigger key (Shift by default), move onto a sector and let go.

The result is written next to the original (or into your output folder) and never overwrites anything unless you
choose "Overwrite": `photo.png` becomes `photo (1).png`.

Tray menu: **Settings...**, **Pause / Resume**, **Start with Windows**, **About**, **Quit**. Double click the tray
icon to open the settings.

Command line (all optional, for scripting and development):

| Flag | What it does |
| --- | --- |
| `--autostart on` / `--autostart off` | Adds or removes the start-with-Windows entry (`HKCU\...\Run`) and exits |
| `--preview out.png [--theme glass] [--hover 2] [--menu 8\|4\|7] [--accent #ff8800] [--dpi 2]` | Renders the menu over a sample wallpaper |
| `--sheet out.png` | Renders every theme into one contact sheet (this is how `docs/themes.png` is made) |
| `--settings out.png [--tab 1] [--step 0..2] [--lang es] [--light]` | Renders the configurator window to a PNG |
| `--bench [--theme x] [--dpi 1.2]` | Prints the time to bake and render a frame |

## Settings file

`%APPDATA%\radial\config` is a `key = value` text file the configurator rewrites; you can edit it by hand (unknown
keys and bad values are ignored, numbers are clamped).

| Key | Values | Default |
| --- | --- | --- |
| `theme` | a theme name | `glass` |
| `accent` | `auto` or `#rrggbb` | `auto` |
| `size` | 0.7 to 1.5 | `1` |
| `ring`, `gap`, `corner` | ring thickness 56 to 150, gap 0 to 24, corner 0 to 40 (logical px) | from the theme |
| `glass` | 0 (plain tint) to 1.5 (strong blur) | `1` |
| `speed` | animation speed 0.4 to 2.5 | `1` |
| `font` | `theme`, `variable`, `bahnschrift`, `consolas`, `georgia` | `theme` |
| `trigger` | `shift`, `ctrl`, `alt`, `none` | `shift` |
| `delay_ms` | 0 to 800, how long the drag is held before the menu opens | `0` |
| `out_dir` | empty (next to the source) or a folder | empty |
| `collision` | `rename`, `overwrite`, `skip` | `rename` |
| `notify` | `off`, `balloon`, `sound`, `both` | `balloon` |
| `autostart` | `true` / `false` | `false` |
| `lang` | `en`, `es` | system language |

## Themes

A theme is a `key = value` file; every key is optional and defaults to Obsidian Glass. The built-in ones live in
[`assets/themes`](assets/themes) and are embedded in the binary. Your own go in `%APPDATA%\radial\themes\*.theme` and
show up in the configurator next to the built-in ones.

```
# my-theme.theme
name = my-theme            # lowercase letters, digits and - only
label = My Theme
accent = #ff8a3d
accent2 = #ff3d81          # second colour of the accent gradient
glass_top = #3a2f4aa0      # tint at the outer edge, #rrggbbaa: alpha is how much of the frosted backdrop it hides
glass_bottom = #120d1ac8   # tint at the inner edge
ring = 96
gap = 4
corner = 18
weight = 500
stiffness = 220            # spring stiffness, higher is snappier
damping = 0.6              # below 1 overshoots, above 1 is sluggish
```

Colours are `#rgb`, `#rrggbb` or `#rrggbbaa`. The full key list, with ranges, is in
[`src/theme.rs`](src/theme.rs) (`Theme::set` and `Theme::validate`); an invalid file is skipped and
`radial --preview out.png --theme my-theme` shows the result without touching your settings. To ship a theme with
the app, drop the file into `assets/themes` and list it in `BUILTIN`.

## How it works

- Low-level mouse and keyboard hooks feed a small state machine (`src/trigger.rs`) that spots "left button dragged,
  trigger key held". Nothing else runs while idle.
- The menu is a topmost, non-activating layered window with a per-pixel-alpha bitmap and an OLE drop target, so it
  receives the drop like any other window. Its buffers only exist while the menu is on screen.
- Rendering (`src/render.rs`) is software, with [tiny-skia](https://github.com/RazrFalcon/tiny-skia). The frosted
  glass, tint, light and shadows are baked once when a menu opens, so a frame only copies them through the sector
  shapes and draws the lit parts, the icons and the text.
- Hit-testing and corner geometry are pure math (`src/geometry.rs`); themes (`src/theme.rs`) and settings
  (`src/config.rs`) are plain data with unit tests.

## Limits

- Drags are only recognised when they start over an Explorer window or the desktop.
- Windows probably blocks drops from elevated (admin) windows onto a normal process (UIPI); not tested.
- The glass is a snapshot of the screen taken when the menu opens, not a live blur: Windows' acrylic and
  blur-behind effects cannot follow the shape of a per-pixel-alpha layered window (they blur the whole rectangle).
  Anything that moves behind the menu while it is open does not show through.
- Windows 11 is the target; the tray menu, settings window and drag handling use only long-standing Win32 calls.

## License

MIT, see [LICENSE](LICENSE).
