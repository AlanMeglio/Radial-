//! The configurator: a custom-drawn window (tiny-skia into a DIB) with a live preview of the radial menu.
//! It is created on demand and every buffer is freed when it closes. It doubles as the first-run onboarding.

use std::cell::RefCell;
use std::ffi::c_void;
use std::mem::{size_of, zeroed};
use std::ptr::{null, null_mut};
use std::time::Instant;

use tiny_skia::{
    Color, FillRule, FilterQuality, Paint, Path, PathBuilder, Pattern, Pixmap, PixmapPaint, SpreadMode, Transform,
};
use windows_sys::Win32::Foundation::*;
use windows_sys::Win32::Graphics::Gdi::*;
use windows_sys::Win32::System::Com::CoTaskMemFree;
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::HiDpi::GetDpiForMonitor;
use windows_sys::Win32::UI::Input::KeyboardAndMouse::*;
use windows_sys::Win32::UI::WindowsAndMessaging::*;

use crate::config::*;
use crate::geometry::sector_at;
use crate::overlay::Spring;
use crate::render::{self, clamp01, demo_scene, linear, solid, stroke, Backdrop, Face, Look, Renderer, Scene};
use crate::theme::{self, FontKind, Rgba, Theme};

// ---------------------------------------------------------------------------------------------------------------
// Layout constants (logical pixels)
// ---------------------------------------------------------------------------------------------------------------

const W: f32 = 940.0;
const H: f32 = 660.0;
const BOX: (f32, f32, f32, f32) = (24.0, 84.0, 440.0, 440.0); // preview card
const RX: f32 = 496.0; // right panel left edge
const RW: f32 = 420.0; // right panel width
const TIMER: usize = 1;
const WM_MOUSELEAVE: u32 = 0x02A3;

// ---------------------------------------------------------------------------------------------------------------
// Palette
// ---------------------------------------------------------------------------------------------------------------

pub(crate) struct Pal {
    pub bg: Rgba,
    pub card: Rgba,
    pub card_hi: Rgba,
    pub border: Rgba,
    pub text: Rgba,
    pub dim: Rgba,
    pub accent: Rgba,
    pub on_accent: Rgba,
}

pub(crate) fn luma(c: Rgba) -> f32 {
    (0.299 * c.0 as f32 + 0.587 * c.1 as f32 + 0.114 * c.2 as f32) / 255.0
}

pub(crate) fn palette(dark: bool, accent: Rgba) -> Pal {
    // The configurator wears the chosen accent, unless it would vanish against the background.
    let fallback = Rgba::rgb(0x5b, 0x8c, 0xff);
    let ok = if dark { luma(accent) > 0.28 } else { luma(accent) < 0.72 };
    let accent = if ok { Rgba(accent.0, accent.1, accent.2, 255) } else { fallback };
    let on_accent = if luma(accent) > 0.6 { Rgba::rgb(0x10, 0x12, 0x18) } else { Rgba::rgb(255, 255, 255) };
    if dark {
        Pal {
            bg: Rgba::rgb(0x12, 0x14, 0x1a),
            card: Rgba::rgb(0x1b, 0x1e, 0x27),
            card_hi: Rgba::rgb(0x26, 0x2a, 0x36),
            border: Rgba::rgb(0x2c, 0x31, 0x3f),
            text: Rgba::rgb(0xe9, 0xec, 0xf4),
            dim: Rgba::rgb(0x8d, 0x95, 0xa9),
            accent,
            on_accent,
        }
    } else {
        Pal {
            bg: Rgba::rgb(0xf1, 0xf3, 0xf8),
            card: Rgba::rgb(0xff, 0xff, 0xff),
            card_hi: Rgba::rgb(0xe6, 0xe9, 0xf2),
            border: Rgba::rgb(0xd7, 0xdb, 0xe6),
            text: Rgba::rgb(0x1a, 0x1f, 0x2e),
            dim: Rgba::rgb(0x67, 0x6f, 0x84),
            accent,
            on_accent,
        }
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Model
// ---------------------------------------------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Debug)]
enum Sl {
    Size,
    Ring,
    Gap,
    Corner,
    Glass,
    Speed,
    Delay,
    Hue,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Sg {
    Font,
    Trigger,
    OutMode,
    Collision,
    Notify,
    Lang,
    Menu,
}

#[derive(Clone, Copy, PartialEq, Debug)]
enum Act {
    Theme(usize),
    Accent(Option<Rgba>),
    Slider(Sl),
    Seg(Sg, usize),
    Autostart,
    Tab(usize),
    Browse,
    Reset,
    Done,
    Next,
    Back,
}

struct Hit {
    r: [f32; 4],
    act: Act,
}

struct Preview {
    wall: Option<Pixmap>,
    menu: Option<Pixmap>,
    renderer: Renderer,
    scene: Scene,
    appear: Vec<Spring>,
    hover_sp: Vec<Spring>,
    hover: Option<usize>,
    kind: usize,
    age: f32,
    last: Instant,
    /// Device-pixel rectangle of the preview card and the render scale inside it.
    rect: [f32; 4],
    dpi_eff: f32,
    center: (f32, f32),
    running: bool,
}

struct Ui {
    hwnd: HWND,
    dpi: f32,
    w: i32,
    h: i32,
    pm: Pixmap,
    dc: HDC,
    dib: HBITMAP,
    old: HGDIOBJ,
    bits: *mut u8,
    face: Face,
    cfg: Config,
    themes: Vec<Theme>,
    look: Look,
    dark: bool,
    tab: usize,
    step: Option<usize>,
    hits: Vec<Hit>,
    hot: Option<Act>,
    drag: Option<Sl>,
    prev: Preview,
    tracking: bool,
}

thread_local! {
    static UI: RefCell<Option<Ui>> = const { RefCell::new(None) };
}

fn with_ui<R>(f: impl FnOnce(&mut Ui) -> R) -> Option<R> {
    UI.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.as_mut().map(f)))
}

fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(Some(0)).collect()
}

// ---------------------------------------------------------------------------------------------------------------
// Drawing primitives
// ---------------------------------------------------------------------------------------------------------------

pub(crate) fn rrect(x: f32, y: f32, w: f32, h: f32, r: f32) -> Option<Path> {
    let r = r.min(w / 2.0).min(h / 2.0).max(0.0);
    let k = r * 0.5523;
    let mut pb = PathBuilder::new();
    pb.move_to(x + r, y);
    pb.line_to(x + w - r, y);
    pb.cubic_to(x + w - r + k, y, x + w, y + r - k, x + w, y + r);
    pb.line_to(x + w, y + h - r);
    pb.cubic_to(x + w, y + h - r + k, x + w - r + k, y + h, x + w - r, y + h);
    pb.line_to(x + r, y + h);
    pb.cubic_to(x + r - k, y + h, x, y + h - r + k, x, y + h - r);
    pb.line_to(x, y + r);
    pb.cubic_to(x, y + r - k, x + r - k, y, x + r, y);
    pb.close();
    pb.finish()
}

/// Everything a widget needs to draw itself: the target, the font, the palette and the hit list being built.
struct Pen<'a> {
    pm: &'a mut Pixmap,
    face: &'a mut Face,
    pal: &'a Pal,
    s: f32,
    hits: &'a mut Vec<Hit>,
    hot: Option<Act>,
}

