//! The editor of one user sector: a name, what it does (move, copy, open with), the folder or program it points to
//! and the extensions it applies to. Saving rewrites `sectors.txt` and tells the Settings tab to refresh.

use std::mem::zeroed;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicIsize, Ordering};

use windows_sys::Win32::Foundation::HWND;
use windows_sys::Win32::UI::Controls::Dialogs::{
    GetOpenFileNameW, OFN_FILEMUSTEXIST, OFN_NOCHANGEDIR, OFN_NODEREFERENCELINKS, OFN_PATHMUSTEXIST, OPENFILENAMEW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::SetForegroundWindow;

use super::ui::{Field, Pen, Style, RED};
use super::{Dialog, Ev, Host, JobResult};
use crate::config::{tr, Lang};
use crate::jobs::fill;
use crate::sectors::{self, Kind, Sector, MAX_LABEL};
use crate::theme::Rgba;

const W: f32 = 520.0;
const H: f32 = 420.0;
const MARGIN: f32 = 24.0;

const ID_NAME: u32 = 100;
const ID_KIND: u32 = 110; // three segments: 110, 111, 112
const ID_BROWSE: u32 = 120;
const ID_TARGET: u32 = 121;
const ID_EXTS: u32 = 130;
const ID_SAVE: u32 = 150;
const ID_CANCEL: u32 = 151;

/// One editor at a time: a second click on Edit or Add brings the open one forward.
static OPEN: AtomicBool = AtomicBool::new(false);
static OPEN_HWND: AtomicIsize = AtomicIsize::new(0);

/// Takes the single editor slot; false when an editor is already open (it is brought to the front).
pub fn claim() -> bool {
    if OPEN.swap(true, Ordering::SeqCst) {
        let h = OPEN_HWND.load(Ordering::SeqCst);
        if h != 0 {
            // SAFETY: a stale handle only makes the call fail.
            unsafe { SetForegroundWindow(h as HWND) };
        }
        return false;
    }
    true
}

pub struct SectorDialog {
    index: Option<usize>,
    name: Field,
    kind: Kind,
    /// Empty until one is picked.
    target: PathBuf,
    exts: Field,
    /// The user did something: only then are the reasons shown (a fresh dialog is not nagging).
    touched: bool,
    /// A problem saving.
    message: Option<String>,
    /// Where `sectors.txt` lives when it is not `%APPDATA%\radial` (tests).
    dir: Option<PathBuf>,
    /// This dialog holds the single editor slot.
    claimed: bool,
}

fn text_field(text: &str, max: usize) -> Field {
    Field { caret: text.chars().count(), text: text.to_string(), all: false, digits: false, max }
}

impl SectorDialog {
    /// A new sector (`None`) or the one at `index` of the saved list.
    pub fn new(index: Option<usize>) -> SectorDialog {
        SectorDialog::build(index, None)
    }

    fn build(index: Option<usize>, dir: Option<PathBuf>) -> SectorDialog {
        let list = match &dir {
            Some(d) => sectors::load_in(d),
            None => sectors::load(),
        };
        let mut d = SectorDialog {
            index,
            name: text_field("", MAX_LABEL),
            kind: Kind::MoveTo,
            target: PathBuf::new(),
            exts: text_field("", 60),
            touched: false,
            message: None,
            dir,
            claimed: false,
        };
        if let Some(s) = index.and_then(|i| list.get(i)) {
            d.name = text_field(&s.label, MAX_LABEL);
            d.kind = s.kind;
            d.target = s.target.clone();
            d.exts = text_field(&sectors::exts_text(&s.exts), 60);
        }
        d
    }

    /// The editor owns the single slot taken with [`claim`] and gives it back when it closes.
    pub fn claimed(mut self) -> SectorDialog {
        self.claimed = true;
        self
    }

    /// A filled-in sector for the offscreen preview: `move`, `copy` or `open`; anything else is a blank editor.
    pub fn preview(kind: Option<&str>) -> SectorDialog {
        let mut d = SectorDialog::build(None, Some(std::env::temp_dir().join("radial-preview-none")));
        let Some(k) = kind.and_then(|k| match k {
            "move" => Some(Kind::MoveTo),
            "copy" => Some(Kind::CopyTo),
            "open" => Some(Kind::OpenWith),
            _ => None,
        }) else {
            return d;
        };
        d.kind = k;
        d.name = text_field(if k == Kind::OpenWith { "Editor" } else { "Invoices" }, MAX_LABEL);
        d.target =
            if k == Kind::OpenWith { PathBuf::from(r"C:\Windows\System32\notepad.exe") } else { std::env::temp_dir() };
        d.exts = text_field("pdf, docx", 60);
        d.touched = true;
        d
    }

    /// The sector the fields describe right now.
    fn current(&self) -> Sector {
        Sector {
            label: self.name.text.trim().to_string(),
            kind: self.kind,
            target: self.target.clone(),
            exts: sectors::parse_exts(&self.exts.text),
        }
    }

    fn problem(&self, lang: Lang) -> Option<String> {
        sectors::validate_in(&self.current(), lang).err()
    }

    fn set_kind(&mut self, kind: Kind) {
        // A folder is no program and the other way round; moving and copying share their folder.
        if kind.wants_folder() != self.kind.wants_folder() {
            self.target = PathBuf::new();
        }
        self.kind = kind;
        self.message = None;
    }

    fn browse(&mut self, host: &mut Host) {
        if host.hwnd.is_null() {
            return;
        }
        // SAFETY: the owner is this dialog's live window; both pickers are modal and return before we go on.
        let picked = unsafe {
            if self.kind.wants_folder() {
                crate::settings::pick_folder(host.hwnd, tr(host.lang, "sec-pick-folder-hint"))
            } else {
                pick_program(host.hwnd, host.lang, &self.target)
            }
        };
        if let Some(p) = picked.filter(|p| !p.is_empty()) {
            self.target = PathBuf::from(p);
            self.message = None;
        }
    }

    fn list(&self) -> Vec<Sector> {
        match &self.dir {
            Some(d) => sectors::load_in(d),
            None => sectors::load(),
        }
    }

    fn write(&self, list: &[Sector]) -> std::io::Result<()> {
        match &self.dir {
            Some(d) => sectors::save_in(d, list),
            None => sectors::save(list),
        }
    }

    fn save(&mut self, host: &mut Host) {
        self.touched = true;
        let s = self.current();
        if let Err(m) = sectors::validate_in(&s, host.lang) {
            self.message = Some(m);
            return;
        }
        let mut list = self.list();
        if let Err(p) = sectors::upsert(&mut list, self.index, s) {
            self.message = Some(p.text(host.lang));
            return;
        }
        match self.write(&list) {
            Ok(()) => {
                crate::settings::sectors_changed();
                host.close = true;
            }
            Err(e) => self.message = Some(fill(tr(host.lang, "sec-e-write"), &[&e.to_string()])),
        }
    }
}

impl Drop for SectorDialog {
    fn drop(&mut self) {
        if self.claimed {
            OPEN_HWND.store(0, Ordering::SeqCst);
            OPEN.store(false, Ordering::SeqCst);
        }
    }
}

/// The "open a program" picker. Shortcuts are returned as themselves, not as what they point to.
unsafe fn pick_program(owner: HWND, lang: Lang, current: &Path) -> Option<String> {
    let units = |s: &str| s.encode_utf16().collect::<Vec<u16>>();
    let mut filter = units(tr(lang, "sec-programs"));
    filter.extend(units(" (*.exe;*.bat;*.cmd;*.lnk)"));
    filter.push(0);
    filter.extend(units("*.exe;*.bat;*.cmd;*.lnk"));
    filter.extend([0, 0]);
    let title: Vec<u16> = units(tr(lang, "sec-pick-program-hint")).into_iter().chain(Some(0)).collect();
    let start: Vec<u16> = current
        .parent()
        .filter(|p| p.is_dir())
        .map(|p| units(&p.to_string_lossy()).into_iter().chain(Some(0)).collect())
        .unwrap_or_default();
    let mut buf = [0u16; 1024];
    let mut ofn: OPENFILENAMEW = zeroed();
    ofn.lStructSize = std::mem::size_of::<OPENFILENAMEW>() as u32;
    ofn.hwndOwner = owner;
    ofn.lpstrFilter = filter.as_ptr();
    ofn.nFilterIndex = 1;
    ofn.lpstrFile = buf.as_mut_ptr();
    ofn.nMaxFile = buf.len() as u32;
    ofn.lpstrTitle = title.as_ptr();
    if !start.is_empty() {
        ofn.lpstrInitialDir = start.as_ptr();
    }
    ofn.Flags = OFN_FILEMUSTEXIST | OFN_PATHMUSTEXIST | OFN_NOCHANGEDIR | OFN_NODEREFERENCELINKS;
    if GetOpenFileNameW(&mut ofn) == 0 {
        return None;
    }
    let n = buf.iter().position(|&c| c == 0).unwrap_or(0);
    Some(String::from_utf16_lossy(&buf[..n]))
}

/// `t` shortened from the front ("...\Invoices") so it is at most `max` logical pixels wide.
fn fit_tail(pen: &Pen, t: &str, px: f32, max: f32) -> String {
    if pen.width(t, px) <= max {
        return t.to_string();
    }
    let chars: Vec<char> = t.chars().collect();
    let mut from = 1;
    while from < chars.len() {
        let s: String = std::iter::once('\u{2026}').chain(chars[from..].iter().copied()).collect();
        if pen.width(&s, px) <= max {
            return s;
        }
        from += 1;
    }
    "\u{2026}".to_string()
}

fn danger(dark: bool) -> Rgba {
    if dark {
        RED
    } else {
        Rgba(0xc4, 0x2b, 0x1c, 255)
    }
}

impl Dialog for SectorDialog {
    fn title(&self, lang: Lang) -> String {
        tr(lang, if self.index.is_some() { "sec-title-edit" } else { "sec-title-new" }).to_string()
    }

    fn size(&self) -> (f32, f32) {
        (W, H)
    }

    fn start(&mut self, host: &mut Host) {
        if self.claimed {
            OPEN_HWND.store(host.hwnd as isize, Ordering::SeqCst);
        }
        host.set_focus(Some(ID_NAME));
        self.name.focus();
    }

    fn busy(&self) -> bool {
        false
    }

    fn saving(&self) -> bool {
        false
    }

    fn job(&mut self, _result: JobResult, _host: &mut Host) {}

    fn default_action(&mut self, host: &mut Host) {
        if self.problem(host.lang).is_none() {
            self.save(host);
        } else {
            self.touched = true;
        }
    }

    fn event(&mut self, ev: Ev, host: &mut Host) {
        match ev {
            Ev::Click(id) => {
                self.touched = true;
                match id {
                    ID_NAME => self.name.focus(),
                    ID_EXTS => self.exts.focus(),
                    i if (ID_KIND..ID_KIND + 3).contains(&i) => self.set_kind(Kind::ALL[(i - ID_KIND) as usize]),
                    ID_BROWSE | ID_TARGET => self.browse(host),
                    ID_SAVE => self.save(host),
                    ID_CANCEL => host.close = true,
                    _ => {}
                }
            }
            Ev::Char(c) => {
                let changed = match host.focus {
                    Some(ID_NAME) => self.name.on_char(c),
                    Some(ID_EXTS) => self.exts.on_char(c),
                    _ => false,
                };
                if changed {
                    self.touched = true;
                    self.message = None;
                }
            }
            Ev::Key { vk, .. } => {
                let changed = match host.focus {
                    Some(ID_NAME) => self.name.on_key(vk),
                    Some(ID_EXTS) => self.exts.on_key(vk),
                    _ => false,
                };
                if changed {
                    self.touched = true;
                    self.message = None;
                }
            }
            _ => {}
        }
        host.redraw();
    }

    fn draw(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        let right = W - MARGIN;
        let inner = W - 2.0 * MARGIN;

        pen.label(tr(l, "sec-name"), MARGIN, 22.0);
        let count = format!("{}/{}", self.name.text.chars().count(), MAX_LABEL);
        pen.field([MARGIN, 42.0, inner, 34.0], &self.name, ID_NAME, &count);

        pen.label(tr(l, "sec-action"), MARGIN, 92.0);
        let labels = [tr(l, "sec-move"), tr(l, "sec-copy"), tr(l, "sec-open")];
        let sel = Kind::ALL.iter().position(|k| *k == self.kind).unwrap_or(0);
        pen.seg([MARGIN, 112.0, inner, 34.0], &labels, sel, ID_KIND);

        pen.label(tr(l, if self.kind.wants_folder() { "sec-folder" } else { "sec-program" }), MARGIN, 162.0);
        let bx = [MARGIN, 182.0, inner - 100.0, 34.0];
        pen.fill(bx, crate::settings::RADIUS, pen.pal.card);
        pen.outline(bx, crate::settings::RADIUS, pen.pal.border);
        let (shown, color) = if self.target.as_os_str().is_empty() {
            let hint = tr(l, if self.kind.wants_folder() { "sec-pick-folder-hint" } else { "sec-pick-program-hint" });
            (hint.to_string(), pen.pal.dim)
        } else {
            (fit_tail(pen, &self.target.to_string_lossy(), 12.5, bx[2] - 16.0), pen.pal.text)
        };
        pen.text(&shown, bx[0] + 8.0, bx[1] + bx[3] / 2.0 + 0.5, 12.5, color);
        pen.hit(bx, ID_TARGET, super::ui::Kind::Button);
        pen.button([right - 92.0, 182.0, 92.0, 34.0], tr(l, "sec-browse"), ID_BROWSE, Style::Secondary, true);

        pen.label(tr(l, "sec-only"), MARGIN, 232.0);
        let fr = [MARGIN, 252.0, inner, 34.0];
        pen.field(fr, &self.exts, ID_EXTS, "");
        if self.exts.text.is_empty() {
            pen.text(tr(l, "sec-all-files"), fr[0] + 8.0, fr[1] + fr[3] / 2.0 + 0.5, 12.5, pen.pal.dim);
        }
        pen.text(tr(l, "sec-exts-hint"), MARGIN, 304.0, 11.0, pen.pal.dim);

        // One line: why saving is not possible yet, or what went wrong saving.
        let problem = self.problem(l);
        let line = self.message.clone().or_else(|| if self.touched { problem.clone() } else { None });
        if let Some(m) = line {
            let t = pen.fit(&m, 12.5, inner);
            pen.text(&t, MARGIN, 332.0, 12.5, danger(pen.dark));
        }

        let by = H - 56.0;
        pen.button([right - 120.0 - 8.0 - 88.0, by, 88.0, 36.0], tr(l, "cancel"), ID_CANCEL, Style::Secondary, true);
        pen.button([right - 120.0, by, 120.0, 36.0], tr(l, "sec-save"), ID_SAVE, Style::Primary, problem.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::super::{Cursor, Host};
    use super::*;

    fn host() -> Host {
        Host {
            hwnd: std::ptr::null_mut(),
            lang: Lang::En,
            s: 1.0,
            dark: true,
            cursor: Cursor::Arrow,
            close: false,
            dirty: false,
            jobs: vec![],
            hits: vec![],
            order: vec![],
            hot: None,
            pressed: None,
            focus: None,
            drag: None,
            time: 0.0,
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("radial-secdlg-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn type_into(d: &mut SectorDialog, h: &mut Host, id: u32, text: &str) {
        h.focus = Some(id);
        d.event(Ev::Click(id), h);
        for c in text.chars() {
            d.event(Ev::Char(c), h);
        }
    }

    fn sample(label: &str, folder: &Path) -> Sector {
        Sector { label: label.into(), kind: Kind::MoveTo, target: folder.to_path_buf(), exts: vec!["pdf".into()] }
    }

    #[test]
    fn a_fresh_editor_cannot_save_and_does_not_nag() {
        let dd = dir("fresh");
        let d = SectorDialog::build(None, Some(dd.clone()));
        assert!(d.problem(Lang::En).is_some());
        assert!(!d.touched && d.message.is_none());
        assert_eq!(d.kind, Kind::MoveTo);
        assert_eq!(d.title(Lang::En), "New sector");
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn typing_a_name_and_picking_a_folder_saves_a_new_sector() {
        let dd = dir("save");
        let folder = dd.join("Invoices");
        std::fs::create_dir(&folder).unwrap();
        let mut d = SectorDialog::build(None, Some(dd.clone()));
        let mut h = host();
        type_into(&mut d, &mut h, ID_NAME, "Invoices and more stuff");
        assert_eq!(d.name.text, "Invoices and", "the name stops at twelve characters");
        type_into(&mut d, &mut h, ID_EXTS, ".PDF, docx");
        d.target = folder.clone();
        assert!(d.problem(Lang::En).is_none());
        d.event(Ev::Click(ID_SAVE), &mut h);
        assert!(h.close, "{:?}", d.message);
        let saved = sectors::load_in(&dd);
        assert_eq!(saved.len(), 1);
        assert_eq!(
            saved[0],
            Sector {
                label: "Invoices and".into(),
                kind: Kind::MoveTo,
                target: folder,
                exts: vec!["pdf".into(), "docx".into()]
            }
        );
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn save_is_refused_with_the_reason_until_the_sector_is_valid() {
        let dd = dir("refuse");
        let mut d = SectorDialog::build(None, Some(dd.clone()));
        let mut h = host();
        d.event(Ev::Click(ID_SAVE), &mut h);
        assert!(!h.close);
        assert_eq!(d.message.as_deref(), Some("Give the sector a name"));
        type_into(&mut d, &mut h, ID_NAME, "Docs");
        assert!(d.message.is_none(), "typing clears the old complaint");
        d.event(Ev::Click(ID_SAVE), &mut h);
        assert_eq!(d.message.as_deref(), Some("Pick a folder"));
        d.target = dd.join("missing");
        d.event(Ev::Click(ID_SAVE), &mut h);
        assert!(d.message.as_deref().unwrap().contains("missing"));
        assert!(sectors::load_in(&dd).is_empty() && !h.close);
        h.lang = Lang::Es;
        d.default_action(&mut h);
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn editing_replaces_the_sector_at_its_index_and_leaves_the_others() {
        let dd = dir("edit");
        let (a, b) = (dd.join("a"), dd.join("b"));
        std::fs::create_dir_all(&a).unwrap();
        std::fs::create_dir_all(&b).unwrap();
        sectors::save_in(&dd, &[sample("First", &a), sample("Second", &a), sample("Third", &a)]).unwrap();
        let mut d = SectorDialog::build(Some(1), Some(dd.clone()));
        assert_eq!(
            (d.name.text.as_str(), d.exts.text.as_str(), d.title(Lang::Es).as_str()),
            ("Second", "pdf", "Editar sector")
        );
        let mut h = host();
        type_into(&mut d, &mut h, ID_NAME, "Renamed");
        d.target = b.clone();
        d.event(Ev::Click(ID_KIND + 1), &mut h);
        d.event(Ev::Click(ID_SAVE), &mut h);
        assert!(h.close, "{:?}", d.message);
        let got = sectors::load_in(&dd);
        assert_eq!(got.iter().map(|s| s.label.as_str()).collect::<Vec<_>>(), ["First", "Renamed", "Third"]);
        assert_eq!((got[1].kind, got[1].target.clone()), (Kind::CopyTo, b));
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn a_full_list_refuses_a_new_sector() {
        let dd = dir("full");
        let list: Vec<Sector> = (0..sectors::MAX_SECTORS).map(|i| sample(&format!("s{i}"), &dd)).collect();
        sectors::save_in(&dd, &list).unwrap();
        let mut d = SectorDialog::build(None, Some(dd.clone()));
        let mut h = host();
        type_into(&mut d, &mut h, ID_NAME, "Ninth");
        d.target = dd.clone();
        d.event(Ev::Click(ID_SAVE), &mut h);
        assert!(!h.close);
        assert!(d.message.as_deref().unwrap().contains('8'));
        assert_eq!(sectors::load_in(&dd).len(), sectors::MAX_SECTORS);
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn switching_between_a_folder_and_a_program_forgets_the_target() {
        let dd = dir("kind");
        let mut d = SectorDialog::build(None, Some(dd.clone()));
        let mut h = host();
        d.target = dd.clone();
        d.event(Ev::Click(ID_KIND + 1), &mut h);
        assert_eq!((d.kind, d.target.clone()), (Kind::CopyTo, dd.clone()), "copy keeps the folder of move");
        d.event(Ev::Click(ID_KIND + 2), &mut h);
        assert_eq!((d.kind, d.target.as_os_str().is_empty()), (Kind::OpenWith, true));
        d.target = PathBuf::from(r"C:\Windows\System32\notepad.exe");
        d.event(Ev::Click(ID_KIND), &mut h);
        assert!(d.target.as_os_str().is_empty(), "a program is no folder");
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn a_program_must_exist_and_look_like_one() {
        let dd = dir("prog");
        let prog = dd.join("ed.cmd");
        std::fs::write(&prog, "@echo off").unwrap();
        let mut d = SectorDialog::build(None, Some(dd.clone()));
        let mut h = host();
        d.event(Ev::Click(ID_KIND + 2), &mut h);
        type_into(&mut d, &mut h, ID_NAME, "Editor");
        d.target = dd.join("notes.txt");
        std::fs::write(&d.target, "x").unwrap();
        assert!(d.problem(Lang::En).unwrap().contains(".exe"));
        d.target = prog;
        assert!(d.problem(Lang::En).is_none());
        d.event(Ev::Click(ID_SAVE), &mut h);
        assert!(h.close);
        assert_eq!(sectors::load_in(&dd)[0].kind, Kind::OpenWith);
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn the_extensions_field_edits_in_place_and_the_name_has_a_caret() {
        let dd = dir("keys");
        let mut d = SectorDialog::build(None, Some(dd.clone()));
        let mut h = host();
        type_into(&mut d, &mut h, ID_EXTS, "pdf");
        d.event(Ev::Key { vk: super::super::ui::VK_BACK, ctrl: false }, &mut h);
        assert_eq!(d.exts.text, "pd");
        h.focus = Some(ID_SAVE);
        d.event(Ev::Char('x'), &mut h);
        assert_eq!(d.exts.text, "pd", "keys only go to the focused field");
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn cancel_closes_without_writing() {
        let dd = dir("cancel");
        let mut d = SectorDialog::build(None, Some(dd.clone()));
        let mut h = host();
        type_into(&mut d, &mut h, ID_NAME, "x");
        d.event(Ev::Click(ID_CANCEL), &mut h);
        assert!(h.close && !dd.join("sectors.txt").exists());
        let _ = std::fs::remove_dir_all(&dd);
    }

    #[test]
    fn the_editor_slot_is_given_back_when_the_dialog_goes() {
        // Another test may hold the slot; only this one claims it.
        let first = claim();
        assert!(!claim(), "a second editor is refused while one is open");
        if first {
            let d = SectorDialog::build(None, Some(std::env::temp_dir())).claimed();
            drop(d);
            assert!(claim(), "closing the editor frees the slot");
            OPEN.store(false, Ordering::SeqCst);
        }
    }

    #[test]
    fn the_dialog_text_exists_in_both_languages() {
        for key in [
            "sec-title-new",
            "sec-title-edit",
            "sec-name",
            "sec-action",
            "sec-move",
            "sec-copy",
            "sec-open",
            "sec-folder",
            "sec-program",
            "sec-pick-folder-hint",
            "sec-pick-program-hint",
            "sec-programs",
            "sec-browse",
            "sec-only",
            "sec-all-files",
            "sec-exts-hint",
            "sec-save",
            "sec-e-write",
        ] {
            for l in [Lang::En, Lang::Es] {
                assert!(!tr(l, key).is_empty(), "{key}");
            }
        }
    }
}
