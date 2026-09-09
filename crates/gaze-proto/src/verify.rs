//! What the application says is under the gaze, for the pointer look.
//!
//! The recogniser reads pixels, and pixels cannot tell a chat message from a button or
//! a sidebar row from a caption: on Discord it calls the short last line of a message a
//! `Button` at 0.9 and the channel list `Text`, and both are wrong in the way that
//! matters, which is whether to mark them. The application's accessibility tree knows
//! (DESIGN.md §2: a11y first, vision for what the tree does not have), and `gaze_a11y`
//! answers "what is at this point" in single-digit milliseconds when the application is
//! on the bus. So the pointer look asks it, once per place the eyes settle, and takes the
//! answer over the recogniser's kind: a control gets marked with the tree's own box, a
//! run of text does not get marked however confident the recogniser was, and where the
//! tree has no answer the pixels decide as before.
//!
//! The question goes to `gaze_clicks::TreeService`'s thread, the same way the edge
//! scroller asks what scrolls (`surface.rs`), and the answer is polled for on later
//! samples: nothing here blocks the gaze loop, and a wedged application is that thread's
//! problem.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::time::{Duration, Instant};

use gaze_a11y::{Hit, Miss};
use gaze_clicks::TreeService;
use gaze_core::{ElementKind, GlobalPx, Rect};
use tracing::{debug, info};

/// Shortest interval between two questions about places the tree has not answered
/// for. Each ask costs the tree thread a few milliseconds and a moving gaze would
/// otherwise ask every sample.
pub const REASK_AFTER: Duration = Duration::from_millis(120);

/// A control the gaze is still on is re-asked about after this long, so a window that
/// moved or a list that scrolled does not keep serving its old box.
pub const REFRESH_AFTER: Duration = Duration::from_secs(2);

/// How long a fresh question may hold the highlight back. Within this the tree usually
/// answers, and marking the recogniser's box for one frame only to take it down when
/// the tree says text is exactly the blink the pointer look exists to avoid.
pub const HOLD_FOR: Duration = Duration::from_millis(60);

/// How far from where a question was asked its "static" answer still applies. Text
/// runs are wide; the gaze drifting along a line should not re-ask every few pixels.
pub const STATIC_REACH_PX: f64 = 48.0;

/// A control taller than this is a row, a card or a picture rather than a button, unless
/// it is also narrow. Discord answers `list item` 880x71 for a message and `image`
/// 546x351 for an attachment, both technically clickable, neither what the eyes are
/// aiming at, and a highlight round either is the noise the pointer look exists to
/// avoid. A channel row is a `link` 286x32, a file-manager row a `table cell` a line
/// tall; those pass.
pub const CONTROL_MAX_H_PX: f64 = 56.0;

/// A control taller than [`CONTROL_MAX_H_PX`] still counts if it is within this on both
/// sides: a tall narrow thing is a scrollbar thumb, a colour swatch, a toolbar button
/// at a big scale.
pub const CONTROL_MAX_SIDE_PX: f64 = 240.0;

/// How many times per session an accessible window answering nothing at the gaze is
/// reported at info level, with where. Beyond that it is debug: the first few say
/// which page it is, the rest would say the same.
const REPORT_EMPTY_ANSWERS: u64 = 5;

/// What the tree said about a place.
#[derive(Clone, Debug, PartialEq)]
pub enum Verdict {
    /// A control with a box of its own. `id` is stable for the object, so the highlight
    /// crossfades between controls and holds on one.
    Control {
        id   : u64,
        rect : Rect,
        kind : ElementKind,
        role : String,
        name : String,
    },
    /// Text, a heading, a cell, a section, or nothing actionable above the point: the
    /// place is being read, not aimed at, whatever the recogniser called it.
    Static,
    /// The tree had no answer (no window, an application off the bus), or has not been
    /// asked yet. The recogniser's kind stands.
    Unknown,
}

/// The tree's current verdict and the question in flight.
pub struct Verifier {
    tree     : TreeService,
    next_id  : u64,
    /// The outstanding question: its id, where it was asked, and when.
    pending  : Option<(u64, GlobalPx, Instant)>,
    /// The last answer, where it was asked, and when it arrived.
    current  : Option<(Verdict, GlobalPx, Instant)>,
    /// When the last question was sent, for the re-ask interval.
    asked_at : Option<Instant>,
    /// Questions asked over the session.
    pub asked    : u64,
    /// Answers that named a control.
    pub controls : u64,
    /// Answers that said text or nothing actionable.
    pub statics  : u64,
    /// Answers from an accessible window that had no node at the point at all.
    pub empty    : u64,
}

