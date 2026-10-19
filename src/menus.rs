//! Turns a [`Menu`] into what the renderer draws: sector labels and icons, the hub texts and the caption pill.
//! Pure and localized; shared by the live overlay and the preview renderers so both always agree.

use crate::config::{tr, Lang};
use crate::engine::SourceKind;
use crate::jobs::fill;
use crate::render::{Icon, Scene, Slot};
use crate::ring::{Item, Menu, Op, Reason, RingKind, Switch};

/// What the hub and the pill say at one moment.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Texts {
    pub main: String,
    pub sub: String,
    /// The pill under the ring (empty hides it).
    pub caption: String,
    /// The hub's main line is an action (drawn in the accent colour).
    pub accent: bool,
}

/// Sector label: format names are universal, action names are translated.
pub fn label(op: Op, lang: Lang) -> &'static str {
    match op {
        Op::Convert(t) => t.label(),
        Op::Action(a) => tr(lang, a.label_key()),
        Op::Custom(_) => "",
    }
}

/// The text of a sector: the user's own words for a custom one, the translated name otherwise.
pub fn item_label(item: &Item, lang: Lang) -> String {
    item.custom.as_ref().map_or_else(|| label(item.op, lang).to_string(), |c| c.label.clone())
}

pub fn slots(menu: &Menu, lang: Lang) -> Vec<Slot> {
    menu.items
        .iter()
        .map(|i| {
            let icon = match i.op {
                Op::Convert(t) => Icon::for_label(t.label()),
                Op::Action(a) => Icon::for_label(tr(Lang::En, a.label_key())),
                Op::Custom(_) => match i.custom.as_ref().map(|c| c.kind) {
                    Some(crate::sectors::Kind::OpenWith) => Icon::Reveal,
                    _ => Icon::Folder,
                },
            };
            Slot { label: item_label(i, lang), enabled: i.enabled, icon }
        })
        .collect()
}

pub fn reason_text(r: &Reason, lang: Lang) -> String {
    match r {
        Reason::Engine(why) => why.clone(),
        Reason::OneFile => tr(lang, "why-one-file").to_string(),
        Reason::Formats(list) => fill(tr(lang, "why-formats"), &[list]),
        Reason::NoPath => tr(lang, "why-no-path").to_string(),
        Reason::FilesOnly => tr(lang, "why-files-only").to_string(),
    }
}

/// "Wheel, Tab or hub: images" - how to reach the next ring and what it is. Empty when the files have one ring.
pub fn switch_hint(menu: &Menu, switch: Switch, lang: Lang) -> String {
    if !menu.switchable() {
        return String::new();
    }
    let how = match switch {
        Switch::Both => tr(lang, "sw-both"),
        Switch::Tab => tr(lang, "sw-tab"),
        Switch::Hub => tr(lang, "sw-hub"),
    };
    fill(tr(lang, "go-next"), &[how, tr(lang, menu.neighbour(1).name_key())])
}

/// Hub texts while nothing is hovered. `size` is the human readable size of the file (or the file count).
pub fn idle(kind: SourceKind, menu: &Menu, size: &str, switch: Switch, lang: Lang) -> Texts {
    let caption = switch_hint(menu, switch, lang);
    let (main, sub) = match (menu.kind, kind) {
        (RingKind::Tools, _) => (tr(lang, "tools-title").to_string(), size.to_string()),
        (RingKind::Files, _) => (tr(lang, "files-title").to_string(), size.to_string()),
        (RingKind::Mine, _) => (tr(lang, "mine-title").to_string(), size.to_string()),
        (_, SourceKind::Image(f)) => (f.label().to_string(), size.to_string()),
        (_, SourceKind::Pdf) => (size.to_string(), "PDF".to_string()),
        (_, SourceKind::Docx) => (size.to_string(), "DOCX".to_string()),
        (_, SourceKind::Text) => (size.to_string(), "TXT".to_string()),
        (_, SourceKind::Sheet) => (size.to_string(), "XLSX".to_string()),
        (_, SourceKind::Csv) => (size.to_string(), "CSV".to_string()),
        (_, SourceKind::Archive) => (size.to_string(), "ZIP".to_string()),
        (_, SourceKind::Folder) => (size.to_string(), tr(lang, "kind-folder").to_uppercase()),
        (_, SourceKind::Other) => (size.to_string(), "FILE".to_string()),
    };
    Texts { main, sub, caption, accent: false }
}

