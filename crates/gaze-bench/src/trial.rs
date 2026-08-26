//! One simulated fixation on one target, start to finish.
//!
//! The landing model is deliberately crude and deliberately pessimistic in the one way
//! that matters: a human asked to look at a button does not land on its centre pixel, so
//! the clean landing point is drawn uniformly from the box shrunk by 20% per side.
//!
//! On top of that goes the provider's angular error, split the way a real remote tracker
//! splits it and the way `gaze-provider-synthetic` now applies it: a **bias** drawn once
//! per fixation, plus **jitter** drawn per sample. A tracker's quoted 0.7 deg accuracy is
//! mostly bias; its sample-to-sample precision is 0.1 to 0.3 deg. The first version of
//! this bench drew the full sigma independently every sample, which is a 50 to 120 deg/s
//! signal to an I-VT classifier with a 30 deg/s threshold, so nothing ever read as a
//! fixation. `--jitter-only-legacy` restores that behaviour for comparison.
//!
//! Either way the error reaches the desk the same way: lift the landing point to a ray
//! from the nominal eye, rotate it by the per-axis error in degrees, re-intersect.

use gaze_core::{
    DesktopGeometry, Element, GazeSample, GlobalPx, NoiseModel, Ray, Rect, SigmaProfile,
};
use gaze_snap::{Candidate, FilterStack, FixationState, Filtered, SnapEngine, SnapTarget};
use rand::Rng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, Normal};

use crate::stats::{FLICK_SECTORS, FLICK_TOP_K, RivalPolicy, px_per_deg_at};

/// Fraction of each side removed before drawing the landing point. A fixation lands
/// inside the target but not reliably at its centre; 20% per side keeps the draw in the
/// middle 60% of the box on each axis.
pub const SHRINK_PER_SIDE : f64 = 0.2;

/// Below this span in logical pixels the shrunk box is not worth sampling from, and the
/// landing point is the box centre on that axis. A one-pixel-wide underline detected as a
/// target would otherwise contribute a meaningless uniform draw.
pub const MIN_SPAN_PX : f64 = 4.0;

/// Samples in one simulated fixation for the sequence feeding mode. 24 samples at 120 Hz
/// is 200 ms, about the shortest fixation anyone commits from.
pub const SEQUENCE_SAMPLES : u32 = 24;

/// Sample rate of the simulated fixation, Hz. Matches `config/desk.toml`'s noise model.
pub const SEQUENCE_RATE_HZ : f64 = 120.0;

/// Timestamp of the single sample in single-sample mode, and the dwell it claims.
pub const SINGLE_T_S : f64 = 0.3;

/// Bisection steps used to find where a perturbed ray leaves the desk. Twenty-four
/// halvings resolve the crossing to about one part in 10^7 of the error angle, far below
/// a pixel.
pub const EDGE_BISECT_STEPS : u32 = 24;

/// How sigma is chosen for a sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SigmaMode {
    /// The same sigma everywhere, independent of where on the desk the target is. This
    /// is the primary metric: it answers "does snapping close the error budget at
    /// ET5-class error" without entangling the answer with this desk's tracker cone.
    Fixed(f64),
    /// Sigma from the desk's profile, looked up by off-axis angle. Secondary: it answers
    /// what the cone does to the far corners of *this* layout.
    Profile(SigmaProfile),
}

/// What one trial did.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Outcome {
    /// The engine returned the element the trial aimed at.
    Correct,
    /// The engine returned a different element.
    Slip { chosen : u64 },
    /// The engine returned nothing, though gaze was available.
    NoTarget,
    /// The provider produced no usable gaze point at all: the perturbed ray missed every
    /// panel, or the sigma profile reports tracking lost at that angle.
    NoGaze,
}

/// Everything a trial needs that does not change between trials.
pub struct TrialSetup<'a> {
    pub geometry : &'a DesktopGeometry,
    /// Supplies `jitter_deg` and `bias_sigma`. Only the error split is read; the rate and
    /// latency belong to the live provider.
    pub model    : &'a NoiseModel,
    pub sigma    : SigmaMode,
    /// Draw the full sigma independently every sample, as the first version of this bench
    /// did. Kept for comparison; the split model is the default.
    pub legacy   : bool,
    /// How close a rival candidate's cost must come to the winner's for the trial to be
    /// called ambiguous, in score units.
    pub margin   : f64,
    /// Which rival candidates are allowed to make a trial ambiguous. See `RivalPolicy`.
    pub rivals   : RivalPolicy,
    /// Clamp a perturbed ray that misses every panel to the panel edge it left through,
    /// instead of scoring the sample as lost. A real tracker still reports a point when
    /// the user glances just past the bezel, and scoring that as no-gaze charges the
    /// bench's edge targets (title bars, taskbars, sidebars) for something the hardware
    /// does not actually do.
    pub clamp    : bool,
}

/// Result of one trial, with the diagnostics the report needs.
#[derive(Clone, Copy, Debug)]
pub struct TrialOutput {
    pub outcome             : Outcome,
    /// Distance from the clean landing point to the first noisy point, logical pixels.
    /// `None` when that sample was lost.
    pub err_px              : Option<f64>,
    /// The same distance as a visual angle from the nominal eye, degrees.
    pub err_deg             : Option<f64>,
    /// Samples the filter classified as fixating, and how many were fed. Both zero in
    /// single-sample mode, which bypasses the filter.
    pub fixating            : (u32, u32),
    /// Whether the engine's *last* `update` (rather than `commit`) named the target.
    /// Sequence mode only; it separates "the engine could not see it" from "the fixation
    /// history was empty because every sample read as a saccade".
    pub last_update_correct : bool,
    /// Whether a rival candidate came within `TrialSetup::margin` of the winner.
    pub ambiguous           : bool,
    /// Cost gap between the winner and the best rival, in score units. `None` when there
    /// was no rival at all, or no answer to compare against.
    pub gap_deg             : Option<f64>,
    /// Where the intended target placed in the candidate list ordered by cost ascending,
    /// zero-based. `None` when the target was not a candidate, or nothing was ranked.
    pub target_rank         : Option<usize>,
    /// Whether the engine produced a candidate list at all. Separates "the target was not
    /// among the candidates" from "there were no candidates", which top-k must not mix.
    pub ranked              : bool,
    /// Candidates within the ambiguity margin of the winner, counting the winner. The
    /// size of the hint a refinement tier would have to show.
    pub near_count          : Option<usize>,
    /// Distance from the point the system would warp to, to the nearest point of the
    /// intended target's box: zero when the warp already landed on it. This is the
    /// correction the fine channel (touchpad thumb, gyro) has to carry. `None` when there
    /// was no warp point at all.
    pub nudge_px            : Option<f64>,
    pub nudge_deg           : Option<f64>,
    /// Whether one directional flick could pick the intended target out of the ranked
    /// top few: it is among them, and its direction from the warp point falls in a
    /// different 45 degree sector from every other one. `None` when nothing was ranked.
    pub flick               : Option<bool>,
}