// --- Verifier ---

impl Verifier {
    /// Starts the tree thread. Never fails; without a bus every verdict is `Unknown`
    /// and the pointer look runs on the recogniser alone.
    pub fn spawn() -> Self {
        Self {
            tree     : TreeService::spawn(),
            next_id  : 1,
            pending  : None,
            current  : None,
            asked_at : None,
            asked    : 0,
            controls : 0,
            statics  : 0,
            empty    : 0,
        }
    }

    /// Collects any answer that has arrived and asks about `point` when the current
    /// verdict does not cover it. `point` is where the eyes are aiming: the snap point
    /// when there is one, the gaze otherwise; `None` (no gaze, or a saccade) asks
    /// nothing, since the answer would be about somewhere the eyes have left.
    pub fn update(&mut self, point: Option<GlobalPx>) {
        self.collect();

        let Some(point) = point else {
            return;
        };

        if self.pending.is_some() {
            return;
        }

        let since_ask = self.asked_at.map_or(Duration::MAX, |at| at.elapsed());

        let due = match &self.current {
            Some((verdict, asked_at_px, answered)) if applies(verdict, *asked_at_px, point) => {
                // A control the gaze is on refreshes slowly; a static answer holds
                // until the gaze leaves its reach.
                matches!(verdict, Verdict::Control { .. }) && answered.elapsed() >= REFRESH_AFTER
            }
            _ => since_ask >= REASK_AFTER,
        };

        if due {
            self.ask(point);
        }
    }

    /// The verdict for `point`, if the last answer covers it.
    pub fn verdict(&self, point: GlobalPx) -> Verdict {
        match &self.current {
            Some((verdict, asked, _)) if applies(verdict, *asked, point) => verdict.clone(),
            _ => Verdict::Unknown,
        }
    }

    /// True while a question asked at or near `point` is young enough that the caller
    /// should wait for it rather than mark the recogniser's box.
    pub fn holding(&self, point: GlobalPx) -> bool {
        self.pending.is_some_and(|(_, asked, at)| {
            at.elapsed() < HOLD_FOR && distance(asked, point) <= STATIC_REACH_PX
        })
    }

    fn ask(&mut self, point: GlobalPx) {
        let id = self.next_id;

        self.next_id  += 1;
        self.asked    += 1;
        self.pending   = Some((id, point, Instant::now()));
        self.asked_at  = Some(Instant::now());

        self.tree.ask(id, point);
    }

    /// Takes the reply to the outstanding question, if it has arrived.
    fn collect(&mut self) {
        let Some((id, asked, _)) = self.pending else {
            return;
        };

        let Some(reply) = self.tree.poll(id) else {
            return;
        };

        self.pending = None;

        let verdict = verdict_of(reply.hit.as_ref(), reply.miss);

        // An accessible window with nothing at the point is worth a line the first few
        // times: it is either a page whose tree really has nothing there, or a toolkit
        // reporting extents that exclude the point, and the difference is a bug report.
        if reply.hit.is_none() && matches!(reply.miss, Some(Miss::Nothing | Miss::Outside)) {
            self.empty += 1;

            if self.empty <= REPORT_EMPTY_ANSWERS {
                info!(
                    x    = asked.x,
                    y    = asked.y,
                    miss = ?reply.miss,
                    ms   = reply.ms,
                    "a11y: an accessible window has no node under the gaze; treating it as text \
                     (RUST_LOG=gaze_a11y=debug says why)",
                );
            }
        }

        match &verdict {
            Verdict::Control { role, name, rect, .. } => {
                self.controls += 1;

                debug!(
                    role = %role,
                    name = %name,
                    x    = rect.x,
                    y    = rect.y,
                    w    = rect.w,
                    h    = rect.h,
                    ms   = reply.ms,
                    "a11y: a control under the gaze",
                );
            }
            Verdict::Static => {
                self.statics += 1;

                debug!(
                    role = reply.hit.as_ref().map(|h| h.leaf.role.as_str()).unwrap_or("none"),
                    ms   = reply.ms,
                    "a11y: text under the gaze",
                );
            }
            Verdict::Unknown => debug!(ms = reply.ms, "a11y: no answer under the gaze"),
        }

        self.current = Some((verdict, asked, Instant::now()));
    }
}

