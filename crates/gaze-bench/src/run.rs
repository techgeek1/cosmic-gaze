//! The Monte Carlo driver: every sigma, every feeding mode, every candidate set, every
//! element of every screenshot.
//!
//! Work is parallel over (run, screenshot) pairs, which is enough to fill a desktop CPU
//! without nesting rayon pools. Determinism does not depend on the split: each trial
//! seeds its own RNG from `(seed, frame, element, trial, sigma)`.

use std::sync::Arc;

use gaze_core::{DesktopGeometry, NoiseModel, SigmaProfile};
use gaze_snap::{FilterStack, ScoreWeights, SnapEngine};
use rand::SeedableRng;
use rand::rngs::StdRng;
use rayon::prelude::{IntoParallelRefIterator, ParallelIterator};

use crate::shots::{DeskScale, Shot};
use crate::stats::{
    ElementStats, FrameResult, Histogram, NEAR_BUCKETS, NUDGE_DEG_BINS, NUDGE_DEG_WIDTH,
    NUDGE_PX_BINS, NUDGE_PX_WIDTH, RivalPolicy, SizeBucket, TOPK, Tally, TargetClass,
    classify_target, nested_in_other_kind, short_side_deg,
};
use crate::trial::{SigmaMode, TrialSetup, run_sequence, run_single, trial_seed};

/// How a trial's samples reach the snap engine.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum FeedMode {
    /// One already-classified fixation sample, straight into `SnapEngine::update`. This
    /// is the optimistic-but-honest reading of "the user was fixating and we snapped".
    Single,
    /// A whole 200 ms fixation at 120 Hz through `FilterStack` and then `update`,
    /// resolved by `commit`. This is what the live loop actually does.
    Sequence,
}

/// Which elements the engine is allowed to choose between.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CandidateSet {
    /// Everything the detector found, text runs and terminal rows included. What the live
    /// loop would see today.
    All,
    /// Only `TargetClass::Widget` elements. Answers whether OCR lines are stealing snaps
    /// from real controls, which would argue for a much larger kind penalty or for
    /// keeping OCR boxes out of the snap candidate set entirely.
    WidgetsOnly,
}

/// Which sigma a run used.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum SigmaSetting {
    /// A constant sigma over the whole desk.
    Fixed(f64),
    /// The desk's own sigma profile, by off-axis angle.
    Profile,
}

/// Identifies one run of the Monte Carlo.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct RunKey {
    pub mode       : FeedMode,
    pub sigma      : SigmaSetting,
    pub candidates : CandidateSet,
    /// Index of the sigma setting in the configured sweep, mixed into every trial seed so
    /// two sigmas never share a noise stream.
    pub sigma_id   : u32,
}

/// Everything one run produced, one entry per screenshot in shot-set order.
#[derive(Clone, Debug)]
pub struct RunResult {
    pub key    : RunKey,
    pub frames : Vec<FrameResult>,
}

/// Knobs for the whole sweep. The engine knobs exist so the scoring can be swept later
/// without touching the crate.
#[derive(Clone, Debug)]
pub struct BenchConfig {
    pub seed        : u64,
    /// Trials per element in single-sample mode.
    pub trials      : u32,
    /// Trials per element in sequence mode, which costs 24 engine updates each and so
    /// gets its own budget.
    pub seq_trials  : u32,
    pub radius_deg  : f64,
    pub hysteresis  : f64,
    pub weights     : ScoreWeights,
    /// Constant sigmas to sweep, degrees.
    pub sigmas      : Vec<f64>,
    /// The desk's sigma profile, when `--sigma-profile` was asked for.
    pub profile     : Option<SigmaProfile>,
    /// Supplies the per-sample jitter and the per-fixation bias split.
    pub model       : NoiseModel,
    /// Draw the full sigma independently per sample, as the first bench did.
    pub legacy      : bool,
    /// Cost gap below which a trial counts as ambiguous.
    pub margin      : f64,
    /// Which rivals are allowed to make a trial ambiguous.
    pub rivals      : RivalPolicy,
    /// Clamp an off-desk sample to the panel edge instead of losing it.
    pub clamp       : bool,
}

