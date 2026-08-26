//! Did the snap engine pick the element the user was actually looking at?
//!
//! The synthetic provider knows the noise-free point the mouse is really at
//! (`SyntheticProvider::truth`), so every commit can be graded on the spot instead of
//! waiting for an offline pass. Three outcomes, following `PLAN.md`'s gaze-proto contract:
//! the truth point is inside the box the engine chose (a hit), inside some other element's
//! box (a slip, and which one matters when reading the log), or inside nothing at all (a
//! miss, which usually means the detector had no box there to begin with).

use std::fmt;
use std::time::Duration;

use gaze_core::{Element, GlobalPx};

/// How one commit turned out.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The truth point is inside the box the engine committed to.
    Hit,
    /// The truth point is inside a different element's box. `other` is that element's id.
    Slip { other : u64 },
    /// The truth point is inside no element's box. Either the detector missed the target
    /// or the user was not looking at anything clickable.
    Miss,
}

/// Running tally over a session, printed as the exit summary.
#[derive(Clone, Copy, Debug, Default)]
pub struct Scoreboard {
    pub commits : u64,
    pub hits    : u64,
    pub slips   : u64,
    pub misses  : u64,
    /// Sum of press-to-click-issue latencies. Kept as a sum so the mean stays exact
    /// regardless of how many commits arrive.
    latency_s   : f64,
}

// --- Classification ---

/// Grades one commit against ground truth.
///
/// `chosen` is what [`gaze_snap::SnapEngine::commit`] returned, which is `None` when the
/// engine had nothing fixated one commit latency ago. That still counts as a graded
/// commit: if the truth point sits in an element the engine should have found it, so a
/// `None` choice over a real element is reported as a slip, not excused.
///
/// When several boxes contain the truth point the smallest one wins. Detector boxes carry
/// no tree, so area is the only nesting signal available, and the tightest box is the
/// thing a person would name if asked what they were looking at.
pub fn classify(truth: GlobalPx, chosen: Option<&Element>, elements: &[Element]) -> Outcome {
    if let Some(element) = chosen
        && element.bbox.contains(truth)
    {
        return Outcome::Hit;
    }

    // The chosen element is excluded by id rather than by pointer, because the element
    // list the engine committed against may already have been replaced by a newer one.
    let chosen_id = chosen.map(|element| element.id);

    let other = elements.iter()
        .filter(|element| Some(element.id) != chosen_id)
        .filter(|element| element.bbox.contains(truth))
        .min_by(|a, b| (a.bbox.w * a.bbox.h).total_cmp(&(b.bbox.w * b.bbox.h)));

    match other {
        Some(element) => Outcome::Slip { other: element.id },
        None          => Outcome::Miss,
    }
}

// --- Scoreboard ---

impl Scoreboard {
    /// Adds one graded commit.
    pub fn record(&mut self, outcome: Outcome, latency: Duration) {
        self.commits   += 1;
        self.latency_s += latency.as_secs_f64();

        match outcome {
            Outcome::Hit          => self.hits += 1,
            Outcome::Slip { .. }  => self.slips += 1,
            Outcome::Miss         => self.misses += 1,
        }
    }

    /// Adds one commit with no ground truth to grade it against.
    ///
    /// Every provider but the synthetic one is in this position: there is no noise-free
    /// point behind a real gaze sample, so a commit can be timed but not judged. Counts
    /// toward `commits` and the latency mean and toward none of hit/slip/miss, which keeps
    /// [`hit_rate`](Self::hit_rate) honest in a mixed session.
    pub fn record_ungraded(&mut self, latency: Duration) {
        self.commits   += 1;
        self.latency_s += latency.as_secs_f64();
    }

    /// Commits that could be graded against ground truth.
    pub fn graded(&self) -> u64 {
        self.hits + self.slips + self.misses
    }

    /// Fraction of *graded* commits that landed on the intended element, or `0.0` when
    /// none could be graded. Ungraded commits are excluded rather than counted as misses.
    pub fn hit_rate(&self) -> f64 {
        let graded = self.graded();

        if graded == 0 {
            return 0.0;
        }

        self.hits as f64 / graded as f64
    }

    /// Mean press-to-click-issue latency in seconds, or `0.0` with no commits.
    pub fn mean_latency_s(&self) -> f64 {
        if self.commits == 0 {
            return 0.0;
        }

        self.latency_s / self.commits as f64
    }
}

// --- Display ---

impl fmt::Display for Outcome {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Outcome::Hit           => write!(f, "hit"),
            Outcome::Slip { other } => write!(f, "slip->{other}"),
            Outcome::Miss          => write!(f, "miss"),
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    use gaze_core::{ElementKind, ElementSource, Rect};