// --- Verdicts ---

/// Folds a tree answer into a verdict.
///
/// A hit whose actionable ancestor is a control names it; one whose nearest actionable
/// ancestor is a text-like role, or that has none, is static. No hit from a window
/// whose application is off the bus is unknown, since that says nothing about the
/// pixels; no hit from an accessible window is static, since the application would
/// have named a control there if it had one, and the recogniser's guess over an
/// accessible page is the thing the tree is there to overrule.
pub fn verdict_of(hit: Option<&Hit>, miss: Option<Miss>) -> Verdict {
    let Some(hit) = hit else {
        return match miss {
            Some(Miss::Nothing | Miss::Outside) => Verdict::Static,
            Some(Miss::Unreachable) | None      => Verdict::Unknown,
        };
    };

    let Some(target) = &hit.target else {
        return Verdict::Static;
    };

    let Some(kind) = control_kind(&target.role) else {
        return Verdict::Static;
    };

    let Some(rect) = target.rect else {
        // A control with no extents cannot be marked; it is not text either, so the
        // recogniser's box, if any, stands.
        return Verdict::Unknown;
    };

    if !markable(rect) {
        return Verdict::Static;
    }

    Verdict::Control {
        id   : object_id(&target.bus, &target.path),
        rect : rect,
        kind : kind,
        role : target.role.clone(),
        name : target.name.clone(),
    }
}

/// The element kind a control role maps to, or `None` for roles that are actionable to
/// the click collector (text is a caret target) but are not controls the pointer look
/// should mark.
pub fn control_kind(role: &str) -> Option<ElementKind> {
    Some(match role {
        "push button" | "button" | "toggle button" | "list item" | "menu item"
            | "page tab" | "tree item"                                        => ElementKind::Button,
        "link"                                                                => ElementKind::Link,
        "entry" | "password text" | "combo box" | "spin button"               => ElementKind::Input,
        "check box" | "radio button" | "check menu item" | "radio menu item"  => ElementKind::Checkbox,
        "slider"                                                              => ElementKind::Slider,
        "image"                                                               => ElementKind::Icon,
        _                                                                     => return None,
    })
}

/// Whether a control's box is the size of something aimed at rather than read. See
/// [`CONTROL_MAX_H_PX`].
pub fn markable(rect: Rect) -> bool {
    rect.h <= CONTROL_MAX_H_PX || (rect.w <= CONTROL_MAX_SIDE_PX && rect.h <= CONTROL_MAX_SIDE_PX)
}

/// A stable id for an accessible object, so the same control asked about twice is the
/// same highlight. Hashed rather than kept as strings because the pointer look keys on
/// a `u64` like every recognised element; the top bit is set so it cannot collide with
/// the perception thread's small ids.
fn object_id(bus: &str, path: &str) -> u64 {
    let mut hasher = DefaultHasher::new();

    bus.hash(&mut hasher);
    path.hash(&mut hasher);

    hasher.finish() | (1 << 63)
}

/// Whether a verdict asked at `asked` still speaks for `point`.
fn applies(verdict: &Verdict, asked: GlobalPx, point: GlobalPx) -> bool {
    match verdict {
        Verdict::Control { rect, .. } => rect.contains(point),
        Verdict::Static               => distance(asked, point) <= STATIC_REACH_PX,
        Verdict::Unknown              => distance(asked, point) <= STATIC_REACH_PX,
    }
}

