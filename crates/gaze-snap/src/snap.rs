//! Fuzzy hit testing: turn a noisy gaze point into the target the user meant.
//!
//! The shape is Apple's fuzzy hit testing patent ([patent reference removed]): enumerate the
//! candidates near the gaze point, rank them by element type, then by nesting depth,
//! then by angular distance, and favour the target that is already selected so the
//! highlight does not flicker between neighbours. Two deliberate departures:
//!
//! * The ranking is a weighted sum, not a lexicographic sort. Lexicographic ranking says
//!   a button two degrees away always beats a text run under the gaze point, which is
//!   wrong at the sigmas we care about. The weights are documented below and are
//!   builder-configurable.
//! * Nesting depth is approximated by box area, because detector boxes carry no tree.
//!   A child inside a parent is smaller, so smaller wins.
//! * There are two optional terms for where inside a box the gaze fell, both off by
//!   default. Angular distance is measured to the nearest edge, so it is zero for every
//!   box that contains the gaze point, and in a dense or nested layout that is most of
//!   them: sidebar rows at half a degree of pitch, a label box inside its row. See
//!   [`ScoreWeights::center`] and [`ScoreWeights::center_deg`].
//!
//! On top of that sits late-trigger correction (DESIGN.md section 3, principle 7):
//! 86.6% of gaze-plus-commit errors are commits that land after the eyes have already
//! moved on, so [`SnapEngine::commit`] attributes to the target that was fixated one
//! commit latency ago rather than to whatever is under the gaze right now.
//!
//! Pure and deterministic: no clocks, no devices, timestamps arrive on the samples.

use std::collections::VecDeque;
use std::fmt;

use gaze_core::{Element, ElementKind, GlobalPx};

use crate::filter::{Filtered, FixationState};
use crate::scale::{ConstPxScale, PxScale};

/// Default snap radius. visionOS's minimum eye target is about 2.5 deg and its snap
/// acceptance radius about 1.5 deg; 2.0 sits between them and matches an ET5-class
/// sigma of 0.7 with headroom.
pub const DEFAULT_RADIUS_DEG : f64 = 2.0;

/// Default hysteresis margin, in score units. With the default weights, score units are
/// degrees of angular distance, so this is "a challenger must be 0.15 deg better".
pub const DEFAULT_HYSTERESIS_MARGIN : f64 = 0.15;

/// Default ring buffer span. Comfortably longer than the 150-400 ms of commit latency it
/// exists to correct, without keeping targets around long enough to be irrelevant.
pub const DEFAULT_RING_WINDOW_S : f64 = 1.5;

/// How far past the radius the cheap pixel reject reaches, as a multiple.
///
/// The reject box is sized with the scale at the gaze point, but a candidate may sit on a
/// neighbouring output with a different scale, where the same pixel offset is a smaller
/// angle. The slack keeps such candidates alive until the exact angular test can judge
/// them. Two covers outputs differing by up to 2x in px/deg.
pub const DEFAULT_REJECT_SLACK : f64 = 2.0;

/// Type-priority penalties, indexed by [`kind_slot`]. Lower is better.
///
/// Interactive controls form one tier, text runs are a worse target than any control at
/// the same distance, and an unclassified box is worse still. Icons share the top tier
/// because a detector cannot tell a toolbar button from a decorative glyph.
pub const KIND_PENALTY : [f64; 8] = [
    0.0, // Button
    0.0, // Icon
    0.0, // Input
    0.0, // Link
    1.0, // Text
    0.0, // Checkbox
    0.0, // Slider
    2.0, // Unknown
];

/// A resolved target: the element, the point an injector should click, and the score it
/// won with.
#[derive(Clone, Debug, PartialEq)]
pub struct SnapTarget {
    pub element : Element,
    /// The gaze point clamped into the element's box, so a click always lands on the
    /// element even when the gaze fell just outside it.
    pub point   : GlobalPx,
    /// Match cost, lower is better. See [`ScoreWeights`].
    pub score   : f64,
}

/// Weights of the three ranking terms. All are costs: lower total wins.
///
/// The units are chosen so `distance` is the natural yardstick. With the defaults, one
/// score unit is one degree of angular distance, which makes the other two weights
/// readable as "how many degrees of extra distance this element type / this much extra
/// area is worth".
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ScoreWeights {
    /// Multiplier on [`KIND_PENALTY`]. At the default, a text run has to be 0.6 deg
    /// closer than a control to win, and an unclassified box 1.2 deg closer.
    pub kind       : f64,
    /// Multiplier on `ln(1 + area_deg2)`. The log keeps a full-screen container from
    /// swamping the distance term while still separating a 0.4 deg icon (0.15) from a
    /// 6 x 1.7 deg toolbar (2.5): about 0.35 deg of distance at the default weight.
    pub area       : f64,
    /// Multiplier on angular distance in degrees from the gaze point to the nearest
    /// point of the box. One by definition: it sets the unit.
    pub distance   : f64,
    /// Multiplier on the normalised offset from the box centre: the gaze-to-centre
    /// offset divided by the box half-extent per axis, combined across the axes and
    /// clamped at 1. Zero at the centre, 1 at or beyond the edge.
    ///
    /// Unitless, so the weight carries the scale: `center: 0.3` reads as "sitting at the
    /// edge of a box rather than its centre costs 0.3 deg". The clamp stops it
    /// double-counting with `distance`, which already handles everything outside the box,
    /// and stops a thin OCR line box being penalised out of existence by its own tiny
    /// half-height.
    ///
    /// This is the term that separates boxes which all contain the gaze point, where
    /// `distance` is zero for every one of them: a label nested in a list row, a dense
    /// column of rows. Default 0.0, so it changes nothing until the bench sweeps it.
    pub center     : f64,
    /// Multiplier on raw angular distance in degrees from the gaze point to the box
    /// centre. The unclamped, size-independent alternative to `center`: it penalises a
    /// wide row hard when the gaze sits at one end of it, where `center` has saturated.
    /// Directly comparable to `distance`, and unbounded. Default 0.0.
    pub center_deg : f64,
}

