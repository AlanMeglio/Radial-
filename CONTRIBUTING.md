# Contributing

Small, focused pull requests are easiest to review. Before opening one:

```
cargo fmt
cargo clippy --all-targets -- -D warnings
cargo test
```

- Pure logic is unit tested and new logic starts with a failing test: hit-testing, drag detection, ring building
  (`src/ring.rs`, `src/menus.rs`), the crop rectangle maths (`src/dialogs/geom.rs`), the edit and annotation state
  machines, settings round trips, the engine (`src/engine`). The MSRV is Rust 1.88 (the `image` crate needs it); clippy checks it.
- Win32 and rendering code cannot be unit tested. Look at it instead:
  - `radial --preview out.png --menu 8|4|7|6 [--hover N] [--theme T] [--dpi 2]` renders a ring over a sample wallpaper.
  - `radial --settings out.png [--tab 1] [--step 0] [--lang es] [--light]` renders the configurator.
  - `radial --dialog crop|bg|edit|annotate|resize|rename|sector --out out.png [--script "..."]` renders a dialog, optionally after
    driving it (`click:ID`, `drag:x1,y1,x2,y2`, `text:abc`, `key:13`, `focus:ID`; the ids are the `ID_*` constants at
    the top of each dialog file). `radial --sheet out.png` renders every theme.
  - `radial --bench` times a frame, `radial --formats` shows what the machine can read and write.
  - Regenerate the pictures in `docs/` with those flags when the look changes (the README lists them).
- Two scripts drive the real thing against a release build. They use the mouse and keyboard, so do not touch either
  while they run:
  - `powershell -File tests\e2e.ps1` drags files out of a test form and checks the results over a matrix of every
    kind of file (picture, PDF, Word, text, CSV, workbook, ZIP, folder, an unknown `.ogg`), every ring they have and
    the sectors that matter, reached by the wheel, Tab and the hub, plus several files at once and the CROP dialog
    (it must open in front, Enter must apply it and the window must close). `-Only "XLSX"` runs the cases whose
    name has that text; `-Repeat 10` repeats each one (to hunt for something intermittent).
  - `powershell -File tests\ui-smoke.ps1` opens the settings window and every action dialog on a 6 megapixel picture,
    checks they draw and close with Esc, and reports memory before, during and after plus idle CPU.
- Keep the idle cost at zero: no polling loops or timers that run while no menu or dialog is open. A dialog may
  animate only while a worker thread is busy.
- Nothing in the engine may overwrite a file or panic on a damaged input; use `engine::io::commit_unique`.
- Strings the user sees go in `STRINGS` in `src/config.rs` with both an English and a Spanish text (a test checks
  that every key has both).
- Conventional Commit messages (`feat:`, `fix:`, `docs:`...).

## Adding a theme

A theme is a text file, see [Themes](README.md#themes) for every key.

1. Copy `assets/themes/glass.theme` to `assets/themes/my-theme.theme` and change the colours (`name` is lowercase
   letters, digits and `-`).
2. List it in `BUILTIN` in `src/theme.rs` so it is embedded in the binary.
3. Check it with `radial --preview out.png --theme my-theme`, then run `radial --sheet docs/themes.png` and commit the
   new sheet. `cargo test` checks that every built-in theme parses and renders.

Users can also drop a `.theme` file into `%APPDATA%\radial\themes` without rebuilding.

## Adding an action or a sector

Rings are data, not drawing code: add the operation to `src/ring.rs` (`ActionKind`, then the list it belongs to:
`PICTURE_TOOLS`, `PDF_TOOLS` or `FILE_TOOLS`, and `EVERY`), its label and hint strings to `STRINGS`, an icon in
`src/render.rs`, and the work to `src/universal.rs` (runs on drop, any file) or `src/jobs.rs`, or a dialog in
`src/dialogs` (implement `Dialog`, register it in `dialogs::open`). The engine function belongs in `src/engine`. A
conversion between two kinds of file also needs its row in `engine::supports` and its arm in
`pipeline::convert_bytes`; a test keeps the two in step.

## Windows resources

The manifest (per-monitor DPI awareness, long paths), the icon and the version information live in `assets/` and are
linked from the committed `assets/radial.res` by `build.rs`, so a normal build needs nothing but Rust. After changing
the version, the manifest or the icon, run `tools\make-icon.ps1` (rebuilds `assets/radial.ico` from the app's own
icon renderer) and `tools\build-resources.ps1` (needs `rc.exe` from the Windows SDK) and commit the results.

## Releasing

Update the version in `Cargo.toml` and `assets/radial.rc`, add a section to `CHANGELOG.md`, rebuild the resources,
then tag `vX.Y.Z` and push the tag. (If you build a release by hand, set
`RUSTFLAGS="--remap-path-prefix=%USERPROFILE%=~"` first: panic locations of dependencies otherwise embed your home
directory in the executable. The workflow does this.) `.github/workflows/release.yml` builds the executable, zips it with the licence and
README, and attaches both to the GitHub release.