impl Pen<'_> {
    fn fill(&mut self, r: [f32; 4], rad: f32, c: Rgba) {
        let s = self.s;
        if let Some(p) = rrect(r[0] * s, r[1] * s, r[2] * s, r[3] * s, rad * s) {
            self.pm.fill_path(&p, &solid(c, 1.0), FillRule::Winding, Transform::identity(), None);
        }
    }

    fn outline(&mut self, r: [f32; 4], rad: f32, c: Rgba) {
        let s = self.s;
        if let Some(p) = rrect(r[0] * s + 0.5, r[1] * s + 0.5, r[2] * s - 1.0, r[3] * s - 1.0, rad * s) {
            self.pm.stroke_path(&p, &solid(c, 1.0), &stroke(1.0), Transform::identity(), None);
        }
    }

    fn text(&mut self, t: &str, x: f32, cy: f32, px: f32, c: Rgba, tracking: f32) {
        let s = self.s;
        self.face.draw_left(self.pm, t, x * s, cy * s, px * s, c, 1.0, tracking * px * s);
    }

    fn text_c(&mut self, t: &str, cx: f32, cy: f32, px: f32, c: Rgba, tracking: f32) {
        let s = self.s;
        self.face.draw(self.pm, t, cx * s, cy * s, px * s, c, 1.0, tracking * px * s);
    }

    fn text_r(&mut self, t: &str, right: f32, cy: f32, px: f32, c: Rgba) {
        let s = self.s;
        let w = self.face.measure(t, px * s, 0.0) / s;
        self.text(t, right - w, cy, px, c, 0.0);
    }

    fn hit(&mut self, r: [f32; 4], act: Act) {
        self.hits.push(Hit { r, act });
    }

    fn hovered(&self, act: Act) -> bool {
        self.hot == Some(act)
    }

    fn label(&mut self, t: &str, x: f32, y: f32) {
        let c = self.pal.dim;
        self.text(&t.to_uppercase(), x, y + 7.0, 10.5, c, 0.12);
    }

    /// Segmented control; `labels` are shown left to right.
    fn seg(&mut self, id: Sg, y: f32, labels: &[&str], sel: usize) {
        let r = [RX, y, RW, 30.0];
        self.fill(r, 9.0, self.pal.card);
        self.outline(r, 9.0, self.pal.border);
        let n = labels.len() as f32;
        let w = (RW - 6.0) / n;
        for (i, l) in labels.iter().enumerate() {
            let cell = [RX + 3.0 + w * i as f32, y + 3.0, w, 24.0];
            let act = Act::Seg(id, i);
            let (fill, txt) = if i == sel {
                (Some(self.pal.accent), self.pal.on_accent)
            } else if self.hovered(act) {
                (Some(self.pal.card_hi), self.pal.text)
            } else {
                (None, self.pal.dim)
            };
            if let Some(f) = fill {
                self.fill(cell, 7.0, f);
            }
            self.text_c(l, cell[0] + w / 2.0, cell[1] + 12.5, 12.0, txt, 0.0);
            self.hit(cell, act);
        }
    }

    /// Slider row: label on the left, track in the middle, value on the right.
    fn slider(&mut self, id: Sl, y: f32, label: &str, t: f32, value: &str) {
        self.text(label, RX, y + 12.0, 12.5, self.pal.text, 0.0);
        let track = [RX + 168.0, y + 6.0, 180.0, 12.0];
        self.track(id, track, t, self.pal.accent, true);
        self.text_r(value, RX + RW, y + 12.0, 12.0, self.pal.dim);
    }

    fn track(&mut self, id: Sl, r: [f32; 4], t: f32, knob: Rgba, filled: bool) {
        let mid = r[1] + r[3] / 2.0;
        self.fill([r[0], mid - 2.5, r[2], 5.0], 2.5, self.pal.card_hi);
        let kx = r[0] + r[2] * clamp01(t);
        if filled {
            self.fill([r[0], mid - 2.5, kx - r[0], 5.0], 2.5, self.pal.accent);
        }
        let big = self.hovered(Act::Slider(id));
        let rad = if big { 7.5 } else { 6.5 };
        self.fill([kx - rad - 1.0, mid - rad - 1.0, rad * 2.0 + 2.0, rad * 2.0 + 2.0], rad + 1.0, self.pal.bg);
        self.fill([kx - rad, mid - rad, rad * 2.0, rad * 2.0], rad, knob);
        self.hit([r[0] - 8.0, r[1] - 4.0, r[2] + 16.0, r[3] + 8.0], Act::Slider(id));
    }

    fn button(&mut self, r: [f32; 4], label: &str, act: Act, primary: bool) {
        let hov = self.hovered(act);
        let (bg, fg) = if primary {
            (
                if hov { self.pal.accent.mix(Rgba(255, 255, 255, 255), 0.15) } else { self.pal.accent },
                self.pal.on_accent,
            )
        } else {
            (if hov { self.pal.card_hi } else { self.pal.card }, self.pal.text)
        };
        self.fill(r, 10.0, bg);
        if !primary {
            self.outline(r, 10.0, self.pal.border);
        }
        self.text_c(label, r[0] + r[2] / 2.0, r[1] + r[3] / 2.0 + 0.5, 13.0, fg, 0.0);
        self.hit(r, act);
    }
}

// ---------------------------------------------------------------------------------------------------------------
// Slider value mapping
// ---------------------------------------------------------------------------------------------------------------

fn slider_t(cfg: &Config, id: Sl) -> f32 {
    let norm = |v: f32, (lo, hi): (f32, f32)| (v - lo) / (hi - lo);
    match id {
        Sl::Size => norm(cfg.size, SIZE_RANGE),
        Sl::Ring => norm(cfg.ring, RING_RANGE),
        Sl::Gap => norm(cfg.gap, GAP_RANGE),
        Sl::Corner => norm(cfg.corner, CORNER_RANGE),
        Sl::Glass => norm(cfg.glass, GLASS_RANGE),
        Sl::Speed => norm(cfg.speed, SPEED_RANGE),
        Sl::Delay => cfg.delay_ms as f32 / DELAY_MAX as f32,
        Sl::Hue => cfg.accent.map_or(0.0, |a| hue_of(a) / 360.0),
    }
}

fn set_slider(cfg: &mut Config, id: Sl, t: f32) {
    let t = clamp01(t);
    let map = |(lo, hi): (f32, f32)| lo + (hi - lo) * t;
    match id {
        Sl::Size => cfg.size = (map(SIZE_RANGE) * 20.0).round() / 20.0,
        Sl::Ring => cfg.ring = map(RING_RANGE).round(),
        Sl::Gap => cfg.gap = (map(GAP_RANGE) * 2.0).round() / 2.0,
        Sl::Corner => cfg.corner = map(CORNER_RANGE).round(),
        Sl::Glass => cfg.glass = (map(GLASS_RANGE) * 20.0).round() / 20.0,
        Sl::Speed => cfg.speed = (map(SPEED_RANGE) * 20.0).round() / 20.0,
        Sl::Delay => cfg.delay_ms = ((t * DELAY_MAX as f32 / 10.0).round() * 10.0) as u32,
        Sl::Hue => cfg.accent = Some(Rgba::from_hsv(t * 360.0, 0.62, 1.0)),
    }
}

fn hue_of(c: Rgba) -> f32 {
    let (r, g, b) = (c.0 as f32 / 255.0, c.1 as f32 / 255.0, c.2 as f32 / 255.0);
    let (mx, mn) = (r.max(g).max(b), r.min(g).min(b));
    let d = mx - mn;
    if d < 1e-4 {
        return 0.0;
    }
    let h = if mx == r {
        ((g - b) / d).rem_euclid(6.0)
    } else if mx == g {
        (b - r) / d + 2.0
    } else {
        (r - g) / d + 4.0
    };
    h * 60.0
}

fn slider_text(cfg: &Config, id: Sl, lang: Lang) -> String {
    match id {
        Sl::Size => format!("{:.0}%", cfg.size * 100.0),
        Sl::Ring => format!("{:.0}", cfg.ring),
        Sl::Gap => format!("{:.1}", cfg.gap),
        Sl::Corner => format!("{:.0}", cfg.corner),
        Sl::Glass => format!("{:.0}%", cfg.glass * 100.0),
        Sl::Speed => format!("{:.2}x", cfg.speed),
        Sl::Delay if cfg.delay_ms == 0 => tr(lang, "off").to_string(),
        Sl::Delay => format!("{} {}", cfg.delay_ms, tr(lang, "ms")),
        Sl::Hue => String::new(),
    }
}

const ACCENTS: [Rgba; 9] = [
    Rgba(0x6e, 0xf2, 0xc9, 255),
    Rgba(0x5a, 0xd2, 0xff, 255),
    Rgba(0x4f, 0x6b, 0xff, 255),
    Rgba(0x9a, 0x5b, 0xff, 255),
    Rgba(0xff, 0x4f, 0x8b, 255),
    Rgba(0xff, 0x9a, 0x5a, 255),
    Rgba(0xff, 0xd4, 0x5a, 255),
    Rgba(0x39, 0xff, 0x88, 255),
    Rgba(0xf2, 0xf2, 0xf2, 255),
];

// ---------------------------------------------------------------------------------------------------------------
// UI drawing
// ---------------------------------------------------------------------------------------------------------------