/// One candidate considered during an update. Kept in a reused scratch buffer so the
/// overlay and the offline bench can see why a decision went the way it did.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Candidate {
    /// Index into the `elements` slice passed to [`SnapEngine::update`].
    pub index        : usize,
    /// Element id, so a candidate list outlives the slice it came from.
    pub id           : u64,
    /// Total cost, the number the ranking sorts on.
    pub score        : f64,
    /// Angular distance from the gaze point to the nearest point of the box, degrees.
    /// Zero for any box containing the gaze point, which is what makes the centre terms
    /// necessary in dense and nested layouts.
    pub distance_deg : f64,
    pub area_deg2    : f64,
    /// Offset from the box centre, normalised per axis by the half-extent and clamped
    /// at 1. Feeds [`ScoreWeights::center`].
    pub center_norm  : f64,
    /// Angular distance from the gaze point to the box centre, degrees, unclamped.
    /// Feeds [`ScoreWeights::center_deg`].
    pub center_deg   : f64,
}

/// Resolves gaze samples to targets and remembers recent ones for late commits.
pub struct SnapEngine {
    scale             : Box<dyn PxScale>,
    radius_deg        : f64,
    hysteresis_margin : f64,
    ring_window_s     : f64,
    reject_slack      : f64,
    weights           : ScoreWeights,
    current           : Option<SnapTarget>,
    /// Recent fixation targets as runs, newest last. A run is one uninterrupted stretch
    /// of samples that resolved to the same element, which keeps a long fixation to a
    /// single entry instead of one per sample.
    ring              : VecDeque<TargetRun>,
    /// Candidate scratch, cleared and refilled per update so steady state does not
    /// allocate.
    scratch           : Vec<Candidate>,
}

/// Builder for [`SnapEngine`]. See the constants in this module for the defaults.
pub struct SnapEngineBuilder {
    scale             : Option<Box<dyn PxScale>>,
    radius_deg        : f64,
    hysteresis_margin : f64,
    ring_window_s     : f64,
    reject_slack      : f64,
    weights           : ScoreWeights,
}

/// One uninterrupted stretch of fixation samples that resolved to the same target.
#[derive(Clone, Debug)]
struct TargetRun {
    t_start_s : f64,
    t_end_s   : f64,
    target    : SnapTarget,
}

// --- SnapEngine ---

impl SnapEngine {
    /// Starts building a snap engine.
    pub fn create() -> SnapEngineBuilder {
        SnapEngineBuilder::new()
    }

    /// Resolves one filtered sample against the current element index.
    ///
    /// Call this for every sample. Cost is one cheap rejection test per element plus two
    /// scale lookups per surviving candidate, so a few thousand boxes on an ultrawide are
    /// fine at gaze rate. Returns `None`, and forgets the current target, when the sample
    /// is unusable: invalid, without a point, or with a sigma wider than the snap radius,
    /// which is the handoff to the coarse tier (head pose, zoom refine) rather than a
    /// guess the user would have to undo.
    pub fn update(&mut self, f: &Filtered, elements: &[Element]) -> Option<SnapTarget> {
        self.scratch.clear();

        // A sigma wider than the radius means the candidate set is a coin flip; refuse.
        if !f.sample.valid
            || !f.sample.sigma_deg.is_finite()
            || f.sample.sigma_deg > self.radius_deg
        {
            self.current = None;

            return None;
        }

        let Some(gaze) = f.sample.point else {
            self.current = None;

            return None;
        };

        let (gaze_ppd_x, gaze_ppd_y) = self.scale.px_per_deg(gaze);

        if gaze_ppd_x <= 0.0 || gaze_ppd_y <= 0.0 {
            self.current = None;

            return None;
        }

        self.collect(gaze, gaze_ppd_x, gaze_ppd_y, elements);

        match self.choose() {
            Some(winner) => {
                let target = self.adopt(&winner, gaze, elements);

                if let FixationState::Fixating { .. } = f.state {
                    self.record(f.sample.t_s, &target);
                }

                Some(target)
            }

            None => {
                self.current = None;

                None
            }
        }
    }

