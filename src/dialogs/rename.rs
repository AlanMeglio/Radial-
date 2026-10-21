//! The RENAME dialog: a live old -> new preview of every dropped file on the left, the rule on the right.
//! Planning and renaming are the engine's (`engine::rename`); this file owns the controls, the scrolling list and the
//! "renamed, undo?" state that follows Apply.

use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use super::ui::{Field, Kind, Pen, Style, R, RED, VK_END, VK_HOME};
use super::{Dialog, Ev, Host, JobResult};
use crate::config::{tr, Lang};
use crate::engine::rename::{
    self, Case, DateFormat, DatePos, DateSource, DateStamp, Item, NumPos, Numbering, Rule, Spaces, Status, Undo,
    UndoReport,
};
use crate::jobs::fill;
use crate::settings::RADIUS_CARD;

const W: f32 = 960.0;
const H: f32 = 640.0;
const LIST: R = [24.0, 84.0, 584.0, 484.0];
const HEAD_H: f32 = 32.0;
const ROW_H: f32 = 28.0;
const VIS: usize = 16;
const PX: f32 = 632.0;
const PW: f32 = 304.0;

const ID_FIND: u32 = 20;
const ID_REPLACE: u32 = 21;
const ID_PREFIX: u32 = 22;
const ID_SUFFIX: u32 = 23;
const ID_START: u32 = 24;
const ID_DIGITS: u32 = 25;
const ID_ACCENTS: u32 = 30;
const ID_EXT: u32 = 31;
const ID_NUMBERING: u32 = 32;
const ID_DATE: u32 = 33;
const ID_RESET: u32 = 40;
const ID_APPLY: u32 = 41;
const ID_CANCEL: u32 = 42;
const ID_UNDO: u32 = 43;
const ID_CLOSE: u32 = 44;
const ID_SCROLL: u32 = 50;
const ID_CASE: u32 = 100;
const ID_SPACES: u32 = 110;
const ID_NUMPOS: u32 = 120;
const ID_DATEPOS: u32 = 130;
const ID_DATESRC: u32 = 140;
const ID_DATEFMT: u32 = 150;

const VK_PRIOR: u32 = 0x21;
const VK_NEXT: u32 = 0x22;
const VK_UP: u32 = 0x26;
const VK_DOWN: u32 = 0x28;

const CASES: [Case; 4] = [Case::None, Case::Lower, Case::Upper, Case::Title];
const SPACES: [Spaces; 4] = [Spaces::Keep, Spaces::Underscore, Spaces::Dash, Spaces::Remove];
const NUMPOS: [NumPos; 3] = [NumPos::Prefix, NumPos::Suffix, NumPos::Replace];
const DATEPOS: [DatePos; 2] = [DatePos::Prefix, DatePos::Suffix];
const DATESRC: [DateSource; 3] = [DateSource::Modified, DateSource::Created, DateSource::Taken];
const DATEFMT: [DateFormat; 3] = [DateFormat::Iso, DateFormat::Compact, DateFormat::IsoTime];

/// What a worker thread left for the dialog (the job result itself only wakes the dialog up).
enum Outcome {
    Applied(Result<Undo, String>),
    Undone(Result<UndoReport, String>),
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Stage {
    Edit,
    Working,
    Done,
}

struct Done {
    count: usize,
    undo: Undo,
    /// `(restored, left alone)` once Undo ran.
    restored: Option<(usize, usize)>,
}

pub struct RenameDialog {
    paths: Vec<PathBuf>,
    find: Field,
    replace: Field,
    prefix: Field,
    suffix: Field,
    start: Field,
    digits: Field,
    case: usize,
    spaces: usize,
    accents: bool,
    ext_lower: bool,
    numbering: bool,
    num_pos: usize,
    date: bool,
    date_pos: usize,
    date_src: usize,
    date_fmt: usize,
    items: Vec<Item>,
    /// File facts kept between plans so a keystroke does not stat thousands of files again.
    scan: rename::Scan,
    changing: usize,
    problems: usize,
    /// Index of the first visible row.
    top: usize,
    scrolling: bool,
    stage: Stage,
    done: Option<Done>,
    error: Option<String>,
    mailbox: Arc<Mutex<Option<Outcome>>>,
    /// Where undo files go; `None` is `%APPDATA%\radial\undo`.
    undo_dir: Option<PathBuf>,
}

fn text_field(max: usize) -> Field {
    Field { text: String::new(), caret: 0, all: false, digits: false, max }
}

impl RenameDialog {
    pub fn new(paths: Vec<PathBuf>) -> RenameDialog {
        let mut paths = paths;
        paths.sort_by(|a, b| {
            let key = |p: &PathBuf| p.parent().map(|d| d.to_string_lossy().to_lowercase()).unwrap_or_default();
            key(a).cmp(&key(b)).then_with(|| {
                let name = |p: &PathBuf| p.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
                rename::natural_cmp(&name(a), &name(b))
            })
        });
        paths.dedup();
        let mut d = RenameDialog {
            paths,
            find: text_field(80),
            replace: text_field(80),
            prefix: text_field(80),
            suffix: text_field(80),
            start: Field::number(1),
            digits: Field::number(3),
            case: 0,
            spaces: 0,
            accents: false,
            ext_lower: false,
            numbering: false,
            num_pos: 0,
            date: false,
            date_pos: 0,
            date_src: 0,
            date_fmt: 0,
            items: Vec::new(),
            scan: rename::Scan::default(),
            changing: 0,
            problems: 0,
            top: 0,
            scrolling: false,
            stage: Stage::Edit,
            done: None,
            error: None,
            mailbox: Arc::new(Mutex::new(None)),
            undo_dir: None,
        };
        d.replan();
        d
    }

    /// The rule the controls describe.
    fn rule(&self) -> Rule {
        Rule {
            find: self.find.text.clone(),
            replace: self.replace.text.clone(),
            case: CASES[self.case],
            strip_accents: self.accents,
            spaces: SPACES[self.spaces],
            prefix: self.prefix.text.clone(),
            suffix: self.suffix.text.clone(),
            numbering: self.numbering.then(|| Numbering {
                start: u64::from(self.start.value().unwrap_or(1)),
                step: 1,
                digits: self.digits.value().unwrap_or(1).clamp(1, 9) as usize,
                position: NUMPOS[self.num_pos],
            }),
            date: self.date.then(|| DateStamp {
                source: DATESRC[self.date_src],
                format: DATEFMT[self.date_fmt],
                position: DATEPOS[self.date_pos],
            }),
            ext_lowercase: self.ext_lower,
        }
    }