// --- Driving ---

/// Runs every (sigma, feeding mode, candidate set) combination over every screenshot.
///
/// The returned runs are ordered candidate-set-major, then sigma, then single-sample
/// before sequence, which is the order the report prints them in.
pub fn run_all(shots: &[Shot], geometry: &Arc<DesktopGeometry>, config: &BenchConfig)
    -> Vec<RunResult>
{
    let keys = run_keys(config);

    // Target geometry is a property of the element, not of the run, so it is measured
    // once per candidate set and shared by every run over it.
    let sizes_all     = measure(shots, geometry, CandidateSet::All);
    let sizes_widgets = measure(shots, geometry, CandidateSet::WidgetsOnly);

    // Flat over (run, shot): a few hundred tasks for the usual sweep, which balances well
    // enough without nesting a second rayon pool inside each one.
    let tasks: Vec<(usize, usize)> = (0..keys.len())
        .flat_map(|r| (0..shots.len()).map(move |s| (r, s)))
        .collect();

    let mut frames: Vec<Vec<FrameResult>> = keys.iter().map(|_| Vec::new()).collect();

    let mut done: Vec<(usize, FrameResult)> = tasks.par_iter()
        .map(|&(run, shot)| {
            let key = keys[run];

            let sizes = match key.candidates {
                CandidateSet::All         => &sizes_all[shot],
                CandidateSet::WidgetsOnly => &sizes_widgets[shot],
            };

            (run, run_frame(&shots[shot], shot, sizes, geometry, config, key))
        })
        .collect();

    // Grouping by run and sorting by shot restores shot-set order inside each run.
    done.sort_by_key(|(run, frame)| (*run, frame.shot));

    for (run, frame) in done {
        frames[run].push(frame);
    }

    keys.into_iter()
        .zip(frames)
        .map(|(key, frames)| RunResult { key: key, frames: frames })
        .collect()
}

/// Per-element geometry for one candidate set, one row per screenshot.
///
/// The nesting flag is quadratic in the element count, which is why this is computed once
/// per candidate set and shared by every run over it rather than per run.
fn measure(shots: &[Shot], geometry: &DesktopGeometry, set: CandidateSet) -> Vec<Vec<Measured>> {
    shots.iter()
        .map(|shot| {
            let elements = shot.candidates(set);

            elements.iter()
                .enumerate()
                .map(|(i, e)| {
                    let deg = short_side_deg(geometry, &e.bbox);

                    Measured {
                        bucket    : SizeBucket::for_deg(deg),
                        short_deg : deg,
                        class     : classify_target(&e.bbox, e.kind),
                        nested    : nested_in_other_kind(i, elements),
                    }
                })
                .collect()
        })
        .collect()
}

/// What the bench knows about one element before any trial runs.
#[derive(Clone, Copy, Debug)]
struct Measured {
    bucket    : SizeBucket,
    short_deg : f64,
    class     : TargetClass,
    /// See `ElementStats::nested`.
    nested    : bool,
}

/// The runs to perform. Candidate set outermost, then sigma, then feeding mode.
fn run_keys(config: &BenchConfig) -> Vec<RunKey> {
    let mut settings: Vec<SigmaSetting> = config.sigmas.iter()
        .map(|&s| SigmaSetting::Fixed(s))
        .collect();

    if config.profile.is_some() {
        settings.push(SigmaSetting::Profile);
    }

    [CandidateSet::All, CandidateSet::WidgetsOnly].into_iter()
        .flat_map(|candidates| {
            settings.clone().into_iter().enumerate().flat_map(move |(i, sigma)| {
                [FeedMode::Single, FeedMode::Sequence].into_iter().map(move |mode| RunKey {
                    mode       : mode,
                    sigma      : sigma,
                    candidates : candidates,
                    sigma_id   : i as u32,
                })
            })
        })
        .collect()
}