impl Default for TrialOutput {
    /// A trial that produced nothing: no gaze, no ranking, no nudge. Exists so callers
    /// building a `TrialOutput` by hand only have to name the fields they care about, and
    /// so adding a metric to this struct does not break every one of them.
    fn default() -> TrialOutput {
        TrialOutput {
            outcome             : Outcome::NoGaze,
            err_px              : None,
            err_deg             : None,
            fixating            : (0, 0),
            last_update_correct : false,
            ambiguous           : false,
            gap_deg             : None,
            target_rank         : None,
            ranked              : false,
            near_count          : None,
            nudge_px            : None,
            nudge_deg           : None,
            flick               : None,
        }
    }
}

/// The error split in force for one fixation: a bias drawn once, jitter drawn per sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FixationNoise {
    /// Per-sample 1-sigma, degrees per axis.
    pub jitter_deg : f64,
    pub bias_x_deg : f64,
    pub bias_y_deg : f64,
}

// --- Trials ---

/// Runs one single-sample trial: one already-classified fixation sample straight into the
/// engine, no filter in the way.
///
/// The sample carries the fixation's bias plus one jitter draw, so its marginal error
/// distribution is the same full sigma the legacy model used; only the *correlation*
/// between samples differs, which is why single-sample numbers barely move between the
/// two models and sequence numbers move a lot.
///
/// The engine is reset first, so hysteresis from the previous trial cannot leak in. That
/// is the pessimistic choice: a real session's previous target is often the neighbour
/// the trial is about to be tempted by.
pub fn run_single(
    setup    : &TrialSetup<'_>,
    engine   : &mut SnapEngine,
    elements : &[Element],
    target   : usize,
    rng      : &mut StdRng,
)
    -> TrialOutput
{
    engine.reset();

    let landing = landing_point(&elements[target].bbox, rng);
    let want    = elements[target].id;

    let Some((ray, sigma_deg)) = fixation_base(setup, landing) else {
        return lost_trial(None, None);
    };

    let noise  = FixationNoise::draw(setup, sigma_deg, rng);
    let sample = sample_at(setup, &ray, sigma_deg, &noise, SINGLE_T_S, rng);

    let (err_px, err_deg) = errors(setup.geometry, landing, &sample);

    if !sample.valid {
        return lost_trial(err_px, err_deg);
    }

    let filtered = Filtered {
        sample : sample,
        state  : FixationState::Fixating { since_s: SINGLE_T_S },
    };

    let answer  = engine.update(&filtered, elements);
    let gap_deg = ambiguity_gap(engine, answer.as_ref(), elements, setup.rivals);
    let ranking = rank_target(engine, elements, target, setup);

    // Where the pointer would actually be put: the snapped target's clamped point, or the
    // raw gaze when the engine declined to snap.
    let warp    = answer.as_ref().map(|t| t.point).or(sample.point);
    let nudge   = warp.map(|w| nudge_distance(setup.geometry, &elements[target].bbox, w));
    let flick   = warp.and_then(|w| flick_ok(engine, elements, target, w, setup));
    let outcome = classify(answer, want);

    TrialOutput {
        outcome             : outcome,
        err_px              : err_px,
        err_deg             : err_deg,
        fixating            : (0, 0),
        last_update_correct : outcome == Outcome::Correct,
        ambiguous           : gap_deg.is_some_and(|g| g <= setup.margin),
        gap_deg             : gap_deg,
        target_rank         : ranking.0,
        ranked              : ranking.1,
        near_count          : ranking.2,
        nudge_px            : nudge.map(|n| n.0),
        nudge_deg           : nudge.map(|n| n.1),
        flick               : flick,
    }
}

/// Runs one fixation-sequence trial: `SEQUENCE_SAMPLES` samples at `SEQUENCE_RATE_HZ`
/// about one landing point, sharing one bias draw and each with its own jitter, through
/// the filter stack and then the engine, resolved by a zero-latency commit at the last
/// timestamp.
///
/// This is what a real fixation looks like to the pipeline: the bias is what the tracker
/// got wrong about this fixation and does not average away, the jitter does. The trial is
/// only `NoGaze` when every sample was lost.
///
/// Ambiguity is read from the final `update`'s candidate set. When `commit` answers with
/// an earlier target than the last update's winner, the gap is still measured against the
/// committed element if it was a candidate, so the number describes the answer that was
/// actually returned.
pub fn run_sequence(
    setup    : &TrialSetup<'_>,
    filter   : &mut FilterStack,
    engine   : &mut SnapEngine,
    elements : &[Element],
    target   : usize,
    rng      : &mut StdRng,
)
    -> TrialOutput
{
    filter.reset();
    engine.reset();

    let landing = landing_point(&elements[target].bbox, rng);
    let want    = elements[target].id;

    let Some((ray, sigma_deg)) = fixation_base(setup, landing) else {
        return lost_trial(None, None);
    };

    let noise = FixationNoise::draw(setup, sigma_deg, rng);

    let mut first_err = (None, None);
    let mut fixating  = 0_u32;
    let mut valid     = 0_u32;
    let mut last      = None;
    let mut last_gaze = None;
    let mut t_s       = 0.0;

    for i in 0..SEQUENCE_SAMPLES {
        let sample = sample_at(setup, &ray, sigma_deg, &noise, t_s, rng);

        if i == 0 {
            first_err = errors(setup.geometry, landing, &sample);
        }

        if sample.valid {
            valid += 1;
        }

        let filtered = filter.push(sample);

        if let Some(p) = filtered.sample.point {
            last_gaze = Some(p);
        }


        if let FixationState::Fixating { .. } = filtered.state {
            fixating += 1;
        }

        last = engine.update(&filtered, elements);
        t_s += 1.0 / SEQUENCE_RATE_HZ;
    }

    // The last timestamp emitted, not the one the loop left behind.
    let t_last = t_s - 1.0 / SEQUENCE_RATE_HZ;

    let answer = {
        if valid == 0 {
            None
        }
        else {
            engine.commit(t_last, 0.0)
        }
    };

    let gap_deg = ambiguity_gap(engine, answer.as_ref(), elements, setup.rivals);
    let ranking = rank_target(engine, elements, target, setup);

    // The committed target's point, or the last filtered gaze point when nothing snapped.
    let warp  = answer.as_ref().map(|t| t.point).or(last_gaze);
    let nudge = warp.map(|w| nudge_distance(setup.geometry, &elements[target].bbox, w));
    let flick = warp.and_then(|w| flick_ok(engine, elements, target, w, setup));

    let outcome = {
        if valid == 0 {
            Outcome::NoGaze
        }
        else {
            classify(answer, want)
        }
    };

    TrialOutput {
        outcome             : outcome,
        err_px              : first_err.0,
        err_deg             : first_err.1,
        fixating            : (fixating, SEQUENCE_SAMPLES),
        last_update_correct : last.is_some_and(|t| t.element.id == want),
        ambiguous           : gap_deg.is_some_and(|g| g <= setup.margin),
        gap_deg             : gap_deg,
        target_rank         : ranking.0,
        ranked              : ranking.1,
        near_count          : ranking.2,
        nudge_px            : nudge.map(|n| n.0),
        nudge_deg           : nudge.map(|n| n.1),
        flick               : flick,
    }
}

