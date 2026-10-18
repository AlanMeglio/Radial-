//! User settings: a tiny `key = value` file in `%APPDATA%\radial\config`, plus the English/Spanish strings.

use std::path::{Path, PathBuf};

use crate::ring::{RingKind, Switch};
use crate::theme::{FontKind, Rgba, Theme};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TriggerKey {
    Shift,
    Ctrl,
    Alt,
    /// No key: any drag of a file opens the menu.
    None,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Collision {
    /// `photo (1).png`, never touches existing files.
    Rename,
    Overwrite,
    Skip,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Notify {
    Off,
    Balloon,
    Sound,
    Both,
}

impl Notify {
    pub fn balloon(self) -> bool {
        matches!(self, Notify::Balloon | Notify::Both)
    }

    pub fn sound(self) -> bool {
        matches!(self, Notify::Sound | Notify::Both)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lang {
    En,
    Es,
}

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    pub theme: String,
    /// `None` keeps the theme's own accent.
    pub accent: Option<Rgba>,
    /// Overall scale of the menu, 0.7..1.5.
    pub size: f32,
    pub ring: f32,
    pub gap: f32,
    pub corner: f32,
    /// Glass strength 0..1.5: 0 is a plain tinted surface, 1 the theme's frosted blur.
    pub glass: f32,
    /// Animation speed multiplier 0.4..2.5.
    pub speed: f32,
    /// `None` uses the theme's font.
    pub font: Option<FontKind>,
    pub trigger: TriggerKey,
    /// Milliseconds the drag has to be held (with the key) before the menu opens.
    pub delay_ms: u32,
    /// `None` writes results next to the source file.
    pub out_dir: Option<String>,
    pub collision: Collision,
    pub notify: Notify,
    pub autostart: bool,
    pub lang: Lang,
    pub onboarded: bool,
    /// How to flip between the conversion ring and the actions ring while dragging an image.
    pub ring_switch: Switch,
    /// Which ring an image drag opens with.
    pub start_ring: RingKind,
}

pub const SIZE_RANGE: (f32, f32) = (0.7, 1.5);
pub const GLASS_RANGE: (f32, f32) = (0.0, 1.5);
pub const SPEED_RANGE: (f32, f32) = (0.4, 2.5);
pub const RING_RANGE: (f32, f32) = (56.0, 150.0);
pub const GAP_RANGE: (f32, f32) = (0.0, 24.0);
pub const CORNER_RANGE: (f32, f32) = (0.0, 40.0);
pub const DELAY_MAX: u32 = 800;

impl Default for Config {
    fn default() -> Config {
        let t = Theme::base();
        Config {
            theme: "glass".into(),
            accent: None,
            size: 1.0,
            ring: t.ring,
            gap: t.gap,
            corner: t.corner,
            glass: 1.0,
            speed: 1.0,
            font: None,
            trigger: TriggerKey::Shift,
            delay_ms: 0,
            out_dir: None,
            collision: Collision::Rename,
            notify: Notify::Balloon,
            autostart: false,
            lang: Lang::En,
            onboarded: false,
            ring_switch: Switch::Both,
            start_ring: RingKind::Convert,
        }
    }
}

/// Keyword <-> enum tables so parsing and writing can never disagree.
pub const TRIGGERS: [(&str, TriggerKey); 4] =
    [("shift", TriggerKey::Shift), ("ctrl", TriggerKey::Ctrl), ("alt", TriggerKey::Alt), ("none", TriggerKey::None)];
const COLLISIONS: [(&str, Collision); 3] =
    [("rename", Collision::Rename), ("overwrite", Collision::Overwrite), ("skip", Collision::Skip)];
const NOTIFIES: [(&str, Notify); 4] =
    [("off", Notify::Off), ("balloon", Notify::Balloon), ("sound", Notify::Sound), ("both", Notify::Both)];
const LANGS: [(&str, Lang); 2] = [("en", Lang::En), ("es", Lang::Es)];
const SWITCHES: [(&str, Switch); 3] = [("both", Switch::Both), ("tab", Switch::Tab), ("hub", Switch::Hub)];
const RINGS: [(&str, RingKind); 2] = [("convert", RingKind::Convert), ("actions", RingKind::Actions)];

fn word<T: Copy>(table: &[(&'static str, T)], v: &str) -> Option<T> {
    table.iter().find(|(k, _)| *k == v.to_ascii_lowercase()).map(|(_, t)| *t)
}

fn name_of<T: Copy + PartialEq>(table: &[(&'static str, T)], v: T) -> &'static str {
    table.iter().find(|(_, t)| *t == v).map_or("", |(k, _)| k)
}

fn number(v: &str, (lo, hi): (f32, f32)) -> Result<f32, String> {
    let n: f32 = v.parse().map_err(|_| format!("`{v}` is not a number"))?;
    if n.is_finite() {
        Ok(n.clamp(lo, hi))
    } else {
        Err(format!("`{v}` is not a finite number"))
    }
}

fn flag(v: &str) -> Result<bool, String> {
    match v {
        "true" | "yes" | "on" | "1" => Ok(true),
        "false" | "no" | "off" | "0" => Ok(false),
        _ => Err(format!("`{v}` is not true or false")),
    }
}

impl Config {
    /// Lenient parse: unknown keys and bad values are skipped (and reported), never fatal.
    pub fn parse(text: &str) -> (Config, Vec<String>) {
        let mut c = Config::default();
        let mut warns = Vec::new();
        for (n, line) in text.lines().enumerate() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            let Some((k, v)) = line.split_once('=') else {
                warns.push(format!("line {}: expected key = value", n + 1));
                continue;
            };
            if let Err(e) = c.set(k.trim(), v.trim()) {
                warns.push(format!("line {}: {e}", n + 1));
            }
        }
        (c, warns)
    }

    fn set(&mut self, key: &str, v: &str) -> Result<(), String> {
        let bad = |what: &str| format!("unknown {what} `{v}`");
        match key {
            "theme" => self.theme = v.to_ascii_lowercase(),
            "accent" if v == "auto" => self.accent = None,
            "accent" => self.accent = Some(Rgba::parse(v)?),
            "size" => self.size = number(v, SIZE_RANGE)?,
            "ring" => self.ring = number(v, RING_RANGE)?,
            "gap" => self.gap = number(v, GAP_RANGE)?,
            "corner" => self.corner = number(v, CORNER_RANGE)?,
            "glass" => self.glass = number(v, GLASS_RANGE)?,
            "speed" => self.speed = number(v, SPEED_RANGE)?,
            "font" if v == "theme" => self.font = None,
            "font" => self.font = Some(FontKind::from_key(v).ok_or_else(|| bad("font"))?),
            "trigger" => self.trigger = word(&TRIGGERS, v).ok_or_else(|| bad("trigger"))?,
            "delay_ms" => {
                self.delay_ms = v.parse::<u32>().map_err(|_| format!("`{v}` is not a whole number"))?.min(DELAY_MAX)
            }
            "out_dir" => self.out_dir = if v.is_empty() { None } else { Some(v.to_string()) },
            "collision" => self.collision = word(&COLLISIONS, v).ok_or_else(|| bad("collision policy"))?,
            "notify" => self.notify = word(&NOTIFIES, v).ok_or_else(|| bad("notify mode"))?,
            "autostart" => self.autostart = flag(v)?,
            "lang" => self.lang = word(&LANGS, v).ok_or_else(|| bad("language"))?,
            "onboarded" => self.onboarded = flag(v)?,
            "ring_switch" => self.ring_switch = word(&SWITCHES, v).ok_or_else(|| bad("ring switch"))?,
            "start_ring" => self.start_ring = word(&RINGS, v).ok_or_else(|| bad("ring"))?,
            _ => return Err(format!("unknown key `{key}`")),
        }
        Ok(())
    }

    pub fn to_text(&self) -> String {
        let accent = self.accent.map_or("auto".to_string(), |c| c.to_hex());
        let font = self.font.map_or("theme", |f| f.key());
        format!(
            "# Radial settings. Edit freely; the Settings window rewrites this file.\n\
             theme = {}\naccent = {accent}\nsize = {}\nring = {}\ngap = {}\ncorner = {}\nglass = {}\nspeed = {}\n\
             font = {font}\ntrigger = {}\ndelay_ms = {}\nout_dir = {}\ncollision = {}\nnotify = {}\n\
             autostart = {}\nlang = {}\nonboarded = {}\nring_switch = {}\nstart_ring = {}\n",
            self.theme,
            self.size,
            self.ring,
            self.gap,
            self.corner,
            self.glass,
            self.speed,
            name_of(&TRIGGERS, self.trigger),
            self.delay_ms,
            self.out_dir.as_deref().unwrap_or(""),
            name_of(&COLLISIONS, self.collision),
            name_of(&NOTIFIES, self.notify),
            self.autostart,
            name_of(&LANGS, self.lang),
            self.onboarded,
            name_of(&SWITCHES, self.ring_switch),
            name_of(&RINGS, self.start_ring),
        )
    }

    /// Switches to a preset and takes over its geometry, dropping a custom accent.
    pub fn apply_theme(&mut self, t: &Theme) {
        self.theme = t.name.clone();
        self.accent = None;
        self.ring = t.ring;
        self.gap = t.gap;
        self.corner = t.corner;
    }

    /// The theme with this config's overrides applied.
    pub fn resolve(&self, base: &Theme) -> Theme {
        let mut t = base.clone();
        t.ring = self.ring;
        t.gap = self.gap;
        t.corner = self.corner;
        if let Some(f) = self.font {
            t.font = f;
        }
        if let Some(a) = self.accent {
            t.accent = a;
            t.accent2 = a.mix(Rgba::rgb(255, 255, 255), 0.35);
            t.hover_fill = Rgba(a.0, a.1, a.2, base.hover_fill.3);
            // Dark text on light accents, white on the rest.
            let luma = (0.299 * a.0 as f32 + 0.587 * a.1 as f32 + 0.114 * a.2 as f32) / 255.0;
            t.hover_text = if luma > 0.62 { Rgba::rgb(0x0c, 0x0e, 0x14) } else { Rgba::rgb(255, 255, 255) };
        }
        t
    }

    /// The directory for results, if the user picked one.
    pub fn out_path(&self) -> Option<PathBuf> {
        self.out_dir.as_deref().filter(|d| !d.is_empty()).map(PathBuf::from)
    }
}

pub enum Dest {
    /// The converted file is already where it should be.
    Keep(PathBuf),
    /// Move it here (the target is free).
    Move(PathBuf),
    /// Move it here, replacing what is there.
    Replace(PathBuf),
    /// Delete the converted file, the target exists and the policy says skip.
    Discard,
}

/// Decides where a freshly converted file ends up. Pure: `exists` is injected.
pub fn plan_destination(
    source: &Path,
    result: &Path,
    out_dir: Option<&Path>,
    policy: Collision,
    exists: impl Fn(&Path) -> bool,
) -> Dest {
    let same = |a: &Path, b: &Path| a.to_string_lossy().eq_ignore_ascii_case(&b.to_string_lossy());
    let (Some(stem), Some(ext)) = (source.file_stem(), result.extension()) else { return Dest::Keep(result.into()) };
    let (stem, ext) = (stem.to_string_lossy(), ext.to_string_lossy());
    let dir = out_dir.or_else(|| source.parent()).unwrap_or(Path::new(""));
    let intended = dir.join(format!("{stem}.{ext}"));
    // Already in place, or the target is the original file, which is never touched.
    if same(&intended, result) || same(&intended, source) {
        return Dest::Keep(result.into());
    }
    if !exists(&intended) {
        return Dest::Move(intended);
    }
    match (policy, out_dir) {
        (Collision::Rename, None) => Dest::Keep(result.into()),
        (Collision::Rename, Some(d)) => Dest::Move(crate::convert::unique_path(d, &stem, &ext, &exists)),
        (Collision::Overwrite, _) => Dest::Replace(intended),
        (Collision::Skip, _) => Dest::Discard,
    }
}

/// `%APPDATA%\radial`, if the variable is set.
pub fn data_dir() -> Option<PathBuf> {
    std::env::var_os("APPDATA").map(|a| PathBuf::from(a).join("radial"))
}

pub fn load() -> Config {
    let text = data_dir().and_then(|d| std::fs::read_to_string(d.join("config")).ok()).unwrap_or_default();
    Config::parse(&text).0
}

/// Writes the config atomically enough for a settings file: temp file, then rename.
pub fn save(c: &Config) -> std::io::Result<()> {
    let Some(dir) = data_dir() else { return Ok(()) };
    std::fs::create_dir_all(&dir)?;
    let tmp = dir.join("config.tmp");
    std::fs::write(&tmp, c.to_text())?;
    std::fs::rename(&tmp, dir.join("config"))
}

/// Theme files the user dropped into `%APPDATA%\radial\themes`. Broken files are skipped.
pub fn user_themes() -> Vec<Theme> {
    let Some(dir) = data_dir().map(|d| d.join("themes")) else { return vec![] };
    let Ok(rd) = std::fs::read_dir(dir) else { return vec![] };
    let mut v: Vec<Theme> = rd
        .filter_map(|e| e.ok())
        .filter(|e| e.path().extension().is_some_and(|x| x == "theme"))
        .filter_map(|e| std::fs::read_to_string(e.path()).ok())
        .filter_map(|s| Theme::parse(&s).ok())
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// `(key, English, Spanish)`. `{}` placeholders are filled by the caller.
pub const STRINGS: &[(&str, &str, &str)] = &[
    ("settings", "Settings...", "Configuración..."),
    ("pause", "Pause", "Pausar"),
    ("resume", "Resume", "Reanudar"),
    ("startup", "Start with Windows", "Iniciar con Windows"),
    ("about", "About", "Acerca de"),
    ("quit", "Quit", "Salir"),
    ("title", "Radial Settings", "Configuración de Radial"),
    ("tab-look", "Look", "Estética"),
    ("tab-behavior", "Behavior", "Comportamiento"),
    ("theme", "Theme", "Tema"),
    ("accent", "Accent", "Acento"),
    ("size", "Size", "Tamaño"),
    ("ring", "Ring thickness", "Grosor del anillo"),
    ("gap", "Gap", "Separación"),
    ("corner", "Corner roundness", "Redondeo"),
    ("glass", "Glass", "Cristal"),
    ("speed", "Animation speed", "Velocidad de animación"),
    ("font", "Font", "Fuente"),
    ("font-theme", "Theme", "Del tema"),
    ("trigger", "Trigger key", "Tecla de activación"),
    ("key-none", "None", "Ninguna"),
    ("delay", "Open delay", "Retardo de apertura"),
    ("output", "Output folder", "Carpeta de salida"),
    ("out-source", "Next to source", "Junto al original"),
    ("out-custom", "Custom folder", "Carpeta propia"),
    ("browse", "Browse...", "Elegir..."),
    ("collision", "If the name exists", "Si el nombre existe"),
    ("col-rename", "Rename", "Renombrar"),
    ("col-over", "Overwrite", "Reemplazar"),
    ("col-skip", "Skip", "Omitir"),
    ("notify", "When done", "Al terminar"),
    ("not-off", "Nothing", "Nada"),
    ("not-balloon", "Notice", "Aviso"),
    ("not-sound", "Sound", "Sonido"),
    ("not-both", "Both", "Ambos"),
    ("language", "Language", "Idioma"),
    ("autostart", "Start with Windows", "Iniciar con Windows"),
    ("done", "Done", "Listo"),
    ("next", "Next", "Siguiente"),
    ("back", "Back", "Atrás"),
    ("start", "Start using Radial", "Empezar a usar Radial"),
    ("reset", "Reset look", "Restablecer estética"),
    ("step1", "Pick a look", "Elige tu estética"),
    (
        "step1-body",
        "Try the themes, the preview reacts to your mouse.",
        "Prueba los temas, la vista previa reacciona al mouse.",
    ),
    ("step2", "Choose your key", "Elige tu tecla"),
    (
        "step2-body",
        "Hold it while dragging a file to open the menu.",
        "Mantenla mientras arrastras un archivo para abrir el menú.",
    ),
    ("step3", "You are set", "Todo listo"),
    (
        "step3-body",
        "Drag a file, hold {}, drop it on a sector.",
        "Arrastra un archivo, mantén {} y suéltalo sobre un sector.",
    ),
    ("hint", "Move over the preview to try it", "Mueve el cursor sobre la vista previa"),
    (
        "about-text",
        "Radial {}\nDrag a file, hold the key, drop it on a sector.\nMIT licensed open source.",
        "Radial {}\nArrastra un archivo, mantén la tecla y suéltalo sobre un sector.\nCódigo abierto con licencia MIT.",
    ),
    ("saved", "Saved {}", "Guardado {}"),
    ("converted", "Converted {} files to {}", "Convertidos {} archivos a {}"),
    ("failed", "Failed: {}", "Error: {}"),
    ("skipped", "Skipped, {} already exists", "Omitido, {} ya existe"),
    ("tip", "Radial: hold {} while dragging a file", "Radial: mantén {} mientras arrastras un archivo"),
    ("tip-paused", "Radial (paused)", "Radial (en pausa)"),
    ("key-any", "any drag", "cualquier arrastre"),
    ("folder-pick", "Choose the output folder", "Elige la carpeta de salida"),
    ("custom", "Custom", "Propio"),
    ("menu-img", "Image menu", "Menú de imágenes"),
    ("menu-doc", "Document menu", "Menú de documentos"),
    ("menu-act", "Actions menu", "Menú de acciones"),
    ("step-of", "Step {} of 3", "Paso {} de 3"),
    ("hold", "Hold", "Mantén"),
    ("no-key-hint", "No key: every file drag opens the menu.", "Sin tecla: cada arrastre de archivo abre el menú."),
    (
        "tray-hint",
        "You can reopen Settings from the tray icon.",
        "Puedes volver a abrir la configuración desde el icono de la bandeja.",
    ),
    ("how1", "Drag a file in Explorer", "Arrastra un archivo en el Explorador"),
    ("how2", "Hold {}", "Mantén {}"),
    ("how3", "Drop it on a sector", "Suéltalo sobre un sector"),
    ("ms", "ms", "ms"),
    ("off", "Off", "Apagado"),
    ("act-compress", "COMPRESS", "COMPRIMIR"),
    ("act-metadata", "METADATA", "METADATOS"),
    ("act-edit", "EDIT", "EDITAR"),
    ("act-annotate", "ANNOTATE", "ANOTAR"),
    ("act-addbg", "ADD BG", "FONDO"),
    ("act-crop", "CROP", "RECORTAR"),
    ("act-redact", "REDACT", "TACHAR"),
    ("act-compress-hint", "Smaller file", "Más liviano"),
    ("act-metadata-hint", "Strip EXIF", "Quita EXIF"),
    ("act-edit-hint", "Rotate, tone", "Girar, tono"),
    ("act-annotate-hint", "Draw on it", "Dibujar"),
    ("act-addbg-hint", "Add backdrop", "Con fondo"),
    ("act-crop-hint", "Trim edges", "Recortar"),
    ("act-redact-hint", "Black out", "Tapar zonas"),
    ("convert-to", "Convert to", "Convertir a"),
    ("unavailable", "Unavailable", "No disponible"),
    ("actions-title", "Actions", "Acciones"),
    ("files-n", "{} files", "{} archivos"),
    ("sw-both", "Tab or hub", "Tab o centro"),
    ("sw-tab", "Tab", "Tab"),
    ("sw-hub", "Hub", "Centro"),
    ("go-actions", "{}: actions", "{}: acciones"),
    ("go-convert", "{}: convert", "{}: convertir"),
    ("why-one-file", "Drop a single image to use this", "Suelta una sola imagen para usar esto"),
    ("why-formats", "Works on {} only", "Solo funciona con {}"),
    ("compressed", "Compressed {}: {} to {} ({})", "Comprimido {}: {} a {} ({})"),
    ("compressed-many", "Compressed {} files: {} to {} ({})", "Comprimidos {} archivos: {} a {} ({})"),
    ("optimal", "{} is already as small as it gets ({})", "{} ya es lo más liviano posible ({})"),
    ("meta-none", "No metadata found in {}", "No hay metadatos en {}"),
    ("meta-done", "Saved {} (removed {} entries, {})", "Guardado {} (quitadas {} entradas, {})"),
    (
        "meta-gps",
        "Saved {} without location data (removed {} entries, {})",
        "Guardado {} sin datos de ubicación (quitadas {} entradas, {})",
    ),
    ("tab-ring", "Drag ring", "Anillo de arrastre"),
    ("ring-switch", "Switch rings with", "Cambiar de anillo con"),
    ("start-ring", "Image menu opens on", "El menú de imágenes abre en"),
    ("ring-convert", "Convert", "Convertir"),
    ("ring-actions", "Actions", "Acciones"),
    ("sw-label-both", "Tab + hub", "Tab + centro"),
];

/// Looks a UI string up; unknown keys give an empty string (caught by the unit tests, not at runtime).
pub fn tr(lang: Lang, key: &str) -> &'static str {
    STRINGS.iter().find(|s| s.0 == key).map_or("", |s| if lang == Lang::Es { s.2 } else { s.1 })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::theme;

    #[test]
    fn default_config_round_trips() {
        let c = Config::default();
        let (back, warns) = Config::parse(&c.to_text());
        assert_eq!(back, c);
        assert!(warns.is_empty(), "{warns:?}");
    }

    #[test]
    fn every_field_round_trips() {
        let c = Config {
            theme: "sunset".into(),
            accent: Some(Rgba(1, 2, 3, 255)),
            size: 1.25,
            ring: 88.5,
            gap: 2.0,
            corner: 20.0,
            glass: 0.4,
            speed: 1.75,
            font: Some(FontKind::Consolas),
            trigger: TriggerKey::Alt,
            delay_ms: 250,
            out_dir: Some(r"D:\Out Dir\with = sign".into()),
            collision: Collision::Skip,
            notify: Notify::Both,
            autostart: true,
            lang: Lang::Es,
            onboarded: true,
            ring_switch: Switch::Hub,
            start_ring: RingKind::Actions,
        };
        let (back, warns) = Config::parse(&c.to_text());
        assert_eq!(back, c);
        assert!(warns.is_empty());
    }

    #[test]
    fn ring_settings_default_to_tab_or_hub_and_the_conversion_ring() {
        let c = Config::default();
        assert_eq!((c.ring_switch, c.start_ring), (Switch::Both, RingKind::Convert));
        let (c, w) = Config::parse(
            "ring_switch = tab
start_ring = actions
",
        );
        assert_eq!((c.ring_switch, c.start_ring), (Switch::Tab, RingKind::Actions));
        assert!(w.is_empty());
        let (c, w) = Config::parse(
            "ring_switch = sideways
start_ring = third
",
        );
        assert_eq!((c.ring_switch, c.start_ring), (Switch::Both, RingKind::Convert));
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn empty_or_garbage_file_gives_defaults() {
        assert_eq!(Config::parse("").0, Config::default());
        let (c, w) = Config::parse("not a pair\n\u{1}\u{2}\n");
        assert_eq!(c, Config::default());
        assert_eq!(w.len(), 2);
    }

    #[test]
    fn bad_values_fall_back_and_are_reported() {
        let (c, w) = Config::parse("size = big\ntrigger = hyper\naccent = nope\nunknown = 1\ndelay_ms = -4\n");
        let d = Config::default();
        assert_eq!((c.size, c.trigger, c.accent, c.delay_ms), (d.size, d.trigger, d.accent, d.delay_ms));
        assert_eq!(w.len(), 5);
    }

    #[test]
    fn numbers_are_clamped_into_their_range() {
        let (c, _) =
            Config::parse("size = 9\nglass = -3\nspeed = 0\ndelay_ms = 99999\nring = 1\ngap = 99\ncorner = 99\n");
        assert_eq!(c.size, SIZE_RANGE.1);
        assert_eq!(c.glass, GLASS_RANGE.0);
        assert_eq!(c.speed, SPEED_RANGE.0);
        assert_eq!(c.delay_ms, DELAY_MAX);
        assert_eq!((c.ring, c.gap, c.corner), (56.0, 24.0, 40.0));
    }

    #[test]
    fn comments_blank_lines_and_crlf_are_fine() {
        let (c, w) = Config::parse("# hi\r\n\r\ntheme = ink\r\nnotify = sound\r\n");
        assert_eq!((c.theme.as_str(), c.notify), ("ink", Notify::Sound));
        assert!(w.is_empty());
    }

    #[test]
    fn applying_a_theme_takes_its_geometry_and_clears_the_accent() {
        let ink = theme::builtin().into_iter().find(|t| t.name == "ink").unwrap();
        let mut c = Config { accent: Some(Rgba(9, 9, 9, 255)), ..Config::default() };
        c.apply_theme(&ink);
        assert_eq!((c.theme.as_str(), c.accent), ("ink", None));
        assert_eq!((c.ring, c.gap, c.corner), (ink.ring, ink.gap, ink.corner));
    }

    #[test]
    fn resolve_applies_overrides_over_the_theme() {
        let base = Theme::base();
        let c = Config {
            accent: Some(Rgba(200, 10, 10, 255)),
            ring: 70.0,
            gap: 9.0,
            corner: 3.0,
            font: Some(FontKind::Georgia),
            ..Config::default()
        };
        let t = c.resolve(&base);
        assert_eq!(
            (t.accent, t.ring, t.gap, t.corner, t.font),
            (Rgba(200, 10, 10, 255), 70.0, 9.0, 3.0, FontKind::Georgia)
        );
        assert_eq!(t.text, base.text);
        let plain = Config::default().resolve(&base);
        assert_eq!((plain.accent, plain.font), (base.accent, base.font));
    }

    fn p(s: &str) -> PathBuf {
        PathBuf::from(s)
    }

    #[test]
    fn destination_next_to_source_keeps_the_numbered_name_by_default() {
        let d = plan_destination(&p(r"C:\a\x.png"), &p(r"C:\a\x (1).webp"), None, Collision::Rename, |q| {
            q == p(r"C:\a\x.webp")
        });
        assert!(matches!(d, Dest::Keep(q) if q == p(r"C:\a\x (1).webp")));
    }

    #[test]
    fn overwrite_replaces_the_existing_target() {
        let d = plan_destination(&p(r"C:\a\x.png"), &p(r"C:\a\x (1).webp"), None, Collision::Overwrite, |q| {
            q == p(r"C:\a\x.webp")
        });
        assert!(matches!(d, Dest::Replace(q) if q == p(r"C:\a\x.webp")));
    }

    #[test]
    fn skip_discards_when_the_target_exists() {
        let d = plan_destination(&p(r"C:\a\x.png"), &p(r"C:\a\x (1).webp"), None, Collision::Skip, |_| true);
        assert!(matches!(d, Dest::Discard));
    }

    #[test]
    fn custom_folder_moves_and_renames_on_collision() {
        let out = p(r"D:\out");
        let free = plan_destination(&p(r"C:\a\x.png"), &p(r"C:\a\x.webp"), Some(&out), Collision::Rename, |_| false);
        assert!(matches!(free, Dest::Move(q) if q == p(r"D:\out\x.webp")));
        let taken = plan_destination(&p(r"C:\a\x.png"), &p(r"C:\a\x.webp"), Some(&out), Collision::Rename, |q| {
            q == p(r"D:\out\x.webp")
        });
        assert!(matches!(taken, Dest::Move(q) if q == p(r"D:\out\x (1).webp")));
    }

    #[test]
    fn the_source_file_is_never_overwritten() {
        // Converting a.png into the same folder with the same extension would target the source itself.
        let d = plan_destination(&p(r"C:\a\x.png"), &p(r"C:\a\x (1).png"), None, Collision::Overwrite, |_| true);
        assert!(matches!(d, Dest::Keep(_)));
    }

    #[test]
    fn every_string_has_both_languages() {
        let mut seen = std::collections::HashSet::new();
        for (k, en, es) in STRINGS {
            assert!(seen.insert(*k), "duplicate key {k}");
            assert!(!en.is_empty() && !es.is_empty(), "{k}");
        }
        assert_eq!(tr(Lang::Es, "settings"), STRINGS.iter().find(|s| s.0 == "settings").unwrap().2);
        assert_eq!(tr(Lang::En, "no-such-key"), "");
    }
}