fn draw_theme_grid(p: &mut Pen, y: f32, themes: &[Theme], cfg: &Config) {
    p.label(tr(cfg.lang, "theme"), RX, y);
    let (cw, ch, gx) = (98.0, 58.0, 9.333);
    for (i, t) in themes.iter().take(8).enumerate() {
        let (cx, cy) = (RX + (i % 4) as f32 * (cw + gx), y + 20.0 + (i / 4) as f32 * (ch + 9.0));
        let r = [cx, cy, cw, ch];
        let sel = cfg.theme == t.name;
        let act = Act::Theme(i);
        // Card: the theme's sample wallpaper with a tiny glass chip in its colours.
        let s = p.s;
        if let Some(path) = rrect(r[0] * s, r[1] * s, r[2] * s, r[3] * s, 10.0 * s) {
            let paint = linear(
                (r[0] * s, r[1] * s),
                ((r[0] + r[2]) * s, (r[1] + r[3]) * s),
                &[(0.0, t.preview_a, 1.0), (1.0, t.preview_b, 1.0)],
            );
            p.pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        }
        let chip = [cx + 10.0, cy + 10.0, cw - 20.0, 22.0];
        p.fill(
            chip,
            (t.corner / 3.0).clamp(3.0, 10.0),
            t.glass_bottom.mix(t.glass_top, 0.5).fade(1.0).mix(Rgba(0, 0, 0, 255), 0.0),
        );
        p.fill([chip[0] + 6.0, chip[1] + 8.0, 22.0, 6.0], 3.0, t.accent);
        p.fill([chip[0] + 34.0, chip[1] + 8.0, 24.0, 6.0], 3.0, t.text.fade(0.6));
        let ink = if t.light { Rgba(0x1b, 0x22, 0x36, 255) } else { Rgba(255, 255, 255, 255) };
        p.text_c(&t.label, cx + cw / 2.0, cy + ch - 12.0, 10.5, ink, 0.0);
        if sel {
            p.outline([r[0] - 2.0, r[1] - 2.0, r[2] + 4.0, r[3] + 4.0], 12.0, p.pal.accent);
            p.outline([r[0] - 3.0, r[1] - 3.0, r[2] + 6.0, r[3] + 6.0], 13.0, p.pal.accent);
        } else if p.hovered(act) {
            p.outline([r[0] - 1.0, r[1] - 1.0, r[2] + 2.0, r[3] + 2.0], 11.0, p.pal.dim);
        }
        p.hit(r, act);
    }
}

fn draw_accent(p: &mut Pen, y: f32, cfg: &Config, theme: &Theme) {
    p.label(tr(cfg.lang, "accent"), RX, y);
    let mut x = RX;
    // First swatch: the theme's own accent ("auto").
    let mut swatches: Vec<(Option<Rgba>, Rgba)> = vec![(None, theme.accent)];
    swatches.extend(ACCENTS.iter().map(|&c| (Some(c), c)));
    for (sel_c, c) in swatches {
        let r = [x, y + 20.0, 28.0, 28.0];
        let act = Act::Accent(sel_c);
        let sel = cfg.accent == sel_c;
        p.fill(r, 14.0, c);
        p.outline(r, 14.0, p.pal.border);
        if sel_c.is_none() {
            // A ring marks "follow the theme".
            p.outline([r[0] + 4.0, r[1] + 4.0, r[2] - 8.0, r[3] - 8.0], 10.0, Rgba(0, 0, 0, 120));
        }
        if sel {
            p.outline([r[0] - 3.0, r[1] - 3.0, r[2] + 6.0, r[3] + 6.0], 17.0, p.pal.text);
        } else if p.hovered(act) {
            p.outline([r[0] - 2.0, r[1] - 2.0, r[2] + 4.0, r[3] + 4.0], 16.0, p.pal.dim);
        }
        p.hit(r, act);
        x += RW / 10.0;
    }
    // Hue strip for any colour.
    let strip = [RX, y + 58.0, RW, 12.0];
    let s = p.s;
    if let Some(path) = rrect(strip[0] * s, strip[1] * s, strip[2] * s, strip[3] * s, 6.0 * s) {
        let stops: Vec<(f32, Rgba, f32)> =
            (0..=6).map(|i| (i as f32 / 6.0, Rgba::from_hsv(i as f32 * 60.0, 0.62, 1.0), 1.0)).collect();
        let paint = linear((strip[0] * s, 0.0), ((strip[0] + strip[2]) * s, 0.0), &stops);
        p.pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
    }
    if let Some(a) = cfg.accent {
        let kx = RX + RW * hue_of(a) / 360.0;
        p.fill([kx - 8.0, strip[1] - 2.0, 16.0, 16.0], 8.0, p.pal.bg);
        p.fill([kx - 6.0, strip[1], 12.0, 12.0], 6.0, a);
    }
    p.hit([strip[0] - 6.0, strip[1] - 4.0, strip[2] + 12.0, strip[3] + 8.0], Act::Slider(Sl::Hue));
}

fn font_labels() -> [&'static str; 4] {
    ["Segoe UI", "Bahnschrift", "Consolas", "Georgia"]
}

fn draw_look_tab(p: &mut Pen, cfg: &Config, themes: &[Theme], theme: &Theme) {
    let l = cfg.lang;
    draw_theme_grid(p, 84.0, themes, cfg);
    draw_accent(p, 84.0 + 20.0 + 2.0 * 67.0 + 6.0, cfg, theme);
    let mut y = 84.0 + 20.0 + 2.0 * 67.0 + 6.0 + 84.0;
    for (id, key) in [
        (Sl::Size, "size"),
        (Sl::Ring, "ring"),
        (Sl::Gap, "gap"),
        (Sl::Corner, "corner"),
        (Sl::Glass, "glass"),
        (Sl::Speed, "speed"),
    ] {
        let (t, v) = (slider_t(cfg, id), slider_text(cfg, id, l));
        p.slider(id, y, tr(l, key), t, &v);
        y += 29.0;
    }
    y += 8.0;
    p.label(tr(l, "font"), RX, y);
    let sel = cfg.font.map_or(0, |f| FontKind::ALL.iter().position(|&k| k == f).map_or(0, |i| i + 1));
    let mut labels = vec![tr(l, "font-theme")];
    labels.extend(font_labels());
    p.seg(Sg::Font, y + 18.0, &labels, sel);
}

fn draw_behavior_tab(p: &mut Pen, cfg: &Config) {
    let l = cfg.lang;
    let mut y = 84.0;
    p.label(tr(l, "trigger"), RX, y);
    let tsel = TRIGGERS.iter().position(|(_, t)| *t == cfg.trigger).unwrap_or(0);
    p.seg(Sg::Trigger, y + 18.0, &["Shift", "Ctrl", "Alt", tr(l, "key-none")], tsel);
    y += 58.0;
    let (t, v) = (slider_t(cfg, Sl::Delay), slider_text(cfg, Sl::Delay, l));
    p.slider(Sl::Delay, y, tr(l, "delay"), t, &v);
    y += 40.0;
    p.label(tr(l, "output"), RX, y);
    let custom = cfg.out_dir.is_some();
    p.seg(Sg::OutMode, y + 18.0, &[tr(l, "out-source"), tr(l, "out-custom")], custom as usize);
    y += 58.0;
    if let Some(dir) = &cfg.out_dir {
        let r = [RX, y - 4.0, RW - 96.0, 28.0];
        p.fill(r, 8.0, p.pal.card);
        p.outline(r, 8.0, p.pal.border);
        let mut shown = dir.clone();
        while shown.chars().count() > 38 {
            shown = format!("...{}", shown.chars().skip(shown.chars().count() - 36).collect::<String>());
        }
        p.text(&shown, RX + 10.0, y + 10.0, 11.5, p.pal.dim, 0.0);
        p.button([RX + RW - 88.0, y - 4.0, 88.0, 28.0], tr(l, "browse"), Act::Browse, false);
        y += 38.0;
    }
    p.label(tr(l, "collision"), RX, y);
    let csel = [Collision::Rename, Collision::Overwrite, Collision::Skip]
        .iter()
        .position(|c| *c == cfg.collision)
        .unwrap_or(0);
    p.seg(Sg::Collision, y + 18.0, &[tr(l, "col-rename"), tr(l, "col-over"), tr(l, "col-skip")], csel);
    y += 58.0;
    p.label(tr(l, "notify"), RX, y);
    let nsel =
        [Notify::Off, Notify::Balloon, Notify::Sound, Notify::Both].iter().position(|c| *c == cfg.notify).unwrap_or(0);
    p.seg(Sg::Notify, y + 18.0, &[tr(l, "not-off"), tr(l, "not-balloon"), tr(l, "not-sound"), tr(l, "not-both")], nsel);
    y += 58.0;
    draw_autostart(p, y, cfg);
    y += 38.0;
    p.label(tr(l, "language"), RX, y);
    p.seg(Sg::Lang, y + 18.0, &["English", "Español"], (cfg.lang == Lang::Es) as usize);
}