/// Runs every element of one screenshot for one run key.
fn run_frame(
    shot     : &Shot,
    index    : usize,
    sizes    : &[Measured],
    geometry : &Arc<DesktopGeometry>,
    config   : &BenchConfig,
    key      : RunKey,
)
    -> FrameResult
{
    let mut result   = FrameResult::new(index);
    let elements     = shot.candidates(key.candidates);

    if elements.is_empty() {
        return result;
    }

    let sigma = match key.sigma {
        SigmaSetting::Fixed(s) => SigmaMode::Fixed(s),
        SigmaSetting::Profile  => SigmaMode::Profile(
            config.profile.expect("a profile run implies a configured profile")
        ),
    };

    let setup = TrialSetup {
        geometry : geometry,
        model    : &config.model,
        sigma    : sigma,
        legacy   : config.legacy,
        margin   : config.margin,
        rivals   : config.rivals,
        clamp    : config.clamp,
    };

    let mut engine = SnapEngine::create()
        .scale(Box::new(DeskScale::new(Arc::clone(geometry))))
        .radius_deg(config.radius_deg)
        .hysteresis_margin(config.hysteresis)
        .weights(config.weights)
        .build();

    let mut filter = FilterStack::create()
        .scale(Box::new(DeskScale::new(Arc::clone(geometry))))
        .build();

    let trials = match key.mode {
        FeedMode::Single   => config.trials,
        FeedMode::Sequence => config.seq_trials,
    };

    let mut outputs = Vec::with_capacity(trials as usize);

    for (element, measured) in sizes.iter().enumerate() {
        outputs.clear();

        let mut tally = Tally::default();

        for trial in 0..trials {
            let seed    = trial_seed(config.seed, index, element, trial, key.sigma_id);
            let mut rng = StdRng::seed_from_u64(seed);

            let out = match key.mode {
                FeedMode::Single => {
                    run_single(&setup, &mut engine, elements, element, &mut rng)
                }

                FeedMode::Sequence => {
                    run_sequence(&setup, &mut filter, &mut engine, elements, element, &mut rng)
                }
            };

            tally.add(out.outcome, out.ambiguous);
            outputs.push(out);
        }

        let stats = ElementStats {
            index     : element,
            kind      : elements[element].kind,
            class     : measured.class,
            bucket    : measured.bucket,
            short_deg : measured.short_deg,
            nested    : measured.nested,
            tally     : tally,
            topk      : [0; TOPK.len()],
            ranked    : 0,
        };

        result.push(stats, &outputs, elements);
    }

    result
}

// --- Per-element outcomes ---

/// How an element's trials mostly went, for the debug overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum OverlayState {
    /// Mostly right, and the engine was confident about it.
    ConfidentCorrect,
    /// Mostly wrong, and the engine was confident about it. The expensive failure.
    ConfidentWrong,
    /// Mostly resolved, but with a rival close enough that refinement would step in.
    Ambiguous,
    /// Mostly unresolved.
    NoTarget,
    /// Mostly no usable gaze at all.
    NoGaze,
}

/// The state that best describes an element's trials: the most frequent one, with ties
/// broken toward the worse answer so a 50/50 box never reads green.
pub fn dominant_state(tally: &Tally) -> OverlayState {
    // Worst first, so a strictly-greater test leaves ties on the earlier, worse entry.
    let mut best = (tally.no_gaze, OverlayState::NoGaze);

    for candidate in [
        (tally.none, OverlayState::NoTarget),
        (tally.confident_wrong(), OverlayState::ConfidentWrong),
        (tally.ambiguous(), OverlayState::Ambiguous),
        (tally.confident_correct(), OverlayState::ConfidentCorrect),
    ] {
        if candidate.0 > best.0 {
            best = candidate;
        }
    }

    best.1
}

// --- Aggregation helpers ---

/// Total tally over a run, optionally restricted to one target class.
pub fn run_tally(run: &RunResult, class: Option<TargetClass>) -> Tally {
    let mut total = Tally::default();

    for frame in &run.frames {
        for element in &frame.elements {
            if class.is_none_or(|c| element.class == c) {
                total.merge(&element.tally);
            }
        }
    }

    total
}

