//! Counters. Everything the report prints is an aggregation of these.
//!
//! Per-element tallies are kept rather than only per-frame totals, because the questions
//! that matter ("which kinds fail", "how small is too small", "what does it slip to") are
//! all breakdowns over the target, and re-running the Monte Carlo per breakdown would be
//! silly.

use std::collections::HashMap;

use gaze_core::{DesktopGeometry, Element, ElementKind, GlobalPx, Rect};
use gaze_snap::FALLBACK_PX_PER_DEG;

use crate::trial::{Outcome, TrialOutput};

/// Upper bounds of the size buckets, in degrees on the target's shorter side.
pub const BUCKET_BOUNDS_DEG : [f64; 3] = [0.5, 1.0, 2.0];

/// Intersection-over-union at or above which two boxes are the same thing on screen, not
/// two things. The detector routinely emits a widget box and an OCR box for one control
/// (`fuse_text` only drops text that is 70% inside a widget, and a text box a few pixels
/// larger than the widget it labels is not), and a strict id comparison scores landing on
/// the wrong copy as a miss even though the click would be identical.
pub const DUPLICATE_IOU : f64 = 0.5;

/// Fraction of the smaller box that must lie inside the larger one for the pair to count
/// as nested rather than separate: a label inside its button, a line inside its pane.
pub const NESTED_CONTAINMENT : f64 = 0.8;

/// Width-to-height ratio above which a box is a line of text rather than a control,
/// whatever the detector called it. Terminal rows, chat messages and OCR runs all land
/// here; so does the occasional very wide toolbar, which is a acceptable price for a rule
/// that needs no semantics.
pub const LINE_ASPECT : f64 = 6.0;

/// Height, logical pixels, below which a wide box is a line rather than a banner.
pub const LINE_MAX_H_PX : f64 = 40.0;

/// Ambiguity margins swept in the report, in score units (degrees of angular distance at
/// the default weights).
pub const SWEEP_MARGINS : [f64; 3] = [0.25, 0.5, 1.0];

/// Cut-offs for the top-k accuracy table. `k = 3` is the interesting one: if the intended
/// target is nearly always in the best three candidates, an ambiguous snap can be settled
/// with a two-to-four-way hint (numbered labels, one spoken or pressed token) instead of a
/// zoom step, which is a different product from a refinement UI.
pub const TOPK : [usize; 4] = [1, 2, 3, 5];

/// Buckets in the near-candidate histogram. The last one is an overflow, so a percentile
/// read out of it is reported as "at least" that many.
pub const NEAR_BUCKETS : usize = 32;

/// Bin width and count of the nudge-distance histogram in degrees. 0.02 deg resolution
/// out to 20 deg: fine enough for a median, wide enough that the overflow bin is only
/// reached by a warp that landed on another output.
pub const NUDGE_DEG_WIDTH : f64   = 0.02;
pub const NUDGE_DEG_BINS  : usize = 1001;

/// The same histogram in logical pixels, one pixel per bin out to 1200.
pub const NUDGE_PX_WIDTH : f64   = 1.0;
pub const NUDGE_PX_BINS  : usize = 1201;

/// Direction sectors a flick can distinguish. Eight 45 degree wedges, which is what a
/// thumb on a touchpad can hit reliably.
pub const FLICK_SECTORS : usize = 8;

/// How many of the ranked candidates a directional flick chooses between.
pub const FLICK_TOP_K : usize = 3;

/// Outcome counts for some slice of the trials.
///
/// `correct` and `slip` are totals; the `amb_` counters are the subsets the engine was not
/// confident about. A wrong click costs far more than a refinement step, so the numbers
/// that matter are `confident_wrong` (must be near zero) and `ambiguous` (the refinement
/// load).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tally {
    pub correct     : u64,
    pub slip        : u64,
    pub none        : u64,
    pub no_gaze     : u64,
    /// Correct answers where a rival candidate was within the ambiguity margin.
    pub amb_correct : u64,
    /// Wrong answers where a rival candidate was within the ambiguity margin.
    pub amb_slip    : u64,
}

/// Target size class, by the shorter side of the box in degrees of visual angle at its
/// centre. The interesting boundary is around 1 degree: below it a target is smaller than
/// an ET5-class sigma and can only be reached by snapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum SizeBucket {
    /// Under 0.5 deg.
    Tiny,
    /// 0.5 to 1 deg.
    Small,
    /// 1 to 2 deg.
    Medium,
    /// Over 2 deg.
    Large,
}

/// Which rival candidates are allowed to make a trial ambiguous.
///
/// The question the ambiguity metric asks is "would a refinement step have anything to
/// resolve", and that depends on whether the rival is a different thing on screen.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RivalPolicy {
    /// Every rival counts, however much it overlaps the winner. The strictest reading.
    All,
    /// A rival that is the same box as the winner does not count: the detector emitting a
    /// widget box and an OCR box for one control is not something to ask the user about.
    Distinct,
    /// Neither duplicates nor nestings count. A label inside its row, or a row inside its
    /// pane, is one place to click, not two.
    Separate,
}

