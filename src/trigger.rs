//! Drag + Shift detection as a pure state machine fed by the low-level hooks.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Pt {
    pub x: i32,
    pub y: i32,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Act {
    None,
    /// Open the menu centred here.
    Show(Pt),
    /// Close the menu (shift released or button up).
    Hide,
}

/// Pixels the cursor must travel with the button held before it counts as a drag.
pub const DRAG_THRESHOLD: i32 = 6;

#[derive(Default)]
pub struct Tracker {
    down: Option<Pt>,
    eligible: bool,
    dragging: bool,
    shown: bool,
    pos: Option<Pt>,
    shift: bool,
}

impl Tracker {
    /// Left button pressed. `eligible` tells whether it was pressed over a file surface.
    pub fn button_down(&mut self, p: Pt, eligible: bool) -> Act {
        self.down = Some(p);
        self.eligible = eligible;
        self.dragging = false;
        self.pos = Some(p);
        self.update()
    }

    pub fn moved(&mut self, p: Pt, shift: bool) -> Act {
        self.pos = Some(p);
        self.shift = shift;
        if let Some(d) = self.down {
            let (dx, dy) = ((p.x - d.x).abs(), (p.y - d.y).abs());
            self.dragging |= dx >= DRAG_THRESHOLD || dy >= DRAG_THRESHOLD;
        }
        self.update()
    }

    pub fn shift_changed(&mut self, shift: bool) -> Act {
        self.shift = shift;
        self.update()
    }

    pub fn button_up(&mut self) -> Act {
        self.down = None;
        self.dragging = false;
        self.update()
    }

    fn update(&mut self) -> Act {
        let want = self.down.is_some() && self.eligible && self.dragging && self.shift;
        match (want, self.shown, self.pos) {
            (true, false, Some(p)) => {
                self.shown = true;
                Act::Show(p)
            }
            (false, true, _) => {
                self.shown = false;
                Act::Hide
            }
            _ => Act::None,
        }
    }
}

/// Window classes whose children host draggable files (Explorer windows and the desktop).
pub fn is_file_surface(class: &str) -> bool {
    matches!(class, "CabinetWClass" | "ExploreWClass" | "Progman" | "WorkerW")
}

#[cfg(test)]
mod tests {
    use super::*;

    const A: Pt = Pt { x: 100, y: 100 };
    const FAR: Pt = Pt { x: 160, y: 100 };

    #[test]
    fn shift_drag_shows_menu_at_cursor() {
        let mut t = Tracker::default();
        t.button_down(A, true);
        assert_eq!(t.moved(FAR, true), Act::Show(FAR));
        assert_eq!(t.moved(Pt { x: 170, y: 100 }, true), Act::None); // already shown
    }

    #[test]
    fn plain_drag_does_nothing() {
        let mut t = Tracker::default();
        t.button_down(A, true);
        assert_eq!(t.moved(FAR, false), Act::None);
    }

    #[test]
    fn tiny_movement_is_a_click_not_a_drag() {
        let mut t = Tracker::default();
        t.button_down(A, true);
        assert_eq!(t.moved(Pt { x: 102, y: 101 }, true), Act::None);
    }

    #[test]
    fn pressing_shift_mid_drag_opens_at_last_position() {
        let mut t = Tracker::default();
        t.button_down(A, true);
        assert_eq!(t.moved(FAR, false), Act::None);
        assert_eq!(t.shift_changed(true), Act::Show(FAR));
    }

    #[test]
    fn releasing_shift_or_button_hides() {
        let mut t = Tracker::default();
        t.button_down(A, true);
        t.moved(FAR, true);
        assert_eq!(t.shift_changed(false), Act::Hide);
        assert_eq!(t.shift_changed(true), Act::Show(FAR)); // can reopen
        assert_eq!(t.button_up(), Act::Hide);
        assert_eq!(t.button_up(), Act::None);
    }

    #[test]
    fn shift_without_a_button_never_shows() {
        let mut t = Tracker::default();
        assert_eq!(t.shift_changed(true), Act::None);
        assert_eq!(t.moved(FAR, true), Act::None);
    }

    #[test]
    fn ineligible_surface_is_ignored() {
        let mut t = Tracker::default();
        t.button_down(A, false);
        assert_eq!(t.moved(FAR, true), Act::None);
    }

    #[test]
    fn explorer_classes_are_file_surfaces() {
        assert!(is_file_surface("CabinetWClass"));
        assert!(is_file_surface("Progman"));
        assert!(is_file_surface("WorkerW"));
        assert!(!is_file_surface("Chrome_WidgetWin_1"));
    }
}
