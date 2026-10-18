//! Themes are plain data: a `key = value` text file. Built-in presets live in `assets/themes/*.theme`
//! and are embedded at build time; users can drop more files into `%APPDATA%\radial\themes`.
//! Every key is optional: a file only lists what differs from [`Theme::base`].

use std::fmt::Write as _;

/// 8-bit straight-alpha colour. Text form: `#rgb`, `#rrggbb` or `#rrggbbaa`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Rgba(pub u8, pub u8, pub u8, pub u8);

impl Rgba {
    pub const fn rgb(r: u8, g: u8, b: u8) -> Rgba {
        Rgba(r, g, b, 255)
    }

    pub fn parse(s: &str) -> Result<Rgba, String> {
        let h = s.trim().strip_prefix('#').ok_or_else(|| format!("colour `{s}` must start with #"))?;
        if !h.is_ascii() {
            return Err(format!("colour `{s}` is not hex"));
        }
        let nib = |i: usize| u8::from_str_radix(&h[i..i + 1], 16).map_err(|_| format!("colour `{s}` is not hex"));
        let byte = |i: usize| u8::from_str_radix(&h[i..i + 2], 16).map_err(|_| format!("colour `{s}` is not hex"));
        match h.len() {
            3 => Ok(Rgba(nib(0)? * 17, nib(1)? * 17, nib(2)? * 17, 255)),
            6 => Ok(Rgba(byte(0)?, byte(2)?, byte(4)?, 255)),
            8 => Ok(Rgba(byte(0)?, byte(2)?, byte(4)?, byte(6)?)),
            _ => Err(format!("colour `{s}` must have 3, 6 or 8 hex digits")),
        }
    }

    pub fn to_hex(self) -> String {
        if self.3 == 255 {
            format!("#{:02x}{:02x}{:02x}", self.0, self.1, self.2)
        } else {
            format!("#{:02x}{:02x}{:02x}{:02x}", self.0, self.1, self.2, self.3)
        }
    }

    /// Same colour with the alpha multiplied by `k` (0..1).
    pub fn fade(self, k: f32) -> Rgba {
        Rgba(self.0, self.1, self.2, (self.3 as f32 * k.clamp(0.0, 1.0)).round() as u8)
    }