/// What kind of wrong answer a slip was, geometrically.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SlipClass {
    /// The chosen box is the same thing on screen as the target: a click on it would be
    /// the same click. Detector duplication, not a snapping failure.
    Duplicate,
    /// One box is almost wholly inside the other, but they are not the same box. A label
    /// inside its button, or a terminal line inside its pane.
    Nested,
    /// Two genuinely different targets.
    Separate,
}

/// What sort of thing a target is, decided from its box rather than from the detector's
/// class, because the detector calls a terminal row a Button.
///
/// The split exists because the three classes ask different questions. `Widget` is what
/// Phase 0 is about: can gaze plus snapping click a real control. `Line` is a row of text
/// at roughly 0.35 deg pitch, below the physiological floor for any gaze system, and is
/// what the refinement tier exists for. `TextOther` is everything else the OCR stage
/// produced.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum TargetClass {
    Widget,
    Line,
    TextOther,
}

/// One element's trials within one run.
#[derive(Clone, Debug)]
pub struct ElementStats {
    /// Index into the frame's element slice, which is also the element's id.
    pub index     : usize,
    pub kind      : ElementKind,
    pub class     : TargetClass,
    pub bucket    : SizeBucket,
    /// Shorter side of the box in degrees, the number the bucket came from.
    pub short_deg : f64,
    pub tally     : Tally,
    /// Trials where the intended target placed within each `TOPK` cut-off.
    pub topk      : [u64; TOPK.len()],
    /// Trials that produced a ranked candidate list at all, the denominator for `topk`.
    /// Excludes lost gaze and the sigma-over-radius bail, which rank nothing.
    pub ranked    : u64,
    /// Whether some larger candidate of a different detector kind wraps this target: a
    /// Text label inside a Button row, an Icon inside a list entry. This is the case the
    /// centre terms exist to fix, because `distance` is zero for both boxes whenever the
    /// gaze is inside the inner one.
    pub nested    : bool,
}

/// A fixed-width histogram with an overflow bin, for percentiles over a run.
///
/// Percentiles come out of a histogram rather than a sorted sample because the bench
/// generates millions of trials and keeping them all to sort would dominate its memory.
#[derive(Clone, Debug)]
pub struct Histogram {
    width : f64,
    bins  : Vec<u64>,
    n     : u64,
}

/// One screenshot's trials within one run.
#[derive(Clone, Debug)]
pub struct FrameResult {
    /// Index into the shot set.
    pub shot         : usize,
    pub elements     : Vec<ElementStats>,
    /// Sums for the injected-error sanity line, over samples that produced a point.
    pub err_px_sum   : f64,
    pub err_deg_sum  : f64,
    pub err_n        : u64,
    /// Samples the filter called a fixation, out of samples fed. Sequence mode only.
    pub fixating     : u64,
    pub fed          : u64,
    /// Trials whose final `update` named the target, whatever `commit` then said.
    pub last_correct : u64,
    /// `(intended id, chosen id) -> count` for slips.
    pub confusion    : HashMap<(u64, u64), u32>,
    /// Slips split by `SlipClass`, indexed by `slip_slot`.
    pub slips        : [u64; 3],
    /// Widget-class trials only, one row per `SWEEP_MARGINS` entry, each
    /// `[confident correct, confident wrong, ambiguous]`. Lets the report sweep the
    /// ambiguity threshold without re-running the Monte Carlo.
    pub sweep        : [[u64; 3]; SWEEP_MARGINS.len()],
    /// Candidates within the ambiguity margin of the winner, per target class, as a
    /// histogram over `NEAR_BUCKETS`. This is the size of the hint the refinement tier
    /// would have to show.
    pub near_hist    : [[u64; NEAR_BUCKETS]; 3],
    /// Running sum and count of the same, per target class, for the mean.
    pub near_sum     : [u64; 3],
    pub near_n       : [u64; 3],
    /// Distance from the point the system would warp to, to the intended target's box,
    /// per target class. This is the correction the fine channel has to carry.
    pub nudge_deg    : [Histogram; 3],
    pub nudge_px     : [Histogram; 3],
    /// Trials whose warp already landed inside the intended target, and trials that
    /// produced a warp point at all.
    pub nudge_inside : [u64; 3],
    pub nudge_n      : [u64; 3],
    /// Trials a single directional flick could resolve, and trials that ranked anything.
    pub flick_ok     : [u64; 3],
    pub flick_n      : [u64; 3],
}

// --- Tally ---