    /// Builds a bare detector element. Only id and box matter to the classifier.
    fn element(id: u64, x: f64, y: f64, w: f64, h: f64) -> Element {
        Element {
            id     : id,
            bbox   : Rect { x: x, y: y, w: w, h: h },
            kind   : ElementKind::Button,
            source : ElementSource::Detector,
            score  : 0.9,
            text   : None,
        }
    }

    fn at(x: f64, y: f64) -> GlobalPx {
        GlobalPx { x: x, y: y }
    }

    #[test]
    fn truth_inside_the_chosen_box_is_a_hit() {
        let elements = [element(1, 0.0, 0.0, 100.0, 50.0)];

        assert_eq!(classify(at(50.0, 25.0), Some(&elements[0]), &elements), Outcome::Hit);
    }

    #[test]
    fn a_hit_is_judged_on_the_box_edge_too() {
        let elements = [element(1, 0.0, 0.0, 100.0, 50.0)];

        // `Rect::contains` is inclusive, and a target's click point is clamped onto the
        // edge, so the edge has to count as inside or a legitimate commit reads as a slip.
        assert_eq!(classify(at(100.0, 50.0), Some(&elements[0]), &elements), Outcome::Hit);
    }

    #[test]
    fn truth_in_a_neighbour_is_a_slip_naming_that_neighbour() {
        let elements = [
            element(1, 0.0, 0.0, 100.0, 50.0),
            element(2, 200.0, 0.0, 100.0, 50.0),
        ];

        let outcome = classify(at(250.0, 25.0), Some(&elements[0]), &elements);

        assert_eq!(outcome, Outcome::Slip { other: 2 });
    }

    #[test]
    fn a_slip_reports_the_smallest_containing_box() {
        // A button nested inside a toolbar inside a window: all three contain the point,
        // and the button is the one the user meant.
        let elements = [
            element(1, 0.0, 0.0, 40.0, 20.0),
            element(2, 500.0, 500.0, 1000.0, 600.0),
            element(3, 520.0, 520.0, 400.0, 40.0),
            element(4, 600.0, 530.0, 30.0, 20.0),
        ];

        let outcome = classify(at(610.0, 540.0), Some(&elements[0]), &elements);

        assert_eq!(outcome, Outcome::Slip { other: 4 });
    }

    #[test]
    fn truth_in_no_box_is_a_miss() {
        let elements = [element(1, 0.0, 0.0, 100.0, 50.0)];

        assert_eq!(classify(at(900.0, 900.0), Some(&elements[0]), &elements), Outcome::Miss);
    }

    #[test]
    fn no_chosen_target_over_an_element_is_a_slip() {
        let elements = [element(7, 0.0, 0.0, 100.0, 50.0)];

        assert_eq!(classify(at(10.0, 10.0), None, &elements), Outcome::Slip { other: 7 });
    }

    #[test]
    fn no_chosen_target_over_nothing_is_a_miss() {
        let elements = [element(7, 0.0, 0.0, 100.0, 50.0)];

        assert_eq!(classify(at(900.0, 900.0), None, &elements), Outcome::Miss);
    }

    #[test]
    fn an_empty_element_list_is_always_a_miss() {
        assert_eq!(classify(at(10.0, 10.0), None, &[]), Outcome::Miss);
    }

    #[test]
    fn the_chosen_element_is_never_reported_as_its_own_slip() {
        // The chosen box does not contain the truth point but a stale duplicate of it
        // could still be in the list; excluding by id keeps the outcome honest.
        let chosen   = element(1, 0.0, 0.0, 100.0, 50.0);
        let elements = [element(1, 0.0, 0.0, 100.0, 50.0)];

        assert_eq!(classify(at(900.0, 900.0), Some(&chosen), &elements), Outcome::Miss);
    }

    #[test]
    fn the_scoreboard_counts_and_averages() {
        let mut board = Scoreboard::default();

        board.record(Outcome::Hit, Duration::from_millis(10));
        board.record(Outcome::Hit, Duration::from_millis(20));
        board.record(Outcome::Slip { other: 3 }, Duration::from_millis(30));
        board.record(Outcome::Miss, Duration::from_millis(40));

        assert_eq!(board.commits, 4);
        assert_eq!(board.hits, 2);
        assert_eq!(board.slips, 1);
        assert_eq!(board.misses, 1);
        assert!((board.hit_rate() - 0.5).abs() < 1.0e-12);
        assert!((board.mean_latency_s() - 0.025).abs() < 1.0e-9);
    }

    #[test]
    fn an_empty_scoreboard_does_not_divide_by_zero() {
        let board = Scoreboard::default();

        assert_eq!(board.hit_rate(), 0.0);
        assert_eq!(board.mean_latency_s(), 0.0);
    }
}