fn distance(a: GlobalPx, b: GlobalPx) -> f64 {
    (a.x - b.x).hypot(a.y - b.y)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_a11y::{CoordMode, Node};

    use super::*;

    fn node(role: &str, rect: Option<Rect>) -> Node {
        Node {
            bus  : ":1.7".into(),
            path : format!("/org/a11y/{role}"),
            role : role.into(),
            name : "x".into(),
            rect : rect,
            span : None,
        }
    }

    fn hit(leaf: &str, target: Option<Node>) -> Hit {
        Hit {
            leaf    : node(leaf, None),
            target  : target,
            climbed : 0,
            coord   : CoordMode::Window,
        }
    }

    /// Discord's channel row is a `link` with extents: a control, marked with the
    /// tree's box, not the OCR line's.
    #[test]
    fn a_link_row_is_a_control_with_its_own_box() {
        let rect = Rect { x: 10.0, y: 20.0, w: 200.0, h: 32.0 };
        let v    = verdict_of(Some(&hit("static", Some(node("link", Some(rect))))), None);

        match v {
            Verdict::Control { rect: r, kind, .. } => {
                assert_eq!(r, rect);
                assert_eq!(kind, ElementKind::Link);
            }
            other => panic!("expected a control, got {other:?}"),
        }
    }

    /// A message body answers `text`, which the collector treats as actionable (a caret
    /// target) and the pointer look does not: static, so the recogniser's "button" over
    /// it is not marked.
    #[test]
    fn text_and_no_ancestor_are_static_and_no_answer_is_unknown() {
        assert_eq!(verdict_of(Some(&hit("text", Some(node("text", None)))), None), Verdict::Static);
        assert_eq!(verdict_of(Some(&hit("section", None)), None), Verdict::Static);
        assert_eq!(verdict_of(None, None), Verdict::Unknown);
        assert_eq!(verdict_of(None, Some(Miss::Unreachable)), Verdict::Unknown);
    }

    /// An accessible window that answers nothing at the point (Firefox on a page, a
    /// toolkit whose extents exclude the point) is text, not a licence for the
    /// recogniser; an application off the bus leaves the recogniser in charge.
    #[test]
    fn nothing_from_an_accessible_window_is_static() {
        assert_eq!(verdict_of(None, Some(Miss::Nothing)), Verdict::Static);
        assert_eq!(verdict_of(None, Some(Miss::Outside)), Verdict::Static);
    }

    /// Discord's message row is a `list item` 880x71 and an attachment an `image`
    /// 546x351: clickable, not aimed at. A one-line row and a toolbar button pass.
    #[test]
    fn rows_and_pictures_are_static_and_buttons_are_controls() {
        let row   = Rect { x: 381.0, y: 844.0, w: 880.0, h: 71.0 };
        let image = Rect { x: 453.0, y: 957.0, w: 546.0, h: 351.0 };
        let line  = Rect { x: 86.0, y: 674.0, w: 286.0, h: 32.0 };
        let tall  = Rect { x: 0.0, y: 0.0, w: 24.0, h: 120.0 };

        assert_eq!(verdict_of(Some(&hit("section", Some(node("list item", Some(row))))), None), Verdict::Static);
        assert_eq!(verdict_of(Some(&hit("image", Some(node("image", Some(image))))), None), Verdict::Static);
        assert!(matches!(verdict_of(Some(&hit("section", Some(node("link", Some(line))))), None), Verdict::Control { .. }));
        assert!(matches!(verdict_of(Some(&hit("push button", Some(node("push button", Some(tall))))), None), Verdict::Control { .. }));
    }

    /// The same object gets the same id, a different one does not, and no id can be
    /// one of the perception thread's.
    #[test]
    fn object_ids_are_stable_and_out_of_the_recogniser_range() {
        assert_eq!(object_id(":1.7", "/a"), object_id(":1.7", "/a"));
        assert_ne!(object_id(":1.7", "/a"), object_id(":1.7", "/b"));
        assert!(object_id(":1.7", "/a") >= 1 << 63);
    }

    /// A control speaks for its box; a static answer for a neighbourhood of where it
    /// was asked.
    #[test]
    fn verdicts_apply_where_they_were_asked() {
        let asked   = GlobalPx { x: 100.0, y: 100.0 };
        let control = Verdict::Control {
            id   : 1,
            rect : Rect { x: 0.0, y: 0.0, w: 300.0, h: 40.0 },
            kind : ElementKind::Button,
            role : "push button".into(),
            name : String::new(),
        };

        assert!(applies(&control, asked, GlobalPx { x: 290.0, y: 10.0 }));
        assert!(!applies(&control, asked, GlobalPx { x: 100.0, y: 50.0 }));
        assert!(applies(&Verdict::Static, asked, GlobalPx { x: 140.0, y: 100.0 }));
        assert!(!applies(&Verdict::Static, asked, GlobalPx { x: 160.0, y: 100.0 }));
    }
}