    pub fn mix(self, o: Rgba, t: f32) -> Rgba {
        let t = t.clamp(0.0, 1.0);
        let l = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * t).round() as u8;
        Rgba(l(self.0, o.0), l(self.1, o.1), l(self.2, o.2), l(self.3, o.3))
    }

    /// Hue (0..360), saturation and value (0..1) to an opaque colour.
    pub fn from_hsv(h: f32, s: f32, v: f32) -> Rgba {
        let h = h.rem_euclid(360.0) / 60.0;
        let (c, x) = (v * s, v * s * (1.0 - (h % 2.0 - 1.0).abs()));
        let (r, g, b) = match h as u32 {
            0 => (c, x, 0.0),
            1 => (x, c, 0.0),
            2 => (0.0, c, x),
            3 => (0.0, x, c),
            4 => (x, 0.0, c),
            _ => (c, 0.0, x),
        };
        let m = v - c;
        let q = |f: f32| ((f + m) * 255.0).round().clamp(0.0, 255.0) as u8;
        Rgba(q(r), q(g), q(b), 255)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FontKind {
    /// Segoe UI Variable (weight and optical size axes), falling back to Segoe UI.
    Variable,
    Bahnschrift,
    Consolas,
    Georgia,
}

impl FontKind {
    pub const ALL: [FontKind; 4] = [FontKind::Variable, FontKind::Bahnschrift, FontKind::Consolas, FontKind::Georgia];

    pub fn key(self) -> &'static str {
        match self {
            FontKind::Variable => "variable",
            FontKind::Bahnschrift => "bahnschrift",
            FontKind::Consolas => "consolas",
            FontKind::Georgia => "georgia",
        }
    }

    pub fn from_key(s: &str) -> Option<FontKind> {
        FontKind::ALL.into_iter().find(|f| f.key() == s.trim().to_ascii_lowercase())
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct Theme {
    /// Unique id, also the file stem of the theme file.
    pub name: String,
    /// Human readable name shown in the configurator.
    pub label: String,
    /// True for light backgrounds (the configurator uses it for its own chrome in the swatch).
    pub light: bool,

    // Colours.
    pub accent: Rgba,
    pub accent2: Rgba,
    pub text: Rgba,
    pub text_dim: Rgba,
    /// Glass tint at the outer edge of a sector; alpha is how much of the frosted backdrop it hides.
    pub glass_top: Rgba,
    pub glass_bottom: Rgba,
    pub hub: Rgba,
    pub rim: Rgba,
    pub highlight: Rgba,
    pub shadow: Rgba,
    /// Colour of the sector under the cursor, before the accent glow.
    pub hover_fill: Rgba,
    /// Text colour on a hovered sector.
    pub hover_text: Rgba,

    // Material.
    pub sheen: f32,
    pub glow: f32,
    pub shadow_spread: f32,
    pub shadow_dy: f32,
    /// Backdrop blur radius in logical pixels.
    pub blur: f32,
    pub saturation: f32,
    pub orbit: f32,
    pub ticks: bool,
    /// CRT scanlines over the glass, 0..1.
    pub scan: f32,
    /// Distance in px of the inner highlight line from the sector edge.
    pub inset: f32,

    // Geometry (logical pixels at size 1.0).
    pub ring: f32,
    pub gap: f32,
    pub corner: f32,

    // Type.
    pub font: FontKind,
    pub weight: f32,
    pub tracking: f32,
    pub caps: bool,

    // Motion.
    pub stiffness: f32,
    pub damping: f32,
    pub stagger: f32,
    pub hover_lift: f32,
    pub hover_grow: f32,

    // Sample wallpaper used by the configurator preview and the README sheet.
    pub preview_a: Rgba,
    pub preview_b: Rgba,
}

impl Theme {
    /// Defaults every theme file starts from (the "Obsidian Glass" look).
    pub fn base() -> Theme {
        Theme {
            name: "custom".into(),
            label: "Custom".into(),
            light: false,
            accent: Rgba::rgb(0x6e, 0xf2, 0xc9),
            accent2: Rgba::rgb(0x6e, 0xf2, 0xc9),
            text: Rgba::rgb(0xee, 0xf2, 0xfa),
            text_dim: Rgba(0xee, 0xf2, 0xfa, 150),
            glass_top: Rgba(0x3a, 0x42, 0x58, 150),
            glass_bottom: Rgba(0x0b, 0x0e, 0x18, 190),
            hub: Rgba(0x0a, 0x0d, 0x16, 200),
            rim: Rgba(255, 255, 255, 36),
            highlight: Rgba(255, 255, 255, 120),
            shadow: Rgba(0, 0, 0, 120),
            hover_fill: Rgba(0x6e, 0xf2, 0xc9, 70),
            hover_text: Rgba::rgb(0xff, 0xff, 0xff),
            sheen: 0.5,
            glow: 0.6,
            shadow_spread: 22.0,
            shadow_dy: 10.0,
            blur: 22.0,
            saturation: 1.35,
            orbit: 0.5,
            ticks: false,
            scan: 0.0,
            inset: 1.5,
            ring: 100.0,
            gap: 5.0,
            corner: 14.0,
            font: FontKind::Variable,
            weight: 600.0,
            tracking: 0.12,
            caps: true,
            stiffness: 260.0,
            damping: 0.62,
            stagger: 0.035,
            hover_lift: 9.0,
            hover_grow: 6.0,
            preview_a: Rgba::rgb(0x2b, 0x3a, 0x67),
            preview_b: Rgba::rgb(0x8a, 0x4f, 0x9e),
        }
    }

    /// Applies one `key = value` pair.
    pub fn set(&mut self, key: &str, val: &str) -> Result<(), String> {
        let f = |v: &str| v.trim().parse::<f32>().map_err(|_| format!("`{key}` needs a number, got `{v}`"));
        let b = |v: &str| match v.trim() {
            "true" | "yes" | "on" | "1" => Ok(true),
            "false" | "no" | "off" | "0" => Ok(false),
            _ => Err(format!("`{key}` needs true or false, got `{v}`")),
        };
        let v = val.trim();
        match key {
            "name" => self.name = v.to_ascii_lowercase(),
            "label" => self.label = v.into(),
            "light" => self.light = b(v)?,
            "accent" => self.accent = Rgba::parse(v)?,
            "accent2" => self.accent2 = Rgba::parse(v)?,
            "text" => self.text = Rgba::parse(v)?,
            "text_dim" => self.text_dim = Rgba::parse(v)?,
            "glass_top" => self.glass_top = Rgba::parse(v)?,
            "glass_bottom" => self.glass_bottom = Rgba::parse(v)?,
            "hub" => self.hub = Rgba::parse(v)?,
            "rim" => self.rim = Rgba::parse(v)?,
            "highlight" => self.highlight = Rgba::parse(v)?,
            "shadow" => self.shadow = Rgba::parse(v)?,
            "hover_fill" => self.hover_fill = Rgba::parse(v)?,
            "hover_text" => self.hover_text = Rgba::parse(v)?,
            "sheen" => self.sheen = f(v)?,
            "glow" => self.glow = f(v)?,
            "shadow_spread" => self.shadow_spread = f(v)?,
            "shadow_dy" => self.shadow_dy = f(v)?,
            "blur" => self.blur = f(v)?,
            "saturation" => self.saturation = f(v)?,
            "orbit" => self.orbit = f(v)?,
            "ticks" => self.ticks = b(v)?,
            "scan" => self.scan = f(v)?,
            "inset" => self.inset = f(v)?,
            "ring" => self.ring = f(v)?,
            "gap" => self.gap = f(v)?,
            "corner" => self.corner = f(v)?,
            "font" => self.font = FontKind::from_key(v).ok_or_else(|| format!("unknown font `{v}`"))?,
            "weight" => self.weight = f(v)?,
            "tracking" => self.tracking = f(v)?,
            "caps" => self.caps = b(v)?,
            "stiffness" => self.stiffness = f(v)?,
            "damping" => self.damping = f(v)?,
            "stagger" => self.stagger = f(v)?,
            "hover_lift" => self.hover_lift = f(v)?,
            "hover_grow" => self.hover_grow = f(v)?,
            "preview_a" => self.preview_a = Rgba::parse(v)?,
            "preview_b" => self.preview_b = Rgba::parse(v)?,
            _ => return Err(format!("unknown key `{key}`")),
        }
        Ok(())
    }

    /// Parses a theme file. `#` starts a comment line; the first error aborts with its line number.
    pub fn parse(text: &str) -> Result<Theme, String> {
        let mut t = Theme::base();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let (k, v) = line.split_once('=').ok_or_else(|| format!("line {}: expected key = value", n + 1))?;
            t.set(k.trim(), v).map_err(|e| format!("line {}: {e}", n + 1))?;
        }
        t.validate()?;
        Ok(t)
    }

    /// Rejects values that would break rendering (division by zero, negative sizes, unusable ids).
    pub fn validate(&self) -> Result<(), String> {
        let range = |what: &str, v: f32, lo: f32, hi: f32| {
            if v.is_finite() && (lo..=hi).contains(&v) {
                Ok(())
            } else {
                Err(format!("{what} = {v} is outside {lo}..{hi}"))
            }
        };
        if self.name.is_empty() || !self.name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
        {
            return Err(format!("name `{}` must be lowercase letters, digits or -", self.name));
        }
        range("sheen", self.sheen, 0.0, 1.0)?;
        range("glow", self.glow, 0.0, 1.0)?;
        range("orbit", self.orbit, 0.0, 1.0)?;
        range("scan", self.scan, 0.0, 1.0)?;
        range("inset", self.inset, 0.5, 6.0)?;
        range("shadow_spread", self.shadow_spread, 0.0, 60.0)?;
        range("shadow_dy", self.shadow_dy, -30.0, 40.0)?;
        range("blur", self.blur, 0.0, 60.0)?;
        range("saturation", self.saturation, 0.0, 3.0)?;
        range("ring", self.ring, 56.0, 150.0)?;
        range("gap", self.gap, 0.0, 24.0)?;
        range("corner", self.corner, 0.0, 40.0)?;
        range("weight", self.weight, 100.0, 900.0)?;
        range("tracking", self.tracking, 0.0, 0.5)?;
        range("stiffness", self.stiffness, 40.0, 1200.0)?;
        range("damping", self.damping, 0.15, 1.5)?;
        range("stagger", self.stagger, 0.0, 0.15)?;
        range("hover_lift", self.hover_lift, 0.0, 30.0)?;
        range("hover_grow", self.hover_grow, 0.0, 30.0)?;
        Ok(())
    }

    /// Text form that [`Theme::parse`] reads back to an equal theme.
    pub fn to_text(&self) -> String {
        let mut s = String::new();
        let mut line = |k: &str, v: String| {
            let _ = writeln!(s, "{k} = {v}");
        };
        line("name", self.name.clone());
        line("label", self.label.clone());
        line("light", self.light.to_string());
        for (k, c) in [
            ("accent", self.accent),
            ("accent2", self.accent2),
            ("text", self.text),
            ("text_dim", self.text_dim),
            ("glass_top", self.glass_top),
            ("glass_bottom", self.glass_bottom),
            ("hub", self.hub),
            ("rim", self.rim),
            ("highlight", self.highlight),
            ("shadow", self.shadow),
            ("hover_fill", self.hover_fill),
            ("hover_text", self.hover_text),
        ] {
            line(k, c.to_hex());
        }
        for (k, v) in [
            ("sheen", self.sheen),
            ("glow", self.glow),
            ("shadow_spread", self.shadow_spread),
            ("shadow_dy", self.shadow_dy),
            ("blur", self.blur),
            ("saturation", self.saturation),
            ("orbit", self.orbit),
            ("scan", self.scan),
            ("inset", self.inset),
            ("ring", self.ring),
            ("gap", self.gap),
            ("corner", self.corner),
            ("weight", self.weight),
            ("tracking", self.tracking),
            ("stiffness", self.stiffness),
            ("damping", self.damping),
            ("stagger", self.stagger),
            ("hover_lift", self.hover_lift),
            ("hover_grow", self.hover_grow),
        ] {
            line(k, v.to_string());
        }
        line("ticks", self.ticks.to_string());
        line("caps", self.caps.to_string());
        line("font", self.font.key().into());
        line("preview_a", self.preview_a.to_hex());
        line("preview_b", self.preview_b.to_hex());
        s
    }
}

/// Built-in presets in display order. To add one: drop a file in `assets/themes/` and list it here.
pub const BUILTIN: &[(&str, &str)] = &[
    ("glass", include_str!("../assets/themes/glass.theme")),
    ("frost", include_str!("../assets/themes/frost.theme")),
    ("ink", include_str!("../assets/themes/ink.theme")),
    ("aurora", include_str!("../assets/themes/aurora.theme")),
    ("sunset", include_str!("../assets/themes/sunset.theme")),
    ("neon", include_str!("../assets/themes/neon.theme")),
    ("noir", include_str!("../assets/themes/noir.theme")),
];

/// All built-in themes. A preset that fails to parse is a bug caught by the unit tests.
pub fn builtin() -> Vec<Theme> {
    BUILTIN.iter().filter_map(|(_, src)| Theme::parse(src).ok()).collect()
}

/// Looks a theme up by id: built-ins first, then `extra` (user theme files).
pub fn find(name: &str, extra: &[Theme]) -> Theme {
    let all = builtin();
    all.into_iter()
        .chain(extra.iter().cloned())
        .find(|t| t.name == name)
        .or_else(|| builtin().into_iter().next())
        .unwrap_or_else(Theme::base)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colours_parse_in_all_three_forms() {
        assert_eq!(Rgba::parse("#fa0"), Ok(Rgba(255, 170, 0, 255)));
        assert_eq!(Rgba::parse("#102030"), Ok(Rgba(16, 32, 48, 255)));
        assert_eq!(Rgba::parse(" #10203040 "), Ok(Rgba(16, 32, 48, 64)));
    }

    #[test]
    fn bad_colours_are_rejected() {
        assert!(Rgba::parse("102030").is_err());
        assert!(Rgba::parse("#12345").is_err());
        assert!(Rgba::parse("#gg0000").is_err());
        assert!(Rgba::parse("#éé").is_err());
    }

    #[test]
    fn colour_hex_round_trips() {
        for c in [Rgba(1, 2, 3, 255), Rgba(250, 128, 0, 17)] {
            assert_eq!(Rgba::parse(&c.to_hex()), Ok(c));
        }
    }

    #[test]
    fn theme_file_overrides_only_listed_keys() {
        let t = Theme::parse("# my theme\nname = mine\naccent = #ff0000\ngap = 8\n").unwrap();
        assert_eq!(t.name, "mine");
        assert_eq!(t.accent, Rgba(255, 0, 0, 255));
        assert_eq!(t.gap, 8.0);
        assert_eq!(t.corner, Theme::base().corner);
    }

    #[test]
    fn theme_errors_carry_the_line_number() {
        let e = Theme::parse("name = x\n\nbogus = 1\n").unwrap_err();
        assert!(e.starts_with("line 3:"), "{e}");
        assert!(Theme::parse("name = x\ngap\n").unwrap_err().starts_with("line 2:"));
        assert!(Theme::parse("accent = red\n").is_err());
        assert!(Theme::parse("font = comic\n").is_err());
    }

    #[test]
    fn validation_rejects_values_that_break_rendering() {
        assert!(Theme::parse("ring = 0\n").is_err());
        assert!(Theme::parse("gap = -1\n").is_err());
        assert!(Theme::parse("damping = 0\n").is_err());
        assert!(Theme::parse("blur = nan\n").is_err());
        assert!(Theme::parse("name = Has Space\n").is_err());
        assert!(Theme::parse("stiffness = 5000\n").is_err());
    }

    #[test]
    fn theme_text_round_trips() {
        let t = builtin().into_iter().find(|t| t.name == "sunset").unwrap();
        assert_eq!(Theme::parse(&t.to_text()), Ok(t));
    }

    #[test]
    fn at_least_six_distinct_valid_presets() {
        let all = builtin();
        assert_eq!(all.len(), BUILTIN.len(), "a preset failed to parse");
        assert!(all.len() >= 6);
        for (i, t) in all.iter().enumerate() {
            assert_eq!(t.name, BUILTIN[i].0, "file key and `name` must agree");
            assert!(all.iter().filter(|o| o.name == t.name).count() == 1);
            assert!(t.validate().is_ok());
        }
    }

    #[test]
    fn unknown_name_falls_back_to_first_preset() {
        assert_eq!(find("nope", &[]).name, BUILTIN[0].0);
        let mine = Theme::parse("name = mine\n").unwrap();
        assert_eq!(find("mine", std::slice::from_ref(&mine)).name, "mine");
    }

    #[test]
    fn hsv_matches_primaries() {
        assert_eq!(Rgba::from_hsv(0.0, 1.0, 1.0), Rgba(255, 0, 0, 255));
        assert_eq!(Rgba::from_hsv(120.0, 1.0, 1.0), Rgba(0, 255, 0, 255));
        assert_eq!(Rgba::from_hsv(240.0, 1.0, 0.5), Rgba(0, 0, 128, 255));
        assert_eq!(Rgba::from_hsv(77.0, 0.0, 1.0), Rgba(255, 255, 255, 255));
    }
}