fn draw_autostart(p: &mut Pen, y: f32, cfg: &Config) {
    p.text(tr(cfg.lang, "autostart"), RX, y + 12.0, 12.5, p.pal.text, 0.0);
    let r = [RX + RW - 46.0, y + 1.0, 46.0, 24.0];
    let on = cfg.autostart;
    p.fill(r, 12.0, if on { p.pal.accent } else { p.pal.card_hi });
    let kx = if on { r[0] + r[2] - 13.0 } else { r[0] + 13.0 };
    p.fill([kx - 9.0, r[1] + 3.0, 18.0, 18.0], 9.0, if on { p.pal.on_accent } else { p.pal.dim });
    p.hit(r, Act::Autostart);
}

fn draw_step(p: &mut Pen, ui_step: usize, cfg: &Config, themes: &[Theme], theme: &Theme) {
    let l = cfg.lang;
    let keys = ["step1", "step2", "step3"];
    let bodies = ["step1-body", "step2-body", ""];
    p.text(tr(l, keys[ui_step]), RX, 100.0, 24.0, p.pal.text, 0.0);
    let body = if ui_step == 2 {
        tr(l, "step3-body").replace("{}", crate::key_name(cfg.trigger, l))
    } else {
        tr(l, bodies[ui_step]).to_string()
    };
    p.text(&body, RX, 130.0, 13.0, p.pal.dim, 0.0);
    match ui_step {
        0 => {
            // Shift the shared look controls down under the heading.
            let y0 = 152.0;
            draw_theme_grid(p, y0, themes, cfg);
            draw_accent(p, y0 + 20.0 + 2.0 * 67.0 + 6.0, cfg, theme);
            // The language is needed from the very first screen.
            let ly = y0 + 20.0 + 2.0 * 67.0 + 6.0 + 88.0;
            p.label(tr(l, "language"), RX, ly);
            p.seg(Sg::Lang, ly + 18.0, &["English", "Español"], (cfg.lang == Lang::Es) as usize);
        }
        1 => {
            let tsel = TRIGGERS.iter().position(|(_, t)| *t == cfg.trigger).unwrap_or(0);
            p.label(tr(l, "trigger"), RX, 170.0);
            p.seg(Sg::Trigger, 188.0, &["Shift", "Ctrl", "Alt", tr(l, "key-none")], tsel);
            // A big key cap showing what to hold.
            let name = crate::key_name(cfg.trigger, l);
            let cap = [RX + RW / 2.0 - 70.0, 252.0, 140.0, 92.0];
            p.fill([cap[0], cap[1] + 6.0, cap[2], cap[3]], 18.0, p.pal.border);
            p.fill(cap, 18.0, p.pal.card_hi);
            p.outline(cap, 18.0, p.pal.dim.fade(0.5));
            p.text_c(
                name,
                cap[0] + cap[2] / 2.0,
                cap[1] + cap[3] / 2.0,
                if name.len() > 6 { 15.0 } else { 26.0 },
                p.pal.text,
                0.04,
            );
            if cfg.trigger == TriggerKey::None {
                p.text_c(tr(l, "no-key-hint"), RX + RW / 2.0, 372.0, 12.0, p.pal.dim, 0.0);
            }
            let (t, v) = (slider_t(cfg, Sl::Delay), slider_text(cfg, Sl::Delay, l));
            p.slider(Sl::Delay, 400.0, tr(l, "delay"), t, &v);
            p.label(tr(l, "notify"), RX, 452.0);
            let nsel = [Notify::Off, Notify::Balloon, Notify::Sound, Notify::Both]
                .iter()
                .position(|c| *c == cfg.notify)
                .unwrap_or(0);
            p.seg(
                Sg::Notify,
                470.0,
                &[tr(l, "not-off"), tr(l, "not-balloon"), tr(l, "not-sound"), tr(l, "not-both")],
                nsel,
            );
        }
        _ => {
            let key = crate::key_name(cfg.trigger, l);
            let lines = [tr(l, "how1").to_string(), tr(l, "how2").replace("{}", key), tr(l, "how3").to_string()];
            for (i, line) in lines.iter().enumerate() {
                let r = [RX, 168.0 + i as f32 * 66.0, RW, 54.0];
                p.fill(r, 14.0, p.pal.card);
                p.outline(r, 14.0, p.pal.border);
                p.fill([r[0] + 14.0, r[1] + 13.0, 28.0, 28.0], 14.0, p.pal.accent);
                p.text_c(&(i + 1).to_string(), r[0] + 28.0, r[1] + 27.5, 13.0, p.pal.on_accent, 0.0);
                p.text(line, r[0] + 58.0, r[1] + 27.5, 14.0, p.pal.text, 0.0);
            }
            draw_autostart(p, 392.0, cfg);
            p.text(tr(l, "tray-hint"), RX, 450.0, 12.0, p.pal.dim, 0.0);
        }
    }
}