    /// Attributes a commit that happened at `t_s` to the target the user was looking at
    /// `latency_s` earlier.
    ///
    /// This is the late-trigger correction. A commit channel with hundreds of
    /// milliseconds of latency (voice, and to a lesser degree a pedal) lands after the
    /// eyes have already left the target, so `latency_s` should be that channel's
    /// measured lag, typically 0.15 to 0.4 s. Zero attributes to the present.
    ///
    /// Resolution rule: the run that contains `t_s - latency_s`, or if that instant falls
    /// in a saccade, the run that ended before it. Nothing at all is remembered inside the
    /// ring window, so a commit during an unbroken saccade returns `None` rather than
    /// clicking whatever happens to be under the gaze.
    pub fn commit(&mut self, t_s: f64, latency_s: f64) -> Option<SnapTarget> {
        self.prune_ring(t_s);

        let at_s = t_s - latency_s;

        // Newest first: the last run that had already started by `at_s`.
        if let Some(run) = self.ring.iter().rev().find(|r| r.t_start_s <= at_s) {
            return Some(run.target.clone());
        }

        // `at_s` predates everything remembered. The oldest run is the closest thing to
        // an answer; a longer ring window is the fix if this happens often.
        self.ring.front().map(|r| r.target.clone())
    }

    /// The target as of the last update, if any.
    pub fn current(&self) -> Option<&SnapTarget> {
        self.current.as_ref()
    }

    /// Candidates scored by the last update, cheapest first. Debug and bench use.
    pub fn candidates(&self) -> &[Candidate] {
        &self.scratch
    }

    /// Candidates scored by the last update in ranked order, best first.
    ///
    /// Same data as [`SnapEngine::candidates`], for callers that want the top few: how
    /// often the intended target is in the top 2 to 4 is what decides whether an
    /// ambiguous snap can be settled with a small disambiguation hint instead of a full
    /// refinement step. Note that the head of this iterator is the cheapest candidate,
    /// which is not necessarily what [`SnapEngine::update`] returned: hysteresis can hold
    /// a target that is no longer the cheapest.
    pub fn ranked(&self) -> impl Iterator<Item = &Candidate> {
        self.scratch.iter()
    }

    /// Forgets the current target and the fixation history. Use when the element index is
    /// replaced wholesale or the session is otherwise discontinuous.
    pub fn reset(&mut self) {
        self.current = None;
        self.ring.clear();
        self.scratch.clear();
    }

    /// Snap radius in degrees.
    pub fn radius_deg(&self) -> f64 {
        self.radius_deg
    }

    /// Scoring weights in effect.
    pub fn weights(&self) -> ScoreWeights {
        self.weights
    }
}

impl SnapEngine {
    /// Fills the scratch buffer with every element within the snap radius, scored.
    ///
    /// Two passes over each element in effect: an axis-aligned pixel rejection that costs
    /// four comparisons, then the real work for the handful that survive.
    fn collect(
        &mut self,
        gaze       : GlobalPx,
        gaze_ppd_x : f64,
        gaze_ppd_y : f64,
        elements   : &[Element],
    ) {
        let reject_x = self.radius_deg * gaze_ppd_x * self.reject_slack;
        let reject_y = self.radius_deg * gaze_ppd_y * self.reject_slack;

        for (index, e) in elements.iter().enumerate() {
            let b = &e.bbox;

            if gaze.x < b.x - reject_x
                || gaze.x > b.x + b.w + reject_x
                || gaze.y < b.y - reject_y
                || gaze.y > b.y + b.h + reject_y
            {
                continue;
            }

            // The scale at the element and the scale at the gaze point can differ (the
            // two ends may be on different outputs), so integrate across the gap with
            // their mean rather than assuming the gaze point's scale holds all the way.
            let near             = b.clamp(gaze);
            let (near_x, near_y) = self.scale.px_per_deg(near);
            let ppd_x            = 0.5 * (gaze_ppd_x + near_x);
            let ppd_y            = 0.5 * (gaze_ppd_y + near_y);

            if ppd_x <= 0.0 || ppd_y <= 0.0 || near_x <= 0.0 || near_y <= 0.0 {
                continue;
            }

            let distance_deg = ((gaze.x - near.x) / ppd_x).hypot((gaze.y - near.y) / ppd_y);

            if distance_deg > self.radius_deg {
                continue;
            }

            // Area in degrees squared, at the element's own scale: the same widget is the
            // same target whichever output it is on.
            let area_deg2 = (b.w / near_x) * (b.h / near_y);

            // Where in the box the gaze fell. `distance_deg` is zero for every box that
            // contains the gaze point, so in a nested or dense layout these two terms are
            // the only thing left to separate the candidates. The half-extents are floored
            // at half a pixel so a degenerate detector box cannot produce an infinity.
            let centre      = b.center();
            let half_x      = (b.w * 0.5).max(0.5);
            let half_y      = (b.h * 0.5).max(0.5);
            let center_norm = ((gaze.x - centre.x) / half_x)
                .hypot((gaze.y - centre.y) / half_y)
                .min(1.0);
            let center_deg  = ((gaze.x - centre.x) / ppd_x)
                .hypot((gaze.y - centre.y) / ppd_y);

            let score = self.weights.kind * KIND_PENALTY[kind_slot(e.kind)]
                + self.weights.area * (1.0 + area_deg2).ln()
                + self.weights.distance * distance_deg
                + self.weights.center * center_norm
                + self.weights.center_deg * center_deg;

            self.scratch.push(Candidate {
                index        : index,
                id           : e.id,
                score        : score,
                distance_deg : distance_deg,
                area_deg2    : area_deg2,
                center_norm  : center_norm,
                center_deg   : center_deg,
            });
        }

        // Cost order, so `ranked` and `choose` are both a read off the front. Ties break
        // on element order, which keeps the whole engine deterministic under an unstable
        // sort, and an unstable sort is what keeps this allocation-free.
        self.scratch.sort_unstable_by(|a, b| {
            a.score.total_cmp(&b.score).then(a.index.cmp(&b.index))
        });
    }