impl Tally {
    /// Counts one trial. `ambiguous` is whether a rival candidate was within the margin.
    pub fn add(&mut self, outcome: Outcome, ambiguous: bool) {
        match outcome {
            Outcome::Correct => {
                self.correct += 1;

                if ambiguous {
                    self.amb_correct += 1;
                }
            }

            Outcome::Slip { .. } => {
                self.slip += 1;

                if ambiguous {
                    self.amb_slip += 1;
                }
            }

            Outcome::NoTarget => self.none += 1,
            Outcome::NoGaze   => self.no_gaze += 1,
        }
    }

    /// Folds another tally in.
    pub fn merge(&mut self, other: &Tally) {
        self.correct     += other.correct;
        self.slip        += other.slip;
        self.none        += other.none;
        self.no_gaze     += other.no_gaze;
        self.amb_correct += other.amb_correct;
        self.amb_slip    += other.amb_slip;
    }

    /// Right answers the engine was confident about: a click that lands, no refinement.
    pub fn confident_correct(&self) -> u64 {
        self.correct - self.amb_correct
    }

    /// Wrong answers the engine was confident about. The number that must be near zero:
    /// these are the clicks the user has to undo.
    pub fn confident_wrong(&self) -> u64 {
        self.slip - self.amb_slip
    }

    /// Trials that would go to the refinement tier, right or wrong.
    pub fn ambiguous(&self) -> u64 {
        self.amb_correct + self.amb_slip
    }

    /// Trials counted.
    pub fn total(&self) -> u64 {
        self.correct + self.slip + self.none + self.no_gaze
    }

    /// Correct trials as a percentage, zero for an empty tally.
    pub fn correct_pct(&self) -> f64 {
        self.pct(self.correct)
    }

    /// `count` as a percentage of the total, zero for an empty tally.
    pub fn pct(&self, count: u64) -> f64 {
        let total = self.total();

        if total == 0 {
            return 0.0;
        }

        100.0 * count as f64 / total as f64
    }
}

// --- SizeBucket ---

impl SizeBucket {
    /// Bucket for a target whose shorter side subtends `short_deg`.
    pub fn for_deg(short_deg: f64) -> SizeBucket {
        if short_deg < BUCKET_BOUNDS_DEG[0] {
            SizeBucket::Tiny
        }
        else if short_deg < BUCKET_BOUNDS_DEG[1] {
            SizeBucket::Small
        }
        else if short_deg < BUCKET_BOUNDS_DEG[2] {
            SizeBucket::Medium
        }
        else {
            SizeBucket::Large
        }
    }

    /// Every bucket, smallest first. Used to keep report rows in a fixed order.
    pub fn all() -> [SizeBucket; 4] {
        [SizeBucket::Tiny, SizeBucket::Small, SizeBucket::Medium, SizeBucket::Large]
    }

    /// Human label for a report row.
    pub fn label(&self) -> &'static str {
        match self {
            SizeBucket::Tiny   => "< 0.5 deg",
            SizeBucket::Small  => "0.5 - 1 deg",
            SizeBucket::Medium => "1 - 2 deg",
            SizeBucket::Large  => "> 2 deg",
        }
    }
}

// --- Sizing ---

/// Shorter side of `bbox` in degrees of visual angle, measured with the desk's local
/// scale at the box centre.
///
/// The two axes have different scales on a curved or tilted panel, so each side is
/// converted with its own axis before taking the minimum.
pub fn short_side_deg(geometry: &DesktopGeometry, bbox: &Rect) -> f64 {
    let centre = bbox.center();

    let (ppd_x, ppd_y) = match geometry.px_per_deg(geometry.eye(), centre) {
        Some(v) => v,
        // Off every panel: fall back to the same 60 px/deg the snap crate uses, so a
        // stray box still lands in a bucket instead of poisoning the table.
        None    => (FALLBACK_PX_PER_DEG, FALLBACK_PX_PER_DEG),
    };

    (bbox.w / ppd_x).min(bbox.h / ppd_y)
}

/// Pixels per degree at a point, for reporting. Same fallback as `short_side_deg`.
pub fn px_per_deg_at(geometry: &DesktopGeometry, p: GlobalPx) -> (f64, f64) {
    geometry.px_per_deg(geometry.eye(), p)
        .unwrap_or((FALLBACK_PX_PER_DEG, FALLBACK_PX_PER_DEG))
}

// --- FrameResult ---