    /// Plans again after a change; only the visible rows are ever drawn, so this is the whole cost of an edit.
    fn replan(&mut self) {
        let rule = self.rule();
        self.items = rename::plan_with(&mut self.scan, &self.paths, &rule);
        self.changing = self.items.iter().filter(|i| i.status == Status::Ok).count();
        self.problems =
            self.items.iter().filter(|i| matches!(i.status, Status::Conflict(_) | Status::Invalid(_))).count();
        self.top = self.top.min(self.max_top());
        self.error = None;
    }

    fn max_top(&self) -> usize {
        self.items.len().saturating_sub(VIS)
    }

    fn can_apply(&self) -> bool {
        self.stage == Stage::Edit && self.changing > 0 && self.problems == 0
    }

    fn reset(&mut self) {
        for f in [&mut self.find, &mut self.replace, &mut self.prefix, &mut self.suffix] {
            f.set("");
        }
        self.start.set("1");
        self.digits.set("3");
        self.case = 0;
        self.spaces = 0;
        self.accents = false;
        self.ext_lower = false;
        self.numbering = false;
        self.num_pos = 0;
        self.date = false;
        self.date_pos = 0;
        self.date_src = 0;
        self.date_fmt = 0;
        self.replan();
    }

    fn field_mut(&mut self, id: u32) -> Option<&mut Field> {
        match id {
            ID_FIND => Some(&mut self.find),
            ID_REPLACE => Some(&mut self.replace),
            ID_PREFIX => Some(&mut self.prefix),
            ID_SUFFIX => Some(&mut self.suffix),
            ID_START => Some(&mut self.start),
            ID_DIGITS => Some(&mut self.digits),
            _ => None,
        }
    }

    fn scroll_by(&mut self, rows: isize) {
        self.top = self.top.saturating_add_signed(rows).min(self.max_top());
    }

    /// Maps the pointer height onto the scroll bar track.
    fn scroll_to_y(&mut self, y: f32) {
        if self.items.len() <= VIS {
            return;
        }
        let t = ((y - (LIST[1] + HEAD_H)) / (ROW_H * VIS as f32)).clamp(0.0, 1.0);
        self.top = (t * self.max_top() as f32).round() as usize;
    }

    fn apply(&mut self, host: &mut Host) {
        if !self.can_apply() {
            return;
        }
        self.stage = Stage::Working;
        self.error = None;
        let plan = self.items.clone();
        let (mailbox, dir) = (self.mailbox.clone(), self.undo_dir.clone());
        host.spawn(move || {
            let result = rename::apply(&plan).inspect(|undo| {
                // Losing the undo file must not lose the rename: the dialog still holds the undo in memory.
                let _ = match &dir {
                    Some(d) => undo.save_in(d),
                    None => undo.save(),
                };
            });
            post(&mailbox, Outcome::Applied(result.map_err(|e| e.to_string())));
            JobResult::Saved(Ok(PathBuf::new()))
        });
    }

    fn undo(&mut self, host: &mut Host) {
        let Some(done) = self.done.as_ref().filter(|d| d.restored.is_none()) else { return };
        let (undo, mailbox) = (done.undo.clone(), self.mailbox.clone());
        self.stage = Stage::Working;
        self.error = None;
        host.spawn(move || {
            post(&mailbox, Outcome::Undone(rename::undo_report(&undo).map_err(|e| e.to_string())));
            JobResult::Saved(Ok(PathBuf::new()))
        });
    }

    fn click_edit(&mut self, id: u32) {
        let pick = |base: u32, n: usize| (base..base + n as u32).contains(&id).then(|| (id - base) as usize);
        if let Some(f) = self.field_mut(id) {
            f.focus();
            return;
        }
        match id {
            ID_ACCENTS => self.accents = !self.accents,
            ID_EXT => self.ext_lower = !self.ext_lower,
            ID_NUMBERING => self.numbering = !self.numbering,
            ID_DATE => self.date = !self.date,
            ID_RESET => return self.reset(),
            _ => {
                if let Some(i) = pick(ID_CASE, CASES.len()) {
                    self.case = i;
                } else if let Some(i) = pick(ID_SPACES, SPACES.len()) {
                    self.spaces = i;
                } else if let Some(i) = pick(ID_NUMPOS, NUMPOS.len()) {
                    self.num_pos = i;
                } else if let Some(i) = pick(ID_DATEPOS, DATEPOS.len()) {
                    self.date_pos = i;
                } else if let Some(i) = pick(ID_DATESRC, DATESRC.len()) {
                    self.date_src = i;
                } else if let Some(i) = pick(ID_DATEFMT, DATEFMT.len()) {
                    self.date_fmt = i;
                } else {
                    return;
                }
            }
        }
        self.replan();
    }

    fn key(&mut self, vk: u32, focus: Option<u32>) {
        let in_field = focus.is_some_and(|f| self.field_mut(f).is_some());
        match vk {
            VK_UP => self.scroll_by(-1),
            VK_DOWN => self.scroll_by(1),
            VK_PRIOR => self.scroll_by(-(VIS as isize)),
            VK_NEXT => self.scroll_by(VIS as isize),
            VK_HOME if !in_field => self.top = 0,
            VK_END if !in_field => self.top = self.max_top(),
            _ => {
                let edited =
                    self.stage == Stage::Edit && focus.and_then(|f| self.field_mut(f)).is_some_and(|f| f.on_key(vk));
                if edited {
                    self.replan();
                }
            }
        }
    }
}

/// `one` for a count of 1, otherwise `many` with the count filled in.
fn plural(lang: Lang, one: &str, many: &str, n: usize) -> String {
    if n == 1 {
        tr(lang, one).to_string()
    } else {
        fill(tr(lang, many), &[&n.to_string()])
    }
}

/// Leaves a worker's result where `job` will find it.
fn post(mailbox: &Mutex<Option<Outcome>>, outcome: Outcome) {
    if let Ok(mut slot) = mailbox.lock() {
        *slot = Some(outcome);
    }
}

/// Words for an engine reason code; an unknown code is shown as it is.
fn reason_text(lang: Lang, code: &str) -> String {
    if let Some(rest) = code.strip_prefix("illegal:") {
        return match rest.chars().next() {
            Some(c) if c.is_control() => tr(lang, "ren-r-control").to_string(),
            Some(c) => fill(tr(lang, "ren-r-illegal"), &[&c.to_string()]),
            None => code.to_string(),
        };
    }
    let key = match code {
        "missing" => "ren-r-missing",
        "empty" => "ren-r-empty",
        "reserved" => "ren-r-reserved",
        "trailing" => "ren-r-trailing",
        "long" => "ren-r-long",
        "duplicate" => "ren-r-duplicate",
        "exists" => "ren-r-exists",
        _ => return code.to_string(),
    };
    tr(lang, key).to_string()
}

impl Dialog for RenameDialog {
    fn title(&self, lang: Lang) -> String {
        tr(lang, "ren-title").to_string()
    }