/// Hub texts while sector `i` is hovered; a greyed-out sector explains itself in the pill.
pub fn hover(menu: &Menu, i: usize, switch: Switch, lang: Lang) -> Texts {
    let Some(item) = menu.items.get(i) else {
        return Texts {
            main: String::new(),
            sub: String::new(),
            caption: switch_hint(menu, switch, lang),
            accent: false,
        };
    };
    let main = item_label(item, lang);
    if !item.enabled {
        let why = item.reason.as_ref().map(|r| reason_text(r, lang)).unwrap_or_default();
        return Texts { main, sub: tr(lang, "unavailable").to_string(), caption: why, accent: false };
    }
    let sub = match item.op {
        Op::Convert(_) => tr(lang, "convert-to").to_string(),
        Op::Action(a) => tr(lang, a.hint_key()).to_string(),
        Op::Custom(_) => item.custom.as_ref().map(|c| c.hint.clone()).unwrap_or_default(),
    };
    Texts { main, sub, caption: switch_hint(menu, switch, lang), accent: true }
}

/// A fully described scene for `menu`, hub showing the idle texts.
pub fn scene(kind: SourceKind, menu: &Menu, size: &str, switch: Switch, lang: Lang) -> Scene {
    let t = idle(kind, menu, size, switch, lang);
    let mut sc = Scene::new(slots(menu, lang), t.main, t.sub);
    sc.caption = t.caption;
    sc.pager = menu.switchable().then(|| menu.position());
    sc
}