/// Sums the injected-error accumulators over a run, as `(mean px, mean deg, samples)`.
pub fn run_error(run: &RunResult) -> (f64, f64, u64) {
    let (mut px, mut deg, mut n) = (0.0, 0.0, 0);

    for frame in &run.frames {
        px  += frame.err_px_sum;
        deg += frame.err_deg_sum;
        n   += frame.err_n;
    }

    if n == 0 {
        return (0.0, 0.0, 0);
    }

    (px / n as f64, deg / n as f64, n)
}

/// Fixation-classification rate over a run, as `(fixating samples, samples fed)`.
pub fn run_fixation(run: &RunResult) -> (u64, u64) {
    run.frames.iter().fold((0, 0), |(f, t), frame| (f + frame.fixating, t + frame.fed))
}

/// Trials whose last `update` named the target, over a run.
pub fn run_last_correct(run: &RunResult) -> u64 {
    run.frames.iter().map(|f| f.last_correct).sum()
}

/// Top-k counts and the number of ranked trials over a run, for one target class.
pub fn run_topk(run: &RunResult, class: Option<TargetClass>) -> ([u64; TOPK.len()], u64) {
    run_topk_where(run, |e| class.is_none_or(|c| e.class == c))
}

/// Top-k counts and ranked trials over the elements a predicate accepts.
pub fn run_topk_where(run: &RunResult, keep: impl Fn(&ElementStats) -> bool)
    -> ([u64; TOPK.len()], u64)
{
    let mut topk   = [0; TOPK.len()];
    let mut ranked = 0;

    for frame in &run.frames {
        for element in &frame.elements {
            if keep(element) {
                for (i, n) in element.topk.iter().enumerate() {
                    topk[i] += n;
                }

                ranked += element.ranked;
            }
        }
    }

    (topk, ranked)
}

/// Near-candidate histogram, sum and count over a run, for one target class.
pub fn run_near(run: &RunResult, class: Option<TargetClass>)
    -> ([u64; NEAR_BUCKETS], u64, u64)
{
    let mut hist = [0; NEAR_BUCKETS];
    let mut sum  = 0;
    let mut n    = 0;

    for frame in &run.frames {
        for &slot in class_slots(class) {
            for (i, count) in frame.near_hist[slot].iter().enumerate() {
                hist[i] += count;
            }

            sum += frame.near_sum[slot];
            n   += frame.near_n[slot];
        }
    }

    (hist, sum, n)
}

/// Nudge-distance distributions over a run for one target class, as
/// `(degrees, pixels, already inside, trials with a warp point)`.
pub fn run_nudge(run: &RunResult, class: Option<TargetClass>)
    -> (Histogram, Histogram, u64, u64)
{
    let mut deg    = Histogram::new(NUDGE_DEG_WIDTH, NUDGE_DEG_BINS);
    let mut px     = Histogram::new(NUDGE_PX_WIDTH, NUDGE_PX_BINS);
    let mut inside = 0;
    let mut n      = 0;

    for frame in &run.frames {
        for slot in class_slots(class) {
            deg.merge(&frame.nudge_deg[*slot]);
            px.merge(&frame.nudge_px[*slot]);
            inside += frame.nudge_inside[*slot];
            n      += frame.nudge_n[*slot];
        }
    }

    (deg, px, inside, n)
}

/// Flick-resolvable trials over a run for one target class, as `(resolvable, ranked)`.
pub fn run_flick(run: &RunResult, class: Option<TargetClass>) -> (u64, u64) {
    let mut ok = 0;
    let mut n  = 0;

    for frame in &run.frames {
        for slot in class_slots(class) {
            ok += frame.flick_ok[*slot];
            n  += frame.flick_n[*slot];
        }
    }

    (ok, n)
}

/// The per-class array slots a class filter covers.
fn class_slots(class: Option<TargetClass>) -> &'static [usize] {
    match class {
        Some(TargetClass::Widget)    => &[0],
        Some(TargetClass::Line)      => &[1],
        Some(TargetClass::TextOther) => &[2],
        None                         => &[0, 1, 2],
    }
}