    fn size(&self) -> (f32, f32) {
        (W, H)
    }

    fn start(&mut self, host: &mut Host) {
        // Ready to type: the first field takes the focus.
        self.find.focus();
        host.set_focus(Some(ID_FIND));
    }

    fn busy(&self) -> bool {
        self.stage == Stage::Working
    }

    fn saving(&self) -> bool {
        self.stage == Stage::Working
    }

    fn default_action(&mut self, host: &mut Host) {
        match self.stage {
            Stage::Edit => self.apply(host),
            Stage::Done => host.close = true,
            Stage::Working => {}
        }
    }

    fn job(&mut self, result: JobResult, host: &mut Host) {
        if !matches!(result, JobResult::Saved(_)) {
            return;
        }
        let outcome = self.mailbox.lock().ok().and_then(|mut slot| slot.take());
        self.scan.forget(); // the files on disk just changed (or were put back)
        match outcome {
            Some(Outcome::Applied(Ok(undo))) => {
                self.done = Some(Done { count: undo.entries.len(), undo, restored: None });
                self.stage = Stage::Done;
            }
            Some(Outcome::Applied(Err(e))) => {
                self.stage = Stage::Edit;
                self.replan(); // the disk may have changed; this also clears the previous error
                self.error = Some(e);
            }
            Some(Outcome::Undone(Ok(report))) => {
                if let Some(d) = &mut self.done {
                    d.restored = Some((report.restored, report.skipped.len()));
                }
                self.stage = Stage::Done;
            }
            Some(Outcome::Undone(Err(e))) => {
                self.stage = Stage::Done;
                self.error = Some(e);
            }
            None => {}
        }
        host.redraw();
    }

    fn event(&mut self, ev: Ev, host: &mut Host) {
        if self.stage == Stage::Working {
            return;
        }
        match ev {
            Ev::Click(ID_CLOSE) | Ev::Click(ID_CANCEL) => host.close = true,
            Ev::Click(ID_UNDO) if self.stage == Stage::Done => self.undo(host),
            Ev::Click(ID_APPLY) => self.apply(host),
            Ev::Click(id) if self.stage == Stage::Edit => self.click_edit(id),
            Ev::Char(c) if self.stage == Stage::Edit => {
                if host.focus.and_then(|f| self.field_mut(f)).is_some_and(|f| f.on_char(c)) {
                    self.replan();
                }
            }
            Ev::Key { vk, .. } => self.key(vk, host.focus),
            Ev::Down { y, id: ID_SCROLL, .. } => {
                self.scrolling = true;
                self.scroll_to_y(y);
            }
            Ev::Move { y, down: true, .. } if self.scrolling => self.scroll_to_y(y),
            Ev::Up => self.scrolling = false,
            _ => {}
        }
    }

    fn draw(&mut self, pen: &mut Pen) {
        let l = pen.lang;
        pen.text(tr(l, "ren-title"), 24.0, 34.0, 19.0, pen.pal.text);
        let n = self.paths.len();
        let count = plural(l, "ren-one", "ren-many", n);
        pen.text_r(&count, W - 24.0, 36.0, 12.0, pen.pal.dim);
        if let Some(dir) = self.paths.first().and_then(|p| p.parent()) {
            let shown = pen.fit(&dir.to_string_lossy(), 12.0, W - 300.0);
            pen.text(&shown, 24.0, 56.0, 12.0, pen.pal.dim);
        }
        self.draw_list(pen);
        if self.stage == Stage::Done || self.done.is_some() {
            self.draw_done(pen);
        } else {
            self.draw_controls(pen);
        }
        if self.stage == Stage::Working {
            let msg = tr(l, if self.done.is_some() { "ren-restoring" } else { "ren-working" });
            pen.busy(LIST, msg);
        }
    }
}

const OLD_X: f32 = LIST[0] + 16.0;
const OLD_W: f32 = 232.0;
const ARROW_X: f32 = OLD_X + OLD_W + 12.0;
const NEW_X: f32 = ARROW_X + 28.0;
const NEW_W: f32 = LIST[0] + LIST[2] - 22.0 - NEW_X;

impl RenameDialog {
    fn draw_list(&self, pen: &mut Pen) {
        let l = pen.lang;
        let pal = pen.pal;
        pen.fill(LIST, RADIUS_CARD, pal.card);
        pen.outline(LIST, RADIUS_CARD, pal.border);
        pen.text(tr(l, "ren-col-old"), OLD_X, LIST[1] + HEAD_H / 2.0 + 0.5, 11.5, pal.dim);
        pen.text(tr(l, "ren-col-new"), NEW_X, LIST[1] + HEAD_H / 2.0 + 0.5, 11.5, pal.dim);
        let y0 = LIST[1] + HEAD_H;
        pen.line((LIST[0] + 1.0, y0), (LIST[0] + LIST[2] - 1.0, y0), pal.border, 1.0);
        let faint = pal.dim.fade(0.75);
        for (r, it) in self.items.iter().skip(self.top).take(VIS).enumerate() {
            let y = y0 + r as f32 * ROW_H;
            let cy = y + ROW_H / 2.0 + 0.5;
            if (self.top + r) % 2 == 1 {
                pen.fill([LIST[0] + 1.0, y, LIST[2] - 2.0, ROW_H], 0.0, pal.border.fade(0.2));
            }
            let (old_c, new_c) = match &it.status {
                Status::Ok => (pal.dim, pal.text),
                Status::Unchanged => (faint, faint),
                Status::Conflict(_) | Status::Invalid(_) => (pal.dim, RED),
            };
            let old = pen.fit(&it.old_name(), 12.5, OLD_W);
            pen.text(&old, OLD_X, cy, 12.5, old_c);
            pen.line((ARROW_X, cy), (ARROW_X + 12.0, cy), old_c, 1.2);
            pen.line((ARROW_X + 8.0, cy - 3.5), (ARROW_X + 12.0, cy), old_c, 1.2);
            pen.line((ARROW_X + 8.0, cy + 3.5), (ARROW_X + 12.0, cy), old_c, 1.2);
            let mut room = NEW_W;
            if let Status::Conflict(code) | Status::Invalid(code) = &it.status {
                let why = reason_text(l, code);
                let w = pen.width(&why, 11.0).min(NEW_W * 0.55);
                let why = pen.fit(&why, 11.0, w);
                pen.text_r(&why, NEW_X + NEW_W, cy, 11.0, RED);
                room = NEW_W - w - 12.0;
            }
            let new = pen.fit(&it.new_name(), 12.5, room);
            pen.text(&new, NEW_X, cy, 12.5, new_c);
        }
        let n = self.items.len();
        if n > VIS {
            let (track_y, track_h) = (y0, ROW_H * VIS as f32);
            let thumb_h = (track_h * VIS as f32 / n as f32).max(28.0);
            let thumb_y = track_y + (track_h - thumb_h) * self.top as f32 / self.max_top() as f32;
            let x = LIST[0] + LIST[2] - 12.0;
            pen.fill([x, track_y, 4.0, track_h], 2.0, pal.border.fade(0.5));
            pen.fill([x, thumb_y, 4.0, thumb_h], 2.0, pal.dim.fade(0.7));
            pen.hit([x - 8.0, track_y, 20.0, track_h], ID_SCROLL, Kind::Drag);
        }
        if self.stage == Stage::Edit {
            let (text, color) = if self.problems > 0 {
                (plural(l, "ren-problem-one", "ren-problems", self.problems), RED)
            } else if self.changing > 0 {
                (fill(tr(l, "ren-summary"), &[&self.changing.to_string(), &n.to_string()]), pal.dim)
            } else {
                (tr(l, "ren-nothing").to_string(), pal.dim)
            };
            pen.text(&pen.fit(&text, 12.5, LIST[2]), LIST[0], LIST[1] + LIST[3] + 24.0, 12.5, color);
        }
    }