    /// Picks the winning candidate, applying hysteresis.
    ///
    /// The current target is kept unless a challenger beats it by the margin, which is
    /// what stops the highlight flickering between two neighbours while the gaze jitters
    /// across the boundary between them. A current target that has left the radius has no
    /// score to defend and loses outright.
    fn choose(&self) -> Option<Candidate> {
        let best = *self.scratch.first()?;

        let held = self
            .current
            .as_ref()
            .and_then(|cur| self.scratch.iter().find(|c| c.id == cur.element.id));

        match held {
            Some(h) if best.score >= h.score - self.hysteresis_margin => Some(*h),
            _                                                        => Some(best),
        }
    }

    /// Installs the chosen candidate as the current target. Reuses the existing element
    /// clone when the target has not changed, so a steady fixation does not allocate.
    fn adopt(&mut self, winner: &Candidate, gaze: GlobalPx, elements: &[Element])
        -> SnapTarget
    {
        let e     = &elements[winner.index];
        let point = e.bbox.clamp(gaze);

        match &mut self.current {
            Some(cur) if cur.element.id == e.id => {
                cur.point = point;
                cur.score = winner.score;
            }

            slot => {
                *slot = Some(SnapTarget {
                    element : e.clone(),
                    point   : point,
                    score   : winner.score,
                });
            }
        }

        self.current.clone().expect("just installed")
    }

    /// Appends a sample to the fixation history, extending the current run when the
    /// target has not changed.
    fn record(&mut self, t_s: f64, target: &SnapTarget) {
        match self.ring.back_mut() {
            Some(run) if run.target.element.id == target.element.id && t_s >= run.t_end_s => {
                run.t_end_s = t_s;
            }

            _ => {
                self.ring.push_back(TargetRun {
                    t_start_s : t_s,
                    t_end_s   : t_s,
                    target    : target.clone(),
                });
            }
        }

        self.prune_ring(t_s);
    }

    /// Drops runs that ended before the ring window.
    fn prune_ring(&mut self, now_s: f64) {
        let cutoff = now_s - self.ring_window_s;

        while self.ring.front().is_some_and(|r| r.t_end_s < cutoff) {
            self.ring.pop_front();
        }
    }
}

impl fmt::Debug for SnapEngine {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SnapEngine")
            .field("radius_deg"        , &self.radius_deg)
            .field("hysteresis_margin" , &self.hysteresis_margin)
            .field("ring_window_s"     , &self.ring_window_s)
            .field("reject_slack"      , &self.reject_slack)
            .field("weights"           , &self.weights)
            .field("current"           , &self.current.as_ref().map(|t| t.element.id))
            .field("ring_runs"         , &self.ring.len())
            .finish_non_exhaustive()
    }
}

// --- SnapEngineBuilder ---

impl SnapEngineBuilder {
    /// A builder holding the module defaults.
    pub fn new() -> Self {
        Self {
            scale             : None,
            radius_deg        : DEFAULT_RADIUS_DEG,
            hysteresis_margin : DEFAULT_HYSTERESIS_MARGIN,
            ring_window_s     : DEFAULT_RING_WINDOW_S,
            reject_slack      : DEFAULT_REJECT_SLACK,
            weights           : ScoreWeights::default(),
        }
    }

    /// Scale source for degree/pixel conversion. Defaults to a flat
    /// [`crate::scale::FALLBACK_PX_PER_DEG`] world, which is a placeholder: pass the desk
    /// geometry in real use.
    pub fn scale(mut self, scale: Box<dyn PxScale>) -> Self {
        self.scale = Some(scale);

        self
    }

    /// Snap radius: no element further than this from the gaze point can be chosen.
    pub fn radius_deg(mut self, radius_deg: f64) -> Self {
        self.radius_deg = radius_deg;

        self
    }

    /// How much better a challenger must score to take the current target's place.
    pub fn hysteresis_margin(mut self, margin: f64) -> Self {
        self.hysteresis_margin = margin;

        self
    }

    /// How far back [`SnapEngine::commit`] can look.
    pub fn ring_window_s(mut self, window_s: f64) -> Self {
        self.ring_window_s = window_s;

        self
    }

    /// Reach of the cheap pixel rejection, as a multiple of the radius. See
    /// [`DEFAULT_REJECT_SLACK`].
    pub fn reject_slack(mut self, slack: f64) -> Self {
        self.reject_slack = slack;

        self
    }

    /// Ranking weights.
    pub fn weights(mut self, weights: ScoreWeights) -> Self {
        self.weights = weights;

        self
    }

    /// Builds the engine.
    pub fn build(self) -> SnapEngine {
        SnapEngine {
            scale             : self.scale.unwrap_or_else(
                || Box::new(ConstPxScale::default())
            ),
            radius_deg        : self.radius_deg,
            hysteresis_margin : self.hysteresis_margin,
            ring_window_s     : self.ring_window_s,
            reject_slack      : self.reject_slack,
            weights           : self.weights,
            current           : None,
            ring              : VecDeque::with_capacity(16),
            scratch           : Vec::with_capacity(32),
        }
    }
}

