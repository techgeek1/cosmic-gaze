//! Raw button events to clicks: what counts as a deliberate press on a thing.
//!
//! Two rejections live here, and both exist because the label this crate produces is
//! "the user was looking at the element they clicked". A drag is a press whose target
//! is not where the pointer started, so it says nothing about gaze at the press. A
//! double click's second press is a real observation of the same target, so it is kept
//! and tagged rather than merged away: the eye may well have already left.

use evdev::KeyCode;

/// Longest a button may be held and still count as a click, seconds. Past this the
/// user is holding something (a drag, a press-and-hold menu) rather than clicking it.
pub const DRAG_HOLD_S: f64 = 0.400;

/// Furthest the pointer may travel between press and release and still count as a
/// click, logical pixels. Small enough to catch a real drag, large enough to survive
/// the hand tremor a firm click puts into a mouse.
pub const DRAG_MOVE_PX: f64 = 6.0;

/// Longest gap between two presses that still counts as one multi-click, seconds. The
/// same figure as the hold limit, which is roughly every desktop's double-click time.
pub const MULTI_GAP_S: f64 = 0.400;

/// A button that means "act on the thing under the pointer".
///
/// The wheel and the side buttons are ignored on purpose: a wheel event points at a
/// scrollable region rather than at an element, and the side buttons are back and
/// forward, which the user fires without looking at anything in particular.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Right,
}

/// One press or release as the evdev reader saw it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ButtonEvent {
    pub button     : Button,
    /// True for a press, false for a release. Autorepeat is dropped by the reader.
    pub pressed    : bool,
    /// Host time the reader saw the event, seconds since the collector started.
    pub t_s        : f64,
    /// Identifier of the screen capture the reader fired for this press, so the main
    /// thread can claim the frame later. `None` on a release.
    pub capture_id : Option<u64>,
}

/// What a press turned out to be once its release arrived.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PressKind {
    /// A click on whatever was under the pointer.
    Click,
    /// Held too long or moved too far: the press and the release are about different
    /// places, so neither is a gaze label.
    Drag,
}

/// Counts presses that arrive close enough together to be one gesture.
///
/// Kept per collector rather than per button: a left click straight after a right
/// click is a fresh gesture, and the counter is reset by the button changing.
#[derive(Clone, Copy, Debug, Default)]
pub struct MultiCounter {
    /// Time of the last press counted, or `None` before the first one.
    last_t_s  : Option<f64>,
    /// The button that press came from.
    last      : Option<Button>,
    /// How many presses the current run holds.
    count     : u32,
}

// --- Button ---

impl Button {
    /// The value written into the session file.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Left  => "left",
            Self::Right => "right",
        }
    }

    /// The button an evdev key code names, or `None` for a code this crate ignores.
    pub fn from_key(code: KeyCode) -> Option<Button> {
        match code {
            KeyCode::BTN_LEFT  => Some(Button::Left),
            KeyCode::BTN_RIGHT => Some(Button::Right),
            _                  => None,
        }
    }
}

// --- MultiCounter ---

impl MultiCounter {
    /// A counter that has seen nothing yet.
    pub fn new() -> MultiCounter {
        MultiCounter::default()
    }

    /// Records a press and returns its position in the current multi-click: 1 for a
    /// single, 2 for the second of a double, and so on.
    pub fn press(&mut self, button: Button, t_s: f64) -> u32 {
        let continues = self.last == Some(button)
            && self.last_t_s.is_some_and(|last| t_s - last <= MULTI_GAP_S);

        self.count    = if continues { self.count + 1 } else { 1 };
        self.last_t_s = Some(t_s);
        self.last     = Some(button);

        self.count
    }
}

// --- Classification ---

/// Whether a press was a click or a drag.
///
/// `hold_s` is release minus press and `moved_px` the distance the pointer covered
/// between the two, both measured by the caller against the pointer history.
pub fn classify(hold_s: f64, moved_px: f64) -> PressKind {
    if hold_s > DRAG_HOLD_S || moved_px > DRAG_MOVE_PX {
        return PressKind::Drag;
    }

    PressKind::Click
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_short_still_press_is_a_click() {
        assert_eq!(classify(0.08, 0.0), PressKind::Click);
        assert_eq!(classify(0.08, 5.9), PressKind::Click);

        // Exactly on both limits is still a click; the rules are strict inequalities.
        assert_eq!(classify(DRAG_HOLD_S, DRAG_MOVE_PX), PressKind::Click);
    }

    #[test]
    fn a_long_or_moving_press_is_a_drag() {
        assert_eq!(classify(0.41, 0.0), PressKind::Drag);
        assert_eq!(classify(0.08, 6.1), PressKind::Drag);
        assert_eq!(classify(2.00, 400.0), PressKind::Drag);
    }

    #[test]
    fn presses_inside_the_gap_count_up_and_a_slow_one_restarts() {
        let mut counter = MultiCounter::new();

        assert_eq!(counter.press(Button::Left, 1.00), 1);
        assert_eq!(counter.press(Button::Left, 1.20), 2);
        assert_eq!(counter.press(Button::Left, 1.35), 3);

        // A gap past the limit is a fresh gesture, not a quadruple click.
        assert_eq!(counter.press(Button::Left, 2.00), 1);
    }

    #[test]
    fn a_different_button_restarts_the_count() {
        let mut counter = MultiCounter::new();

        assert_eq!(counter.press(Button::Left , 1.00), 1);
        assert_eq!(counter.press(Button::Right, 1.10), 1);
        assert_eq!(counter.press(Button::Right, 1.20), 2);
        assert_eq!(counter.press(Button::Left , 1.30), 1);
    }

    #[test]
    fn only_the_two_pointing_buttons_are_mapped() {
        assert_eq!(Button::from_key(KeyCode::BTN_LEFT) , Some(Button::Left));
        assert_eq!(Button::from_key(KeyCode::BTN_RIGHT), Some(Button::Right));

        // The middle button pastes, the side buttons navigate: neither is aimed.
        assert_eq!(Button::from_key(KeyCode::BTN_MIDDLE), None);
        assert_eq!(Button::from_key(KeyCode::BTN_SIDE)  , None);
        assert_eq!(Button::from_key(KeyCode::KEY_A)     , None);
    }
}