impl FrameResult {
    /// An empty result for one shot.
    pub fn new(shot: usize) -> FrameResult {
        let deg = || Histogram::new(NUDGE_DEG_WIDTH, NUDGE_DEG_BINS);
        let px  = || Histogram::new(NUDGE_PX_WIDTH, NUDGE_PX_BINS);

        FrameResult {
            shot         : shot,
            elements     : Vec::new(),
            err_px_sum   : 0.0,
            err_deg_sum  : 0.0,
            err_n        : 0,
            fixating     : 0,
            fed          : 0,
            last_correct : 0,
            confusion    : HashMap::new(),
            slips        : [0; 3],
            sweep        : [[0; 3]; SWEEP_MARGINS.len()],
            near_hist    : [[0; NEAR_BUCKETS]; 3],
            near_sum     : [0; 3],
            near_n       : [0; 3],
            nudge_deg    : [deg(), deg(), deg()],
            nudge_px     : [px(), px(), px()],
            nudge_inside : [0; 3],
            nudge_n      : [0; 3],
            flick_ok     : [0; 3],
            flick_n      : [0; 3],
        }
    }

    /// Folds one element's trials in, along with everything the trials measured.
    ///
    /// `elements` is the frame's element slice, needed to classify each slip against the
    /// box that actually won.
    pub fn push(&mut self, mut stats: ElementStats, outputs: &[TrialOutput], elements: &[Element]) {
        let want = stats.index as u64;
        let slot = class_slot(stats.class);

        for out in outputs {
            // Top-k only counts trials that ranked something. A lost sample or a
            // sigma-over-radius bail has no candidate list to place the target in.
            if let Some(rank) = out.target_rank {
                stats.ranked += 1;

                for (i, k) in TOPK.iter().enumerate() {
                    if rank < *k {
                        stats.topk[i] += 1;
                    }
                }
            }
            else if out.ranked {
                stats.ranked += 1;
            }

            if let (Some(deg), Some(px)) = (out.nudge_deg, out.nudge_px) {
                self.nudge_deg[slot].add(deg);
                self.nudge_px[slot].add(px);
                self.nudge_n[slot] += 1;

                // Exactly zero, not "in the first bin": a warp that already landed on the
                // target needs no nudge at all, and that is the number worth stating.
                if px == 0.0 {
                    self.nudge_inside[slot] += 1;
                }
            }

            if let Some(ok) = out.flick {
                self.flick_n[slot] += 1;

                if ok {
                    self.flick_ok[slot] += 1;
                }
            }

            if let Some(near) = out.near_count {
                self.near_sum[slot] += near as u64;
                self.near_n[slot]   += 1;
                self.near_hist[slot][near.min(NEAR_BUCKETS - 1)] += 1;
            }

            if let (Some(px), Some(deg)) = (out.err_px, out.err_deg) {
                self.err_px_sum  += px;
                self.err_deg_sum += deg;
                self.err_n       += 1;
            }

            self.fixating += out.fixating.0 as u64;
            self.fed      += out.fixating.1 as u64;

            if out.last_update_correct {
                self.last_correct += 1;
            }

            // The margin sweep only covers widgets: lines are known to be unresolvable at
            // these sigmas and would swamp the table.
            if stats.class == TargetClass::Widget {
                for (slot, margin) in SWEEP_MARGINS.iter().enumerate() {
                    let ambiguous = out.gap_deg.is_some_and(|g| g <= *margin);

                    match out.outcome {
                        Outcome::Correct if ambiguous     => self.sweep[slot][2] += 1,
                        Outcome::Correct                  => self.sweep[slot][0] += 1,
                        Outcome::Slip { .. } if ambiguous => self.sweep[slot][2] += 1,
                        Outcome::Slip { .. }              => self.sweep[slot][1] += 1,
                        _                                 => {}
                    }
                }
            }

            if let Outcome::Slip { chosen } = out.outcome {
                *self.confusion.entry((want, chosen)).or_insert(0) += 1;

                if let (Some(a), Some(b)) = (elements.get(want as usize), elements.get(chosen as usize)) {
                    self.slips[slip_slot(classify_slip(&a.bbox, &b.bbox))] += 1;
                }
            }
        }

        self.elements.push(stats);
    }

    /// Every trial in this frame.
    pub fn tally(&self) -> Tally {
        let mut total = Tally::default();

        for e in &self.elements {
            total.merge(&e.tally);
        }

        total
    }
}

/// Index of a target class in the per-class arrays on `FrameResult`.
pub const fn class_slot(class: TargetClass) -> usize {
    match class {
        TargetClass::Widget    => 0,
        TargetClass::Line      => 1,
        TargetClass::TextOther => 2,
    }
}

/// Mean and `p`-th percentile of a near-candidate histogram, as `(mean, percentile)`.
/// The percentile is reported from the bucket the cumulative count crosses `p` in, so a
/// value landing in the overflow bucket means "at least `NEAR_BUCKETS - 1`".
pub fn near_stats(hist: &[u64; NEAR_BUCKETS], sum: u64, n: u64, p: f64) -> (f64, usize) {
    if n == 0 {
        return (0.0, 0);
    }

    let want = (p * n as f64).ceil() as u64;
    let mut running = 0;

    for (value, count) in hist.iter().enumerate() {
        running += count;

        if running >= want {
            return (sum as f64 / n as f64, value);
        }
    }

    (sum as f64 / n as f64, NEAR_BUCKETS - 1)
}

