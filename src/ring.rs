//! Which sectors a menu shows for a dragged file, and which of them can be used right now.
//!
//! Pure logic: the engine answers (`detect`, `targets_for`, availability) are injected through [`Env`], so the
//! rules are unit tested without touching Windows codecs. The overlay turns a [`Menu`] into a scene.

use crate::convert::Fmt;
use crate::engine::{Availability, SourceKind, Target};

/// The three rings every dragged file gets: document conversions (menu 1), image formats (menu 2) and the seven
/// actions (menu 3). The wheel, Tab and the hub walk through them in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingKind {
    Documents,
    Images,
    Actions,
}

impl RingKind {
    pub const ALL: [RingKind; 3] = [RingKind::Documents, RingKind::Images, RingKind::Actions];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }

    /// The ring `delta` steps away, wrapping around.
    pub fn step(self, delta: i32) -> RingKind {
        Self::ALL[(self.index() as i32 + delta).rem_euclid(Self::ALL.len() as i32) as usize]
    }

    /// The ring a file opens on when the user has no preference: images start on the formats, the rest on documents.
    pub fn home(kind: SourceKind) -> RingKind {
        match kind {
            SourceKind::Image(_) => RingKind::Images,
            _ => RingKind::Documents,
        }
    }

    /// Key of the ring's name (`config::STRINGS`).
    pub fn name_key(self) -> &'static str {
        match self {
            RingKind::Documents => "ring-documents",
            RingKind::Images => "ring-images",
            RingKind::Actions => "ring-name-actions",
        }
    }
}

/// The actions of menu 3, in clockwise order from the top.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Compress,
    Metadata,
    Edit,
    Annotate,
    AddBg,
    Crop,
    Redact,
}

impl ActionKind {
    pub const ALL: [ActionKind; 7] = [
        ActionKind::Compress,
        ActionKind::Metadata,
        ActionKind::Edit,
        ActionKind::Annotate,
        ActionKind::AddBg,
        ActionKind::Crop,
        ActionKind::Redact,
    ];

    /// Runs on drop with no window.
    pub fn is_immediate(self) -> bool {
        matches!(self, ActionKind::Compress | ActionKind::Metadata)
    }

    /// Parses the command line / environment spelling of an action (`crop`, `addbg`, ...).
    pub fn from_name(s: &str) -> Option<ActionKind> {
        Some(match s.to_ascii_lowercase().as_str() {
            "compress" => ActionKind::Compress,
            "metadata" => ActionKind::Metadata,
            "edit" => ActionKind::Edit,
            "annotate" => ActionKind::Annotate,
            "addbg" | "bg" => ActionKind::AddBg,
            "crop" => ActionKind::Crop,
            "redact" => ActionKind::Redact,
            _ => return None,
        })
    }

    /// Key of the localized sector label (see `config::STRINGS`).
    pub fn label_key(self) -> &'static str {
        match self {
            ActionKind::Compress => "act-compress",
            ActionKind::Metadata => "act-metadata",
            ActionKind::Edit => "act-edit",
            ActionKind::Annotate => "act-annotate",
            ActionKind::AddBg => "act-addbg",
            ActionKind::Crop => "act-crop",
            ActionKind::Redact => "act-redact",
        }
    }

    /// Key of the one-line description shown in the hub while the sector is hovered.
    pub fn hint_key(self) -> &'static str {
        match self {
            ActionKind::Compress => "act-compress-hint",
            ActionKind::Metadata => "act-metadata-hint",
            ActionKind::Edit => "act-edit-hint",
            ActionKind::Annotate => "act-annotate-hint",
            ActionKind::AddBg => "act-addbg-hint",
            ActionKind::Crop => "act-crop-hint",
            ActionKind::Redact => "act-redact-hint",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Convert(Target),
    Action(ActionKind),
}

/// Why a sector is greyed out. Shown in the caption under the ring, never hidden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The engine says this machine cannot do it (missing Windows codec): its own explanation.
    Engine(String),
    /// Dialog actions work on one file at a time.
    OneFile,
    /// The action only handles some formats; the list is shown.
    Formats(&'static str),
    /// There is no conversion from this file to that format.
    NoPath,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub op: Op,
    pub enabled: bool,
    pub reason: Option<Reason>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Menu {
    pub kind: RingKind,
    pub items: Vec<Item>,
}

/// What the engine can do on this machine; injected so the rules can be tested.
pub struct Env<'a> {
    pub target: &'a dyn Fn(Target) -> Availability,
    pub source: &'a dyn Fn(SourceKind) -> Availability,
}

