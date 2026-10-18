//! Which sectors a menu shows for a dragged file, and which of them can be used right now.
//!
//! Pure logic: the engine answers (`detect`, `targets_for`, availability) are injected through [`Env`], so the
//! rules are unit tested without touching Windows codecs. The overlay turns a [`Menu`] into a scene.

use crate::convert::Fmt;
use crate::engine::{Availability, SourceKind, Target};

/// The two rings of an image: conversions and the seven actions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingKind {
    Convert,
    Actions,
}

impl RingKind {
    pub fn other(self) -> RingKind {
        match self {
            RingKind::Convert => RingKind::Actions,
            RingKind::Actions => RingKind::Convert,
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
    /// True when the file also has the other ring (images have both).
    pub switchable: bool,
}

/// What the engine can do on this machine; injected so the rules can be tested.
pub struct Env<'a> {
    pub target: &'a dyn Fn(Target) -> Availability,
    pub source: &'a dyn Fn(SourceKind) -> Availability,
}

/// The menu to show for `kind` while dragging `files` files. `ring` is the wanted ring; files that are not images
/// only have the conversion ring.
pub fn build(kind: SourceKind, ring: RingKind, files: usize, env: &Env) -> Menu {
    let src = availability((env.source)(kind));
    let image = match kind {
        SourceKind::Image(f) => Some(f),
        _ => None,
    };
    let Some(fmt) = image.filter(|_| ring == RingKind::Actions) else {
        let items = crate::engine::targets_for(kind)
            .into_iter()
            .map(|t| item(Op::Convert(t), src.clone().and_then(|_| availability((env.target)(t)))))
            .collect();
        return Menu { kind: RingKind::Convert, items, switchable: image.is_some() };
    };
    let items = ActionKind::ALL
        .into_iter()
        .map(|a| {
            let check = src.clone().and_then(|_| match a {
                ActionKind::Compress if !compress_ok(fmt) => Err(Reason::Formats("JPG, PNG, WEBP")),
                ActionKind::Metadata if !metadata_ok(fmt) => Err(Reason::Formats("JPG, PNG, WEBP, TIFF")),
                _ if !a.is_immediate() && files != 1 => Err(Reason::OneFile),
                _ => Ok(()),
            });
            item(Op::Action(a), check)
        })
        .collect();
    Menu { kind: RingKind::Actions, items, switchable: true }
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

/// How the user may switch between the two rings while dragging.
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

/// Resting on the hub for this long flips the ring.
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

    #[test]
    fn a_png_gets_eight_conversion_sectors_with_jpg_in_place_of_png() {
        let m = build(SourceKind::Image(Fmt::Png), RingKind::Convert, 1, &all_ok());
        assert_eq!(labels(&m), ["JPG", "WEBP", "HEIC", "TIFF", "AVIF", "BMP", "PDF", "DOCX"]);
        assert!(m.items.iter().all(|i| i.enabled && i.reason.is_none()));
        assert!(m.switchable);
        assert_eq!(m.kind, RingKind::Convert);
    }

    #[test]
    fn a_pdf_gets_the_four_sector_menu_and_no_second_ring() {
        let m = build(SourceKind::Pdf, RingKind::Actions, 1, &all_ok());
        assert_eq!(labels(&m), ["DOCX", "JPG", "PNG", "TXT"]);
        assert_eq!(m.kind, RingKind::Convert, "documents have no actions ring");
        assert!(!m.switchable);
    }

    #[test]
    fn docx_and_text_have_their_own_rings() {
        assert_eq!(labels(&build(SourceKind::Docx, RingKind::Convert, 1, &all_ok())), ["PDF", "TXT"]);
        assert_eq!(labels(&build(SourceKind::Text, RingKind::Convert, 1, &all_ok())), ["PDF", "DOCX"]);
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
        let m = build(SourceKind::Image(Fmt::Jpg), RingKind::Convert, 1, &env);
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
        for ring in [RingKind::Convert, RingKind::Actions] {
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
    fn ring_kinds_flip() {
        assert_eq!(RingKind::Convert.other(), RingKind::Actions);
        assert_eq!(RingKind::Actions.other().other(), RingKind::Actions);
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