    fn draw_controls(&self, pen: &mut Pen) {
        let l = pen.lang;
        let pal = pen.pal;
        let half = (PW - 8.0) / 2.0;
        let right = PX + half + 8.0;

        let reset = tr(l, "reset-btn");
        let rw = pen.width(reset, 12.0);
        let hot = pen.hovered(ID_RESET);
        let link = if hot { pal.accent.mix(pal.text, 0.35) } else { pal.accent };
        pen.text_r(reset, PX + PW - 4.0, 71.0, 12.0, link);
        let hit = [PX + PW - rw - 8.0, 60.0, rw + 8.0, 22.0];
        pen.hit(hit, ID_RESET, Kind::Button);
        if pen.focus == Some(ID_RESET) {
            pen.ring(hit, 4.0, pal.text, 2.0);
        }

        pen.label(tr(l, "ren-find"), PX, 88.0);
        pen.label(tr(l, "ren-replace"), right, 88.0);
        pen.field([PX, 108.0, half, 30.0], &self.find, ID_FIND, "");
        pen.field([right, 108.0, half, 30.0], &self.replace, ID_REPLACE, "");

        pen.label(tr(l, "ren-case"), PX, 150.0);
        let case = [tr(l, "ren-case-keep"), tr(l, "ren-case-lower"), tr(l, "ren-case-upper"), tr(l, "ren-case-title")];
        pen.seg([PX, 170.0, PW, 28.0], &case, self.case, ID_CASE);

        pen.label(tr(l, "ren-spaces"), PX, 210.0);
        let spaces = [tr(l, "ren-sp-keep"), "_", "-", tr(l, "ren-sp-remove")];
        pen.seg([PX, 230.0, PW, 28.0], &spaces, self.spaces, ID_SPACES);

        pen.label(tr(l, "ren-prefix"), PX, 270.0);
        pen.label(tr(l, "ren-suffix"), right, 270.0);
        pen.field([PX, 290.0, half, 30.0], &self.prefix, ID_PREFIX, "");
        pen.field([right, 290.0, half, 30.0], &self.suffix, ID_SUFFIX, "");

        pen.check([PX, 330.0, PW, 24.0], tr(l, "ren-accents"), self.accents, ID_ACCENTS);
        pen.check([PX, 356.0, PW, 24.0], tr(l, "ren-ext-lower"), self.ext_lower, ID_EXT);

        let mut y = 392.0;
        pen.check([PX, y, 112.0, 26.0], tr(l, "ren-numbering"), self.numbering, ID_NUMBERING);
        if self.numbering {
            let pos = [tr(l, "ren-pos-before"), tr(l, "ren-pos-after"), tr(l, "ren-pos-instead")];
            pen.seg([PX + 120.0, y, PW - 120.0, 26.0], &pos, self.num_pos, ID_NUMPOS);
            pen.field([PX, y + 34.0, half, 30.0], &self.start, ID_START, tr(l, "ren-start"));
            pen.field([right, y + 34.0, half, 30.0], &self.digits, ID_DIGITS, tr(l, "ren-digits"));
        }
        y += if self.numbering { 76.0 } else { 38.0 };
        pen.check([PX, y, 112.0, 26.0], tr(l, "ren-date"), self.date, ID_DATE);
        if self.date {
            let pos = [tr(l, "ren-pos-before"), tr(l, "ren-pos-after")];
            pen.seg([PX + 120.0, y, PW - 120.0, 26.0], &pos, self.date_pos, ID_DATEPOS);
            let src = [tr(l, "ren-src-modified"), tr(l, "ren-src-created"), tr(l, "ren-src-taken")];
            pen.seg([PX, y + 34.0, PW, 26.0], &src, self.date_src, ID_DATESRC);
            let fmt = ["2024-05-01", "20240501", tr(l, "ren-fmt-time")];
            let cw = (PW - 12.0) / 3.0;
            for (i, label) in fmt.iter().enumerate() {
                let r = [PX + i as f32 * (cw + 6.0), y + 66.0, cw, 26.0];
                pen.chip(r, label, i == self.date_fmt, ID_DATEFMT + i as u32);
            }
        }

        let by = H - 56.0;
        self.error_line(pen, by - 14.0);
        pen.button([PX, by, 96.0, 36.0], tr(l, "cancel"), ID_CANCEL, Style::Secondary, true);
        let label = plural(l, "ren-do-one", "ren-do", self.changing);
        pen.button([PX + 104.0, by, PW - 104.0, 36.0], &label, ID_APPLY, Style::Primary, self.can_apply());
    }

    fn error_line(&self, pen: &mut Pen, y: f32) {
        if let Some(e) = &self.error {
            let msg = fill(tr(pen.lang, "failed"), &[e]);
            let shown = pen.fit(&msg, 11.5, PW);
            pen.text(&shown, PX, y, 11.5, RED);
        }
    }