impl Default for SnapEngineBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// --- ScoreWeights ---

impl ScoreWeights {
    /// Reads a weight list in the canonical order: `kind, area, distance, center,
    /// center_deg`. Entries past the end of the list keep their default, so
    /// `from_list(&[0.6, 0.15, 1.0, 0.3])` sweeps the normalised centre term and leaves
    /// the angular one off.
    ///
    /// This exists so the order lives in one place: the offline bench takes
    /// `--weights 0.6,0.15,1.0,0.3` as a bare comma list and hands the parsed floats
    /// straight here.
    pub fn from_list(values: &[f64]) -> Self {
        let mut w = Self::default();

        for (slot, v) in [
            &mut w.kind,
            &mut w.area,
            &mut w.distance,
            &mut w.center,
            &mut w.center_deg,
        ]
        .into_iter()
        .zip(values)
        {
            *slot = *v;
        }

        w
    }

    /// The weights in the same order [`ScoreWeights::from_list`] reads them.
    pub fn to_list(&self) -> [f64; 5] {
        [self.kind, self.area, self.distance, self.center, self.center_deg]
    }
}

impl Default for ScoreWeights {
    fn default() -> Self {
        Self {
            // Bench v3 (2026-08-26, real desktop captures, sigma 0.7 deg, widget targets):
            // kind 1.5 + center_deg 0.2 gave +3 correct, +3.7 top-3, +4.8 on targets nested
            // inside a box of another kind, for +3 confident-wrong. With a hover highlight
            // and a physical commit a confident-wrong snap is a visible wrong highlight the
            // user nudges, not a wrong click, so the accuracy side of that trade is right.
            kind       : 1.5,
            area       : 0.15,
            distance   : 1.0,
            // Normalised centre term measured harmful (saturates for rows and labels alike).
            center     : 0.0,
            center_deg : 0.2,
        }
    }
}