// --- Histogram ---

impl Histogram {
    /// A histogram of `bins` bins of `width` each. The last bin is an overflow that
    /// catches everything above the range.
    pub fn new(width: f64, bins: usize) -> Histogram {
        Histogram { width: width, bins: vec![0; bins.max(1)], n: 0 }
    }

    /// Counts one observation. Negative values land in the first bin, which only happens
    /// for a rounding artefact on a distance that should have been zero.
    pub fn add(&mut self, value: f64) {
        let slot = {
            if value <= 0.0 {
                0
            }
            else {
                ((value / self.width) as usize).min(self.bins.len() - 1)
            }
        };

        self.bins[slot] += 1;
        self.n += 1;
    }

    /// Folds another histogram of the same shape in.
    pub fn merge(&mut self, other: &Histogram) {
        for (slot, count) in other.bins.iter().enumerate() {
            self.bins[slot] += count;
        }

        self.n += other.n;
    }

    /// Observations counted.
    pub fn count(&self) -> u64 {
        self.n
    }

    /// The `p`-th percentile as `(value, saturated)`. The value is the upper edge of the
    /// bin the cumulative count crosses `p` in, so it reads as "at most this much";
    /// `saturated` is true when that bin is the overflow, where the real value is only
    /// known to be at least the range.
    pub fn percentile(&self, p: f64) -> (f64, bool) {
        if self.n == 0 {
            return (0.0, false);
        }

        let want = (p * self.n as f64).ceil().max(1.0) as u64;
        let mut running = 0;

        for (slot, count) in self.bins.iter().enumerate() {
            running += count;

            if running >= want {
                return (
                    (slot + 1) as f64 * self.width,
                    slot + 1 == self.bins.len(),
                );
            }
        }

        ((self.bins.len()) as f64 * self.width, true)
    }
}

// --- Target classes ---

/// Classifies a target from its box and the detector's kind. See `TargetClass`.
///
/// The shape test comes first on purpose: a 500x22 box the detector labelled `Button` is
/// a terminal row, and calling it a widget would put the hardest targets on the desk into
/// the number Phase 0 is judged by.
pub fn classify_target(bbox: &Rect, kind: ElementKind) -> TargetClass {
    if bbox.h > 0.0 && bbox.h < LINE_MAX_H_PX && bbox.w / bbox.h > LINE_ASPECT {
        return TargetClass::Line;
    }

    match kind {
        ElementKind::Button
        | ElementKind::Icon
        | ElementKind::Input
        | ElementKind::Link
        | ElementKind::Checkbox
        | ElementKind::Slider => TargetClass::Widget,

        ElementKind::Text | ElementKind::Unknown => TargetClass::TextOther,
    }
}

/// Whether a larger element of a different detector kind wraps `elements[index]`.
///
/// Quadratic in the element count, so call it once per candidate set rather than once per
/// run. A few hundred boxes per frame makes that a few tens of thousands of rect tests.
pub fn nested_in_other_kind(index: usize, elements: &[Element]) -> bool {
    let Some(target) = elements.get(index) else {
        return false;
    };

    let area = target.bbox.w * target.bbox.h;

    elements.iter().enumerate().any(|(i, e)| {
        i != index
            && e.kind != target.kind
            && e.bbox.w * e.bbox.h > area
            && classify_slip(&target.bbox, &e.bbox) == SlipClass::Nested
    })
}

// --- RivalPolicy ---

impl RivalPolicy {
    /// Whether a rival with box `rival` counts against a winner with box `winner`.
    pub fn counts(&self, winner: &Rect, rival: &Rect) -> bool {
        match self {
            RivalPolicy::All      => true,
            RivalPolicy::Distinct => classify_slip(winner, rival) != SlipClass::Duplicate,
            RivalPolicy::Separate => classify_slip(winner, rival) == SlipClass::Separate,
        }
    }

    /// Parses the CLI spelling.
    pub fn parse(text: &str) -> Option<RivalPolicy> {
        match text {
            "all"      => Some(RivalPolicy::All),
            "distinct" => Some(RivalPolicy::Distinct),
            "separate" => Some(RivalPolicy::Separate),
            _          => None,
        }
    }

    /// Report label.
    pub fn label(&self) -> &'static str {
        match self {
            RivalPolicy::All      => "every rival counts",
            RivalPolicy::Distinct => "duplicate rivals ignored",
            RivalPolicy::Separate => "duplicate and nested rivals ignored",
        }
    }
}

// --- Slips ---

