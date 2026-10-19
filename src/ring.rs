//! Which sectors a menu shows for the dragged files, and which of them can be used right now.
//!
//! Pure logic: the engine answers (`supports`, availability) are injected through [`Env`], so the rules are unit
//! tested without touching Windows codecs. The overlay turns a [`Menu`] into a scene.
//!
//! Every file gets the rings that apply to it: documents (menu 1) and images (menu 2) convert it, tools (menu 3)
//! hold what only this kind of file has (a picture's editing tools, a PDF's split), files (menu 4) hold what works
//! on any file, in the same order every time, and "mine" (menu 5) holds the user's own sectors when they have any. A ring is offered only when something in it can work for the files
//! being dragged, so there are never rings full of dead sectors; a sector is greyed out (with the reason) only when
//! this machine lacks something or just some of the files fit.

use crate::convert::Fmt;
use crate::engine::{supports, Availability, SourceKind, Target};

/// The rings. The wheel, Tab and the hub walk through the ones a file has, in this order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RingKind {
    Documents,
    Images,
    Tools,
    Files,
    /// The user's own sectors; only there when at least one applies to the dragged files.
    Mine,
}

impl RingKind {
    pub const ALL: [RingKind; 5] =
        [RingKind::Documents, RingKind::Images, RingKind::Tools, RingKind::Files, RingKind::Mine];

    pub fn index(self) -> usize {
        Self::ALL.iter().position(|r| *r == self).unwrap_or(0)
    }

    /// The ring `delta` steps away among all three, wrapping around.
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
            RingKind::Tools => "ring-name-tools",
            RingKind::Files => "ring-name-files",
            RingKind::Mine => "ring-name-mine",
        }
    }
}

/// What a sector of the tools and files rings does.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionKind {
    Compress,
    Metadata,
    Edit,
    Annotate,
    AddBg,
    Crop,
    /// Packs the files into one .zip.
    Zip,
    /// Extracts a .zip next to itself.
    Unzip,
    /// Shows the properties of the file.
    Info,
    /// SHA-256 of each file, copied to the clipboard.
    Checksum,
    /// The full path of each file, copied to the clipboard.
    CopyPath,
    /// Opens Explorer with the file selected.
    Reveal,
    /// Reads the text of a picture or PDF onto the clipboard.
    CopyText,
    /// Shrinks pictures to a preset size.
    Resize,
    /// Renames files by a rule.
    Rename,
    /// One PDF per page.
    Split,
    /// Several files into one PDF.
    Merge,
    /// Files of a folder into subfolders by type or month.
    Sort,
}

impl ActionKind {
    /// The picture tools of menu 3, in clockwise order from the top: the quick ones first, then the editors.
    pub const PICTURE_TOOLS: [ActionKind; 8] = [
        ActionKind::CopyText,
        ActionKind::Resize,
        ActionKind::Compress,
        ActionKind::Metadata,
        ActionKind::Crop,
        ActionKind::Edit,
        ActionKind::Annotate,
        ActionKind::AddBg,
    ];

    /// The PDF tools of menu 3.
    pub const PDF_TOOLS: [ActionKind; 2] = [ActionKind::CopyText, ActionKind::Split];

    /// What menu 4 offers for any file, always in this order.
    pub const FILE_TOOLS: [ActionKind; 6] = [
        ActionKind::Rename,
        ActionKind::Zip,
        ActionKind::Info,
        ActionKind::Checksum,
        ActionKind::CopyPath,
        ActionKind::Reveal,
    ];

    /// Every action, in a fixed order: a message can carry an action as its index here.
    pub const EVERY: [ActionKind; 18] = [
        ActionKind::Compress,
        ActionKind::Metadata,
        ActionKind::Edit,
        ActionKind::Annotate,
        ActionKind::AddBg,
        ActionKind::Crop,
        ActionKind::Zip,
        ActionKind::Unzip,
        ActionKind::Info,
        ActionKind::Checksum,
        ActionKind::CopyPath,
        ActionKind::Reveal,
        ActionKind::CopyText,
        ActionKind::Resize,
        ActionKind::Rename,
        ActionKind::Split,
        ActionKind::Merge,
        ActionKind::Sort,
    ];

    /// The index of this action in [`ActionKind::EVERY`].
    pub fn id(self) -> usize {
        Self::EVERY.iter().position(|a| *a == self).unwrap_or(0)
    }

    pub fn from_id(id: usize) -> Option<ActionKind> {
        Self::EVERY.get(id).copied()
    }

    /// Opens a window instead of running on drop.
    pub fn is_dialog(self) -> bool {
        matches!(
            self,
            ActionKind::Edit
                | ActionKind::Annotate
                | ActionKind::AddBg
                | ActionKind::Crop
                | ActionKind::Resize
                | ActionKind::Rename
        )
    }