/// Index into [`KIND_PENALTY`]. Mirrors the declaration order of [`ElementKind`].
const fn kind_slot(kind: ElementKind) -> usize {
    match kind {
        ElementKind::Button   => 0,
        ElementKind::Icon     => 1,
        ElementKind::Input    => 2,
        ElementKind::Link     => 3,
        ElementKind::Text     => 4,
        ElementKind::Checkbox => 5,
        ElementKind::Slider   => 6,
        ElementKind::Unknown  => 7,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::{ElementSource, GazeSample, Rect};

    use super::*;
    use crate::filter::FilterStack;

    /// Flat test world: 60 logical px per degree on both axes.
    const PPD : f64 = 60.0;

    /// A scale that changes at a seam, standing in for two outputs with different pixel
    /// densities. Left of `seam_x` is dense, right of it is coarse.
    struct SeamScale {
        seam_x : f64,
        left   : f64,
        right  : f64,
    }

    impl PxScale for SeamScale {
        fn px_per_deg(&self, p: GlobalPx) -> (f64, f64) {
            let s = {
                if p.x < self.seam_x {
                    self.left
                }
                else {
                    self.right
                }
            };

            (s, s)
        }
    }

    /// An element with a synthetic id.
    fn el(id: u64, kind: ElementKind, x: f64, y: f64, w: f64, h: f64) -> Element {
        Element {
            id     : id,
            bbox   : Rect { x: x, y: y, w: w, h: h },
            kind   : kind,
            source : ElementSource::Detector,
            score  : 0.9,
            text   : None,
        }
    }

    /// A fixating sample at a point, at ET5-class sigma.
    fn fixating(t_s: f64, x: f64, y: f64) -> Filtered {
        Filtered {
            sample : GazeSample {
                t_s       : t_s,
                ray       : None,
                point     : Some(GlobalPx { x: x, y: y }),
                sigma_deg : 0.7,
                valid     : true,
            },
            state  : FixationState::Fixating { since_s: 0.0 },
        }
    }

    /// An engine over the flat test world with the documented defaults.
    /// The pre-v3 weights (no kind emphasis, no centre terms), for tests that isolate a
    /// single term against a neutral baseline.
    fn baseline() -> ScoreWeights {
        ScoreWeights { kind: 0.6, area: 0.15, distance: 1.0, center: 0.0, center_deg: 0.0 }
    }

    fn engine() -> SnapEngine {
        SnapEngine::create()
            .scale(Box::new(ConstPxScale::new(PPD)))
            .radius_deg(DEFAULT_RADIUS_DEG)
            .hysteresis_margin(DEFAULT_HYSTERESIS_MARGIN)
            .ring_window_s(DEFAULT_RING_WINDOW_S)
            .build()
    }

    #[test]
    fn nested_child_wins_over_its_parent() {
        // Same kind and the same zero distance, so only the nesting proxy separates them.
        let elements = vec![
            el(1, ElementKind::Button, 400.0, 400.0, 400.0, 200.0),
            el(2, ElementKind::Button, 560.0, 480.0,  40.0,  20.0),
        ];
        let mut e = engine();
        let t     = e.update(&fixating(0.0, 580.0, 490.0), &elements).unwrap();

        assert_eq!(t.element.id, 2);
        assert_eq!(e.candidates().len(), 2);
    }

    #[test]
    fn snap_point_is_clamped_into_the_box() {
        let elements = vec![el(1, ElementKind::Button, 500.0, 500.0, 40.0, 20.0)];
        let mut e    = engine();

        // 30 px above the box: half a degree, well inside the radius.
        let t = e.update(&fixating(0.0, 520.0, 470.0), &elements).unwrap();

        assert_eq!(t.point, GlobalPx { x: 520.0, y: 500.0 });
    }

    #[test]
    fn nothing_within_the_radius_snaps_to_nothing() {
        // 10 degrees away.
        let elements = vec![el(1, ElementKind::Button, 1100.0, 500.0, 40.0, 20.0)];
        let mut e    = engine();

        assert!(e.update(&fixating(0.0, 500.0, 500.0), &elements).is_none());
        assert!(e.current().is_none());
    }

    /// A row of 24 px icons on a 32 px pitch: the density that actually breaks naive
    /// hit testing at 0.7 deg of gaze noise.
    fn toolbar() -> Vec<Element> {
        (0..8)
            .map(|i| el(i as u64, ElementKind::Icon, 100.0 + i as f64 * 32.0, 100.0, 24.0, 24.0))
            .collect()
    }

    #[test]
    fn dense_toolbar_picks_the_nearest_icon() {
        let elements = toolbar();

        // Icon 3 spans x 196..220, centre 208; the midpoint to icon 4 is at 224.
        for (offset, expected) in [(-15.0, 3), (-8.0, 3), (0.0, 3), (8.0, 3), (15.0, 3)] {
            let mut e = engine();
            let t     = e.update(&fixating(0.0, 208.0 + offset, 112.0), &elements).unwrap();

            assert_eq!(t.element.id, expected, "offset {offset}");
        }
    }

    #[test]
    fn hysteresis_holds_across_the_midpoint() {
        let elements = toolbar();
        let mut e    = engine();

        // Settle on icon 3.
        assert_eq!(e.update(&fixating(0.0, 208.0, 112.0), &elements).unwrap().element.id, 3);

        // Jitter across the 224 midpoint toward icon 4. The distance difference here is
        // 0.067 deg, under the 0.15 margin, so the highlight must not follow.
        for (i, offset) in [18.0, 12.0, 19.0, 14.0].into_iter().enumerate() {
            let t = e
                .update(&fixating(0.01 * i as f64, 208.0 + offset, 112.0), &elements)
                .unwrap();

            assert_eq!(t.element.id, 3, "offset {offset}");
        }

        // A real move onto icon 4 wins: inside its box, 0.3 deg better than icon 3.
        let t = e.update(&fixating(0.1, 240.0, 112.0), &elements).unwrap();

        assert_eq!(t.element.id, 4);
    }

    #[test]
    fn button_beats_text_at_equal_distance() {
        // Same size, same 30 px (0.5 deg) gap on either side of the gaze point.
        let elements = vec![
            el(1, ElementKind::Text  , 410.0, 490.0, 60.0, 20.0),
            el(2, ElementKind::Button, 530.0, 490.0, 60.0, 20.0),
        ];
        let mut e = engine();
        let t     = e.update(&fixating(0.0, 500.0, 500.0), &elements).unwrap();

        assert_eq!(t.element.id, 2);
    }

    #[test]
    fn text_wins_when_it_is_clearly_closer() {
        // Gaze inside the text run, button 1.2 deg away: past the 0.6 deg the type
        // penalty is worth.
        let elements = vec![
            el(1, ElementKind::Text  , 470.0, 490.0, 60.0, 20.0),
            el(2, ElementKind::Button, 572.0, 490.0, 60.0, 20.0),
        ];
        let mut e = engine();
        let t     = e.update(&fixating(0.0, 500.0, 500.0), &elements).unwrap();

        assert_eq!(t.element.id, 1);
    }

    #[test]
    fn angular_distance_uses_the_scale_at_the_element() {
        // Two boxes 60 px either side of the gaze point, which sits on the seam. The
        // left one is on a 60 px/deg output, so it is 1.5 deg away once the gap is
        // integrated across both scales; the right one is on a 20 px/deg output, so the
        // same 60 px is 3 deg and falls outside the 2 deg radius entirely.
        let elements = vec![
            el(1, ElementKind::Button,  900.0, 490.0, 40.0, 20.0),
            el(2, ElementKind::Button, 1060.0, 490.0, 40.0, 20.0),
        ];
        let mut e = SnapEngine::create()
            .scale(Box::new(SeamScale { seam_x: 1000.0, left: 60.0, right: 20.0 }))
            .radius_deg(DEFAULT_RADIUS_DEG)
            .build();

        let t = e.update(&fixating(0.0, 1000.0, 500.0), &elements).unwrap();

        assert_eq!(t.element.id, 1);
        assert_eq!(e.candidates().len(), 1);
        assert!((e.candidates()[0].distance_deg - 1.5).abs() < 1e-9);
    }

    #[test]
    fn sigma_wider_than_the_radius_bails_out() {
        let elements = vec![el(1, ElementKind::Button, 490.0, 490.0, 40.0, 20.0)];
        let mut e    = engine();

        assert!(e.update(&fixating(0.0, 500.0, 500.0), &elements).is_some());

        let mut coarse          = fixating(0.01, 500.0, 500.0);
        coarse.sample.sigma_deg = 2.5;

        assert!(e.update(&coarse, &elements).is_none());
        assert!(e.current().is_none());
    }

    #[test]
    fn invalid_sample_bails_out() {
        let elements = vec![el(1, ElementKind::Button, 490.0, 490.0, 40.0, 20.0)];
        let mut e    = engine();

        assert!(e.update(&fixating(0.0, 500.0, 500.0), &elements).is_some());

        let mut lost      = fixating(0.01, 500.0, 500.0);
        lost.sample.valid = false;

        assert!(e.update(&lost, &elements).is_none());
        assert!(e.current().is_none());
    }

    /// Two buttons 10 degrees apart, the layout the late-trigger tests run on.
    fn two_buttons() -> Vec<Element> {
        vec![
            el(1, ElementKind::Button, 160.0, 190.0, 80.0, 20.0),
            el(2, ElementKind::Button, 760.0, 190.0, 80.0, 20.0),
        ]
    }

    /// Runs the filter and the engine over a fixate-saccade-fixate sequence at 100 Hz:
    /// 400 ms on A, a jump, then 120 ms on B. Returns the engine and the timestamp of
    /// the last sample.
    fn a_then_b() -> (SnapEngine, f64) {
        let elements = two_buttons();
        let mut filt = FilterStack::create()
            .scale(Box::new(ConstPxScale::new(PPD)))
            .build();
        let mut e    = engine();
        let mut last = 0.0;

        for i in 0..54 {
            let t = i as f64 * 0.01;
            let x = {
                if t < 0.41 {
                    200.0
                }
                else {
                    800.0
                }
            };

            let f = filt.push(GazeSample {
                t_s       : t,
                ray       : None,
                point     : Some(GlobalPx { x: x, y: 200.0 }),
                sigma_deg : 0.7,
                valid     : true,
            });

            e.update(&f, &elements);
            last = t;
        }

        (e, last)
    }

    #[test]
    fn late_commit_attributes_to_the_previous_fixation() {
        let (mut e, last) = a_then_b();

        // The eyes have been on B for 120 ms, but the commit channel is 200 ms slow, so
        // the user meant A.
        assert_eq!(e.current().unwrap().element.id, 2);
        assert_eq!(e.commit(last, 0.2).unwrap().element.id, 1);
    }

    #[test]
    fn zero_latency_commit_attributes_to_the_present() {
        let (mut e, last) = a_then_b();

        assert_eq!(e.commit(last, 0.0).unwrap().element.id, 2);
    }

    #[test]
    fn commit_older_than_the_ring_window_falls_back_to_the_oldest_run() {
        let (mut e, last) = a_then_b();

        assert_eq!(e.commit(last, 10.0).unwrap().element.id, 1);
    }

    #[test]
    fn commit_without_any_fixation_returns_nothing() {
        let elements = two_buttons();
        let mut e    = engine();

        // A saccade sample resolves a target for the highlight but must not be committed
        // to: the eye is in flight and was never on it.
        let mut in_flight = fixating(0.0, 200.0, 200.0);
        in_flight.state   = FixationState::Saccade;

        assert!(e.update(&in_flight, &elements).is_some());
        assert!(e.commit(0.0, 0.2).is_none());
    }

    #[test]
    fn ring_forgets_beyond_its_window() {
        let elements = two_buttons();
        let mut e    = SnapEngine::create()
            .scale(Box::new(ConstPxScale::new(PPD)))
            .ring_window_s(0.1)
            .build();

        e.update(&fixating(0.0, 200.0, 200.0), &elements);
        e.update(&fixating(1.0, 800.0, 200.0), &elements);

        // The run on A is long gone, so a 0.9 s lookback lands on the only run left.
        assert_eq!(e.commit(1.0, 0.9).unwrap().element.id, 2);
    }

    /// An engine over the flat test world with non-default weights.
    fn engine_with(weights: ScoreWeights) -> SnapEngine {
        SnapEngine::create()
            .scale(Box::new(ConstPxScale::new(PPD)))
            .radius_deg(DEFAULT_RADIUS_DEG)
            .weights(weights)
            .build()
    }

    /// A 10 x 0.67 deg list row with a text label near its left end, the shape that
    /// makes edge distance useless: the gaze point is inside both boxes, so both score
    /// zero on distance.
    fn row_and_label() -> Vec<Element> {
        vec![
            el(1, ElementKind::Button, 200.0, 480.0, 600.0, 40.0),
            el(2, ElementKind::Text  , 240.0, 490.0, 120.0, 20.0),
        ]
    }

    #[test]
    fn centre_term_prefers_the_nested_label_over_its_row() {
        let elements = row_and_label();

        // Without a centre term the row wins: distance is zero for both, and the row
        // being a control outweighs it being 10x the area.
        let mut plain = engine_with(baseline());

        assert_eq!(
            plain.update(&fixating(0.0, 300.0, 500.0), &elements).unwrap().element.id,
            1
        );

        // The gaze is dead centre in the label and two thirds of the way out from the
        // centre of the row, which is what flips it.
        let mut e = engine_with(ScoreWeights { center: 0.8, ..baseline() });
        let t     = e.update(&fixating(0.0, 300.0, 500.0), &elements).unwrap();

        assert_eq!(t.element.id, 2);
    }

    #[test]
    fn centre_term_widens_the_margin_for_a_button_inside_a_row() {
        let elements = vec![
            el(1, ElementKind::Button, 200.0, 480.0, 600.0, 40.0),
            el(2, ElementKind::Button, 700.0, 485.0,  60.0, 30.0),
        ];

        // Equal kinds, so the area term already picks the button out of the row it sits
        // in. The centre term is not what decides this one, it is what makes it decisive:
        // the margin has to grow, and it has to grow in the same direction.
        let margin = |e: &SnapEngine| {
            let c: Vec<_> = e.ranked().collect();

            assert_eq!(c[0].id, 2, "button did not win");

            c[1].score - c[0].score
        };

        let mut plain = engine();
        plain.update(&fixating(0.0, 730.0, 500.0), &elements);

        let mut centred = engine_with(
            ScoreWeights { center: 0.8, ..ScoreWeights::default() }
        );
        centred.update(&fixating(0.0, 730.0, 500.0), &elements);

        assert!(
            margin(&centred) > margin(&plain),
            "{} did not beat {}",
            margin(&centred),
            margin(&plain)
        );
    }

    /// Two overlapping boxes of identical size and kind. Every other term cancels, so
    /// this isolates the centre terms exactly.
    fn overlapping_pair() -> Vec<Element> {
        vec![
            el(1, ElementKind::Button, 400.0, 480.0, 100.0, 40.0),
            el(2, ElementKind::Button, 460.0, 480.0, 100.0, 40.0),
        ]
    }

    #[test]
    fn overlapping_boxes_tie_without_a_centre_term() {
        let elements = overlapping_pair();
        let mut e    = engine_with(baseline());
        let t        = e.update(&fixating(0.0, 490.0, 500.0), &elements).unwrap();

        // An exact tie resolves to the earlier element, and must do so every run.
        assert_eq!(t.element.id, 1);
        assert_eq!(e.candidates()[0].score, e.candidates()[1].score);
    }

    #[test]
    fn normalised_centre_term_breaks_the_tie() {
        let elements = overlapping_pair();
        let mut e    = engine_with(ScoreWeights { center: 0.5, ..ScoreWeights::default() });

        // x 490 is 0.8 of the way out of the first box and 0.4 out of the second.
        let t = e.update(&fixating(0.0, 490.0, 500.0), &elements).unwrap();

        assert_eq!(t.element.id, 2);
        assert!((e.candidates()[0].center_norm - 0.4).abs() < 1e-9);
    }

    #[test]
    fn angular_centre_term_breaks_the_tie() {
        let elements = overlapping_pair();
        let mut e    = engine_with(
            ScoreWeights { center_deg: 0.5, ..ScoreWeights::default() }
        );

        // The same offsets as degrees: 40 px and 20 px at 60 px/deg.
        let t = e.update(&fixating(0.0, 490.0, 500.0), &elements).unwrap();

        assert_eq!(t.element.id, 2);
        assert!((e.candidates()[0].center_deg - 20.0 / PPD).abs() < 1e-9);
    }

    #[test]
    fn centre_term_survives_a_degenerate_box() {
        // Detectors do emit zero-area boxes. The half-extent floor has to keep the
        // normalised offset finite.
        let elements = vec![el(1, ElementKind::Button, 500.0, 500.0, 0.0, 0.0)];
        let mut e    = engine_with(ScoreWeights { center: 0.8, ..ScoreWeights::default() });

        e.update(&fixating(0.0, 502.0, 500.0), &elements);

        let c = e.candidates()[0];

        assert!(c.score.is_finite(), "score {} is not finite", c.score);
        assert_eq!(c.center_norm, 1.0);
    }

    #[test]
    fn ranked_is_cost_ascending() {
        let elements = toolbar();
        let mut e    = engine();

        e.update(&fixating(0.0, 208.0, 112.0), &elements);

        let ranked : Vec<_> = e.ranked().copied().collect();

        assert_eq!(ranked.len(), e.candidates().len());
        assert!(ranked.len() >= 4, "need a few candidates to rank");
        assert_eq!(ranked[0].id, 3, "cheapest is not the icon under the gaze");

        // The pair either side of icon 3 must be next, in some order.
        assert!([2, 4].contains(&ranked[1].id), "second was {}", ranked[1].id);

        for w in ranked.windows(2) {
            assert!(w[0].score <= w[1].score, "{} then {}", w[0].score, w[1].score);
        }
    }

    #[test]
    fn weight_lists_round_trip_in_the_documented_order() {
        let w = ScoreWeights::from_list(&[0.5, 0.2, 1.0, 0.3, 0.1]);

        assert_eq!(w.kind       , 0.5);
        assert_eq!(w.area       , 0.2);
        assert_eq!(w.distance   , 1.0);
        assert_eq!(w.center     , 0.3);
        assert_eq!(w.center_deg , 0.1);
        assert_eq!(w.to_list()  , [0.5, 0.2, 1.0, 0.3, 0.1]);

        // A short list leaves the tail at its default, which is how the bench sweeps one
        // term without restating the rest.
        let short = ScoreWeights::from_list(&[0.6, 0.15, 1.0, 0.3]);

        assert_eq!(short.center     , 0.3);
        assert_eq!(short.center_deg , ScoreWeights::default().center_deg);
        assert_eq!(ScoreWeights::from_list(&[]), ScoreWeights::default());
    }
}