/// Copies hover texts into a scene.
pub fn apply(sc: &mut Scene, t: Texts) {
    sc.main = t.main;
    sc.sub = t.sub;
    sc.caption = t.caption;
    sc.main_accent = t.accent;
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::convert::Fmt;
    use crate::engine::{Availability, Target};
    use crate::ring::{build, ActionKind, Env};

    fn env() -> Env<'static> {
        Env {
            target: &|_, t| match t {
                Target::Image(Fmt::Avif) => Availability::Unavailable("AVIF needs an AV1 encoder".into()),
                _ => Availability::Available,
            },
            source: &|_| Availability::Available,
            custom: &[],
        }
    }

    fn png_menu(ring: RingKind) -> Menu {
        build(&[SourceKind::Image(Fmt::Png)], ring, 1, &env())
    }

    #[test]
    fn labels_are_translated_for_actions_only() {
        assert_eq!(label(Op::Convert(Target::Pdf), Lang::Es), "PDF");
        assert_eq!(label(Op::Action(ActionKind::Crop), Lang::En), "Crop");
        assert_eq!(label(Op::Action(ActionKind::Crop), Lang::Es), "Recortar");
    }

    #[test]
    fn icons_follow_the_english_label_in_every_language() {
        let m = png_menu(RingKind::Tools);
        for lang in [Lang::En, Lang::Es] {
            let s = slots(&m, lang);
            assert_eq!(s[4].icon, Icon::Crop, "{lang:?}");
            assert_eq!(s[2].icon, Icon::Compress);
            assert_eq!((s[0].icon, s[1].icon), (Icon::CopyText, Icon::Resize));
            assert!(s.iter().all(|x| x.icon != Icon::Folder));
        }
    }

    #[test]
    fn idle_hub_shows_format_and_size_for_images_and_size_for_documents() {
        let m = png_menu(RingKind::Images);
        let t = idle(SourceKind::Image(Fmt::Png), &m, "1.1 MB", Switch::Both, Lang::En);
        assert_eq!((t.main.as_str(), t.sub.as_str()), ("PNG", "1.1 MB"));
        assert_eq!(t.caption, "Wheel, Tab or hub: tools");
        let pdf = build(&[SourceKind::Pdf], RingKind::Documents, 1, &env());
        let t = idle(SourceKind::Pdf, &pdf, "1.1 MB", Switch::Both, Lang::En);
        assert_eq!(
            (t.main.as_str(), t.sub.as_str(), t.caption.as_str()),
            ("1.1 MB", "PDF", "Wheel, Tab or hub: images")
        );
        let other = build(&[SourceKind::Other], RingKind::Documents, 1, &env());
        let t = idle(SourceKind::Other, &other, "3 KB", Switch::Both, Lang::En);
        assert_eq!((t.main.as_str(), t.sub.as_str()), ("Files", "3 KB"), "an unknown file has the files ring only");
        assert_eq!(t.caption, "", "one ring: nothing to switch to");
    }

    #[test]
    fn hovering_a_working_sector_names_it_and_a_greyed_one_explains_itself() {
        let m = png_menu(RingKind::Images);
        let webp = hover(&m, 1, Switch::Both, Lang::En);
        assert_eq!((webp.main.as_str(), webp.sub.as_str(), webp.accent), ("WEBP", "Convert to", true));
        let avif = hover(&m, 4, Switch::Both, Lang::En);
        assert_eq!((avif.main.as_str(), avif.sub.as_str(), avif.accent), ("AVIF", "Unavailable", false));
        assert_eq!(avif.caption, "AVIF needs an AV1 encoder");
    }

    #[test]
    fn action_hover_describes_the_action_in_the_active_language() {
        let m = png_menu(RingKind::Tools);
        assert_eq!(hover(&m, 0, Switch::Both, Lang::En).sub, "Copy the text");
        assert_eq!(hover(&m, 6, Switch::Both, Lang::En).sub, "Draw on it");
        assert_eq!(hover(&m, 6, Switch::Both, Lang::Es).main, "Anotar");
        assert_eq!(hover(&m, 1, Switch::Both, Lang::Es).main, "Achicar");
    }

    #[test]
    fn the_switch_hint_names_the_gesture_and_the_destination() {
        let images = png_menu(RingKind::Images);
        let tools = png_menu(RingKind::Tools);
        assert_eq!(switch_hint(&images, Switch::Tab, Lang::En), "Wheel or Tab: tools");
        assert_eq!(switch_hint(&tools, Switch::Hub, Lang::En), "Wheel or hub: files");
        assert_eq!(switch_hint(&images, Switch::Both, Lang::Es), "Rueda, Tab o centro: herramientas");
    }

    #[test]
    fn dialog_actions_on_many_files_say_why_in_the_pill() {
        let m = build(&[SourceKind::Image(Fmt::Png); 2], RingKind::Tools, 2, &env());
        assert_eq!(hover(&m, 4, Switch::Both, Lang::En).caption, "Drop a single image to use this");
        let bmp = build(&[SourceKind::Image(Fmt::Bmp)], RingKind::Tools, 1, &env());
        assert_eq!(hover(&bmp, 2, Switch::Both, Lang::En).caption, "Works on JPG, PNG, WEBP only");
    }

    #[test]
    fn scenes_carry_slots_pager_and_caption() {
        let m = png_menu(RingKind::Tools);
        let sc = scene(SourceKind::Image(Fmt::Png), &m, "2 KB", Switch::Both, Lang::En);
        assert_eq!(sc.slots.len(), 8);
        assert_eq!(sc.pager, Some((2, 4)));
        assert_eq!((sc.main.as_str(), sc.sub.as_str()), ("Tools", "2 KB"));
        let doc = build(&[SourceKind::Docx], RingKind::Documents, 1, &env());
        assert_eq!(
            scene(SourceKind::Docx, &doc, "9 KB", Switch::Both, Lang::En).pager,
            Some((0, 3)),
            "Word has documents, images and files"
        );
        let other = build(&[SourceKind::Other], RingKind::Files, 1, &env());
        assert_eq!(scene(SourceKind::Other, &other, "1 KB", Switch::Both, Lang::En).pager, None);
    }
}