/// Sectors of the document ring (menu 1), clockwise from the top.
const DOCUMENT_RING: [Target; 4] = [Target::Docx, Target::Image(Fmt::Jpg), Target::Image(Fmt::Png), Target::Txt];

/// Sectors of the image ring (menu 2).
const IMAGE_RING: [Target; 8] = [
    Target::Image(Fmt::Png),
    Target::Image(Fmt::Webp),
    Target::Image(Fmt::Heic),
    Target::Image(Fmt::Tiff),
    Target::Image(Fmt::Avif),
    Target::Image(Fmt::Bmp),
    Target::Pdf,
    Target::Docx,
];

/// The format a source already is, as a conversion target (so its own sector can be swapped for something useful).
fn own_target(kind: SourceKind) -> Option<Target> {
    match kind {
        SourceKind::Image(f) => Some(Target::Image(f)),
        SourceKind::Pdf => Some(Target::Pdf),
        SourceKind::Docx => Some(Target::Docx),
        SourceKind::Text => Some(Target::Txt),
        SourceKind::Other => None,
    }
}

/// Sectors of a conversion ring for `kind`. Converting a file to its own format is pointless, so that sector shows
/// the ring's stand-in instead: PDF in the document ring, JPG in the image ring.
fn ring_targets(ring: RingKind, kind: SourceKind) -> Vec<Target> {
    let (base, stand_in): (&[Target], Target) = match ring {
        RingKind::Documents => (&DOCUMENT_RING, Target::Pdf),
        _ => (&IMAGE_RING, Target::Image(Fmt::Jpg)),
    };
    let own = own_target(kind);
    base.iter().map(|&t| if Some(t) == own { stand_in } else { t }).collect()
}

/// The menu to show for `ring` while dragging `files` files of `kind`. Every file gets all three rings; a sector the
/// engine cannot do for this file is greyed out with the reason, never hidden.
pub fn build(kind: SourceKind, ring: RingKind, files: usize, env: &Env) -> Menu {
    let src = availability((env.source)(kind));
    let image = match kind {
        SourceKind::Image(f) => Some(f),
        _ => None,
    };
    if ring != RingKind::Actions {
        let items = ring_targets(ring, kind)
            .into_iter()
            .map(|t| {
                let check = src
                    .clone()
                    .and_then(|_| if crate::engine::supports(kind, t) { Ok(()) } else { Err(Reason::NoPath) })
                    .and_then(|_| availability((env.target)(t)));
                item(Op::Convert(t), check)
            })
            .collect();
        return Menu { kind: ring, items };
    }
    let items = ActionKind::ALL
        .into_iter()
        .map(|a| {
            let check = src.clone().and_then(|_| match (a, image) {
                (_, None) => Err(Reason::Formats("images")),
                (ActionKind::Compress, Some(f)) if !compress_ok(f) => Err(Reason::Formats("JPG, PNG, WEBP")),
                (ActionKind::Metadata, Some(f)) if !metadata_ok(f) => Err(Reason::Formats("JPG, PNG, WEBP, TIFF")),
                _ if !a.is_immediate() && files != 1 => Err(Reason::OneFile),
                _ => Ok(()),
            });
            item(Op::Action(a), check)
        })
        .collect();
    Menu { kind: RingKind::Actions, items }
}

fn availability(a: Availability) -> Result<(), Reason> {
    match a {
        Availability::Available => Ok(()),
        Availability::Unavailable(why) => Err(Reason::Engine(why)),
    }
}

fn item(op: Op, check: Result<(), Reason>) -> Item {
    Item { op, enabled: check.is_ok(), reason: check.err() }
}

/// Formats COMPRESS can recompress, and the ones the metadata tools understand.
fn compress_ok(f: Fmt) -> bool {
    matches!(f, Fmt::Jpg | Fmt::Png | Fmt::Webp)
}

fn metadata_ok(f: Fmt) -> bool {
    matches!(f, Fmt::Jpg | Fmt::Png | Fmt::Webp | Fmt::Tiff)
}

/// Which gestures besides the mouse wheel (always on) walk through the rings while dragging.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Switch {
    /// Tab key or resting on the hub.
    Both,
    Tab,
    Hub,
}

impl Switch {
    pub fn tab(self) -> bool {
        matches!(self, Switch::Both | Switch::Tab)
    }

    pub fn hub(self) -> bool {
        matches!(self, Switch::Both | Switch::Hub)
    }
}

