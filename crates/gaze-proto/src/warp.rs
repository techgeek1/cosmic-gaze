//! Where the pointer was last sent, and why.
//!
//! Nothing gaze-side moves the pointer on its own initiative any more: the tiers that
//! did (wheel routing to the window under gaze, focus-follows-gaze) went with the
//! borrow-and-return model of 2026-09-09. What is left warps the pointer for a reason
//! the user gave: a thumb landing on the pad, a refine beginning, an edge scroll
//! starting, and the return trips that put it back. [`Warper`] remembers the last
//! destination, which doubles as the pointer position when nothing can measure one,
//! and counts the warps for the exit summary. It never touches an injector itself.

use std::time::Instant;

use gaze_core::GlobalPx;

/// Why the pointer moved. Logged with every warp so a session log says which one fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarpReason {
    /// An edge scroll started with the pointer outside the surface it has to land on.
    EdgeScroll,
    /// A borrowed pointer goes back to where it was taken from: the edge scroll stopped,
    /// or the thumb lifted.
    Return,
    /// The controller's refine gesture began: the pointer goes to the snap point so the
    /// hand can take it from there.
    Refine,
    /// A thumb is on the pad and the marked element changed: the pointer goes to it, so
    /// the pad press lands on what is highlighted.
    Mark,
}

/// The last warp and the counter the exit summary reports.
#[derive(Debug, Default)]
pub struct Warper {
    /// Where the last warp sent the pointer. Doubles as the pointer position when nothing
    /// else can report one, which is the case in a dry run and on the open-loop injector
    /// backends.
    last_point : Option<GlobalPx>,
    /// When that warp happened.
    last_at    : Option<Instant>,
    /// When the session last moved the pointer at all, warp or nudge. Motion the
    /// tracker reports soon after is the session's own, not the mouse's.
    touched_at : Option<Instant>,
    /// Warps performed (or, in a dry run, logged).
    warps      : u64,
}

// --- Warper ---

impl Warper {
    /// A warper that has sent the pointer nowhere yet.
    pub fn new() -> Warper {
        Warper::default()
    }

    /// Records a warp that has just happened (or, in a dry run, been logged).
    pub fn record_warp(&mut self, point: GlobalPx, now: Instant) {
        self.last_point = Some(point);
        self.last_at    = Some(now);
        self.touched_at = Some(now);
        self.warps     += 1;
    }

    /// Records a nudge: the session moved the pointer without a destination of its own
    /// (a refine step), so the motion the tracker reports next is not the mouse's.
    pub fn record_nudge(&mut self, now: Instant) {
        self.touched_at = Some(now);
    }

    /// When this session last moved the pointer by any means, if it has.
    pub fn touched_at(&self) -> Option<Instant> {
        self.touched_at
    }

    /// Where the last warp sent the pointer, if there has been one.
    pub fn last_point(&self) -> Option<GlobalPx> {
        self.last_point
    }

    /// When the last warp happened, if there has been one.
    pub fn last_at(&self) -> Option<Instant> {
        self.last_at
    }

    /// Warps performed this session.
    pub fn warps(&self) -> u64 {
        self.warps
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_last_warp_is_remembered_and_counted() {
        let mut warper = Warper::new();

        assert_eq!(warper.last_point(), None);

        warper.record_warp(GlobalPx { x: 1.0, y: 2.0 }, Instant::now());
        warper.record_warp(GlobalPx { x: 3.0, y: 4.0 }, Instant::now());

        assert_eq!(warper.warps(), 2);
        assert_eq!(warper.last_point(), Some(GlobalPx { x: 3.0, y: 4.0 }));
        assert!(warper.last_at().is_some());
    }
}