/// Slips split by class over a run, indexed by `slip_slot`.
pub fn run_slips(run: &RunResult) -> [u64; 3] {
    run.frames.iter().fold([0; 3], |mut acc, frame| {
        for (slot, n) in frame.slips.iter().enumerate() {
            acc[slot] += n;
        }

        acc
    })
}

/// The widget-only ambiguity margin sweep over a run: one row per margin, each
/// `[confident correct, confident wrong, ambiguous]`.
pub fn run_sweep(run: &RunResult) -> [[u64; 3]; crate::stats::SWEEP_MARGINS.len()] {
    run.frames.iter().fold([[0; 3]; crate::stats::SWEEP_MARGINS.len()], |mut acc, frame| {
        for (i, row) in frame.sweep.iter().enumerate() {
            for (j, n) in row.iter().enumerate() {
                acc[i][j] += n;
            }
        }

        acc
    })
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A config sweeping two sigmas and the profile.
    fn config(profile: Option<SigmaProfile>) -> BenchConfig {
        BenchConfig {
            seed       : 0,
            trials     : 1,
            seq_trials : 1,
            radius_deg : 2.0,
            hysteresis : 0.15,
            weights    : ScoreWeights::default(),
            sigmas     : vec![0.5, 1.0],
            profile    : profile,
            model      : NoiseModel {
                profile         : SigmaProfile::default(),
                jitter_deg      : 0.2,
                bias_redraw_deg : 1.0,
                drift_deg       : 0.0,
                latency_s       : 0.0,
                rate_hz         : 120.0,
            },
            legacy     : false,
            margin     : 0.5,
            rivals     : RivalPolicy::Distinct,
            clamp      : true,
        }
    }

    #[test]
    fn run_keys_cover_both_candidate_sets_sigma_major() {
        let keys = run_keys(&config(Some(SigmaProfile::default())));

        // 3 sigma settings x 2 modes x 2 candidate sets.
        assert_eq!(keys.len(), 12);

        assert_eq!(keys[0], RunKey {
            mode       : FeedMode::Single,
            sigma      : SigmaSetting::Fixed(0.5),
            candidates : CandidateSet::All,
            sigma_id   : 0,
        });

        assert_eq!(keys[1].mode, FeedMode::Sequence);
        assert_eq!(keys[4].sigma, SigmaSetting::Profile);
        assert_eq!(keys[4].sigma_id, 2);
        assert!(keys[..6].iter().all(|k| k.candidates == CandidateSet::All));
        assert!(keys[6..].iter().all(|k| k.candidates == CandidateSet::WidgetsOnly));

        // The sigma ids must repeat across candidate sets so the same trial seeds are
        // reused: the two sets are a paired comparison of the same fixations.
        assert_eq!(keys[6].sigma_id, 0);
    }

    #[test]
    fn the_profile_is_skipped_when_it_was_not_asked_for() {
        assert_eq!(run_keys(&config(None)).len(), 8);
    }

    #[test]
    fn the_dominant_state_prefers_the_worse_answer_on_a_tie() {
        let even = Tally { correct: 5, slip: 5, ..Default::default() };

        assert_eq!(dominant_state(&even), OverlayState::ConfidentWrong);

        let mostly_right = Tally { correct: 6, slip: 5, ..Default::default() };

        assert_eq!(dominant_state(&mostly_right), OverlayState::ConfidentCorrect);

        // Ambiguity is drawn from both correct and wrong trials, so it can dominate even
        // when neither of them does.
        let mostly_unsure = Tally {
            correct     : 6,
            slip        : 5,
            amb_correct : 5,
            amb_slip    : 4,
            ..Default::default()
        };

        assert_eq!(dominant_state(&mostly_unsure), OverlayState::Ambiguous);

        let lost = Tally { no_gaze: 4, ..Default::default() };

        assert_eq!(dominant_state(&lost), OverlayState::NoGaze);
    }
}