/// Draws the whole window; rebuilds the hit list.
fn draw_all(ui: &mut Ui) {
    let s = ui.dpi;
    let pal = palette(ui.dark, ui.look.theme.accent);
    ui.pm.fill(Color::from_rgba8(pal.bg.0, pal.bg.1, pal.bg.2, 255));
    ui.hits.clear();
    let theme = theme::find(&ui.cfg.theme, &ui.themes);
    let step = ui.step;
    let tab = ui.tab;
    let hot = ui.hot;
    let (cfg, themes) = (ui.cfg.clone(), ui.themes.clone());
    // Preview card background is drawn by the preview itself; here only its frame and the caption.
    draw_preview_card(ui, &pal);
    let mut pen = Pen { pm: &mut ui.pm, face: &mut ui.face, pal: &pal, s, hits: &mut ui.hits, hot };
    let l = cfg.lang;

    // Header: logo, name, tabs or step dots.
    if let Some(ic) = render::icon_pixmap((30.0 * s) as u32, theme.accent) {
        pen.pm.draw_pixmap(
            (24.0 * s) as i32,
            (22.0 * s) as i32,
            ic.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
    pen.text("Radial", 64.0, 36.0, 18.0, pal.text, 0.0);
    match step {
        None => {
            let labels = [tr(l, "tab-look"), tr(l, "tab-behavior")];
            let mut x = RX;
            for (i, lab) in labels.iter().enumerate() {
                let w = pen.face.measure(lab, 13.5 * s, 0.0) / s + 28.0;
                let r = [x, 22.0, w, 30.0];
                let act = Act::Tab(i);
                if i == tab {
                    pen.fill(r, 15.0, pal.accent);
                    pen.text_c(lab, x + w / 2.0, 37.5, 13.5, pal.on_accent, 0.0);
                } else {
                    if pen.hovered(act) {
                        pen.fill(r, 15.0, pal.card_hi);
                    }
                    pen.text_c(lab, x + w / 2.0, 37.5, 13.5, pal.dim, 0.0);
                }
                pen.hit(r, act);
                x += w + 6.0;
            }
        }
        Some(n) => {
            for i in 0..3 {
                let r = [RX + i as f32 * 22.0, 33.0, if i == n { 16.0 } else { 8.0 }, 8.0];
                pen.fill(r, 4.0, if i <= n { pal.accent } else { pal.border });
            }
            let t = tr(l, "step-of").replace("{}", &(n + 1).to_string());
            pen.text_r(&t, RX + RW, 37.0, 12.0, pal.dim);
        }
    }

    match step {
        None if tab == 0 => draw_look_tab(&mut pen, &cfg, &themes, &theme),
        None => draw_behavior_tab(&mut pen, &cfg),
        Some(n) => draw_step(&mut pen, n, &cfg, &themes, &theme),
    }
    // Footer.
    let by = H - 56.0;
    match step {
        None => {
            if tab == 0 {
                pen.button([RX, by, 150.0, 36.0], tr(l, "reset"), Act::Reset, false);
            }
            pen.button([RX + RW - 130.0, by, 130.0, 36.0], tr(l, "done"), Act::Done, true);
        }
        Some(n) => {
            if n > 0 {
                pen.button([RX, by, 110.0, 36.0], tr(l, "back"), Act::Back, false);
            }
            let label = if n == 2 { tr(l, "start") } else { tr(l, "next") };
            let w = if n == 2 { 210.0 } else { 130.0 };
            pen.button([RX + RW - w, by, w, 36.0], label, Act::Next, true);
        }
    }
}

fn draw_preview_card(ui: &mut Ui, pal: &Pal) {
    let s = ui.dpi;
    let r = ui.prev.rect;
    let (x, y, w, h) = (r[0], r[1], r[2], r[3]);
    // Frame behind the wallpaper (shows as a hairline border).
    if let Some(p) = rrect(x - s, y - s, w + 2.0 * s, h + 2.0 * s, 20.0 * s) {
        ui.pm.fill_path(&p, &solid(pal.border, 1.0), FillRule::Winding, Transform::identity(), None);
    }
    draw_preview_pixels(ui);
    let l = ui.cfg.lang;
    let mut pen = Pen { pm: &mut ui.pm, face: &mut ui.face, pal, s, hits: &mut ui.hits, hot: ui.hot };
    // Menu kind switch and hint under the card.
    let sel = [8, 4, 7].iter().position(|&k| k == ui.prev.kind).unwrap_or(0);
    let y0 = BOX.1 + BOX.3 + 14.0;
    let r2 = [BOX.0, y0, BOX.2, 30.0];
    pen.fill(r2, 9.0, pal.card);
    pen.outline(r2, 9.0, pal.border);
    let labels = [tr(l, "menu-img"), tr(l, "menu-doc"), tr(l, "menu-act")];
    let w = (BOX.2 - 6.0) / 3.0;
    for (i, lab) in labels.iter().enumerate() {
        let cell = [BOX.0 + 3.0 + w * i as f32, y0 + 3.0, w, 24.0];
        let act = Act::Seg(Sg::Menu, i);
        if i == sel {
            pen.fill(cell, 7.0, pal.accent);
        } else if pen.hovered(act) {
            pen.fill(cell, 7.0, pal.card_hi);
        }
        pen.text_c(lab, cell[0] + w / 2.0, cell[1] + 12.5, 12.0, if i == sel { pal.on_accent } else { pal.dim }, 0.0);
        pen.hit(cell, act);
    }
    pen.text_c(tr(l, "hint"), BOX.0 + BOX.2 / 2.0, y0 + 50.0, 11.5, pal.dim, 0.0);
}

// ---------------------------------------------------------------------------------------------------------------
// Preview
// ---------------------------------------------------------------------------------------------------------------

/// Rebuilds the wallpaper, backdrop and baked layers after the look changed.
fn rebuild_preview(ui: &mut Ui, replay: bool) {
    let s = ui.dpi;
    let look = Look::new(&ui.cfg, &ui.themes);
    let (bx, by, bw, bh) = (BOX.0 * s, BOX.1 * s, BOX.2 * s, BOX.3 * s);
    let fit = (bw / (look.m.size * s)).min(1.0);
    let dpi_eff = s * fit;
    let side = look.side_px(dpi_eff);
    let p = &mut ui.prev;
    p.rect = [bx.round(), by.round(), bw.round(), bh.round()];
    p.dpi_eff = dpi_eff;
    p.center = (p.rect[0] + p.rect[2] / 2.0, p.rect[1] + p.rect[3] / 2.0);
    let (wd, ht) = (p.rect[2] as u32, p.rect[3] as u32);
    if let Some(mut wall) = Pixmap::new(wd, ht) {
        render::wallpaper(&mut wall, &look.theme);
        // Backdrop: the wallpaper area under the menu canvas.
        let (ox, oy) = ((wd.saturating_sub(side)) / 2, (ht.saturating_sub(side)) / 2);
        let mut crop = vec![0u8; (side * side * 4) as usize];
        for row in 0..side.min(ht) {
            let src = (((oy + row) * wd + ox) * 4) as usize;
            let n = (side.min(wd) * 4) as usize;
            crop[(row * side * 4) as usize..(row * side * 4) as usize + n].copy_from_slice(&wall.data()[src..src + n]);
        }
        let blur = look.theme.blur * look.glass * look.scale * dpi_eff;
        let bd = Backdrop::new(&crop, side, side, false, blur, look.theme.saturation);
        p.menu = Pixmap::new(side, side);
        p.renderer.prepare(&look, dpi_eff, side, bd.as_ref(), p.scene.slots.len());
        p.wall = Some(wall);
    }
    ui.look = look;
    if replay {
        let n = ui.prev.scene.slots.len();
        ui.prev.scene.open = 0.0;
        ui.prev.appear = vec![Spring::default(); n];
        ui.prev.age = 0.0;
        ui.prev.last = Instant::now();
    }
}

fn set_kind(ui: &mut Ui, kind: usize) {
    let sc = demo_scene(kind, None, ui.cfg.lang);
    let n = sc.slots.len();
    ui.prev.kind = kind;
    ui.prev.scene = Scene { open: 1.0, ..sc };
    ui.prev.appear = vec![Spring::default(); n];
    ui.prev.hover_sp = vec![Spring::default(); n];
    ui.prev.scene.appear = vec![0.0; n];
    ui.prev.hover = None;
    ui.prev.age = 0.0;
    ui.prev.last = Instant::now();
    rebuild_preview(ui, true);
}

/// Advances the preview springs; true when everything is at rest.
fn step_preview(ui: &mut Ui, dt: f32) -> bool {
    let th = ui.look.theme.clone();
    let speed = ui.cfg.speed;
    let p = &mut ui.prev;
    let dt = dt * speed;
    p.age += dt;
    let n = p.scene.slots.len();
    let mut settled = true;
    p.scene.open = (p.scene.open + dt / 0.2).min(1.0);
    settled &= p.scene.open >= 1.0;
    for i in 0..n {
        let go = p.age >= th.stagger * i as f32;
        let target = go as u8 as f32;
        p.appear[i].step(target, th.stiffness, th.damping, dt);
        settled &= p.appear[i].settled(target) && go;
        p.scene.appear[i] = p.appear[i].x;
        let ht = if Some(i) == p.hover && p.scene.slots[i].enabled { 1.0 } else { 0.0 };
        p.hover_sp[i].step(ht, th.stiffness * 1.4, (th.damping + 0.25).min(1.0), dt);
        settled &= p.hover_sp[i].settled(ht);
        p.scene.hover[i] = p.hover_sp[i].x.clamp(0.0, 1.15);
    }
    settled
}

fn draw_preview_pixels(ui: &mut Ui) {
    let s = ui.dpi;
    let p = &mut ui.prev;
    let (Some(wall), Some(menu)) = (p.wall.as_ref(), p.menu.as_mut()) else { return };
    p.renderer.render(menu, &p.scene, &ui.look, p.dpi_eff);
    let r = p.rect;
    if let Some(path) = rrect(r[0], r[1], r[2], r[3], 19.0 * s) {
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.shader = Pattern::new(
            wall.as_ref(),
            SpreadMode::Pad,
            FilterQuality::Nearest,
            1.0,
            Transform::from_translate(r[0], r[1]),
        );
        ui.pm.fill_path(&path, &paint, FillRule::Winding, Transform::identity(), None);
        // The menu canvas is transparent outside the ring; clip it to the card by drawing it centred.
        let mx = p.center.0 - menu.width() as f32 / 2.0;
        let my = p.center.1 - menu.height() as f32 / 2.0;
        ui.pm.draw_pixmap(
            mx.round() as i32,
            my.round() as i32,
            menu.as_ref(),
            &PixmapPaint::default(),
            Transform::identity(),
            None,
        );
    }
}

fn set_hover_from_mouse(ui: &mut Ui, x: f32, y: f32) -> bool {
    let p = &ui.prev;
    let m = ui.look.m;
    let (dx, dy) = ((x - p.center.0) / p.dpi_eff, (y - p.center.1) / p.dpi_eff);
    let hit = sector_at(dx, dy, p.scene.slots.len(), m.hub, m.hit_out);
    if hit == ui.prev.hover {
        return false;
    }
    ui.prev.hover = hit;
    let sc = demo_scene(ui.prev.kind, hit, ui.cfg.lang);
    ui.prev.scene.main = sc.main;
    ui.prev.scene.sub = sc.sub;
    ui.prev.scene.caption = sc.caption;
    ui.prev.scene.main_accent = sc.main_accent;
    true
}

// ---------------------------------------------------------------------------------------------------------------
// Window plumbing
// ---------------------------------------------------------------------------------------------------------------

#[repr(C)]
struct BrowseInfo {
    owner: isize,
    root: *const c_void,
    display: *mut u16,
    title: *const u16,
    flags: u32,
    callback: *const c_void,
    lparam: isize,
    image: i32,
}

#[link(name = "shell32")]
extern "system" {
    fn SHBrowseForFolderW(bi: *const BrowseInfo) -> *mut c_void;
    fn SHGetPathFromIDListW(pidl: *const c_void, path: *mut u16) -> i32;
}

#[link(name = "user32")]
extern "system" {
    fn AdjustWindowRectExForDpi(rect: *mut RECT, style: u32, menu: i32, ex: u32, dpi: u32) -> i32;
}

#[link(name = "dwmapi")]
extern "system" {
    fn DwmSetWindowAttribute(hwnd: isize, attr: u32, val: *const c_void, size: u32) -> i32;
}

#[link(name = "advapi32")]
extern "system" {
    fn RegGetValueW(
        key: isize,
        sub: *const u16,
        name: *const u16,
        flags: u32,
        ty: *mut u32,
        data: *mut c_void,
        cb: *mut u32,
    ) -> i32;
}

pub(crate) fn system_is_dark() -> bool {
    let (sub, name) =
        (wide("Software\\Microsoft\\Windows\\CurrentVersion\\Themes\\Personalize"), wide("AppsUseLightTheme"));
    let mut v: u32 = 1;
    let mut cb = 4u32;
    let r = unsafe {
        RegGetValueW(
            0x8000_0001u32 as i32 as isize,
            sub.as_ptr(),
            name.as_ptr(),
            0x10,
            null_mut(),
            &mut v as *mut u32 as *mut c_void,
            &mut cb,
        )
    };
    r == 0 && v == 0
}

fn present(ui: &mut Ui, rows: Option<(i32, i32)>) {
    let (y0, y1) = rows.unwrap_or((0, ui.h));
    let (y0, y1) = (y0.clamp(0, ui.h), y1.clamp(0, ui.h));
    let stride = ui.w as usize * 4;
    let src = ui.pm.data();
    let dst = unsafe { std::slice::from_raw_parts_mut(ui.bits, ui.h as usize * stride) };
    for y in y0 as usize..y1 as usize {
        for (d, s) in
            dst[y * stride..(y + 1) * stride].chunks_exact_mut(4).zip(src[y * stride..(y + 1) * stride].chunks_exact(4))
        {
            d[0] = s[2];
            d[1] = s[1];
            d[2] = s[0];
            d[3] = 255;
        }
    }
    unsafe {
        let r = RECT { left: 0, top: y0, right: ui.w, bottom: y1 };
        InvalidateRect(ui.hwnd, &r, 0);
    }
}

fn commit(ui: &Ui) {
    crate::set_config(ui.cfg.clone());
}

fn start_timer(ui: &mut Ui) {
    if !ui.prev.running {
        ui.prev.running = true;
        ui.prev.last = Instant::now();
        unsafe { SetTimer(ui.hwnd, TIMER, 8, None) };
    }
}

fn redraw(ui: &mut Ui) {
    draw_all(ui);
    present(ui, None);
}

/// Applies a change that affects the look: rebuild the preview, replay the entrance, redraw.
fn look_changed(ui: &mut Ui, replay: bool) {
    rebuild_preview(ui, replay);
    redraw(ui);
    if replay {
        start_timer(ui);
    }
}

/// After a slider moved: the delay only needs a redraw, everything else changes the preview.
fn slider_changed(ui: &mut Ui, id: Sl) {
    if id == Sl::Delay {
        redraw(ui);
    } else {
        look_changed(ui, false);
    }
}

fn act_at(ui: &Ui, x: f32, y: f32) -> Option<Act> {
    let (x, y) = (x / ui.dpi, y / ui.dpi);
    ui.hits
        .iter()
        .rev()
        .find(|h| x >= h.r[0] && x < h.r[0] + h.r[2] && y >= h.r[1] && y < h.r[1] + h.r[3])
        .map(|h| h.act)
}

fn slider_from_x(ui: &mut Ui, id: Sl, x: f32) {
    // Sliders live at fixed x positions: the plain ones in the middle of the row, the hue strip full width.
    let (left, width) = if id == Sl::Hue { (RX, RW) } else { (RX + 168.0, 180.0) };
    let t = (x / ui.dpi - left) / width;
    set_slider(&mut ui.cfg, id, t);
}

unsafe fn pick_folder(owner: HWND, title: &str) -> Option<String> {
    let t = wide(title);
    let bi = BrowseInfo {
        owner: owner as isize,
        root: null(),
        display: null_mut(),
        title: t.as_ptr(),
        flags: 0x1 | 0x40, // BIF_RETURNONLYFSDIRS | BIF_NEWDIALOGSTYLE
        callback: null(),
        lparam: 0,
        image: 0,
    };
    let pidl = SHBrowseForFolderW(&bi);
    if pidl.is_null() {
        return None;
    }
    let mut buf = [0u16; 260];
    let ok = SHGetPathFromIDListW(pidl, buf.as_mut_ptr()) != 0;
    CoTaskMemFree(pidl as *const c_void);
    ok.then(|| String::from_utf16_lossy(&buf[..buf.iter().position(|&c| c == 0).unwrap_or(0)]))
}

fn do_act(ui: &mut Ui, act: Act) {
    let themes = ui.themes.clone();
    match act {
        Act::Theme(i) => {
            if let Some(t) = themes.get(i) {
                ui.cfg.apply_theme(t);
                commit(ui);
                look_changed(ui, true);
            }
        }
        Act::Accent(a) => {
            ui.cfg.accent = a;
            commit(ui);
            look_changed(ui, false);
        }
        Act::Slider(id) => {
            // Handled on mouse down / move; nothing to do on a plain click.
            let _ = id;
        }
        Act::Seg(id, i) => {
            match id {
                Sg::Font => ui.cfg.font = if i == 0 { None } else { FontKind::ALL.get(i - 1).copied() },
                Sg::Trigger => ui.cfg.trigger = TRIGGERS[i.min(3)].1,
                Sg::OutMode => {
                    ui.cfg.out_dir = if i == 0 { None } else { Some(default_out_dir()) };
                }
                Sg::Collision => {
                    ui.cfg.collision = [Collision::Rename, Collision::Overwrite, Collision::Skip][i.min(2)]
                }
                Sg::Notify => ui.cfg.notify = [Notify::Off, Notify::Balloon, Notify::Sound, Notify::Both][i.min(3)],
                Sg::Lang => ui.cfg.lang = if i == 0 { Lang::En } else { Lang::Es },
                Sg::Menu => {
                    set_kind(ui, [8, 4, 7][i.min(2)]);
                    redraw(ui);
                    start_timer(ui);
                    return;
                }
            }
            commit(ui);
            if id == Sg::Font {
                // The face has to be reloaded for the preview's text.
                reload_face(ui);
                look_changed(ui, false);
            } else {
                redraw(ui);
            }
        }
        Act::Autostart => {
            ui.cfg.autostart = !ui.cfg.autostart;
            commit(ui);
            redraw(ui);
        }
        Act::Tab(i) => {
            ui.tab = i;
            redraw(ui);
        }
        Act::Reset => {
            // The look goes back to the default; behaviour settings are left alone.
            let o = &ui.cfg;
            let mut c = Config {
                trigger: o.trigger,
                lang: o.lang,
                out_dir: o.out_dir.clone(),
                collision: o.collision,
                notify: o.notify,
                autostart: o.autostart,
                delay_ms: o.delay_ms,
                onboarded: true,
                ..Config::default()
            };
            if let Some(t) = themes.first() {
                c.apply_theme(t);
            }
            ui.cfg = c;
            commit(ui);
            reload_face(ui);
            look_changed(ui, true);
        }
        Act::Done => close(ui),
        Act::Next => match ui.step {
            Some(n) if n >= 2 => close(ui),
            Some(n) => {
                ui.step = Some(n + 1);
                redraw(ui);
            }
            None => {}
        },
        Act::Back => {
            if let Some(n) = ui.step {
                ui.step = Some(n.saturating_sub(1));
                redraw(ui);
            }
        }
        Act::Browse => {}
    }
}

fn default_out_dir() -> String {
    std::env::var("USERPROFILE").map(|p| format!("{p}\\Pictures")).unwrap_or_default()
}

fn reload_face(ui: &mut Ui) {
    // The UI face is independent of the theme font; only the overlay preview face changes.
    let t = ui.cfg.resolve(&theme::find(&ui.cfg.theme, &ui.themes));
    ui.prev.renderer.face = Face::load(t.font, t.weight);
}

fn close(ui: &mut Ui) {
    ui.cfg.onboarded = true;
    commit(ui);
    // Destroying from inside a handler that still holds the UI borrow would skip the cleanup, so go through WM_CLOSE.
    unsafe { PostMessageW(ui.hwnd, WM_CLOSE, 0, 0) };
}

unsafe fn make_buffers(hwnd: HWND, w: i32, h: i32) -> Option<(Pixmap, HDC, HBITMAP, HGDIOBJ, *mut u8)> {
    let screen = GetDC(hwnd);
    let dc = CreateCompatibleDC(screen);
    let mut bi: BITMAPINFO = zeroed();
    bi.bmiHeader.biSize = size_of::<BITMAPINFOHEADER>() as u32;
    bi.bmiHeader.biWidth = w;
    bi.bmiHeader.biHeight = -h;
    bi.bmiHeader.biPlanes = 1;
    bi.bmiHeader.biBitCount = 32;
    let mut bits: *mut c_void = null_mut();
    let dib = CreateDIBSection(screen, &bi, DIB_RGB_COLORS, &mut bits, null_mut(), 0);
    ReleaseDC(hwnd, screen);
    if dib.is_null() || bits.is_null() {
        DeleteDC(dc);
        return None;
    }
    let old = SelectObject(dc, dib);
    Some((Pixmap::new(w as u32, h as u32)?, dc, dib, old, bits as *mut u8))
}

unsafe fn free_buffers(ui: &Ui) {
    SelectObject(ui.dc, ui.old);
    DeleteObject(ui.dib);
    DeleteDC(ui.dc);
}

unsafe fn cursor_dpi() -> u32 {
    let mut pt: POINT = zeroed();
    GetCursorPos(&mut pt);
    let mon = MonitorFromPoint(pt, MONITOR_DEFAULTTONEAREST);
    let (mut dx, mut dy) = (96u32, 96u32);
    GetDpiForMonitor(mon, 0, &mut dx, &mut dy);
    dx
}

/// Opens the settings window (or focuses it when already open). `onboarding` starts the three-step first run.
pub unsafe fn open(onboarding: bool) {
    let existing = UI.with(|c| c.try_borrow().ok().and_then(|g| g.as_ref().map(|u| u.hwnd)));
    if let Some(h) = existing {
        ShowWindow(h, SW_RESTORE);
        SetForegroundWindow(h);
        return;
    }
    let hinst = GetModuleHandleW(null());
    let cls = wide("RadialSettings");
    let mut wc: WNDCLASSW = zeroed();
    wc.lpfnWndProc = Some(proc);
    wc.hInstance = hinst;
    wc.lpszClassName = cls.as_ptr();
    wc.hCursor = LoadCursorW(null_mut(), IDC_ARROW);
    RegisterClassW(&wc);

    let dpi = cursor_dpi();
    let s = dpi as f32 / 96.0;
    let (cw, ch) = ((W * s).round() as i32, (H * s).round() as i32);
    let style = WS_OVERLAPPED | WS_CAPTION | WS_SYSMENU | WS_MINIMIZEBOX;
    let mut r = RECT { left: 0, top: 0, right: cw, bottom: ch };
    AdjustWindowRectExForDpi(&mut r, style, 0, 0, dpi);
    let (ww, wh) = (r.right - r.left, r.bottom - r.top);
    let (sw, sh) = (GetSystemMetrics(SM_CXSCREEN), GetSystemMetrics(SM_CYSCREEN));
    let title = wide(tr(crate::current_config().lang, "title"));
    let hwnd = CreateWindowExW(
        0,
        cls.as_ptr(),
        title.as_ptr(),
        style,
        (sw - ww) / 2,
        (sh - wh) / 3,
        ww,
        wh,
        null_mut(),
        null_mut(),
        hinst,
        null(),
    );
    if hwnd.is_null() {
        return;
    }
    let dark = system_is_dark();
    let d: i32 = dark as i32;
    DwmSetWindowAttribute(hwnd as isize, 20, &d as *const i32 as *const c_void, 4);
    let ui = build_ui(hwnd, cw, ch, s, dark, crate::current_config(), crate::user_themes(), onboarding.then_some(0));
    let Some(mut ui) = ui else {
        DestroyWindow(hwnd);
        return;
    };
    rebuild_preview(&mut ui, true);
    draw_all(&mut ui);
    present(&mut ui, None);
    start_timer(&mut ui);
    UI.with(|c| *c.borrow_mut() = Some(ui));
    ShowWindow(hwnd, SW_SHOW);
    SetForegroundWindow(hwnd);
}

#[allow(clippy::too_many_arguments)]
unsafe fn build_ui(
    hwnd: HWND,
    w: i32,
    h: i32,
    dpi: f32,
    dark: bool,
    cfg: Config,
    user: Vec<Theme>,
    step: Option<usize>,
) -> Option<Ui> {
    let (pm, dc, dib, old, bits) = make_buffers(hwnd, w, h)?;
    let themes: Vec<Theme> = theme::builtin().into_iter().chain(user).collect();
    let look = Look::new(&cfg, &themes);
    let face = Face::load(FontKind::Variable, 520.0)?;
    let t = cfg.resolve(&theme::find(&cfg.theme, &themes));
    let sc = demo_scene(8, None, cfg.lang);
    let n = sc.slots.len();
    let prev = Preview {
        wall: None,
        menu: None,
        renderer: Renderer::new(Face::load(t.font, t.weight)),
        scene: Scene { open: 0.0, ..sc },
        appear: vec![Spring::default(); n],
        hover_sp: vec![Spring::default(); n],
        hover: None,
        kind: 8,
        age: 0.0,
        last: Instant::now(),
        rect: [0.0; 4],
        dpi_eff: dpi,
        center: (0.0, 0.0),
        running: false,
    };
    Some(Ui {
        hwnd,
        dpi,
        w,
        h,
        pm,
        dc,
        dib,
        old,
        bits,
        face,
        cfg,
        themes,
        look,
        dark,
        tab: 0,
        step,
        hits: vec![],
        hot: None,
        drag: None,
        prev,
        tracking: false,
    })
}

unsafe extern "system" fn proc(hwnd: HWND, msg: u32, wp: WPARAM, lp: LPARAM) -> LRESULT {
    let (mx, my) = ((lp & 0xFFFF) as i16 as f32, ((lp >> 16) & 0xFFFF) as i16 as f32);
    match msg {
        WM_PAINT => {
            let mut ps: PAINTSTRUCT = zeroed();
            let hdc = BeginPaint(hwnd, &mut ps);
            with_ui(|ui| {
                BitBlt(hdc, 0, 0, ui.w, ui.h, ui.dc, 0, 0, SRCCOPY);
            });
            EndPaint(hwnd, &ps);
            0
        }
        WM_ERASEBKGND => 1,
        WM_LBUTTONDOWN => {
            let act = with_ui(|ui| {
                let a = act_at(ui, mx, my);
                if let Some(Act::Slider(id)) = a {
                    ui.drag = Some(id);
                    slider_from_x(ui, id, mx);
                    slider_changed(ui, id);
                    SetCapture(ui.hwnd);
                }
                a
            })
            .flatten();
            match act {
                Some(Act::Slider(_)) | None => {}
                Some(Act::Browse) => {
                    let lang = crate::current_config().lang;
                    let picked = pick_folder(hwnd, tr(lang, "folder-pick"));
                    if let Some(dir) = picked {
                        with_ui(|ui| {
                            ui.cfg.out_dir = Some(dir);
                            commit(ui);
                            redraw(ui);
                        });
                    }
                }
                Some(a) => {
                    with_ui(|ui| do_act(ui, a));
                }
            }
            0
        }
        WM_MOUSEMOVE => {
            with_ui(|ui| {
                if !ui.tracking {
                    let mut t = TRACKMOUSEEVENT {
                        cbSize: size_of::<TRACKMOUSEEVENT>() as u32,
                        dwFlags: TME_LEAVE,
                        hwndTrack: ui.hwnd,
                        dwHoverTime: 0,
                    };
                    TrackMouseEvent(&mut t);
                    ui.tracking = true;
                }
                if let Some(id) = ui.drag {
                    slider_from_x(ui, id, mx);
                    slider_changed(ui, id);
                    return;
                }
                let act = act_at(ui, mx, my);
                let hover_changed = set_hover_from_mouse(ui, mx, my);
                if act != ui.hot {
                    ui.hot = act;
                    redraw(ui);
                } else if hover_changed {
                    redraw_preview_only(ui);
                }
                if hover_changed {
                    start_timer(ui);
                }
            });
            0
        }
        WM_MOUSELEAVE => {
            with_ui(|ui| {
                ui.tracking = false;
                ui.hot = None;
                if ui.prev.hover.take().is_some() {
                    let sc = demo_scene(ui.prev.kind, None, ui.cfg.lang);
                    ui.prev.scene.main = sc.main;
                    ui.prev.scene.sub = sc.sub;
                    ui.prev.scene.caption = sc.caption;
                    ui.prev.scene.main_accent = false;
                    start_timer(ui);
                }
                redraw(ui);
            });
            0
        }
        WM_LBUTTONUP => {
            with_ui(|ui| {
                if ui.drag.take().is_some() {
                    ReleaseCapture();
                    commit(ui);
                }
            });
            0
        }
        WM_TIMER if wp == TIMER => {
            with_ui(|ui| {
                let now = Instant::now();
                let dt = (now - ui.prev.last).as_secs_f32().min(0.05);
                ui.prev.last = now;
                let settled = step_preview(ui, dt);
                redraw_preview_only(ui);
                if settled {
                    ui.prev.running = false;
                    KillTimer(ui.hwnd, TIMER);
                }
            });
            0
        }
        WM_KEYDOWN => {
            if wp as u32 == VK_ESCAPE as u32 {
                with_ui(close);
            } else if wp as u32 == VK_RETURN as u32 {
                with_ui(|ui| {
                    let a = if ui.step.is_some() { Act::Next } else { Act::Done };
                    do_act(ui, a)
                });
            }
            0
        }
        WM_DPICHANGED => {
            let new_dpi = (wp & 0xFFFF) as u32;
            let r = &*(lp as *const RECT);
            SetWindowPos(
                hwnd,
                null_mut(),
                r.left,
                r.top,
                r.right - r.left,
                r.bottom - r.top,
                SWP_NOZORDER | SWP_NOACTIVATE,
            );
            with_ui(|ui| {
                let s = new_dpi as f32 / 96.0;
                let (w, h) = ((W * s).round() as i32, (H * s).round() as i32);
                free_buffers(ui);
                if let Some((pm, dc, dib, old, bits)) = make_buffers(hwnd, w, h) {
                    (ui.pm, ui.dc, ui.dib, ui.old, ui.bits, ui.w, ui.h, ui.dpi) = (pm, dc, dib, old, bits, w, h, s);
                    rebuild_preview(ui, false);
                    redraw(ui);
                }
            });
            0
        }
        WM_CLOSE => {
            with_ui(|ui| {
                ui.cfg.onboarded = true;
                commit(ui);
            });
            DestroyWindow(hwnd);
            0
        }
        WM_DESTROY => {
            // Drop everything: pixmaps, DIB, fonts, baked layers.
            let ui = UI.with(|c| c.try_borrow_mut().ok().and_then(|mut g| g.take()));
            if let Some(ui) = ui {
                KillTimer(hwnd, TIMER);
                free_buffers(&ui);
            }
            0
        }
        _ => DefWindowProcW(hwnd, msg, wp, lp),
    }
}

fn redraw_preview_only(ui: &mut Ui) {
    draw_preview_pixels(ui);
    let r = ui.prev.rect;
    present(ui, Some((r[1] as i32, (r[1] + r[3]) as i32)));
}

// ---------------------------------------------------------------------------------------------------------------
// Offscreen rendering for review: `radial --settings out.png [--tab 1] [--step n] [--theme x] [--lang es] [--light]`
// ---------------------------------------------------------------------------------------------------------------

pub fn cli(args: &[String]) -> bool {
    let get = |flag: &str| args.iter().position(|a| a == flag).and_then(|i| args.get(i + 1)).cloned();
    let Some(out) = get("--settings") else { return false };
    let mut cfg = Config::default();
    let user = crate::config::user_themes();
    let themes: Vec<Theme> = theme::builtin().into_iter().chain(user).collect();
    if let Some(t) = get("--theme") {
        cfg.apply_theme(&theme::find(&t, &themes));
    }
    if get("--lang").as_deref() == Some("es") {
        cfg.lang = Lang::Es;
    }
    if args.iter().any(|a| a == "--custom-out") {
        cfg.out_dir = Some("D:\\Converted files".into());
    }
    let s: f32 = get("--dpi").and_then(|v| v.parse().ok()).unwrap_or(1.0);
    let (w, h) = ((W * s) as u32, (H * s) as u32);
    let Some(pm) = Pixmap::new(w, h) else { return true };
    let look = Look::new(&cfg, &themes);
    let t = cfg.resolve(&theme::find(&cfg.theme, &themes));
    let kind = get("--menu").and_then(|v| v.parse().ok()).unwrap_or(8);
    let sc = demo_scene(kind, None, cfg.lang);
    let n = sc.slots.len();
    let Some(face) = Face::load(FontKind::Variable, 520.0) else { return true };
    let prev = Preview {
        wall: None,
        menu: None,
        renderer: Renderer::new(Face::load(t.font, t.weight)),
        scene: Scene { open: 1.0, appear: vec![1.0; n], ..sc },
        appear: vec![Spring { x: 1.0, v: 0.0 }; n],
        hover_sp: vec![Spring::default(); n],
        hover: None,
        kind,
        age: 1.0,
        last: Instant::now(),
        rect: [0.0; 4],
        dpi_eff: s,
        center: (0.0, 0.0),
        running: false,
    };
    // No window: the DIB fields are never touched by drawing or rebuilding.
    let mut ui = Ui {
        hwnd: null_mut(),
        dpi: s,
        w: w as i32,
        h: h as i32,
        pm,
        dc: null_mut(),
        dib: null_mut(),
        old: null_mut(),
        bits: null_mut(),
        face,
        cfg,
        themes,
        look,
        dark: !args.iter().any(|a| a == "--light"),
        tab: get("--tab").and_then(|v| v.parse().ok()).unwrap_or(0),
        step: get("--step").and_then(|v| v.parse().ok()),
        hits: vec![],
        hot: None,
        drag: None,
        prev,
        tracking: false,
    };
    rebuild_preview(&mut ui, false);
    let hover = get("--hover").and_then(|v| v.parse::<usize>().ok());
    if let Some(i) = hover {
        ui.prev.hover = Some(i);
        ui.prev.hover_sp[i] = Spring { x: 1.0, v: 0.0 };
        ui.prev.scene.hover[i] = 1.0;
        let sc = demo_scene(kind, Some(i), ui.cfg.lang);
        ui.prev.scene.main = sc.main;
        ui.prev.scene.sub = sc.sub;
        ui.prev.scene.caption = sc.caption;
        ui.prev.scene.main_accent = sc.main_accent;
    }
    draw_all(&mut ui);
    let img = image::RgbaImage::from_raw(w, h, ui.pm.data().to_vec()).expect("pixmap size");
    img.save(out).expect("save settings png");
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sliders_round_trip_through_their_ranges() {
        let mut c = Config::default();
        for id in [Sl::Size, Sl::Ring, Sl::Gap, Sl::Corner, Sl::Glass, Sl::Speed, Sl::Delay] {
            for t in [0.0, 0.25, 0.5, 1.0] {
                set_slider(&mut c, id, t);
                let back = slider_t(&c, id);
                assert!((back - t).abs() < 0.06, "{id:?} {t} -> {back}");
            }
        }
    }

    #[test]
    fn slider_values_stay_inside_the_config_ranges() {
        let mut c = Config::default();
        set_slider(&mut c, Sl::Size, 7.0);
        set_slider(&mut c, Sl::Delay, -3.0);
        assert_eq!((c.size, c.delay_ms), (SIZE_RANGE.1, 0));
        set_slider(&mut c, Sl::Delay, 1.0);
        assert_eq!(c.delay_ms, DELAY_MAX);
    }

    #[test]
    fn hue_slider_sets_a_custom_accent_and_reads_it_back() {
        let mut c = Config::default();
        set_slider(&mut c, Sl::Hue, 0.5);
        let a = c.accent.expect("accent set");
        assert!((hue_of(a) - 180.0).abs() < 2.0);
        assert!((hue_of(Rgba(255, 0, 0, 255))).abs() < 0.1);
        assert!((hue_of(Rgba(0, 0, 255, 255)) - 240.0).abs() < 0.1);
    }

    #[test]
    fn palette_avoids_accents_that_vanish() {
        let white = Rgba(255, 255, 255, 255);
        let black = Rgba(0, 0, 0, 255);
        assert_ne!(palette(false, white).accent, white);
        assert_ne!(palette(true, black).accent, black);
        assert_eq!(palette(true, white).accent, white);
    }

    #[test]
    fn rounded_rect_handles_degenerate_sizes() {
        assert!(rrect(0.0, 0.0, 10.0, 10.0, 99.0).is_some());
        assert!(rrect(0.0, 0.0, 10.0, 10.0, 0.0).is_some());
    }
}