    /// A window that works on exactly one picture (the others, resize and rename, take any number of files).
    pub fn needs_one_file(self) -> bool {
        matches!(self, ActionKind::Edit | ActionKind::Annotate | ActionKind::AddBg | ActionKind::Crop)
    }

    /// Runs on drop with no window.
    pub fn is_immediate(self) -> bool {
        !self.is_dialog()
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
            "zip" => ActionKind::Zip,
            "unzip" => ActionKind::Unzip,
            "info" => ActionKind::Info,
            "checksum" | "sha256" => ActionKind::Checksum,
            "copypath" | "path" => ActionKind::CopyPath,
            "reveal" => ActionKind::Reveal,
            "copytext" | "ocr" => ActionKind::CopyText,
            "resize" => ActionKind::Resize,
            "rename" => ActionKind::Rename,
            "split" => ActionKind::Split,
            "merge" => ActionKind::Merge,
            "sort" => ActionKind::Sort,
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
            ActionKind::Zip => "act-zip",
            ActionKind::Unzip => "act-unzip",
            ActionKind::Info => "act-info",
            ActionKind::Checksum => "act-checksum",
            ActionKind::CopyPath => "act-copypath",
            ActionKind::Reveal => "act-reveal",
            ActionKind::CopyText => "act-copytext",
            ActionKind::Resize => "act-resize",
            ActionKind::Rename => "act-rename",
            ActionKind::Split => "act-split",
            ActionKind::Merge => "act-merge",
            ActionKind::Sort => "act-sort",
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
            ActionKind::Zip => "act-zip-hint",
            ActionKind::Unzip => "act-unzip-hint",
            ActionKind::Info => "act-info-hint",
            ActionKind::Checksum => "act-checksum-hint",
            ActionKind::CopyPath => "act-copypath-hint",
            ActionKind::Reveal => "act-reveal-hint",
            ActionKind::CopyText => "act-copytext-hint",
            ActionKind::Resize => "act-resize-hint",
            ActionKind::Rename => "act-rename-hint",
            ActionKind::Split => "act-split-hint",
            ActionKind::Merge => "act-merge-hint",
            ActionKind::Sort => "act-sort-hint",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Op {
    Convert(Target),
    Action(ActionKind),
    /// One of the user's own sectors, by its index in `sectors::load()`.
    Custom(usize),
}

/// A user sector as the menu needs it: which one it is and how it reads.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Custom {
    /// Index in `sectors::load()`.
    pub index: usize,
    pub label: String,
    /// What it does, for the hub while hovered ("Move to Invoices").
    pub hint: String,
    pub kind: crate::sectors::Kind,
}

/// Why a sector is greyed out. Shown in the caption under the ring, never hidden.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Reason {
    /// The engine says this machine cannot do it (missing Windows codec or OCR language): its own explanation.
    Engine(String),
    /// Dialog actions work on one file at a time.
    OneFile,
    /// The action only handles some formats; the list is shown.
    Formats(&'static str),
    /// Some of the dragged files cannot be converted to that format.
    NoPath,
    /// The action reads file contents, and a folder was dragged.
    FilesOnly,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Item {
    pub op: Op,
    pub enabled: bool,
    pub reason: Option<Reason>,
    /// The user's own wording for a custom sector (label and hint); every other sector reads from its `op`.
    pub custom: Option<Custom>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Menu {
    pub kind: RingKind,
    pub items: Vec<Item>,
    /// The rings these files have, in wheel order (this one included).
    pub rings: Vec<RingKind>,
}

impl Menu {
    /// The ring `delta` steps along the rings these files have, wrapping around.
    pub fn neighbour(&self, delta: i32) -> RingKind {
        let at = self.rings.iter().position(|r| *r == self.kind).unwrap_or(0) as i32;
        let n = self.rings.len().max(1) as i32;
        self.rings.get((at + delta).rem_euclid(n) as usize).copied().unwrap_or(self.kind)
    }

    /// Where this ring sits among the rings of these files, and how many there are.
    pub fn position(&self) -> (usize, usize) {
        (self.rings.iter().position(|r| *r == self.kind).unwrap_or(0), self.rings.len())
    }

    /// True when there is another ring to go to.
    pub fn switchable(&self) -> bool {
        self.rings.len() > 1
    }
}

/// What the engine can do on this machine; injected so the rules can be tested.
pub struct Env<'a> {
    /// Can a file of this kind be turned into that format here (codecs, OCR language)?
    pub target: &'a dyn Fn(SourceKind, Target) -> Availability,
    /// Can this kind of file be read here?
    pub source: &'a dyn Fn(SourceKind) -> Availability,
    /// The user's sectors that apply to the dragged files.
    pub custom: &'a [Custom],
}

/// Sectors of the document ring (menu 1) for a PDF, clockwise from the top.
const DOCUMENT_BASE: [Target; 4] = [Target::Docx, Target::Image(Fmt::Jpg), Target::Image(Fmt::Png), Target::Txt];

/// Sectors of the image ring (menu 2) for a picture.
const IMAGE_BASE: [Target; 8] = [
    Target::Image(Fmt::Png),
    Target::Image(Fmt::Webp),
    Target::Image(Fmt::Heic),
    Target::Image(Fmt::Tiff),
    Target::Image(Fmt::Avif),
    Target::Image(Fmt::Bmp),
    Target::Pdf,
    Target::Docx,
];

/// The picture formats offered for anything that is not a picture.
const PICTURE_FORMATS: [Target; 7] = [
    Target::Image(Fmt::Png),
    Target::Image(Fmt::Jpg),
    Target::Image(Fmt::Webp),
    Target::Image(Fmt::Heic),
    Target::Image(Fmt::Tiff),
    Target::Image(Fmt::Avif),
    Target::Image(Fmt::Bmp),
];

/// The format a source already is, as a conversion target (so its own sector can be swapped for something useful).
fn own_target(kind: SourceKind) -> Option<Target> {
    match kind {
        SourceKind::Image(f) => Some(Target::Image(f)),
        SourceKind::Pdf => Some(Target::Pdf),
        SourceKind::Docx => Some(Target::Docx),
        SourceKind::Text => Some(Target::Txt),
        SourceKind::Csv => Some(Target::Csv),
        SourceKind::Sheet => Some(Target::Xlsx),
        SourceKind::Archive | SourceKind::Folder | SourceKind::Other => None,
    }
}

/// The sectors of a conversion ring, laid out for the first file's kind. Converting to the file's own format is
/// pointless, so that sector shows a stand-in: PDF in the document ring, JPG in the image ring.
fn layout(ring: RingKind, lead: SourceKind) -> Vec<Target> {
    let own = own_target(lead);
    if ring == RingKind::Images {
        if !matches!(lead, SourceKind::Image(_)) {
            return PICTURE_FORMATS.to_vec();
        }
        return IMAGE_BASE.iter().map(|&t| if Some(t) == own { Target::Image(Fmt::Jpg) } else { t }).collect();
    }
    let mut out: Vec<Target> = match lead {
        SourceKind::Sheet => vec![Target::Csv],
        SourceKind::Csv => vec![Target::Xlsx],
        _ => vec![],
    };
    if lead == SourceKind::Pdf {
        out.extend(DOCUMENT_BASE);
    } else if own.is_some_and(|o| DOCUMENT_BASE.contains(&o)) {
        out.extend(DOCUMENT_BASE.iter().map(|&t| if Some(t) == own { Target::Pdf } else { t }));
    } else {
        out.extend(DOCUMENT_BASE);
        out.push(Target::Pdf);
    }
    out
}

fn all_images(kinds: &[SourceKind]) -> bool {
    !kinds.is_empty() && kinds.iter().all(|k| matches!(k, SourceKind::Image(_)))
}

fn all_of(kinds: &[SourceKind], want: SourceKind) -> bool {
    !kinds.is_empty() && kinds.iter().all(|k| *k == want)
}

/// The operations a ring has for these files, before availability: only what at least one file can do.
fn ring_ops(ring: RingKind, kinds: &[SourceKind], custom: &[Custom]) -> Vec<Op> {
    let Some(&lead) = kinds.first() else { return vec![] };
    let act = |list: &[ActionKind]| list.iter().map(|&a| Op::Action(a)).collect::<Vec<_>>();
    match ring {
        RingKind::Tools if all_images(kinds) => act(&ActionKind::PICTURE_TOOLS),
        RingKind::Tools if all_of(kinds, SourceKind::Pdf) => act(&ActionKind::PDF_TOOLS),
        // A picture and a PDF together can still be read out as text.
        RingKind::Tools if kinds.iter().all(|k| matches!(k, SourceKind::Image(_) | SourceKind::Pdf)) => {
            act(&[ActionKind::CopyText])
        }
        RingKind::Tools => vec![],
        RingKind::Mine => custom.iter().map(|c| Op::Custom(c.index)).collect(),
        RingKind::Files => {
            let mut ops = Vec::new();
            if all_of(kinds, SourceKind::Archive) {
                ops.push(Op::Action(ActionKind::Unzip));
            }
            if all_of(kinds, SourceKind::Folder) {
                ops.push(Op::Action(ActionKind::Sort));
            }
            ops.extend(act(&ActionKind::FILE_TOOLS));
            ops
        }
        _ => {
            let mut ops: Vec<Op> = layout(ring, lead)
                .into_iter()
                .filter(|&t| kinds.iter().any(|&k| supports(k, t)))
                .map(Op::Convert)
                .collect();
            // Several files that can all be a PDF can be one PDF.
            let pages = |k: SourceKind| k == SourceKind::Pdf || supports(k, Target::Pdf);
            if ring == RingKind::Documents && kinds.len() > 1 && kinds.iter().all(|&k| pages(k)) {
                ops.push(Op::Action(ActionKind::Merge));
            }
            ops
        }
    }
}

/// The rings these files have, in wheel order, given how many of the user's sectors apply. The files ring is always
/// there, so there is always one.
pub fn rings_with(kinds: &[SourceKind], custom: &[Custom]) -> Vec<RingKind> {
    RingKind::ALL.into_iter().filter(|&r| !ring_ops(r, kinds, custom).is_empty() || r == RingKind::Files).collect()
}

/// `rings_with` for someone with no sectors of their own.
pub fn rings_for(kinds: &[SourceKind]) -> Vec<RingKind> {
    rings_with(kinds, &[])
}

/// The ring a drag opens on: the user's choice or the file's natural one, falling back to the first the files have.
pub fn first_ring(kinds: &[SourceKind], wanted: Option<RingKind>) -> RingKind {
    let rings = rings_for(kinds);
    let natural = kinds.first().map_or(RingKind::Documents, |&k| RingKind::home(k));
    [wanted, Some(natural)]
        .into_iter()
        .flatten()
        .find(|r| rings.contains(r))
        .or_else(|| rings.first().copied())
        .unwrap_or(RingKind::Files)
}

/// The menu to show for `ring` while dragging `files` files of these kinds (the first is the one the layout follows).
pub fn build(kinds: &[SourceKind], ring: RingKind, files: usize, env: &Env) -> Menu {
    let rings = rings_with(kinds, env.custom);
    let ring = if rings.contains(&ring) { ring } else { first_ring(kinds, None) };
    let lead = kinds.first().copied().unwrap_or(SourceKind::Other);
    let src = kinds.iter().try_fold((), |(), &k| availability((env.source)(k)));
    // Reading text out of a picture needs a Windows OCR language: asked once, and only when a sector needs it.
    let ocr = if ring == RingKind::Tools { (env.target)(lead, Target::Txt) } else { Availability::Available };
    let items = ring_ops(ring, kinds, env.custom)
        .into_iter()
        .map(|op| {
            let check = src.clone().and_then(|_| match op {
                Op::Convert(t) => {
                    if kinds.iter().all(|&k| supports(k, t)) {
                        availability((env.target)(lead, t))
                    } else {
                        Err(Reason::NoPath)
                    }
                }
                Op::Action(a) => action_check(a, kinds, files, &ocr),
                Op::Custom(_) => Ok(()),
            });
            let mut it = item(op, check);
            if let Op::Custom(i) = op {
                it.custom = env.custom.iter().find(|c| c.index == i).cloned();
            }
            it
        })
        .collect();
    Menu { kind: ring, items, rings }
}

/// Can action `a` work on these files?
fn action_check(a: ActionKind, kinds: &[SourceKind], files: usize, ocr: &Availability) -> Result<(), Reason> {
    let folder = kinds.contains(&SourceKind::Folder);
    let fmts: Vec<Fmt> = kinds
        .iter()
        .filter_map(|k| match k {
            SourceKind::Image(f) => Some(*f),
            _ => None,
        })
        .collect();
    match a {
        ActionKind::Compress if !fmts.iter().all(|&f| compress_ok(f)) => Err(Reason::Formats("JPG, PNG, WEBP")),
        ActionKind::Metadata if !fmts.iter().all(|&f| metadata_ok(f)) => Err(Reason::Formats("JPG, PNG, WEBP, TIFF")),
        ActionKind::CopyText if fmts.is_empty() => Ok(()),
        ActionKind::CopyText => availability(ocr.clone()),
        ActionKind::Info | ActionKind::Checksum if folder => Err(Reason::FilesOnly),
        _ if a.needs_one_file() && files != 1 => Err(Reason::OneFile),
        _ => Ok(()),
    }
}

fn availability(a: Availability) -> Result<(), Reason> {
    match a {
        Availability::Available => Ok(()),
        Availability::Unavailable(why) => Err(Reason::Engine(why)),
    }
}

fn item(op: Op, check: Result<(), Reason>) -> Item {
    Item { op, enabled: check.is_ok(), reason: check.err(), custom: None }
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
        Env { target: &|_, _| Availability::Available, source: &|_| Availability::Available, custom: &[] }
    }

    const PNG: SourceKind = SourceKind::Image(Fmt::Png);

    fn one(kind: SourceKind, ring: RingKind) -> Menu {
        build(&[kind], ring, 1, &all_ok())
    }

    fn name(op: Op) -> &'static str {
        match op {
            Op::Convert(t) => t.label(),
            Op::Action(a) => a.label_key(),
            Op::Custom(_) => "custom",
        }
    }

