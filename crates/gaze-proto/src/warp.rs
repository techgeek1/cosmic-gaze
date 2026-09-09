//! When the pointer is allowed to jump to the gaze point.
//!
//! Two tiers warp the pointer, and both want the same restraint. Scroll-under-gaze warps so
//! a wheel event lands on the window being looked at; focus-follows-gaze warps so a dwell
//! on another output takes the pointer (and with it, keyboard focus on a
//! focus-follows-mouse desktop) along. Neither is a click, so neither needs precision, and
//! that is exactly why they are the tier worth shipping first (DESIGN.md section 3,
//! principle 4).
//!
//! The restraint that matters is the one on a continuous wheel: gaze jitters by degrees, so
//! re-deciding the warp on every detent would drag the pointer around inside the window the
//! user is already scrolling. [`Warper`] answers "should the pointer move" and remembers
//! where it last sent it; it never touches an injector itself, which is what lets the
//! policy be tested without a compositor.

use std::time::{Duration, Instant};

use gaze_core::GlobalPx;

/// Shortest time between two warps driven by the same continuous wheel. A burst of detents
/// through one window warps once, at the start, and then leaves the pointer alone until the
/// eyes actually go somewhere else.
pub const WARP_COOLDOWN: Duration = Duration::from_millis(300);

/// Why the pointer moved. Logged with every warp so a session log says which tier fired.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WarpReason {
    /// The wheel turned while gaze was off the pointer by more than the threshold.
    Scroll,
    /// A fixation outlasted the dwell on an output the pointer was not on.
    Focus,
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

/// Warp policy and the counters the exit summary reports.
pub struct Warper {
    /// How far gaze must be from the pointer, in degrees, to be worth a warp.
    threshold_deg : f64,
    /// Shortest interval between warps that gaze motion has not justified on its own.
    cooldown      : Duration,
    /// Where the last warp sent the pointer. Doubles as the pointer position when nothing
    /// else can report one, which is the case in a dry run and on the open-loop injector
    /// backends.
    last_point    : Option<GlobalPx>,
    /// When that warp happened.
    last_at       : Option<Instant>,
    /// Warps performed (or, in a dry run, logged), by either tier.
    warps         : u64,
    /// Wheel events routed by the scroll tier, warped or not.
    scrolls       : u64,
}

// --- Warper ---

impl Warper {
    /// A warper with the given distance threshold in degrees and re-warp cooldown.
    pub fn new(threshold_deg: f64, cooldown: Duration) -> Warper {
        Warper {
            threshold_deg : threshold_deg,
            cooldown      : cooldown,
            last_point    : None,
            last_at       : None,
            warps         : 0,
            scrolls       : 0,
        }
    }

    /// Whether a wheel event at this moment should warp the pointer to the gaze point.
    ///
    /// `gap_deg` is the angle between the pointer and the gaze point, and `moved_deg` the
    /// angle between the last warp's destination and the gaze point. `None` for either one
    /// means "unknown", which counts as far away: an unknown pointer position is the case
    /// where warping is most likely to be what the user wanted, and the first warp of a
    /// session has no previous destination to compare against.
    ///
    /// The pointer only moves when gaze is genuinely elsewhere *and* either the cooldown has
    /// expired or the eyes have moved a threshold's worth since the last warp. The second
    /// clause is what keeps a deliberate look at another window responsive during a long
    /// scroll, without letting jitter re-warp at the wheel's rate.
    pub fn should_warp(&self, now: Instant, gap_deg: Option<f64>, moved_deg: Option<f64>)
        -> bool
    {
        let far    = gap_deg.is_none_or(|deg| deg > self.threshold_deg);
        let cooled = self.last_at.is_none_or(|at| now.duration_since(at) >= self.cooldown);
        let moved  = moved_deg.is_none_or(|deg| deg > self.threshold_deg);

        far && (cooled || moved)
    }

    /// Records a warp that has just happened (or, in a dry run, been logged).
    pub fn record_warp(&mut self, point: GlobalPx, now: Instant) {
        self.last_point = Some(point);
        self.last_at    = Some(now);
        self.warps     += 1;
    }

    /// Records one wheel event routed by the scroll tier.
    pub fn record_scroll(&mut self) {
        self.scrolls += 1;
    }

    /// Where the last warp sent the pointer, if there has been one.
    pub fn last_point(&self) -> Option<GlobalPx> {
        self.last_point
    }

    /// Warps performed this session, by either tier.
    pub fn warps(&self) -> u64 {
        self.warps
    }

    /// Wheel events routed this session.
    pub fn scrolls(&self) -> u64 {
        self.scrolls
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn at(x: f64, y: f64) -> GlobalPx {
        GlobalPx { x: x, y: y }
    }

    /// A wheel turned inside the window the user is already looking at must not move the
    /// pointer, however long the burst runs.
    #[test]
    fn gaze_near_the_pointer_never_warps() {
        let warper = Warper::new(3.0, WARP_COOLDOWN);

        assert!(!warper.should_warp(Instant::now(), Some(2.9), None));
        assert!(!warper.should_warp(Instant::now(), Some(0.0), None));
    }

    #[test]
    fn the_first_far_wheel_event_warps() {
        let warper = Warper::new(3.0, WARP_COOLDOWN);

        assert!(warper.should_warp(Instant::now(), Some(3.1), None));
    }

    /// An unknown pointer position is the dry-run and open-loop-backend case. Treating it
    /// as "far" is what makes those runs exercise the same path a live one does.
    #[test]
    fn an_unknown_pointer_position_counts_as_far_away() {
        let warper = Warper::new(3.0, WARP_COOLDOWN);

        assert!(warper.should_warp(Instant::now(), None, Some(0.0)));
    }

    /// The reason this type exists: a continuous wheel re-decides at the wheel's rate, and
    /// gaze jitter within the threshold must not drag the pointer with it.
    #[test]
    fn a_continuous_wheel_warps_once_per_cooldown() {
        let now = Instant::now();

        let mut warper = Warper::new(3.0, WARP_COOLDOWN);

        assert!(warper.should_warp(now, Some(10.0), None));
        warper.record_warp(at(100.0, 100.0), now);

        // Same fixation, pointer now elsewhere again only because the eyes jitter: held.
        assert!(!warper.should_warp(now + Duration::from_millis(50), Some(10.0), Some(0.5)));
        assert!(!warper.should_warp(now + Duration::from_millis(299), Some(10.0), Some(2.9)));

        // Cooldown expired.
        assert!(warper.should_warp(now + Duration::from_millis(300), Some(10.0), Some(0.5)));
    }

    /// Looking at another window mid-scroll has to be responsive: a real saccade beats the
    /// cooldown rather than waiting it out.
    #[test]
    fn gaze_moving_past_the_threshold_beats_the_cooldown() {
        let now = Instant::now();

        let mut warper = Warper::new(3.0, WARP_COOLDOWN);

        warper.record_warp(at(100.0, 100.0), now);

        assert!(warper.should_warp(now + Duration::from_millis(10), Some(10.0), Some(3.1)));
    }

    #[test]
    fn counters_track_both_tiers_separately() {
        let mut warper = Warper::new(3.0, WARP_COOLDOWN);

        warper.record_scroll();
        warper.record_scroll();
        warper.record_warp(at(1.0, 2.0), Instant::now());

        assert_eq!(warper.scrolls(), 2);
        assert_eq!(warper.warps(), 1);
        assert_eq!(warper.last_point(), Some(at(1.0, 2.0)));
    }
}