/// Classifies a slip by how much the chosen box and the intended one overlap.
pub fn classify_slip(target: &Rect, chosen: &Rect) -> SlipClass {
    if target.iou(chosen) >= DUPLICATE_IOU {
        return SlipClass::Duplicate;
    }

    let ix = (target.x + target.w).min(chosen.x + chosen.w) - target.x.max(chosen.x);
    let iy = (target.y + target.h).min(chosen.y + chosen.h) - target.y.max(chosen.y);

    if ix <= 0.0 || iy <= 0.0 {
        return SlipClass::Separate;
    }

    // Against the smaller of the two areas, so "wholly inside" reads the same whichever
    // way round the nesting happens to be.
    let smaller = (target.w * target.h).min(chosen.w * chosen.h);

    if smaller > 0.0 && ix * iy / smaller >= NESTED_CONTAINMENT {
        return SlipClass::Nested;
    }

    SlipClass::Separate
}

/// Index of a slip class in `FrameResult::slips`.
pub const fn slip_slot(class: SlipClass) -> usize {
    match class {
        SlipClass::Duplicate => 0,
        SlipClass::Nested    => 1,
        SlipClass::Separate  => 2,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    use gaze_core::ElementSource;

    #[test]
    fn wide_short_boxes_are_lines_whatever_the_detector_called_them() {
        // A terminal row the widget model labelled a Button.
        let row = Rect { x: 0.0, y: 0.0, w: 350.0, h: 22.0 };

        assert_eq!(classify_target(&row, ElementKind::Button), TargetClass::Line);
        assert_eq!(classify_target(&row, ElementKind::Text), TargetClass::Line);

        // A wide but tall banner is not a line.
        let banner = Rect { x: 0.0, y: 0.0, w: 800.0, h: 90.0 };

        assert_eq!(classify_target(&banner, ElementKind::Button), TargetClass::Widget);

        // An ordinary control.
        let button = Rect { x: 0.0, y: 0.0, w: 80.0, h: 30.0 };

        assert_eq!(classify_target(&button, ElementKind::Button), TargetClass::Widget);
        assert_eq!(classify_target(&button, ElementKind::Text), TargetClass::TextOther);

        // A vertical scrollbar is tall and narrow, not a line.
        let scrollbar = Rect { x: 0.0, y: 0.0, w: 12.0, h: 400.0 };

        assert_eq!(classify_target(&scrollbar, ElementKind::Slider), TargetClass::Widget);

        // A degenerate zero-height box must not divide by zero into a line.
        let degenerate = Rect { x: 0.0, y: 0.0, w: 100.0, h: 0.0 };

        assert_eq!(classify_target(&degenerate, ElementKind::Button), TargetClass::Widget);
    }

    #[test]
    fn the_margin_sweep_counts_only_widgets_and_widens_monotonically() {
        let mut frame = FrameResult::new(0);

        let stats = ElementStats {
            index     : 0,
            kind      : ElementKind::Button,
            class     : TargetClass::Widget,
            bucket    : SizeBucket::Small,
            short_deg : 0.6,
            tally     : Tally { correct: 2, ..Default::default() },
            topk      : [0; TOPK.len()],
            ranked    : 0,
            nested    : false,
        };

        let trial = |gap: Option<f64>| TrialOutput {
            outcome     : Outcome::Correct,
            gap_deg     : gap,
            target_rank : Some(0),
            ranked      : true,
            near_count  : Some(2),
            ..TrialOutput::default()
        };

        // Gaps of 0.3 and 0.8: ambiguous at 0.5 for the first, at 1.0 for both.
        frame.push(stats, &[trial(Some(0.3)), trial(Some(0.8))], &[]);

        assert_eq!(frame.sweep[0], [2, 0, 0]);
        assert_eq!(frame.sweep[1], [1, 0, 1]);
        assert_eq!(frame.sweep[2], [0, 0, 2]);
    }

    #[test]
    fn nesting_needs_a_larger_box_of_a_different_kind() {
        let at = |x: f64, y: f64, w: f64, h: f64, kind: ElementKind| Element {
            id     : 0,
            bbox   : Rect { x: x, y: y, w: w, h: h },
            kind   : kind,
            source : ElementSource::Detector,
            score  : 1.0,
            text   : None,
        };

        // A Text label inside a larger Button row: the canonical case.
        let wrapped = [
            at(10.0, 10.0, 40.0, 16.0, ElementKind::Text),
            at(0.0, 0.0, 200.0, 40.0, ElementKind::Button),
        ];

        assert!(nested_in_other_kind(0, &wrapped));
        // The wrapper is not itself wrapped.
        assert!(!nested_in_other_kind(1, &wrapped));

        // Same nesting, same kind: nothing for the kind term to separate, so it does not
        // count as the case the centre terms exist to fix.
        let same_kind = [
            at(10.0, 10.0, 40.0, 16.0, ElementKind::Button),
            at(0.0, 0.0, 200.0, 40.0, ElementKind::Button),
        ];

        assert!(!nested_in_other_kind(0, &same_kind));

        // Neighbours, not nested.
        let beside = [
            at(0.0, 0.0, 40.0, 16.0, ElementKind::Text),
            at(60.0, 0.0, 200.0, 40.0, ElementKind::Button),
        ];

        assert!(!nested_in_other_kind(0, &beside));

        // An out-of-range index is not a panic.
        assert!(!nested_in_other_kind(9, &beside));
    }

    #[test]
    fn rival_policies_filter_by_overlap() {
        let winner = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };
        let twin   = Rect { x: 2.0, y: 2.0, w: 98.0, h: 98.0 };
        let label  = Rect { x: 10.0, y: 10.0, w: 20.0, h: 20.0 };
        let other  = Rect { x: 500.0, y: 0.0, w: 40.0, h: 40.0 };

        for (policy, twin_counts, label_counts) in [
            (RivalPolicy::All, true, true),
            (RivalPolicy::Distinct, false, true),
            (RivalPolicy::Separate, false, false),
        ] {
            assert_eq!(policy.counts(&winner, &twin), twin_counts, "{policy:?} twin");
            assert_eq!(policy.counts(&winner, &label), label_counts, "{policy:?} label");
            // A genuinely different target always counts.
            assert!(policy.counts(&winner, &other), "{policy:?} other");
        }
    }

    #[test]
    fn rival_policies_round_trip_their_cli_spelling() {
        for text in ["all", "distinct", "separate"] {
            assert!(RivalPolicy::parse(text).is_some(), "{text}");
        }

        assert_eq!(RivalPolicy::parse("nonsense"), None);
    }

    #[test]
    fn slips_are_classified_by_overlap() {
        let a = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

        // Nearly the same box: the same thing detected twice.
        let same = Rect { x: 2.0, y: 2.0, w: 98.0, h: 98.0 };

        assert_eq!(classify_slip(&a, &same), SlipClass::Duplicate);

        // A small box wholly inside a big one, far too different in area to be the same.
        let inside = Rect { x: 10.0, y: 10.0, w: 20.0, h: 20.0 };

        assert_eq!(classify_slip(&a, &inside), SlipClass::Nested);
        assert_eq!(classify_slip(&inside, &a), SlipClass::Nested);

        // Touching neighbours in a toolbar.
        let beside = Rect { x: 90.0, y: 0.0, w: 100.0, h: 100.0 };

        assert_eq!(classify_slip(&a, &beside), SlipClass::Separate);

        // Nothing in common at all.
        let far = Rect { x: 500.0, y: 500.0, w: 10.0, h: 10.0 };

        assert_eq!(classify_slip(&a, &far), SlipClass::Separate);
    }

    #[test]
    fn tallies_count_and_merge() {
        let mut a = Tally::default();

        a.add(Outcome::Correct, false);
        a.add(Outcome::Correct, true);
        a.add(Outcome::Slip { chosen: 3 }, true);
        a.add(Outcome::NoTarget, false);
        a.add(Outcome::NoGaze, false);

        assert_eq!(a.total(), 5);
        assert!((a.correct_pct() - 40.0).abs() < 1.0e-12);
        assert_eq!(a.confident_correct(), 1);
        assert_eq!(a.confident_wrong(), 0);
        assert_eq!(a.ambiguous(), 2);

        let mut b = Tally { correct: 3, ..Default::default() };
        b.merge(&a);

        assert_eq!(b.total(), 8);
        assert_eq!(b.correct, 5);
        assert_eq!(b.confident_correct(), 4);
    }

    #[test]
    fn an_empty_tally_reports_zero_rather_than_nan() {
        let empty = Tally::default();

        assert_eq!(empty.correct_pct(), 0.0);
        assert_eq!(empty.pct(0), 0.0);
    }

    #[test]
    fn buckets_split_on_their_lower_bound() {
        assert_eq!(SizeBucket::for_deg(0.0), SizeBucket::Tiny);
        assert_eq!(SizeBucket::for_deg(0.4999), SizeBucket::Tiny);
        assert_eq!(SizeBucket::for_deg(0.5), SizeBucket::Small);
        assert_eq!(SizeBucket::for_deg(0.9999), SizeBucket::Small);
        assert_eq!(SizeBucket::for_deg(1.0), SizeBucket::Medium);
        assert_eq!(SizeBucket::for_deg(1.9999), SizeBucket::Medium);
        assert_eq!(SizeBucket::for_deg(2.0), SizeBucket::Large);
        assert_eq!(SizeBucket::for_deg(90.0), SizeBucket::Large);
    }

    #[test]
    fn the_short_side_is_the_smaller_of_the_two_angular_sides() {
        // 1000 logical px across 500 mm at 500 mm distance: about 53 deg wide, so roughly
        // 19 px/deg horizontally. A 200x40 box is therefore wide and short, and the short
        // side must be the vertical one.
        let desk = DesktopGeometry::from_toml(
            r#"
            eye_mm = [0.0, 0.0, 500.0]

            [[outputs]]
            name          = "TEST-1"
            logical_x     = 0.0
            logical_y     = 0.0
            logical_w     = 1000.0
            logical_h     = 800.0
            physical_w_mm = 500.0
            physical_h_mm = 400.0
            position_mm   = [0.0, 0.0, 0.0]
            "#,
        )
        .unwrap();

        let bbox = Rect { x: 400.0, y: 380.0, w: 200.0, h: 40.0 };
        let deg  = short_side_deg(&desk, &bbox);

        let (ppd_x, ppd_y) = px_per_deg_at(&desk, bbox.center());

        assert!((deg - 40.0 / ppd_y).abs() < 1.0e-9);
        assert!(40.0 / ppd_y < 200.0 / ppd_x);
    }

    #[test]
    fn near_stats_read_the_percentile_out_of_the_histogram() {
        let mut hist = [0_u64; NEAR_BUCKETS];

        // Ten trials: eight saw one candidate, one saw four, one saw twelve.
        hist[1] = 8;
        hist[4] = 1;
        hist[12] = 1;

        let (mean, p90) = near_stats(&hist, 8 + 4 + 12, 10, 0.9);

        assert!((mean - 2.4).abs() < 1.0e-12, "mean = {mean}");
        assert_eq!(p90, 4);

        // An empty histogram reports zeroes rather than dividing by zero.
        assert_eq!(near_stats(&[0; NEAR_BUCKETS], 0, 0, 0.9), (0.0, 0));
    }

    #[test]
    fn frame_results_accumulate_confusions_and_errors() {
        let mut frame = FrameResult::new(0);

        let stats = ElementStats {
            index     : 2,
            kind      : ElementKind::Button,
            class     : TargetClass::Widget,
            bucket    : SizeBucket::Small,
            short_deg : 0.6,
            tally     : Tally { correct: 1, slip: 2, ..Default::default() },
            topk      : [0; TOPK.len()],
            ranked    : 0,
            nested    : false,
        };

        let trials = [
            TrialOutput {
                outcome             : Outcome::Correct,
                err_px              : Some(10.0),
                err_deg             : Some(0.2),
                fixating            : (24, 24),
                last_update_correct : true,
                target_rank         : Some(0),
                ranked              : true,
                near_count          : Some(1),
                nudge_px            : Some(0.0),
                nudge_deg           : Some(0.0),
                flick               : Some(true),
                ..TrialOutput::default()
            },
            TrialOutput {
                outcome     : Outcome::Slip { chosen: 7 },
                err_px      : Some(20.0),
                err_deg     : Some(0.4),
                fixating    : (12, 24),
                ambiguous   : true,
                gap_deg     : Some(0.1),
                target_rank : Some(1),
                ranked      : true,
                near_count  : Some(3),
                nudge_px    : Some(30.0),
                nudge_deg   : Some(0.5),
                flick       : Some(false),
                ..TrialOutput::default()
            },
            TrialOutput {
                outcome  : Outcome::Slip { chosen: 7 },
                fixating : (0, 24),
                ..TrialOutput::default()
            },
        ];

        let elements = [
            Element {
                id     : 2,
                bbox   : Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 },
                kind   : ElementKind::Button,
                source : ElementSource::Detector,
                score  : 1.0,
                text   : None,
            },
        ];

        frame.push(stats, &trials, &elements);

        assert_eq!(frame.err_n, 2);
        assert!((frame.err_px_sum - 30.0).abs() < 1.0e-12);
        assert_eq!(frame.confusion.get(&(2, 7)), Some(&2));
        assert_eq!(frame.fed, 72);
        assert_eq!(frame.fixating, 36);
        assert_eq!(frame.last_correct, 1);
        assert_eq!(frame.tally().total(), 3);

        // Two trials ranked, both with the target inside the top two, one inside the top
        // one; the third ranked nothing and must not reach the denominator.
        let stats = &frame.elements[0];

        assert_eq!(stats.ranked, 2);
        assert_eq!(stats.topk, [1, 2, 2, 2]);
        assert_eq!(frame.near_n[0], 2);
        assert_eq!(frame.near_sum[0], 4);

        // One warp landed on the target and one 30 px away, so the "already there" count
        // is exactly the zero-distance trial, not everything in the first bin.
        assert_eq!(frame.nudge_n[0], 2);
        assert_eq!(frame.nudge_inside[0], 1);
        assert_eq!(frame.flick_n[0], 2);
        assert_eq!(frame.flick_ok[0], 1);
    }
}