/// Resting on the hub for this long moves to the next ring.
pub const DWELL_MS: u32 = 520;

/// Hub dwell state machine: one flip per visit. The menu opens with the pointer on the hub, so a dwell only counts
/// once the pointer has been off the hub at least once; leaving the hub re-arms it.
#[derive(Clone, Copy, Debug, Default)]
pub struct Dwell {
    since: Option<u32>,
    fired: bool,
    armed: bool,
}

impl Dwell {
    /// True while the pointer rests on the hub and the flip has not fired yet.
    pub fn pending(&self) -> bool {
        self.since.is_some() && !self.fired
    }

    /// Feeds the pointer state at time `now_ms`; returns the progress `0..=1` and whether the ring should flip now.
    pub fn update(&mut self, on_hub: bool, now_ms: u32) -> (f32, bool) {
        if !on_hub {
            *self = Dwell { armed: true, ..Dwell::default() };
            return (0.0, false);
        }
        if !self.armed {
            return (0.0, false);
        }
        let start = *self.since.get_or_insert(now_ms);
        if self.fired {
            return (0.0, false);
        }
        let p = (now_ms.saturating_sub(start) as f32 / DWELL_MS as f32).min(1.0);
        if p >= 1.0 {
            self.fired = true;
            return (1.0, true);
        }
        (p, false)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn all_ok() -> Env<'static> {
        Env { target: &|_| Availability::Available, source: &|_| Availability::Available }
    }