    fn labels(m: &Menu) -> Vec<&'static str> {
        m.items.iter().map(|i| name(i.op)).collect()
    }

    fn enabled(m: &Menu) -> Vec<&'static str> {
        m.items.iter().filter(|i| i.enabled).map(|i| name(i.op)).collect()
    }

    fn item(m: &Menu, a: ActionKind) -> &Item {
        m.items.iter().find(|i| i.op == Op::Action(a)).unwrap()
    }

    #[test]
    fn a_png_gets_eight_image_sectors_with_jpg_in_place_of_png() {
        let m = one(PNG, RingKind::Images);
        assert_eq!(labels(&m), ["JPG", "WEBP", "HEIC", "TIFF", "AVIF", "BMP", "PDF", "DOCX"]);
        assert!(m.items.iter().all(|i| i.enabled && i.reason.is_none()));
        assert_eq!(m.kind, RingKind::Images);
    }

    #[test]
    fn a_pdf_gets_the_four_sector_document_ring() {
        let m = one(SourceKind::Pdf, RingKind::Documents);
        assert_eq!(labels(&m), ["DOCX", "JPG", "PNG", "TXT"]);
        assert!(m.items.iter().all(|i| i.enabled));
    }

    #[test]
    fn a_picture_gets_documents_too_and_reads_its_text_with_ocr() {
        let m = one(PNG, RingKind::Documents);
        assert_eq!(labels(&m), ["DOCX", "JPG", "PDF", "TXT"], "PNG's own sector becomes PDF");
        assert!(m.items.iter().all(|i| i.enabled));
        let jpg = one(SourceKind::Image(Fmt::Jpg), RingKind::Documents);
        assert_eq!(labels(&jpg), ["DOCX", "PDF", "PNG", "TXT"]);
        let webp = one(SourceKind::Image(Fmt::Webp), RingKind::Documents);
        assert_eq!(labels(&webp), ["DOCX", "JPG", "PNG", "TXT", "PDF"], "no own sector: PDF is added");
    }

    #[test]
    fn word_and_text_files_reach_pdf_text_and_pictures() {
        let docx = one(SourceKind::Docx, RingKind::Documents);
        assert_eq!(labels(&docx), ["PDF", "JPG", "PNG", "TXT"]);
        let text = one(SourceKind::Text, RingKind::Documents);
        assert_eq!(labels(&text), ["DOCX", "JPG", "PNG", "PDF"]);
        let pics = one(SourceKind::Docx, RingKind::Images);
        assert_eq!(labels(&pics), ["PNG", "JPG", "WEBP", "HEIC", "TIFF", "AVIF", "BMP"], "picture formats only");
    }

    #[test]
    fn spreadsheets_lead_with_csv_and_csv_leads_with_xlsx() {
        let sheet = one(SourceKind::Sheet, RingKind::Documents);
        assert_eq!(labels(&sheet), ["CSV", "JPG", "PNG", "TXT", "PDF"], "no sector the engine cannot do");
        let csv = one(SourceKind::Csv, RingKind::Documents);
        assert_eq!(labels(&csv), ["XLSX", "JPG", "PNG", "PDF"]);
    }

    #[test]
    fn each_kind_of_file_has_exactly_the_rings_that_apply_to_it() {
        use RingKind::*;
        assert_eq!(rings_for(&[PNG]), [Documents, Images, Tools, Files]);
        let mine = [Custom {
            index: 0,
            label: "Invoices".into(),
            hint: "Move to Invoices".into(),
            kind: crate::sectors::Kind::MoveTo,
        }];
        assert_eq!(rings_with(&[SourceKind::Other], &mine), [Files, Mine], "the user's ring comes last");
        assert_eq!(rings_with(&[PNG], &mine), [Documents, Images, Tools, Files, Mine]);
        assert_eq!(rings_for(&[SourceKind::Pdf]), [Documents, Images, Tools, Files]);
        for k in [SourceKind::Docx, SourceKind::Text, SourceKind::Csv, SourceKind::Sheet] {
            assert_eq!(rings_for(&[k]), [Documents, Images, Files], "{k:?} has no tools of its own");
        }
        for k in [SourceKind::Other, SourceKind::Archive, SourceKind::Folder] {
            assert_eq!(rings_for(&[k]), [Files], "{k:?}");
        }
        assert!(rings_for(&[]).contains(&Files));
        let m = one(SourceKind::Other, Documents);
        assert_eq!(m.kind, Files, "asking for an empty ring gives the first one the file has");
        assert!(!m.switchable());
        assert_eq!(m.position(), (0, 1));
    }

    #[test]
    fn a_ring_the_user_asked_for_opens_first_unless_the_file_has_no_such_ring() {
        assert_eq!(first_ring(&[PNG], None), RingKind::Images);
        assert_eq!(first_ring(&[SourceKind::Pdf], None), RingKind::Documents);
        assert_eq!(first_ring(&[PNG], Some(RingKind::Tools)), RingKind::Tools);
        assert_eq!(first_ring(&[SourceKind::Other], None), RingKind::Files);
        assert_eq!(first_ring(&[SourceKind::Archive], Some(RingKind::Images)), RingKind::Files);
        assert_eq!(first_ring(&[SourceKind::Docx], Some(RingKind::Tools)), RingKind::Documents, "no tools for Word");
    }

    #[test]
    fn the_wheel_walks_only_the_rings_a_file_has() {
        let m = one(SourceKind::Other, RingKind::Files);
        assert_eq!(m.neighbour(1), RingKind::Files);
        let pdf = one(SourceKind::Pdf, RingKind::Documents);
        assert_eq!((pdf.neighbour(1), pdf.neighbour(-1)), (RingKind::Images, RingKind::Files));
        assert_eq!(pdf.position(), (0, 4));
        let word = one(SourceKind::Docx, RingKind::Images);
        assert_eq!(word.neighbour(1), RingKind::Files, "Word has no tools ring to stop at");
    }

    #[test]
    fn pictures_get_eight_tools_with_the_quick_ones_first() {
        let m = one(PNG, RingKind::Tools);
        let tools: Vec<ActionKind> = m
            .items
            .iter()
            .map(|i| match i.op {
                Op::Action(a) => a,
                Op::Convert(_) | Op::Custom(_) => panic!("only actions in the tools ring"),
            })
            .collect();
        assert_eq!(tools, ActionKind::PICTURE_TOOLS);
        assert_eq!(
            labels(&m),
            [
                "act-copytext",
                "act-resize",
                "act-compress",
                "act-metadata",
                "act-crop",
                "act-edit",
                "act-annotate",
                "act-addbg"
            ]
        );
        assert!(m.items.iter().all(|i| i.enabled));
    }

    #[test]
    fn a_pdf_can_be_read_out_and_split_and_a_picture_with_a_pdf_can_only_be_read_out() {
        assert_eq!(labels(&one(SourceKind::Pdf, RingKind::Tools)), ["act-copytext", "act-split"]);
        let mixed = build(&[PNG, SourceKind::Pdf], RingKind::Tools, 2, &all_ok());
        assert_eq!(labels(&mixed), ["act-copytext"]);
        assert!(build(&[PNG, SourceKind::Docx], RingKind::Tools, 2, &all_ok()).kind != RingKind::Tools);
    }

    #[test]
    fn copy_text_of_a_picture_needs_an_ocr_language_but_a_pdf_does_not() {
        let env = Env {
            target: &|k, t| match (k, t) {
                (SourceKind::Image(_), Target::Txt) => Availability::Unavailable("no OCR language".into()),
                _ => Availability::Available,
            },
            source: &|_| Availability::Available,
            custom: &[],
        };
        let pic = build(&[PNG], RingKind::Tools, 1, &env);
        let copy = item(&pic, ActionKind::CopyText);
        assert!(!copy.enabled);
        assert_eq!(copy.reason, Some(Reason::Engine("no OCR language".into())));
        assert!(item(&pic, ActionKind::Resize).enabled, "the other tools do not need OCR");
        let pdf = build(&[SourceKind::Pdf], RingKind::Tools, 1, &env);
        assert!(pdf.items.iter().all(|i| i.enabled));
        let docs = build(&[PNG], RingKind::Documents, 1, &env);
        let txt = docs.items.iter().find(|i| i.op == Op::Convert(Target::Txt)).unwrap();
        assert_eq!(txt.reason, Some(Reason::Engine("no OCR language".into())));
        assert_eq!(enabled(&docs), ["DOCX", "JPG", "PDF"]);
    }

    #[test]
    fn the_files_ring_is_the_same_for_every_file_and_starts_with_what_only_some_files_have() {
        let base = ["act-rename", "act-zip", "act-info", "act-checksum", "act-copypath", "act-reveal"];
        for k in [PNG, SourceKind::Pdf, SourceKind::Docx, SourceKind::Sheet, SourceKind::Text, SourceKind::Other] {
            let m = one(k, RingKind::Files);
            assert_eq!(labels(&m), base, "{k:?}");
            assert!(m.items.iter().all(|i| i.enabled), "every file action works on {k:?}");
        }
        let zip = one(SourceKind::Archive, RingKind::Files);
        assert_eq!(labels(&zip)[..2], ["act-unzip", "act-rename"]);
        let two = build(&[SourceKind::Archive; 2], RingKind::Files, 2, &all_ok());
        assert_eq!(labels(&two)[0], "act-unzip");
        let mixed = build(&[SourceKind::Archive, SourceKind::Pdf], RingKind::Files, 2, &all_ok());
        assert_eq!(labels(&mixed), base);
        let folder = one(SourceKind::Folder, RingKind::Files);
        assert_eq!(labels(&folder)[..2], ["act-sort", "act-rename"]);
    }

    #[test]
    fn folders_can_be_zipped_renamed_copied_and_revealed_but_not_hashed() {
        let m = one(SourceKind::Folder, RingKind::Files);
        for a in [ActionKind::Zip, ActionKind::Rename, ActionKind::CopyPath, ActionKind::Reveal, ActionKind::Sort] {
            assert!(item(&m, a).enabled, "{a:?}");
        }
        for a in [ActionKind::Checksum, ActionKind::Info] {
            assert!(!item(&m, a).enabled);
            assert_eq!(item(&m, a).reason, Some(Reason::FilesOnly));
        }
    }

    #[test]
    fn several_files_that_can_all_be_a_pdf_can_be_merged_into_one() {
        let two = build(&[PNG, SourceKind::Docx], RingKind::Documents, 2, &all_ok());
        assert_eq!(labels(&two).last(), Some(&"act-merge"));
        assert!(item(&two, ActionKind::Merge).enabled);
        let three = build(&[SourceKind::Pdf; 3], RingKind::Documents, 3, &all_ok());
        assert_eq!(labels(&three).last(), Some(&"act-merge"));
        assert!(
            !labels(&one(SourceKind::Pdf, RingKind::Documents)).contains(&"act-merge"),
            "one file: nothing to merge"
        );
        let with_zip = build(&[SourceKind::Pdf, SourceKind::Archive], RingKind::Documents, 2, &all_ok());
        assert!(!labels(&with_zip).contains(&"act-merge"), "an archive cannot be a page");
    }

    #[test]
    fn a_batch_greys_a_conversion_only_some_of_the_files_can_do() {
        // A text file cannot be turned into a spreadsheet, a CSV can.
        let m = build(&[SourceKind::Csv, SourceKind::Text], RingKind::Documents, 2, &all_ok());
        let xlsx = m.items.iter().find(|i| i.op == Op::Convert(Target::Xlsx)).unwrap();
        assert!(!xlsx.enabled);
        assert_eq!(xlsx.reason, Some(Reason::NoPath));
        let jpg = m.items.iter().find(|i| i.op == Op::Convert(Target::Image(Fmt::Jpg))).unwrap();
        assert!(jpg.enabled);
    }

    #[test]
    fn unavailable_targets_are_disabled_with_the_engines_reason() {
        let env = Env {
            target: &|_, t| match t {
                Target::Image(Fmt::Avif) => Availability::Unavailable("no AV1 encoder".into()),
                _ => Availability::Available,
            },
            source: &|_| Availability::Available,
            custom: &[],
        };
        let m = build(&[SourceKind::Image(Fmt::Jpg)], RingKind::Images, 1, &env);
        let avif = m.items.iter().find(|i| i.op == Op::Convert(Target::Image(Fmt::Avif))).unwrap();
        assert!(!avif.enabled);
        assert_eq!(avif.reason, Some(Reason::Engine("no AV1 encoder".into())));
        assert_eq!(m.items.iter().filter(|i| i.enabled).count(), 7);
    }

    #[test]
    fn an_unreadable_source_disables_the_whole_ring() {
        let env = Env {
            target: &|_, _| Availability::Available,
            source: &|_| Availability::Unavailable("HEIC decoder missing".into()),
            custom: &[],
        };
        for ring in RingKind::ALL {
            let m = build(&[SourceKind::Image(Fmt::Heic)], ring, 1, &env);
            assert!(!m.items.is_empty() && m.items.iter().all(|i| !i.enabled && i.reason.is_some()), "{ring:?}");
        }
    }

    #[test]
    fn compress_and_metadata_only_offer_formats_the_engine_handles() {
        let m = one(SourceKind::Image(Fmt::Bmp), RingKind::Tools);
        assert!(!item(&m, ActionKind::Compress).enabled);
        assert_eq!(item(&m, ActionKind::Compress).reason, Some(Reason::Formats("JPG, PNG, WEBP")));
        assert!(!item(&m, ActionKind::Metadata).enabled);
        assert!(item(&m, ActionKind::Crop).enabled && item(&m, ActionKind::Resize).enabled);
        let tiff = one(SourceKind::Image(Fmt::Tiff), RingKind::Tools);
        assert!(item(&tiff, ActionKind::Metadata).enabled && !item(&tiff, ActionKind::Compress).enabled);
    }

    #[test]
    fn the_editors_need_exactly_one_picture_but_the_quick_tools_and_resize_take_a_batch() {
        let m = build(&[SourceKind::Image(Fmt::Jpg); 3], RingKind::Tools, 3, &all_ok());
        for a in ActionKind::PICTURE_TOOLS {
            let i = item(&m, a);
            assert_eq!(i.enabled, !a.needs_one_file(), "{a:?}");
            if !i.enabled {
                assert_eq!(i.reason, Some(Reason::OneFile));
            }
        }
        assert!(item(&m, ActionKind::Resize).enabled && item(&m, ActionKind::CopyText).enabled);
        let many = build(&[SourceKind::Pdf; 3], RingKind::Files, 3, &all_ok());
        assert!(many.items.iter().all(|i| i.enabled), "file actions batch");
    }

    #[test]
    fn action_names_parse_for_the_command_line_and_cover_every_action() {
        assert_eq!(ActionKind::from_name("CROP"), Some(ActionKind::Crop));
        assert_eq!(ActionKind::from_name("bg"), Some(ActionKind::AddBg));
        assert_eq!(ActionKind::from_name("nope"), None);
        assert_eq!(ActionKind::from_name("redact"), None, "folded into annotate");
        for (name, a) in [
            ("compress", ActionKind::Compress),
            ("metadata", ActionKind::Metadata),
            ("edit", ActionKind::Edit),
            ("annotate", ActionKind::Annotate),
            ("addbg", ActionKind::AddBg),
            ("zip", ActionKind::Zip),
            ("unzip", ActionKind::Unzip),
            ("info", ActionKind::Info),
            ("sha256", ActionKind::Checksum),
            ("copypath", ActionKind::CopyPath),
            ("reveal", ActionKind::Reveal),
            ("copytext", ActionKind::CopyText),
            ("resize", ActionKind::Resize),
            ("rename", ActionKind::Rename),
            ("split", ActionKind::Split),
            ("merge", ActionKind::Merge),
            ("sort", ActionKind::Sort),
        ] {
            assert_eq!(ActionKind::from_name(name), Some(a));
        }
    }

    #[test]
    fn every_action_has_a_stable_id_a_label_and_a_hint_in_both_languages() {
        for (i, a) in ActionKind::EVERY.iter().enumerate() {
            assert_eq!(a.id(), i);
            assert_eq!(ActionKind::from_id(i), Some(*a));
        }
        assert_eq!(ActionKind::from_id(ActionKind::EVERY.len()), None);
        let mut keys: Vec<_> = ActionKind::EVERY.iter().map(|a| a.label_key()).collect();
        keys.sort_unstable();
        keys.dedup();
        assert_eq!(keys.len(), ActionKind::EVERY.len(), "no two actions share a label");
        for a in ActionKind::EVERY {
            for l in [crate::config::Lang::En, crate::config::Lang::Es] {
                assert!(!crate::config::tr(l, a.label_key()).is_empty(), "{a:?}");
                assert!(!crate::config::tr(l, a.hint_key()).is_empty(), "{a:?}");
            }
        }
        for list in [&ActionKind::PICTURE_TOOLS[..], &ActionKind::PDF_TOOLS[..], &ActionKind::FILE_TOOLS[..]] {
            for a in list {
                assert!(ActionKind::EVERY.contains(a), "{a:?} is in a ring but has no id");
            }
        }
    }

    #[test]
    fn the_users_own_sectors_get_their_own_ring_with_their_own_words() {
        let mine = [
            Custom {
                index: 2,
                label: "Invoices".into(),
                hint: "Move to Invoices".into(),
                kind: crate::sectors::Kind::MoveTo,
            },
            Custom {
                index: 5,
                label: "Editor".into(),
                hint: "Open with Editor".into(),
                kind: crate::sectors::Kind::OpenWith,
            },
        ];
        let env = Env { target: &|_, _| Availability::Available, source: &|_| Availability::Available, custom: &mine };
        let m = build(&[SourceKind::Pdf], RingKind::Mine, 1, &env);
        assert_eq!(m.kind, RingKind::Mine);
        assert_eq!(m.items.iter().map(|i| i.op).collect::<Vec<_>>(), [Op::Custom(2), Op::Custom(5)]);
        assert!(m.items.iter().all(|i| i.enabled));
        assert_eq!(m.items[0].custom.as_ref().map(|c| c.label.as_str()), Some("Invoices"));
        assert_eq!(m.items[1].custom.as_ref().map(|c| c.hint.as_str()), Some("Open with Editor"));
        assert_eq!(m.neighbour(1), RingKind::Documents, "the wheel wraps back to the first ring");
        // Without sectors there is no such ring, even when it is asked for.
        assert_eq!(one(SourceKind::Pdf, RingKind::Mine).kind, RingKind::Documents);
    }

    #[test]
    fn rings_step_forward_and_back_and_wrap_around() {
        assert_eq!(RingKind::Documents.step(1), RingKind::Images);
        assert_eq!(RingKind::Mine.step(1), RingKind::Documents);
        assert_eq!(RingKind::Documents.step(-1), RingKind::Mine);
        assert_eq!(RingKind::Tools.step(-1), RingKind::Images);
        assert_eq!(RingKind::Images.step(5), RingKind::Images);
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