/// Draws the clean landing point of a fixation aimed at `bbox`.
///
/// Uniform inside the box shrunk by `SHRINK_PER_SIDE` on each side, per axis, falling
/// back to the centre on an axis whose shrunk span is under `MIN_SPAN_PX`.
pub fn landing_point(bbox: &Rect, rng: &mut StdRng) -> GlobalPx {
    GlobalPx {
        x : landing_axis(bbox.x, bbox.w, rng),
        y : landing_axis(bbox.y, bbox.h, rng),
    }
}

/// Deterministic per-trial seed.
///
/// Every trial draws from an RNG seeded from its own coordinates rather than from a
/// stream shared with its neighbours, so the numbers do not depend on how rayon split the
/// work. `sigma_id` separates the sigma sweeps; the feeding mode is deliberately *not*
/// mixed in, so single-sample and sequence trials share a landing point, a bias draw and
/// a first jitter draw and can be compared as a paired sample.
pub fn trial_seed(seed: u64, frame: usize, element: usize, trial: u32, sigma_id: u32) -> u64 {
    let mut h = seed;

    for part in [frame as u64, element as u64, trial as u64, sigma_id as u64] {
        h = mix(h ^ part);
    }

    h
}

// --- FixationNoise ---

impl FixationNoise {
    /// Draws the bias for one fixation and fixes the per-sample jitter.
    ///
    /// The jitter is clamped to the total sigma so a target whose sigma is below the
    /// configured jitter does not end up noisier than asked for; the bias then takes up
    /// whatever variance is left, which is what `NoiseModel::bias_sigma` computes.
    pub fn draw(setup: &TrialSetup<'_>, sigma_deg: f64, rng: &mut StdRng) -> FixationNoise {
        if setup.legacy {
            return FixationNoise { jitter_deg: sigma_deg, bias_x_deg: 0.0, bias_y_deg: 0.0 };
        }

        let jitter_deg = setup.model.jitter_deg.min(sigma_deg).max(0.0);
        let bias_sigma = setup.model.bias_sigma(sigma_deg);

        if bias_sigma <= 0.0 {
            return FixationNoise { jitter_deg: jitter_deg, bias_x_deg: 0.0, bias_y_deg: 0.0 };
        }

        let normal = Normal::new(0.0, bias_sigma).expect("bias sigma is finite and positive");

        FixationNoise {
            jitter_deg : jitter_deg,
            bias_x_deg : normal.sample(rng),
            bias_y_deg : normal.sample(rng),
        }
    }

    /// Per-axis error for one sample: the fixation's bias plus a fresh jitter draw.
    fn sample(&self, rng: &mut StdRng) -> (f64, f64) {
        if self.jitter_deg <= 0.0 {
            return (self.bias_x_deg, self.bias_y_deg);
        }

        let normal = Normal::new(0.0, self.jitter_deg).expect("jitter is finite and positive");

        (self.bias_x_deg + normal.sample(rng), self.bias_y_deg + normal.sample(rng))
    }
}

// --- Internals ---

/// Landing coordinate on one axis. See `landing_point`.
fn landing_axis(origin: f64, span: f64, rng: &mut StdRng) -> f64 {
    let inset = span * SHRINK_PER_SIDE;
    let inner = span - 2.0 * inset;

    if inner < MIN_SPAN_PX {
        return origin + span * 0.5;
    }

    origin + inset + rng.random::<f64>() * inner
}

/// The clean ray and the sigma in force for a fixation at `landing`, or `None` when the
/// point is off every panel or the profile reports tracking lost at that angle.
///
/// Both are constant across a fixation: the clean point does not move, so neither does
/// the off-axis angle the profile is a function of.
fn fixation_base(setup: &TrialSetup<'_>, landing: GlobalPx) -> Option<(Ray, f64)> {
    // A landing point is inside a detector box, which is inside an output, so this only
    // fails for a box the detector placed off the panel it came from.
    let ray = setup.geometry.px_to_ray(landing)?;

    let sigma_deg = match setup.sigma {
        SigmaMode::Fixed(s)   => Some(s),
        SigmaMode::Profile(p) => p.sigma_at(setup.geometry.off_axis_deg(&ray)),
    };

    Some((ray, sigma_deg?))
}

/// Builds one noisy sample by rotating the clean ray by this sample's error and
/// re-intersecting the desk.
///
/// When the perturbed ray misses every panel and `setup.clamp` is set, the sample carries
/// the point where the ray left the desk rather than nothing. The ray on the sample is
/// always the true perturbed ray, so it and the clamped point disagree by design.
fn sample_at(
    setup     : &TrialSetup<'_>,
    ray       : &Ray,
    sigma_deg : f64,
    noise     : &FixationNoise,
    t_s       : f64,
    rng       : &mut StdRng,
)
    -> GazeSample
{
    let (dx_deg, dy_deg) = noise.sample(rng);
    let noisy            = DesktopGeometry::perturb_ray(ray, dx_deg, dy_deg);

    let point = {
        match setup.geometry.intersect(&noisy) {
            Some(hit) => Some(hit.px),

            None if setup.clamp => edge_point(setup.geometry, ray, dx_deg, dy_deg),

            None => None,
        }
    };

    GazeSample {
        t_s       : t_s,
        ray       : Some(noisy),
        point     : point,
        sigma_deg : sigma_deg,
        valid     : point.is_some(),
    }
}

/// Where a perturbation that leaves the desk crosses the panel edge on its way out.
///
/// The unperturbed ray hits by construction (the clean landing point is inside a detector
/// box, which is on a panel) and the full perturbation misses, so bisecting the
/// perturbation scale finds a crossing. A perturbation that sweeps off one panel, across a
/// gap and onto another can have several crossings; this returns one of them, which is
/// still a point on a panel edge in the direction the gaze went, and that is all the clamp
/// claims to be.
fn edge_point(geometry: &DesktopGeometry, ray: &Ray, dx_deg: f64, dy_deg: f64)
    -> Option<GlobalPx>
{
    let mut lo = 0.0_f64;
    let mut hi = 1.0_f64;
    let mut best = None;

    for _ in 0..EDGE_BISECT_STEPS {
        let mid   = 0.5 * (lo + hi);
        let probe = DesktopGeometry::perturb_ray(ray, dx_deg * mid, dy_deg * mid);

        match geometry.intersect(&probe) {
            Some(hit) => {
                best = Some(hit.px);
                lo   = mid;
            }

            None => hi = mid,
        }
    }

    best
}

/// A trial that never produced a usable gaze point.
fn lost_trial(err_px: Option<f64>, err_deg: Option<f64>) -> TrialOutput {
    TrialOutput { err_px: err_px, err_deg: err_deg, ..TrialOutput::default() }
}