    /// The panel that replaces the controls once the files are renamed (and again after Undo).
    fn draw_done(&self, pen: &mut Pen) {
        let l = pen.lang;
        let pal = pen.pal;
        let Some(done) = &self.done else { return };
        let card = [PX, 84.0, PW, 150.0];
        pen.fill(card, RADIUS_CARD, pal.card);
        pen.outline(card, RADIUS_CARD, pal.border);
        let (title, note) = match done.restored {
            None => (plural(l, "ren-done-one", "ren-done", done.count), tr(l, "ren-undo-hint").to_string()),
            Some((back, 0)) => (plural(l, "ren-restored-one", "ren-restored", back), String::new()),
            Some((back, left)) => {
                (plural(l, "ren-restored-one", "ren-restored", back), fill(tr(l, "ren-left"), &[&left.to_string()]))
            }
        };
        pen.text(&pen.fit(&title, 17.0, PW - 32.0), PX + 16.0, 116.0, 17.0, pal.text);
        for (i, line) in note.split('|').filter(|s| !s.is_empty()).enumerate() {
            pen.text(&pen.fit(line, 12.0, PW - 32.0), PX + 16.0, 146.0 + i as f32 * 19.0, 12.0, pal.dim);
        }
        let by = H - 56.0;
        self.error_line(pen, by - 14.0);
        if done.restored.is_none() {
            pen.button([PX, by, 96.0, 36.0], tr(l, "undo"), ID_UNDO, Style::Secondary, self.stage == Stage::Done);
        }
        pen.button([PX + 104.0, by, PW - 104.0, 36.0], tr(l, "ren-close"), ID_CLOSE, Style::Primary, true);
    }
}

impl RenameDialog {
    /// A dialog over generated sample files, for `radial --dialog rename` previews; Apply there renames only
    /// the sample files and writes its undo file next to them.
    pub(super) fn preview() -> RenameDialog {
        let dir = std::env::temp_dir().join("radial-preview").join("rename");
        let _ = std::fs::remove_dir_all(&dir);
        let _ = std::fs::create_dir_all(&dir);
        let mut paths = Vec::new();
        for i in 0..24 {
            paths.push(dir.join(format!("IMG_{:04}.JPG", 412 + i)));
        }
        for n in [
            "Ñandú Ç informe final.docx",
            "Presupuesto 2024 (copia).xlsx",
            "don't STOP me.txt",
            "Factura  marzo.pdf",
            "notas reunión.txt",
            "Vacaciones",
        ] {
            paths.push(dir.join(n));
        }
        for p in &paths {
            if p.file_name().is_some_and(|n| n == "Vacaciones") {
                let _ = std::fs::create_dir_all(p);
            } else {
                let _ = std::fs::write(p, p.to_string_lossy().as_bytes());
            }
        }
        let mut d = RenameDialog::new(paths);
        d.undo_dir = Some(dir.join("undo"));
        d
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::dialogs::Cursor;
    use crate::render::Face;
    use crate::settings::palette;
    use crate::theme::{FontKind, Rgba};
    use std::os::windows::fs::OpenOptionsExt;
    use std::time::{Duration, Instant};
    use tiny_skia::Pixmap;

    struct Tmp(PathBuf);

    impl Tmp {
        fn new() -> Tmp {
            use std::sync::atomic::{AtomicU32, Ordering};
            static N: AtomicU32 = AtomicU32::new(0);
            let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos();
            let p = std::env::temp_dir().join(format!(
                "radial-rename-dlg-{}-{}-{nanos}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&p).unwrap();
            Tmp(p)
        }

        fn files(&self, names: &[&str]) -> Vec<PathBuf> {
            names
                .iter()
                .map(|n| {
                    let p = self.0.join(n);
                    std::fs::write(&p, n).unwrap();
                    p
                })
                .collect()
        }

        fn names(&self) -> Vec<String> {
            let mut v: Vec<String> = std::fs::read_dir(&self.0)
                .unwrap()
                .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
                .filter(|n| n != "undo")
                .collect();
            v.sort();
            v
        }
    }

    impl Drop for Tmp {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

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

    /// A dialog over `names` in a fresh folder; undo files go inside the folder, never to %APPDATA%.
    fn dialog(names: &[&str]) -> (Tmp, RenameDialog, Host) {
        let t = Tmp::new();
        let mut d = RenameDialog::new(t.files(names));
        d.undo_dir = Some(t.0.join("undo"));
        (t, d, host())
    }

    fn type_in(d: &mut RenameDialog, h: &mut Host, id: u32, text: &str) {
        h.focus = Some(id);
        d.event(Ev::Click(id), h);
        for c in text.chars() {
            d.event(Ev::Char(c), h);
        }
    }

    fn run_jobs(d: &mut RenameDialog, h: &mut Host) {
        for job in std::mem::take(&mut h.jobs) {
            let r = job();
            d.job(r, h);
        }
    }

    fn news(d: &RenameDialog) -> Vec<String> {
        d.items.iter().map(Item::new_name).collect()
    }

    fn render(d: &mut RenameDialog, h: &mut Host, dark: bool) -> Pixmap {
        let mut pm = Pixmap::new(W as u32, H as u32).unwrap();
        let mut face = Face::load(FontKind::Variable, 520.0).expect("Segoe UI Variable");
        let pal = palette(dark, Rgba(0, 120, 212, 255));
        pm.fill(tiny_skia::Color::from_rgba8(pal.bg.0, pal.bg.1, pal.bg.2, 255));
        h.hits.clear();
        h.order.clear();
        let mut pen = Pen {
            pm: &mut pm,
            face: &mut face,
            pal: &pal,
            s: 1.0,
            hits: &mut h.hits,
            order: &mut h.order,
            hot: None,
            pressed: None,
            focus: h.focus,
            lang: h.lang,
            dark,
            time: 0.0,
        };
        d.draw(&mut pen);
        pm
    }

    /// Pixels of the danger colour inside `r`.
    fn red_pixels(pm: &Pixmap, r: R) -> usize {
        let (x0, y0, x1, y1) = (r[0] as u32, r[1] as u32, (r[0] + r[2]) as u32, (r[1] + r[3]) as u32);
        let mut n = 0;
        for y in y0..y1.min(pm.height()) {
            for x in x0..x1.min(pm.width()) {
                let p = pm.pixel(x, y).unwrap();
                if p.red() >= 0xb0 && p.red() as i32 - p.green() as i32 > 60 && p.red() as i32 - p.blue() as i32 > 40 {
                    n += 1;
                }
            }
        }
        n
    }

    fn has_hit(h: &Host, id: u32) -> bool {
        h.hits.iter().any(|x| x.id == id)
    }

    // ---------------------------------------------------------------------------------------------- the rule

    #[test]
    fn paths_are_sorted_like_a_file_manager_and_listed_once() {
        let t = Tmp::new();
        let mut ps = t.files(&["img10.jpg", "Img2.jpg", "img1.jpg"]);
        ps.push(ps[0].clone());
        let d = RenameDialog::new(ps);
        let names: Vec<String> = d.items.iter().map(Item::old_name).collect();
        assert_eq!(names, ["img1.jpg", "Img2.jpg", "img10.jpg"]);
    }

    #[test]
    fn typing_in_find_and_replace_updates_the_preview_at_once() {
        let (_t, mut d, mut h) = dialog(&["IMG 1.jpg", "IMG 2.jpg", "other.jpg"]);
        assert_eq!((d.changing, d.problems), (0, 0), "no rule, nothing changes");
        type_in(&mut d, &mut h, ID_FIND, "img");
        assert_eq!(news(&d), [" 1.jpg", " 2.jpg", "other.jpg"], "find alone deletes what it finds");
        type_in(&mut d, &mut h, ID_REPLACE, "photo");
        assert_eq!(news(&d), ["photo 1.jpg", "photo 2.jpg", "other.jpg"]);
        assert_eq!((d.changing, d.problems), (2, 0));
        h.focus = Some(ID_FIND);
        for _ in 0..3 {
            d.event(Ev::Key { vk: 0x08, ctrl: false }, &mut h); // backspace until the field is empty
        }
        assert_eq!((d.changing, d.problems), (0, 0));
    }

    #[test]
    fn segments_and_checks_change_the_rule() {
        let (_t, mut d, mut h) = dialog(&["Ñandú ç.TXT"]);
        d.event(Ev::Click(ID_CASE + 2), &mut h); // UPPER
        d.event(Ev::Click(ID_SPACES + 1), &mut h); // _
        d.event(Ev::Click(ID_ACCENTS), &mut h);
        d.event(Ev::Click(ID_EXT), &mut h);
        assert_eq!(news(&d), ["NANDU_C.txt"]);
        d.event(Ev::Click(ID_CASE + 3), &mut h); // Title
        d.event(Ev::Click(ID_SPACES + 3), &mut h); // remove
        assert_eq!(news(&d), ["Nanduc.txt"]);
        d.event(Ev::Click(ID_ACCENTS), &mut h); // off again
        d.event(Ev::Click(ID_CASE), &mut h); // keep
        d.event(Ev::Click(ID_SPACES + 2), &mut h); // -
        assert_eq!(news(&d), ["Ñandú-ç.txt"]);
    }

    #[test]
    fn prefix_and_suffix_fields_wrap_the_name() {
        let (_t, mut d, mut h) = dialog(&["a.txt"]);
        type_in(&mut d, &mut h, ID_PREFIX, "pre_");
        type_in(&mut d, &mut h, ID_SUFFIX, "_post");
        assert_eq!(news(&d), ["pre_a_post.txt"]);
    }

    #[test]
    fn numbering_controls_set_start_digits_and_position() {
        let (_t, mut d, mut h) = dialog(&["a.txt", "b.txt"]);
        d.event(Ev::Click(ID_NUMBERING), &mut h);
        assert_eq!(news(&d), ["001 a.txt", "002 b.txt"]);
        type_in(&mut d, &mut h, ID_START, "9");
        type_in(&mut d, &mut h, ID_DIGITS, "2");
        assert_eq!(news(&d), ["09 a.txt", "10 b.txt"]);
        d.event(Ev::Click(ID_NUMPOS + 1), &mut h); // after
        assert_eq!(news(&d), ["a 09.txt", "b 10.txt"]);
        d.event(Ev::Click(ID_NUMPOS + 2), &mut h); // instead
        assert_eq!(news(&d), ["09.txt", "10.txt"]);
        d.event(Ev::Click(ID_NUMBERING), &mut h); // off
        assert_eq!(news(&d), ["a.txt", "b.txt"]);
        d.event(Ev::Click(ID_NUMPOS), &mut h); // before again
        type_in(&mut d, &mut h, ID_DIGITS, "0");
        d.event(Ev::Click(ID_NUMBERING), &mut h);
        assert_eq!(news(&d)[0], "9 a.txt", "zero digits means no padding, not an empty name");
    }

    #[test]
    fn date_controls_pick_source_format_and_position() {
        let (_t, mut d, mut h) = dialog(&["x.txt"]);
        d.event(Ev::Click(ID_DATE), &mut h);
        let name = news(&d).remove(0);
        assert!(name.len() == "2024-05-01 x.txt".len() && name.ends_with(" x.txt"), "{name}");
        d.event(Ev::Click(ID_DATEFMT + 1), &mut h); // compact
        d.event(Ev::Click(ID_DATEPOS + 1), &mut h); // after
        d.event(Ev::Click(ID_DATESRC + 2), &mut h); // taken: no EXIF in a text file, falls back
        let name = news(&d).remove(0);
        let stamp = name.strip_prefix("x ").and_then(|r| r.strip_suffix(".txt")).expect(&name);
        assert!(stamp.len() == 8 && stamp.bytes().all(|b| b.is_ascii_digit()), "{name}");
        d.event(Ev::Click(ID_DATEFMT + 2), &mut h); // with time
        assert!(news(&d)[0].contains('_'), "{}", news(&d)[0]);
        d.event(Ev::Click(ID_DATE), &mut h);
        assert_eq!(news(&d), ["x.txt"]);
    }

    #[test]
    fn reset_puts_every_control_back() {
        let (_t, mut d, mut h) = dialog(&["Ab c.TXT"]);
        type_in(&mut d, &mut h, ID_FIND, "b");
        type_in(&mut d, &mut h, ID_PREFIX, "p");
        for id in [ID_CASE + 2, ID_SPACES + 1, ID_ACCENTS, ID_EXT, ID_NUMBERING, ID_NUMPOS + 2, ID_DATE, ID_DATEFMT + 1]
        {
            d.event(Ev::Click(id), &mut h);
        }
        assert_eq!(d.changing, 1);
        d.event(Ev::Click(ID_RESET), &mut h);
        assert_eq!((d.changing, d.problems), (0, 0));
        assert_eq!(news(&d), ["Ab c.TXT"]);
        assert!(d.find.text.is_empty() && d.prefix.text.is_empty());
        assert_eq!((d.start.text.as_str(), d.digits.text.as_str()), ("1", "3"));
        assert!(!d.numbering && !d.date && !d.accents && !d.ext_lower);
        assert_eq!((d.case, d.spaces, d.num_pos, d.date_fmt), (0, 0, 0, 0));
    }

    // ---------------------------------------------------------------------------------------------- apply and undo

    #[test]
    fn rename_stays_off_while_a_name_conflicts_and_works_once_it_is_fixed() {
        let (t, mut d, mut h) = dialog(&["a-1.txt", "a_1.txt", "b.txt"]);
        type_in(&mut d, &mut h, ID_FIND, "-");
        type_in(&mut d, &mut h, ID_REPLACE, "_");
        assert_eq!((d.changing, d.problems), (0, 1), "a-1 would collide with a_1");
        assert!(!d.can_apply());
        d.event(Ev::Click(ID_APPLY), &mut h);
        assert!(h.jobs.is_empty(), "nothing is started");
        render(&mut d, &mut h, true);
        assert!(!has_hit(&h, ID_APPLY), "the button is disabled, so it takes no clicks");
        type_in(&mut d, &mut h, ID_REPLACE, "+");
        assert_eq!((d.changing, d.problems), (1, 0));
        assert_eq!(t.names(), ["a-1.txt", "a_1.txt", "b.txt"], "previewing never touches the disk");
        render(&mut d, &mut h, true);
        assert!(has_hit(&h, ID_APPLY));
    }

    #[test]
    fn apply_renames_on_disk_then_undo_brings_the_names_back() {
        let (t, mut d, mut h) = dialog(&["a b.txt", "c d.txt", "keep.txt"]);
        d.event(Ev::Click(ID_SPACES + 1), &mut h);
        assert!(d.can_apply());
        d.event(Ev::Click(ID_APPLY), &mut h);
        assert_eq!(h.jobs.len(), 1, "the rename runs off the UI thread");
        assert!(d.busy() && d.saving(), "the window cannot be closed mid-rename");
        run_jobs(&mut d, &mut h);
        assert_eq!(t.names(), ["a_b.txt", "c_d.txt", "keep.txt"]);
        assert_eq!(d.stage, Stage::Done);
        assert!(!d.busy() && !d.saving());
        assert_eq!(d.done.as_ref().map(|x| x.count), Some(2));
        let undo_files: Vec<_> = std::fs::read_dir(t.0.join("undo")).unwrap().collect();
        assert_eq!(undo_files.len(), 1, "an undo file was written");
        d.event(Ev::Click(ID_UNDO), &mut h);
        assert!(d.busy());
        run_jobs(&mut d, &mut h);
        assert_eq!(t.names(), ["a b.txt", "c d.txt", "keep.txt"]);
        assert_eq!(d.done.as_ref().and_then(|x| x.restored), Some((2, 0)));
        assert!(!h.close, "the window stays open for the user to read the result");
        d.event(Ev::Click(ID_CLOSE), &mut h);
        assert!(h.close);
    }

    #[test]
    fn undo_reports_files_it_had_to_leave_alone() {
        let (t, mut d, mut h) = dialog(&["a b.txt", "c d.txt"]);
        d.event(Ev::Click(ID_SPACES + 1), &mut h);
        d.event(Ev::Click(ID_APPLY), &mut h);
        run_jobs(&mut d, &mut h);
        std::fs::write(t.0.join("a_b.txt"), "edited meanwhile").unwrap();
        d.event(Ev::Click(ID_UNDO), &mut h);
        run_jobs(&mut d, &mut h);
        assert_eq!(d.done.as_ref().and_then(|x| x.restored), Some((1, 1)));
        assert_eq!(t.names(), ["a_b.txt", "c d.txt"]);
    }

    #[test]
    fn a_failed_rename_returns_to_editing_with_the_reason_and_nothing_changed() {
        let (t, mut d, mut h) = dialog(&["a.txt", "b.txt", "c.txt"]);
        type_in(&mut d, &mut h, ID_PREFIX, "new-");
        let hold = std::fs::OpenOptions::new().read(true).share_mode(0).open(t.0.join("b.txt")).unwrap();
        d.event(Ev::Click(ID_APPLY), &mut h);
        run_jobs(&mut d, &mut h);
        drop(hold);
        assert_eq!(d.stage, Stage::Edit);
        assert!(d.error.as_deref().is_some_and(|e| e.contains("b.txt")), "{:?}", d.error);
        assert_eq!(t.names(), ["a.txt", "b.txt", "c.txt"]);
        assert!(d.can_apply(), "the user can try again");
        type_in(&mut d, &mut h, ID_SUFFIX, "!");
        assert!(d.error.is_none(), "the old error goes away once the rule changes");
    }

    #[test]
    fn enter_applies_when_allowed_and_closes_once_done() {
        let (_t, mut d, mut h) = dialog(&["a.txt"]);
        d.default_action(&mut h);
        assert!(h.jobs.is_empty(), "nothing to rename yet");
        type_in(&mut d, &mut h, ID_PREFIX, "x");
        d.default_action(&mut h);
        assert_eq!(h.jobs.len(), 1);
        run_jobs(&mut d, &mut h);
        d.default_action(&mut h);
        assert!(h.close);
    }

    #[test]
    fn cancel_closes_without_renaming() {
        let (t, mut d, mut h) = dialog(&["a.txt"]);
        type_in(&mut d, &mut h, ID_PREFIX, "x");
        d.event(Ev::Click(ID_CANCEL), &mut h);
        assert!(h.close);
        assert_eq!(t.names(), ["a.txt"]);
    }

    // ---------------------------------------------------------------------------------------------- the list

    fn many(n: usize) -> (Tmp, RenameDialog, Host) {
        let t = Tmp::new();
        let names: Vec<String> = (0..n).map(|i| format!("f{i:04}.txt")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut d = RenameDialog::new(t.files(&refs));
        d.undo_dir = Some(t.0.join("undo"));
        (t, d, host())
    }

    fn key(d: &mut RenameDialog, h: &mut Host, vk: u32) {
        d.event(Ev::Key { vk, ctrl: false }, h);
    }

    #[test]
    fn the_list_scrolls_by_keys_and_never_past_its_ends() {
        let (_t, mut d, mut h) = many(40);
        assert_eq!(d.top, 0);
        key(&mut d, &mut h, VK_UP);
        assert_eq!(d.top, 0);
        key(&mut d, &mut h, VK_DOWN);
        assert_eq!(d.top, 1);
        key(&mut d, &mut h, VK_NEXT);
        assert_eq!(d.top, 1 + VIS);
        key(&mut d, &mut h, VK_NEXT);
        key(&mut d, &mut h, VK_NEXT);
        assert_eq!(d.top, 40 - VIS, "stops with the last row at the bottom");
        key(&mut d, &mut h, VK_DOWN);
        assert_eq!(d.top, 40 - VIS);
        key(&mut d, &mut h, VK_PRIOR);
        assert_eq!(d.top, 40 - 2 * VIS);
        key(&mut d, &mut h, VK_END);
        assert_eq!(d.top, 40 - VIS);
        key(&mut d, &mut h, VK_HOME);
        assert_eq!(d.top, 0);
        let (_t2, mut few, mut h2) = many(5);
        key(&mut few, &mut h2, VK_END);
        key(&mut few, &mut h2, VK_DOWN);
        assert_eq!(few.top, 0, "a short list does not scroll");
    }

    #[test]
    fn arrow_keys_scroll_even_while_a_field_has_the_focus_and_home_end_stay_with_the_field() {
        let (_t, mut d, mut h) = many(40);
        type_in(&mut d, &mut h, ID_PREFIX, "abc");
        key(&mut d, &mut h, VK_DOWN);
        assert_eq!((d.top, d.prefix.text.as_str()), (1, "abc"));
        key(&mut d, &mut h, VK_HOME);
        assert_eq!(d.top, 1, "Home moves the caret in a field");
        assert_eq!(d.prefix.caret, 0);
    }

    #[test]
    fn the_scroll_bar_can_be_dragged_and_clicked() {
        let (_t, mut d, mut h) = many(100);
        render(&mut d, &mut h, true);
        let track = h.rect(ID_SCROLL).expect("the bar registers a drag area");
        let (x, top, bottom) = (track[0] + 8.0, track[1], track[1] + track[3]);
        d.event(Ev::Down { x, y: bottom, id: ID_SCROLL }, &mut h);
        assert_eq!(d.top, 100 - VIS, "pressing at the bottom jumps to the end");
        d.event(Ev::Move { x, y: (top + bottom) / 2.0, down: true }, &mut h);
        assert!(d.top > 30 && d.top < 70, "dragging follows the pointer: {}", d.top);
        d.event(Ev::Move { x, y: top - 50.0, down: true }, &mut h);
        assert_eq!(d.top, 0);
        d.event(Ev::Up, &mut h);
        d.event(Ev::Move { x, y: bottom, down: true }, &mut h);
        assert_eq!(d.top, 0, "moving after release does nothing");
        let (_t2, mut few, mut h2) = many(5);
        render(&mut few, &mut h2, true);
        assert!(h2.rect(ID_SCROLL).is_none(), "no bar when everything fits");
    }

    #[test]
    fn five_thousand_files_keep_the_preview_instant() {
        let t = Tmp::new();
        let names: Vec<String> = (0..5000).map(|i| format!("IMG {i:05}.JPG")).collect();
        let refs: Vec<&str> = names.iter().map(String::as_str).collect();
        let mut d = RenameDialog::new(t.files(&refs));
        let mut h = host();
        let started = Instant::now();
        type_in(&mut d, &mut h, ID_FIND, "img");
        type_in(&mut d, &mut h, ID_REPLACE, "photo");
        d.event(Ev::Click(ID_NUMBERING), &mut h);
        d.event(Ev::Click(ID_CASE + 1), &mut h);
        let per_change = started.elapsed() / 10; // 3 + 5 characters and 2 clicks
        assert_eq!(d.items.len(), 5000);
        assert_eq!((d.changing, d.problems), (5000, 0));
        assert_eq!(d.items[4999].new_name(), "5000 photo 04999.JPG");
        assert!(per_change < Duration::from_millis(250), "a change took {per_change:?} on average");
        h.focus = None;
        key(&mut d, &mut h, VK_END);
        assert_eq!(d.top, 5000 - VIS);
        let drawn = Instant::now();
        render(&mut d, &mut h, true);
        assert!(drawn.elapsed() < Duration::from_millis(400), "a frame took {:?}", drawn.elapsed());
    }

    // ---------------------------------------------------------------------------------------------- drawing

    #[test]
    fn problem_rows_are_drawn_in_the_danger_colour_and_clean_ones_are_not() {
        let (_t, mut d, mut h) = dialog(&["a-1.txt", "a_1.txt"]);
        let rows = [LIST[0], LIST[1] + HEAD_H, LIST[2], ROW_H * 3.0];
        let clean = render(&mut d, &mut h, true);
        assert_eq!(red_pixels(&clean, rows), 0);
        type_in(&mut d, &mut h, ID_FIND, "-");
        type_in(&mut d, &mut h, ID_REPLACE, "_");
        for dark in [true, false] {
            let pm = render(&mut d, &mut h, dark);
            assert!(red_pixels(&pm, rows) > 40, "dark={dark}: the conflicting row is red");
            assert!(red_pixels(&pm, [PX, 84.0, PW, 460.0]) == 0, "the controls stay neutral");
        }
        h.lang = Lang::Es;
        render(&mut d, &mut h, true);
    }

    #[test]
    fn every_state_draws_in_both_languages_and_both_themes() {
        let (_t, mut d, mut h) = dialog(&["a b.txt", "c d.txt"]);
        for lang in [Lang::En, Lang::Es] {
            h.lang = lang;
            for dark in [true, false] {
                render(&mut d, &mut h, dark);
            }
        }
        for id in [ID_NUMBERING, ID_DATE] {
            d.event(Ev::Click(id), &mut h);
        }
        render(&mut d, &mut h, true);
        d.event(Ev::Click(ID_SPACES + 1), &mut h);
        d.event(Ev::Click(ID_APPLY), &mut h);
        assert_eq!(d.stage, Stage::Working);
        render(&mut d, &mut h, true);
        run_jobs(&mut d, &mut h);
        assert_eq!(d.stage, Stage::Done);
        for lang in [Lang::En, Lang::Es] {
            h.lang = lang;
            render(&mut d, &mut h, false);
            assert!(has_hit(&h, ID_UNDO) && has_hit(&h, ID_CLOSE));
        }
        d.event(Ev::Click(ID_UNDO), &mut h);
        run_jobs(&mut d, &mut h);
        render(&mut d, &mut h, true);
        assert!(!has_hit(&h, ID_UNDO), "once undone there is nothing left to undo");
    }

    #[test]
    fn every_reason_code_has_text_in_both_languages() {
        for code in
            ["missing", "empty", "illegal:?", "illegal:\u{7}", "reserved", "trailing", "long", "duplicate", "exists"]
        {
            for lang in [Lang::En, Lang::Es] {
                let t = reason_text(lang, code);
                assert!(!t.is_empty() && t != code, "{code} in {lang:?}: {t}");
            }
        }
        assert_eq!(reason_text(Lang::En, "illegal:?"), "Cannot contain ?");
        assert_eq!(reason_text(Lang::En, "something new"), "something new", "unknown codes are shown as they are");
    }
}