    fn labels(m: &Menu) -> Vec<&'static str> {
        m.items
            .iter()
            .map(|i| match i.op {
                Op::Convert(t) => t.label(),
                Op::Action(a) => a.label_key(),
            })
            .collect()
    }

    fn enabled(m: &Menu) -> Vec<&'static str> {
        m.items
            .iter()
            .filter(|i| i.enabled)
            .map(|i| match i.op {
                Op::Convert(t) => t.label(),
                Op::Action(a) => a.label_key(),
            })
            .collect()
    }

    #[test]
    fn a_png_gets_eight_image_sectors_with_jpg_in_place_of_png() {
        let m = build(SourceKind::Image(Fmt::Png), RingKind::Images, 1, &all_ok());
        assert_eq!(labels(&m), ["JPG", "WEBP", "HEIC", "TIFF", "AVIF", "BMP", "PDF", "DOCX"]);
        assert!(m.items.iter().all(|i| i.enabled && i.reason.is_none()));
        assert_eq!(m.kind, RingKind::Images);
    }

    #[test]
    fn a_pdf_gets_the_four_sector_document_ring() {
        let m = build(SourceKind::Pdf, RingKind::Documents, 1, &all_ok());
        assert_eq!(labels(&m), ["DOCX", "JPG", "PNG", "TXT"]);
        assert!(m.items.iter().all(|i| i.enabled));
        assert_eq!(m.kind, RingKind::Documents);
    }

    #[test]
    fn every_file_type_gets_all_three_rings_and_a_greyed_sector_always_says_why() {
        let kinds =
            [SourceKind::Image(Fmt::Png), SourceKind::Pdf, SourceKind::Docx, SourceKind::Text, SourceKind::Other];
        for kind in kinds {
            for ring in RingKind::ALL {
                let m = build(kind, ring, 1, &all_ok());
                assert_eq!(m.kind, ring, "{kind:?}");
                assert_eq!(m.items.len(), [4, 8, 7][ring.index()], "{kind:?} {ring:?}");
                for i in &m.items {
                    assert_eq!(i.enabled, i.reason.is_none(), "{kind:?} {ring:?}");
                }
            }
        }
    }

    #[test]
    fn documents_reach_the_image_ring_and_images_reach_the_document_ring() {
        let pdf = build(SourceKind::Pdf, RingKind::Images, 1, &all_ok());
        assert_eq!(labels(&pdf), ["PNG", "WEBP", "HEIC", "TIFF", "AVIF", "BMP", "JPG", "DOCX"]);
        assert_eq!(enabled(&pdf), ["PNG", "WEBP", "HEIC", "TIFF", "AVIF", "BMP", "JPG", "DOCX"]);
        let png = build(SourceKind::Image(Fmt::Png), RingKind::Documents, 1, &all_ok());
        assert_eq!(labels(&png), ["DOCX", "JPG", "PDF", "TXT"], "PNG's own sector becomes PDF");
        assert_eq!(enabled(&png), ["DOCX", "JPG", "PDF"], "no OCR to text from a picture");
        assert_eq!(png.items[3].reason, Some(Reason::NoPath));
    }

    #[test]
    fn docx_and_text_keep_what_they_could_always_do_and_grey_the_rest() {
        let docx = build(SourceKind::Docx, RingKind::Documents, 1, &all_ok());
        assert_eq!(labels(&docx), ["PDF", "JPG", "PNG", "TXT"]);
        assert_eq!(enabled(&docx), ["PDF", "TXT"]);
        let text = build(SourceKind::Text, RingKind::Documents, 1, &all_ok());
        assert_eq!(labels(&text), ["DOCX", "JPG", "PNG", "PDF"]);
        assert_eq!(enabled(&text), ["DOCX", "PDF"]);
    }

    #[test]
    fn an_unknown_file_still_opens_all_rings_with_every_sector_greyed() {
        for ring in RingKind::ALL {
            let m = build(SourceKind::Other, ring, 1, &all_ok());
            assert!(m.items.iter().all(|i| !i.enabled && i.reason.is_some()), "{ring:?}");
        }
        let actions = build(SourceKind::Other, RingKind::Actions, 1, &all_ok());
        assert_eq!(actions.items[0].reason, Some(Reason::Formats("images")));
    }

    #[test]
    fn actions_on_a_document_are_greyed_because_they_work_on_images_only() {
        let m = build(SourceKind::Pdf, RingKind::Actions, 1, &all_ok());
        assert_eq!(m.items.len(), 7);
        assert!(m.items.iter().all(|i| !i.enabled && i.reason == Some(Reason::Formats("images"))));
    }

    #[test]
    fn unavailable_targets_are_disabled_with_the_engines_reason() {
        let env = Env {
            target: &|t| match t {
                Target::Image(Fmt::Avif) => Availability::Unavailable("no AV1 encoder".into()),
                _ => Availability::Available,
            },
            source: &|_| Availability::Available,
        };
        let m = build(SourceKind::Image(Fmt::Jpg), RingKind::Images, 1, &env);
        let avif = m.items.iter().find(|i| i.op == Op::Convert(Target::Image(Fmt::Avif))).unwrap();
        assert!(!avif.enabled);
        assert_eq!(avif.reason, Some(Reason::Engine("no AV1 encoder".into())));
        assert_eq!(m.items.iter().filter(|i| i.enabled).count(), 7);
    }

    #[test]
    fn an_unreadable_source_disables_the_whole_ring() {
        let env = Env {
            target: &|_| Availability::Available,
            source: &|_| Availability::Unavailable("HEIC decoder missing".into()),
        };
        for ring in RingKind::ALL {
            let m = build(SourceKind::Image(Fmt::Heic), ring, 1, &env);
            assert!(!m.items.is_empty() && m.items.iter().all(|i| !i.enabled && i.reason.is_some()), "{ring:?}");
        }
    }

    #[test]
    fn the_actions_ring_has_the_seven_actions_in_order() {
        let m = build(SourceKind::Image(Fmt::Png), RingKind::Actions, 1, &all_ok());
        assert_eq!(m.kind, RingKind::Actions);
        let order: Vec<ActionKind> = m
            .items
            .iter()
            .map(|i| match i.op {
                Op::Action(a) => a,
                Op::Convert(_) => panic!("conversion in the actions ring"),
            })
            .collect();
        assert_eq!(order, ActionKind::ALL);
        assert!(m.items.iter().all(|i| i.enabled));
    }

    #[test]
    fn compress_and_metadata_only_offer_formats_the_engine_handles() {
        let m = build(SourceKind::Image(Fmt::Bmp), RingKind::Actions, 1, &all_ok());
        let by = |a: ActionKind| m.items.iter().find(|i| i.op == Op::Action(a)).unwrap();
        assert!(!by(ActionKind::Compress).enabled);
        assert_eq!(by(ActionKind::Compress).reason, Some(Reason::Formats("JPG, PNG, WEBP")));
        assert!(!by(ActionKind::Metadata).enabled);
        assert!(by(ActionKind::Crop).enabled);
        let tiff = build(SourceKind::Image(Fmt::Tiff), RingKind::Actions, 1, &all_ok());
        assert!(tiff.items[1].enabled && !tiff.items[0].enabled, "TIFF has metadata but cannot be compressed");
    }

    #[test]
    fn dialog_actions_need_exactly_one_file_but_immediate_ones_batch() {
        let m = build(SourceKind::Image(Fmt::Jpg), RingKind::Actions, 3, &all_ok());
        assert_eq!(m.items.len(), 7);
        for i in &m.items {
            let Op::Action(a) = i.op else { unreachable!() };
            assert_eq!(i.enabled, a.is_immediate(), "{a:?}");
            if !i.enabled {
                assert_eq!(i.reason, Some(Reason::OneFile));
            }
        }
    }

    #[test]
    fn action_names_parse_for_the_command_line_and_cover_every_action() {
        assert_eq!(ActionKind::from_name("CROP"), Some(ActionKind::Crop));
        assert_eq!(ActionKind::from_name("bg"), Some(ActionKind::AddBg));
        assert_eq!(ActionKind::from_name("nope"), None);
        for (name, a) in [
            ("compress", ActionKind::Compress),
            ("metadata", ActionKind::Metadata),
            ("edit", ActionKind::Edit),
            ("annotate", ActionKind::Annotate),
            ("addbg", ActionKind::AddBg),
            ("redact", ActionKind::Redact),
        ] {
            assert_eq!(ActionKind::from_name(name), Some(a));
        }
    }

    #[test]
    fn rings_step_forward_and_back_and_wrap_around() {
        assert_eq!(RingKind::Documents.step(1), RingKind::Images);
        assert_eq!(RingKind::Actions.step(1), RingKind::Documents);
        assert_eq!(RingKind::Documents.step(-1), RingKind::Actions);
        assert_eq!(RingKind::Images.step(-1), RingKind::Documents);
        assert_eq!(RingKind::Images.step(3), RingKind::Images);
    }

    #[test]
    fn images_open_on_the_formats_and_everything_else_on_documents() {
        assert_eq!(RingKind::home(SourceKind::Image(Fmt::Heic)), RingKind::Images);
        for k in [SourceKind::Pdf, SourceKind::Docx, SourceKind::Text, SourceKind::Other] {
            assert_eq!(RingKind::home(k), RingKind::Documents);
        }
    }

    #[test]
    fn switch_modes_choose_the_gestures() {
        assert!(Switch::Both.tab() && Switch::Both.hub());
        assert!(Switch::Tab.tab() && !Switch::Tab.hub());
        assert!(!Switch::Hub.tab() && Switch::Hub.hub());
    }

    #[test]
    fn resting_on_the_hub_flips_once_and_rearms_after_leaving() {
        let mut d = Dwell::default();
        assert_eq!(d.update(false, 0), (0.0, false), "off the hub: armed");
        assert_eq!(d.update(true, 100), (0.0, false));
        let (p, flip) = d.update(true, 100 + DWELL_MS / 2);
        assert!((p - 0.5).abs() < 0.01 && !flip);
        assert_eq!(d.update(true, 100 + DWELL_MS), (1.0, true));
        assert_eq!(d.update(true, 100 + DWELL_MS * 3), (0.0, false), "one flip per visit");
        assert_eq!(d.update(false, 5000), (0.0, false));
        assert_eq!(d.update(true, 5100), (0.0, false));
        assert!(d.update(true, 5100 + DWELL_MS).1, "re-armed");
    }

    #[test]
    fn a_menu_that_opens_under_a_resting_pointer_does_not_flip_until_the_pointer_has_left_the_hub() {
        let mut d = Dwell::default();
        for t in [0, 100, DWELL_MS, DWELL_MS * 4] {
            assert_eq!(d.update(true, t), (0.0, false), "disarmed at t={t}");
            assert!(!d.pending());
        }
        d.update(false, DWELL_MS * 5);
        d.update(true, DWELL_MS * 5 + 10);
        assert!(d.update(true, DWELL_MS * 6 + 10).1, "armed after leaving");
    }

    #[test]
    fn pending_is_true_only_while_waiting_on_the_hub() {
        let mut d = Dwell::default();
        assert!(!d.pending());
        d.update(false, 0);
        d.update(true, 0);
        assert!(d.pending());
        d.update(true, DWELL_MS);
        assert!(!d.pending(), "fired");
        d.update(false, DWELL_MS + 1);
        assert!(!d.pending());
    }

    #[test]
    fn a_short_pass_over_the_hub_does_not_flip() {
        let mut d = Dwell::default();
        d.update(false, 0);
        d.update(true, 0);
        assert!(!d.update(true, DWELL_MS - 1).1);
        d.update(false, DWELL_MS);
        assert!(!d.update(true, DWELL_MS + 10).1);
    }
}