/// How far the fine channel would have to move the pointer to reach the intended target,
/// as `(pixels, degrees)`. Zero when the warp already landed inside the box.
///
/// The angular figure uses the mean local scale at the two ends, the same way the snap
/// engine measures its own distances, so a nudge across a seam is not measured entirely
/// at one panel's density.
fn nudge_distance(geometry: &DesktopGeometry, bbox: &Rect, warp: GlobalPx) -> (f64, f64) {
    let near = bbox.clamp(warp);
    let dx   = warp.x - near.x;
    let dy   = warp.y - near.y;
    let px   = dx.hypot(dy);

    if px == 0.0 {
        return (0.0, 0.0);
    }

    let (wx, wy) = px_per_deg_at(geometry, warp);
    let (nx, ny) = px_per_deg_at(geometry, near);

    let ppd_x = 0.5 * (wx + nx);
    let ppd_y = 0.5 * (wy + ny);

    if ppd_x <= 0.0 || ppd_y <= 0.0 {
        return (px, 0.0);
    }

    (px, (dx / ppd_x).hypot(dy / ppd_y))
}

/// Whether one directional flick from the warp point could pick the intended target.
///
/// True when the target is among the best `FLICK_TOP_K` candidates and its direction from
/// the warp point falls in a different 45 degree sector from every other candidate in that
/// set. Candidates the rival policy calls the same thing as the target do not compete with
/// it: flicking onto a duplicate detection is the same gesture with the same result.
///
/// `None` when nothing was ranked, which is not a flick failure but an absence of any
/// candidate to flick between.
fn flick_ok(
    engine   : &SnapEngine,
    elements : &[Element],
    target   : usize,
    warp     : GlobalPx,
    setup    : &TrialSetup<'_>,
)
    -> Option<bool>
{
    let top = engine.ranked().take(FLICK_TOP_K).collect::<Vec<_>>();

    if top.is_empty() {
        return None;
    }

    let want = elements[target].id;
    let bbox = elements[target].bbox;

    let equivalent = |c: &Candidate| {
        c.id == want
            || elements.get(c.index).is_some_and(|e| !setup.rivals.counts(&bbox, &e.bbox))
    };

    // Not in the top few: no flick between them can reach it.
    if !top.iter().any(|c| equivalent(c)) {
        return Some(false);
    }

    let mine = sector(warp, bbox.center());

    let clash = top.iter()
        .filter(|c| !equivalent(c))
        .filter_map(|c| elements.get(c.index))
        .any(|e| sector(warp, e.bbox.center()) == mine);

    Some(!clash)
}

/// Which of the `FLICK_SECTORS` wedges `to` lies in as seen from `from`.
///
/// A zero-length offset gets its own pseudo-sector past the real ones: a candidate whose
/// centre is exactly under the warp point has no direction, and treating that as distinct
/// from every real direction is the honest reading (there is nothing to flick).
fn sector(from: GlobalPx, to: GlobalPx) -> usize {
    let dx = to.x - from.x;
    let dy = to.y - from.y;

    if dx == 0.0 && dy == 0.0 {
        return FLICK_SECTORS;
    }

    let turns = dy.atan2(dx) / std::f64::consts::TAU;
    let wedge = (turns.rem_euclid(1.0) * FLICK_SECTORS as f64) as usize;

    wedge.min(FLICK_SECTORS - 1)
}

/// Where the intended target placed in the cost-ordered candidate list, whether anything
/// was ranked at all, and how many candidates sat within the ambiguity margin.
///
/// A candidate that the rival policy says is the same thing on screen as the target
/// counts *as* the target: the detector emitting a widget box and an OCR box for one
/// control must not push its own target down the ranking. Ties break by element id so the
/// ranking is total and reproducible.
///
/// The near count applies the same policy, because it is the size of the hint the
/// refinement tier would show and there is no point showing one control twice.
fn rank_target(
    engine   : &SnapEngine,
    elements : &[Element],
    target   : usize,
    setup    : &TrialSetup<'_>,
)
    -> (Option<usize>, bool, Option<usize>)
{
    // `ranked()` is the engine's own cost order. The counting below does not depend on
    // that order, so this stays correct if the engine's ordering ever changes.
    let candidates = engine.ranked().collect::<Vec<_>>();

    let Some(winner) = candidates.first().copied() else {
        return (None, false, None);
    };

    let want = elements[target].id;
    let bbox = elements[target].bbox;

    // Every candidate that would click the same thing as the intended target.
    let equivalent = |c: &Candidate| {
        c.id == want
            || elements.get(c.index).is_some_and(|e| !setup.rivals.counts(&bbox, &e.bbox))
    };

    let best = candidates.iter().copied()
        .filter(|c| equivalent(c))
        .min_by(|a, b| a.score.total_cmp(&b.score).then(a.id.cmp(&b.id)));

    let rank = best.map(|target| {
        candidates.iter().copied()
            .filter(|c| {
                (c.score, c.id) < (target.score, target.id) && !equivalent(c)
            })
            .count()
    });

    // The hint the refinement tier would show: the winner plus every distinct rival
    // within the margin of it.
    let near = 1 + candidates.iter().copied()
        .filter(|c| c.id != winner.id)
        .filter(|c| c.score <= winner.score + setup.margin)
        .filter(|c| {
            elements.get(winner.index).is_none_or(|w| {
                elements.get(c.index).is_none_or(|e| setup.rivals.counts(&w.bbox, &e.bbox))
            })
        })
        .count();

    (rank, true, Some(near))
}

/// Pixel and angular distance between the clean landing point and where the noisy sample
/// actually landed. `(None, None)` for a lost sample.
fn errors(geometry: &DesktopGeometry, landing: GlobalPx, sample: &GazeSample)
    -> (Option<f64>, Option<f64>)
{
    let Some(point) = sample.point else {
        return (None, None);
    };

    let px  = (point.x - landing.x).hypot(point.y - landing.y);
    let deg = geometry.angle_between_deg(geometry.eye(), landing, point);

    (Some(px), deg)
}

/// Cost gap between the answer and the best rival in the engine's last candidate set.
///
/// `None` means nothing to be ambiguous about: no answer, or no rival worth counting. A
/// negative gap (the answer was not the cheapest candidate, which `commit` can produce by
/// reaching back in time) is left negative, so it reads as maximally ambiguous.
///
/// `policy` decides which rivals count. Refinement cannot resolve a difference the user
/// cannot see, so a rival that is the same box as the winner (or, under
/// `RivalPolicy::Separate`, one nested inside it) is not something to be unsure about.
fn ambiguity_gap(
    engine   : &SnapEngine,
    answer   : Option<&SnapTarget>,
    elements : &[Element],
    policy   : RivalPolicy,
)
    -> Option<f64>
{
    let answer     = answer?;
    let candidates = engine.candidates();

    // The answer's own cost as this update scored it, falling back to the cheapest
    // candidate when the answer came from the ring rather than from this update.
    let base = candidates.iter()
        .find(|c| c.id == answer.element.id)
        .map(|c| c.score)
        .or_else(|| candidates.iter().map(|c| c.score).min_by(f64::total_cmp))?;

    candidates.iter()
        .filter(|c| c.id != answer.element.id)
        .filter(|c| {
            elements.get(c.index)
                .is_none_or(|e| policy.counts(&answer.element.bbox, &e.bbox))
        })
        .map(|c| c.score - base)
        .min_by(f64::total_cmp)
}

/// Turns an engine answer into an outcome against the intended target id.
fn classify(target: Option<SnapTarget>, want: u64) -> Outcome {
    match target {
        Some(t) if t.element.id == want => Outcome::Correct,
        Some(t)                         => Outcome::Slip { chosen: t.element.id },
        None                            => Outcome::NoTarget,
    }
}

/// SplitMix64's finaliser. Any decent avalanche would do; this one is short and has no
/// dependencies.
fn mix(mut x: u64) -> u64 {
    x = x.wrapping_add(0x9e37_79b9_7f4a_7c15);
    x = (x ^ (x >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
    x = (x ^ (x >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);

    x ^ (x >> 31)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    use gaze_core::{ElementKind, ElementSource};
    use gaze_snap::{ConstPxScale, ScoreWeights};
    use rand::SeedableRng;

    /// A flat single-output desk, 1000x800 logical px at the origin, eye 500 mm back.
    /// Small enough to reason about, real enough to exercise the ray round trip.
    fn flat_desk() -> DesktopGeometry {
        DesktopGeometry::from_toml(
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
        .expect("fixture parses")
    }

    /// The desk's own noise model: jitter 0.2 deg, no drift, 120 Hz.
    fn model() -> NoiseModel {
        NoiseModel {
            profile         : SigmaProfile::default(),
            jitter_deg      : 0.2,
            bias_redraw_deg : 1.0,
            drift_deg       : 0.0,
            latency_s       : 0.0,
            rate_hz         : 120.0,
        }
    }

    /// A setup over the flat fixture desk at a fixed sigma.
    fn setup<'a>(desk: &'a DesktopGeometry, model: &'a NoiseModel, sigma: f64)
        -> TrialSetup<'a>
    {
        TrialSetup {
            geometry : desk,
            model    : model,
            sigma    : SigmaMode::Fixed(sigma),
            legacy   : false,
            margin   : 0.5,
            rivals   : RivalPolicy::Distinct,
            clamp    : true,
        }
    }

    /// One element at a chosen box, always a Button from the detector.
    fn element(id: u64, x: f64, y: f64, w: f64, h: f64) -> Element {
        Element {
            id     : id,
            bbox   : Rect { x: x, y: y, w: w, h: h },
            kind   : ElementKind::Button,
            source : ElementSource::Detector,
            score  : 1.0,
            text   : None,
        }
    }

    /// An engine over a flat 60 px/deg world, which is close enough to the fixture desk
    /// and keeps the expected radii easy to state. Uses whatever weights the snap crate
    /// currently defaults to.
    fn engine() -> SnapEngine {
        SnapEngine::create().scale(Box::new(ConstPxScale::new(60.0))).build()
    }

    /// The same engine with distance-only ranking plus the original kind penalty. Tests
    /// about the ambiguity machinery use this so retuning the shipped default weights
    /// moves the product's numbers without moving assertions about the mechanism.
    fn plain_engine() -> SnapEngine {
        SnapEngine::create()
            .scale(Box::new(ConstPxScale::new(60.0)))
            .weights(ScoreWeights::from_list(&[0.6, 0.15, 1.0, 0.0, 0.0]))
            .build()
    }

    #[test]
    fn landing_stays_inside_the_shrunk_box() {
        let mut rng = StdRng::seed_from_u64(1);
        let bbox    = Rect { x: 100.0, y: 200.0, w: 100.0, h: 50.0 };

        for _ in 0..1000 {
            let p = landing_point(&bbox, &mut rng);

            assert!(p.x >= 120.0 && p.x <= 180.0, "x = {}", p.x);
            assert!(p.y >= 210.0 && p.y <= 240.0, "y = {}", p.y);
        }
    }

    #[test]
    fn landing_covers_the_shrunk_box() {
        let mut rng = StdRng::seed_from_u64(2);
        let bbox    = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

        let (mut lo, mut hi) = (f64::MAX, f64::MIN);

        for _ in 0..2000 {
            let p = landing_point(&bbox, &mut rng);

            lo = lo.min(p.x);
            hi = hi.max(p.x);
        }

        // Uniform over [20, 80]: 2000 draws should crowd both ends.
        assert!(lo < 21.0, "low end never reached: {lo}");
        assert!(hi > 79.0, "high end never reached: {hi}");
    }

    #[test]
    fn landing_falls_back_to_the_centre_for_a_thin_box() {
        let mut rng = StdRng::seed_from_u64(3);

        // 6 px wide shrinks to 3.6, under the 4 px floor; 100 px tall does not.
        let bbox = Rect { x: 10.0, y: 0.0, w: 6.0, h: 100.0 };

        for _ in 0..100 {
            let p = landing_point(&bbox, &mut rng);

            assert_eq!(p.x, 13.0);
            assert!(p.y >= 20.0 && p.y <= 80.0);
        }
    }

    #[test]
    fn trial_seeds_differ_across_every_coordinate() {
        let base = trial_seed(7, 1, 2, 3, 0);

        assert_ne!(base, trial_seed(7, 2, 2, 3, 0));
        assert_ne!(base, trial_seed(7, 1, 3, 3, 0));
        assert_ne!(base, trial_seed(7, 1, 2, 4, 0));
        assert_ne!(base, trial_seed(7, 1, 2, 3, 1));
        assert_ne!(base, trial_seed(8, 1, 2, 3, 0));
        assert_eq!(base, trial_seed(7, 1, 2, 3, 0));
    }

    #[test]
    fn the_split_noise_keeps_the_total_sigma() {
        let desk  = flat_desk();
        let model = model();
        let s     = setup(&desk, &model, 0.7);

        let mut rng   = StdRng::seed_from_u64(4);
        let mut sum   = 0.0;
        let mut count = 0.0;

        // One draw per fixation, one sample each: the marginal per-sample error must have
        // variance bias^2 + jitter^2 = sigma^2 on each axis.
        for _ in 0..20_000 {
            let noise    = FixationNoise::draw(&s, 0.7, &mut rng);
            let (dx, dy) = noise.sample(&mut rng);

            sum   += dx * dx + dy * dy;
            count += 2.0;
        }

        let sigma = (sum / count).sqrt();

        assert!((sigma - 0.7).abs() < 0.02, "marginal sigma drifted to {sigma}");
    }

    #[test]
    fn the_split_noise_correlates_samples_within_a_fixation() {
        let desk  = flat_desk();
        let model = model();
        let s     = setup(&desk, &model, 0.7);

        let mut rng = StdRng::seed_from_u64(5);

        let mut split_step  = 0.0;
        let mut legacy_step = 0.0;

        for _ in 0..5_000 {
            let noise = FixationNoise::draw(&s, 0.7, &mut rng);
            let a     = noise.sample(&mut rng);
            let b     = noise.sample(&mut rng);

            split_step += (b.0 - a.0).hypot(b.1 - a.1);
        }

        let legacy = TrialSetup { legacy: true, ..setup(&desk, &model, 0.7) };

        for _ in 0..5_000 {
            let noise = FixationNoise::draw(&legacy, 0.7, &mut rng);
            let a     = noise.sample(&mut rng);
            let b     = noise.sample(&mut rng);

            legacy_step += (b.0 - a.0).hypot(b.1 - a.1);
        }

        // Sample-to-sample motion is what the I-VT classifier sees. The split model must
        // move far less between samples than the independent one, because the bias is
        // shared: roughly 0.2/0.7 of the step size.
        assert!(
            split_step < legacy_step * 0.45,
            "split {split_step:.1} vs legacy {legacy_step:.1}"
        );
    }

    #[test]
    fn legacy_noise_has_no_bias_term() {
        let desk   = flat_desk();
        let model  = model();
        let legacy = TrialSetup { legacy: true, ..setup(&desk, &model, 0.7) };

        let mut rng = StdRng::seed_from_u64(6);
        let noise   = FixationNoise::draw(&legacy, 0.7, &mut rng);

        assert_eq!(noise.bias_x_deg, 0.0);
        assert_eq!(noise.bias_y_deg, 0.0);
        assert_eq!(noise.jitter_deg, 0.7);
    }

    #[test]
    fn jitter_never_exceeds_the_total_sigma() {
        let desk  = flat_desk();
        let model = model();
        let s     = setup(&desk, &model, 0.1);

        let mut rng = StdRng::seed_from_u64(7);
        let noise   = FixationNoise::draw(&s, 0.1, &mut rng);

        // Sigma 0.1 is below the 0.2 configured jitter: the jitter is clamped and the
        // bias vanishes rather than the total error growing past what was asked for.
        assert_eq!(noise.jitter_deg, 0.1);
        assert_eq!(noise.bias_x_deg, 0.0);
    }

    #[test]
    fn zero_sigma_always_lands_on_the_intended_target() {
        let desk     = flat_desk();
        let model    = model();
        let s        = setup(&desk, &model, 0.0);
        let elements = [element(0, 400.0, 300.0, 80.0, 40.0), element(1, 600.0, 300.0, 80.0, 40.0)];
        let mut e    = engine();

        for trial in 0..50 {
            let mut rng = StdRng::seed_from_u64(trial);
            let out     = run_single(&s, &mut e, &elements, 0, &mut rng);

            assert_eq!(out.outcome, Outcome::Correct);
            assert!(out.err_px.unwrap() < 0.01, "noise leaked in: {:?}", out.err_px);
        }
    }

    #[test]
    fn an_isolated_target_survives_a_wide_sigma_by_snapping_from_outside() {
        let desk     = flat_desk();
        let model    = model();
        let s        = setup(&desk, &model, 0.7);
        let elements = [element(0, 400.0, 300.0, 40.0, 20.0)];
        let mut e    = engine();

        let mut correct = 0;

        for trial in 0..200 {
            let mut rng = StdRng::seed_from_u64(trial);
            let out     = run_single(&s, &mut e, &elements, 0, &mut rng);

            if out.outcome == Outcome::Correct {
                correct += 1;
            }

            // Nothing else is on screen, so nothing can be ambiguous.
            assert!(!out.ambiguous);
            assert_eq!(out.gap_deg, None);
        }

        assert!(correct > 190, "only {correct}/200 snapped to a lone target");
    }

    #[test]
    fn a_crowded_row_produces_slips_and_flags_them_ambiguous() {
        let desk  = flat_desk();
        let model = model();
        let s     = setup(&desk, &model, 1.5);

        // Five 30 px buttons at a 40 px pitch: about half a degree apart at this desk, so
        // a 1.5 deg sigma must mostly land on a neighbour, never on nothing.
        let elements: Vec<Element> = (0..5)
            .map(|i| element(i, 300.0 + i as f64 * 40.0, 400.0, 30.0, 20.0))
            .collect();

        let mut e = engine();
        let (mut correct, mut slip, mut none, mut no_gaze) = (0, 0, 0, 0);
        let mut ambiguous = 0;

        for trial in 0..300 {
            let mut rng = StdRng::seed_from_u64(trial);
            let out     = run_single(&s, &mut e, &elements, 2, &mut rng);

            match out.outcome {
                Outcome::Correct     => correct += 1,
                Outcome::Slip { .. } => slip += 1,
                Outcome::NoTarget    => none += 1,
                Outcome::NoGaze      => no_gaze += 1,
            }

            if out.ambiguous {
                ambiguous += 1;
            }
        }

        assert_eq!(correct + slip + none + no_gaze, 300);
        assert!(slip > 0, "a crowded row produced no slips at all");
        assert!(correct > 0, "a crowded row produced no hits at all");
        assert_eq!(none, 0);
        assert_eq!(no_gaze, 0);
        // Neighbours half a degree apart cannot be told apart confidently.
        assert!(ambiguous > 150, "only {ambiguous}/300 crowded trials read as ambiguous");
    }

    #[test]
    fn a_duplicate_detection_is_not_an_ambiguity_unless_strict() {
        let desk  = flat_desk();
        let model = model();

        // Two boxes on top of each other: the same control detected twice.
        let elements = [
            element(0, 400.0, 300.0, 60.0, 30.0),
            element(1, 402.0, 302.0, 60.0, 30.0),
        ];

        let mut e = engine();

        let count = |s: &TrialSetup<'_>, e: &mut SnapEngine| {
            (0..100)
                .filter(|trial| {
                    let mut rng = StdRng::seed_from_u64(*trial);

                    run_single(s, e, &elements, 0, &mut rng).ambiguous
                })
                .count()
        };

        let lenient = setup(&desk, &model, 0.3);
        let strict  = TrialSetup { rivals: RivalPolicy::All, ..setup(&desk, &model, 0.3) };

        // Strictly there are always two candidates a hair apart; sensibly there is one
        // thing on screen and nothing for a refinement step to ask about.
        assert_eq!(count(&strict, &mut e), 100);
        assert_eq!(count(&lenient, &mut e), 0);
    }

    #[test]
    fn distinct_neighbours_are_ambiguous_either_way() {
        let desk  = flat_desk();
        let model = model();

        // Two separate 30 px buttons 40 px apart: half a degree, no overlap.
        let elements = [
            element(0, 400.0, 300.0, 30.0, 20.0),
            element(1, 440.0, 300.0, 30.0, 20.0),
        ];

        let mut e         = plain_engine();
        let s             = setup(&desk, &model, 0.7);
        let mut ambiguous = 0;

        for trial in 0..100 {
            let mut rng = StdRng::seed_from_u64(trial);

            if run_single(&s, &mut e, &elements, 0, &mut rng).ambiguous {
                ambiguous += 1;
            }
        }

        // Not every trial: a landing that drifts well clear of the pair leaves the far
        // button more than the margin behind. But most of them, and that is the point.
        assert!(ambiguous > 60, "only {ambiguous}/100 half-degree neighbours read ambiguous");
    }

    #[test]
    fn the_target_ranks_first_when_it_is_the_only_thing_near() {
        let desk     = flat_desk();
        let model    = model();
        let s        = setup(&desk, &model, 0.3);
        let elements = [element(0, 400.0, 300.0, 60.0, 30.0)];
        let mut e    = engine();

        for trial in 0..50 {
            let mut rng = StdRng::seed_from_u64(trial);
            let out     = run_single(&s, &mut e, &elements, 0, &mut rng);

            assert_eq!(out.target_rank, Some(0));
            assert!(out.ranked);
            assert_eq!(out.near_count, Some(1));
        }
    }

    #[test]
    fn a_duplicate_of_the_target_does_not_push_the_target_down_the_ranking() {
        let desk  = flat_desk();
        let model = model();

        // The same control detected twice, plus a real neighbour further away. Whichever
        // copy scores better, the target must still rank first.
        let elements = [
            element(0, 400.0, 300.0, 60.0, 30.0),
            element(1, 402.0, 302.0, 60.0, 30.0),
            element(2, 600.0, 300.0, 60.0, 30.0),
        ];

        let s      = setup(&desk, &model, 0.3);
        let strict = TrialSetup { rivals: RivalPolicy::All, ..setup(&desk, &model, 0.3) };
        let mut e  = engine();

        // Aim at the *second* copy. The two boxes score identically whenever the gaze is
        // inside both, and the rank tie-break favours the lower id, so a strict ranking
        // reports the target as runner-up to its own duplicate.
        let ranks = |s: &TrialSetup<'_>, e: &mut SnapEngine| {
            (0..50)
                .map(|trial| {
                    let mut rng = StdRng::seed_from_u64(trial);

                    run_single(s, e, &elements, 1, &mut rng).target_rank
                })
                .collect::<Vec<_>>()
        };

        let strict_ranks = ranks(&strict, &mut e);
        let lenient      = ranks(&s, &mut e);

        assert!(
            strict_ranks.iter().filter(|r| **r == Some(1)).count() > 25,
            "strict ranking rarely placed the target second: {strict_ranks:?}"
        );
        assert!(
            lenient.iter().all(|r| *r == Some(0)),
            "a duplicate pushed the target down its own ranking: {lenient:?}"
        );
    }

    #[test]
    fn ranks_grow_with_the_number_of_closer_neighbours() {
        let desk  = flat_desk();
        let model = model();

        // A row of six 20 px buttons at a 30 px pitch, aiming at the far left one: at a
        // wide sigma the target is often several places down the list.
        let elements: Vec<Element> = (0..6)
            .map(|i| element(i, 400.0 + i as f64 * 30.0, 400.0, 20.0, 20.0))
            .collect();

        let s     = setup(&desk, &model, 1.5);
        let mut e = engine();

        let mut top1 = 0;
        let mut top3 = 0;

        for trial in 0..300 {
            let mut rng = StdRng::seed_from_u64(trial);
            let out     = run_single(&s, &mut e, &elements, 0, &mut rng);

            if let Some(rank) = out.target_rank {
                if rank < 1 {
                    top1 += 1;
                }

                if rank < 3 {
                    top3 += 1;
                }
            }
        }

        // Top-3 must dominate top-1 and both must be reachable: this pins the ordering
        // rather than a particular value.
        assert!(top3 > top1, "top-3 {top3} did not beat top-1 {top1}");
        assert!(top1 > 0 && top3 < 300);
    }

    #[test]
    fn the_edge_clamp_keeps_a_sample_that_leaves_the_desk() {
        let desk  = flat_desk();
        let model = model();

        // A target hard against the top-left corner: a wide sigma pushes most samples off
        // the single panel of this fixture desk.
        let elements = [element(0, 2.0, 2.0, 20.0, 20.0)];

        let clamped = TrialSetup { clamp: true, ..setup(&desk, &model, 1.5) };
        let lost    = TrialSetup { clamp: false, ..setup(&desk, &model, 1.5) };

        let count_lost = |s: &TrialSetup<'_>, e: &mut SnapEngine| {
            (0..300)
                .filter(|trial| {
                    let mut rng = StdRng::seed_from_u64(*trial);

                    run_single(s, e, &elements, 0, &mut rng).outcome == Outcome::NoGaze
                })
                .count()
        };

        let mut e = engine();

        assert_eq!(count_lost(&clamped, &mut e), 0, "the clamp must never lose a sample");
        assert!(count_lost(&lost, &mut e) > 20, "the fixture never leaves the desk");
    }

    #[test]
    fn the_edge_clamp_lands_on_the_panel_it_left_through() {
        let desk = flat_desk();
        let ray  = desk.px_to_ray(GlobalPx { x: 10.0, y: 10.0 }).unwrap();

        // A big swing up and to the user's left, well past the top-left corner.
        let point = edge_point(&desk, &ray, 20.0, 20.0).expect("the clean ray hits");

        // The clamped point must sit on the panel, on the edge the ray left through, not
        // somewhere out in the void.
        assert!(desk.output_at(point).is_some(), "clamped off the panel: {point:?}");
        assert!(point.x < 12.0 && point.y < 12.0, "clamped the wrong way: {point:?}");
        assert!(point.x >= -0.01 && point.y >= -0.01, "clamped past the edge: {point:?}");
    }

    #[test]
    fn sectors_split_the_compass_into_eight_wedges() {
        let o = GlobalPx { x: 100.0, y: 100.0 };

        // Screen axes: +x right, +y down. Opposite directions must never share a sector.
        let right = sector(o, GlobalPx { x: 200.0, y: 100.0 });
        let left  = sector(o, GlobalPx { x: 0.0, y: 100.0 });
        let down  = sector(o, GlobalPx { x: 100.0, y: 200.0 });
        let up    = sector(o, GlobalPx { x: 100.0, y: 0.0 });

        for pair in [(right, left), (up, down), (right, up), (right, down)] {
            assert_ne!(pair.0, pair.1, "{pair:?}");
        }

        // A small angular step within one wedge stays in it.
        assert_eq!(right, sector(o, GlobalPx { x: 200.0, y: 110.0 }));

        // Every real direction lands in range, and a coincident point gets the pseudo
        // sector past the end so it never collides with a real one.
        for step in 0..64 {
            let a = step as f64 * std::f64::consts::TAU / 64.0;
            let p = GlobalPx { x: 100.0 + 50.0 * a.cos(), y: 100.0 + 50.0 * a.sin() };

            assert!(sector(o, p) < FLICK_SECTORS);
        }

        assert_eq!(sector(o, o), FLICK_SECTORS);
    }

    #[test]
    fn a_warp_onto_the_target_needs_no_nudge() {
        let desk = flat_desk();
        let bbox = Rect { x: 400.0, y: 300.0, w: 80.0, h: 40.0 };

        // Inside the box, and on its edge, are both zero.
        for p in [GlobalPx { x: 440.0, y: 320.0 }, GlobalPx { x: 400.0, y: 300.0 }] {
            assert_eq!(nudge_distance(&desk, &bbox, p), (0.0, 0.0));
        }

        // Outside, the pixel distance is to the nearest point of the box, and the angular
        // figure is that distance at the local scale.
        let (px, deg) = nudge_distance(&desk, &bbox, GlobalPx { x: 500.0, y: 320.0 });

        assert!((px - 20.0).abs() < 1.0e-9, "px = {px}");

        let (ppd_x, _) = px_per_deg_at(&desk, GlobalPx { x: 490.0, y: 320.0 });

        assert!((deg - 20.0 / ppd_x).abs() < 0.02, "deg = {deg}");
    }

    #[test]
    fn a_nudge_is_measured_to_the_intended_target_not_the_snapped_one() {
        let desk  = flat_desk();
        let model = model();
        let s     = setup(&desk, &model, 0.7);

        // Two well-separated buttons: when the engine snaps to the wrong one the nudge is
        // the whole gap, and when it snaps right the nudge is zero.
        let elements = [
            element(0, 400.0, 300.0, 40.0, 20.0),
            element(1, 470.0, 300.0, 40.0, 20.0),
        ];

        let mut e = engine();

        for trial in 0..200 {
            let mut rng = StdRng::seed_from_u64(trial);
            let out     = run_single(&s, &mut e, &elements, 0, &mut rng);

            match out.outcome {
                Outcome::Correct => assert_eq!(out.nudge_px, Some(0.0), "trial {trial}"),

                Outcome::Slip { .. } => {
                    let px = out.nudge_px.expect("a slip still warps somewhere");

                    assert!(px > 0.0, "a slip measured a zero nudge on trial {trial}");
                }

                _ => {}
            }
        }
    }

    #[test]
    fn a_flick_resolves_targets_that_lie_in_different_directions() {
        let desk  = flat_desk();
        let model = model();
        let s     = setup(&desk, &model, 0.7);

        // Three small buttons arranged around a triangle, far apart in direction from
        // anywhere near the middle: a flick should almost always separate them.
        let spread = [
            element(0, 480.0, 300.0, 30.0, 20.0),
            element(1, 400.0, 380.0, 30.0, 20.0),
            element(2, 560.0, 380.0, 30.0, 20.0),
        ];

        // Three in a horizontal row: the two on the same side of the warp point share a
        // direction, so a flick cannot always tell them apart.
        let row: Vec<Element> = (0..3)
            .map(|i| element(i, 460.0 + i as f64 * 34.0, 340.0, 30.0, 20.0))
            .collect();

        let rate = |elements: &[Element], target: usize, e: &mut SnapEngine| {
            let ok = (0..200)
                .filter_map(|trial| {
                    let mut rng = StdRng::seed_from_u64(trial);

                    run_single(&s, e, elements, target, &mut rng).flick
                })
                .filter(|ok| *ok)
                .count();

            ok as f64 / 200.0
        };

        let mut e = engine();

        let spread_rate = rate(&spread, 0, &mut e);
        let row_rate    = rate(&row, 1, &mut e);

        // Neither is near 1: the warp sits inside whichever box won, so a slip puts the
        // intended target and its neighbour on nearly the same bearing. What the metric
        // has to capture is that geometry that spreads targets around the compass is
        // flickable and a row of them is much less so.
        assert!(spread_rate > 0.5, "a spread-out triple was barely flickable: {spread_rate}");
        assert!(
            spread_rate > row_rate + 0.15,
            "a row ({row_rate}) came too close to a triangle ({spread_rate})"
        );
    }

    #[test]
    fn a_sigma_wider_than_the_radius_reports_no_target_not_a_slip() {
        let desk     = flat_desk();
        let model    = model();
        let s        = setup(&desk, &model, 2.5);
        let elements = [element(0, 400.0, 300.0, 80.0, 40.0)];
        let mut e    = engine();
        let mut rng  = StdRng::seed_from_u64(5);

        // The engine bails above its radius (2 deg by default): that is the handoff to
        // the coarse tier, and the bench must not score it as a wrong answer.
        let out = run_single(&s, &mut e, &elements, 0, &mut rng);

        assert!(matches!(out.outcome, Outcome::NoTarget | Outcome::NoGaze));
    }

    #[test]
    fn a_lost_profile_angle_counts_as_no_gaze() {
        let desk     = flat_desk();
        let model    = model();
        let elements = [element(0, 400.0, 300.0, 80.0, 40.0)];
        let mut e    = engine();
        let mut rng  = StdRng::seed_from_u64(6);

        // `invalid_at_deg = 0` rejects every angle, so the profile never yields a sigma.
        let profile = SigmaProfile {
            sigma_deg      : 0.7,
            flat_to_deg    : 25.0,
            ramp_to_deg    : 35.0,
            ramp_factor    : 3.0,
            invalid_at_deg : 0.0,
        };

        let s = TrialSetup {
            sigma : SigmaMode::Profile(profile),
            ..setup(&desk, &model, 0.7)
        };

        let out = run_single(&s, &mut e, &elements, 0, &mut rng);

        assert_eq!(out.outcome, Outcome::NoGaze);
        assert!(out.err_px.is_none());
    }

    #[test]
    fn the_sequence_mode_reads_as_a_fixation_under_the_split_model() {
        let desk     = flat_desk();
        let model    = model();
        let s        = setup(&desk, &model, 0.7);
        let elements = [element(0, 400.0, 300.0, 80.0, 40.0)];
        let mut f    = FilterStack::create().scale(Box::new(ConstPxScale::new(60.0))).build();
        let mut e    = engine();

        let mut fixating = 0;
        let mut fed      = 0;

        for trial in 0..50 {
            let mut rng = StdRng::seed_from_u64(trial);
            let out     = run_sequence(&s, &mut f, &mut e, &elements, 0, &mut rng);

            fixating += out.fixating.0;
            fed      += out.fixating.1;
        }

        // The whole point of the split: 0.2 deg of per-sample jitter at 120 Hz is well
        // under the 30 deg/s I-VT threshold, so most of the fixation reads as one.
        let rate = fixating as f64 / fed as f64;

        assert!(rate > 0.6, "only {:.0}% of samples read as fixating", rate * 100.0);
    }

    #[test]
    fn the_legacy_model_starves_the_fixation_classifier() {
        let desk     = flat_desk();
        let model    = model();
        let legacy   = TrialSetup { legacy: true, ..setup(&desk, &model, 0.7) };
        let elements = [element(0, 400.0, 300.0, 80.0, 40.0)];
        let mut f    = FilterStack::create().scale(Box::new(ConstPxScale::new(60.0))).build();
        let mut e    = engine();

        let mut fixating = 0;
        let mut fed      = 0;

        for trial in 0..50 {
            let mut rng = StdRng::seed_from_u64(trial);
            let out     = run_sequence(&legacy, &mut f, &mut e, &elements, 0, &mut rng);

            fixating += out.fixating.0;
            fed      += out.fixating.1;
        }

        // This is the bug the split model fixes, pinned so it cannot come back silently.
        let rate = fixating as f64 / fed as f64;

        assert!(rate < 0.4, "legacy noise unexpectedly read as fixating {:.0}%", rate * 100.0);
    }

    #[test]
    fn single_and_sequence_share_their_first_noise_draw() {
        let desk     = flat_desk();
        let model    = model();
        let s        = setup(&desk, &model, 0.7);
        let elements = [element(0, 400.0, 300.0, 80.0, 40.0)];
        let mut f    = FilterStack::create().scale(Box::new(ConstPxScale::new(60.0))).build();
        let mut e    = engine();

        let mut rng_a = StdRng::seed_from_u64(11);
        let mut rng_b = StdRng::seed_from_u64(11);

        let a = run_single(&s, &mut e, &elements, 0, &mut rng_a);
        let b = run_sequence(&s, &mut f, &mut e, &elements, 0, &mut rng_b);

        // Same seed, same landing point, same bias, same first jitter: the two feeding
        // modes are a paired comparison, not two independent samples.
        assert_eq!(a.err_px, b.err_px);
    }
}
