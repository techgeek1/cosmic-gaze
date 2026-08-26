//! Fixation classification and smoothing, in that order.
//!
//! The architecture is Tobii's zero-delay filtering ([patent reference removed]): an I-VT state
//! machine decides whether the eye is holding still or in flight, and a one-euro
//! smoother runs *only* while it is holding still. Saccades pass through untouched, so a
//! warp lands where the eye landed instead of being dragged back toward where the eye
//! was, and the smoother restarts from the new fixation's first sample.
//!
//! Everything here is pure: timestamps arrive on the samples, no clock is read.

use std::collections::VecDeque;
use std::f64::consts::PI;
use std::fmt;

use gaze_core::{GazeSample, GlobalPx};
use glam::DVec3;

use crate::scale::{ConstPxScale, PxScale};

/// I-VT velocity threshold, degrees per second. Tobii's shipped default.
pub const DEFAULT_VELOCITY_THRESHOLD_DEG_S : f64 = 30.0;

/// I-VT velocity window, seconds. Tobii's shipped default. Long enough that per-sample
/// noise does not read as motion at 30 Hz, short enough not to blur a 50 ms saccade.
pub const DEFAULT_WINDOW_S : f64 = 0.02;

/// Sample gap beyond which the previous sample tells us nothing about current velocity.
/// A gap this long is a blink or a dropped packet, so classification restarts.
pub const DEFAULT_MAX_GAP_S : f64 = 0.1;

/// One-euro minimum cutoff, Hz. Reported as a good gaze value alongside `beta` 0.3.
pub const DEFAULT_MIN_CUTOFF_HZ : f64 = 0.3;

/// One-euro speed coefficient: cutoff Hz added per degree per second of estimated speed.
pub const DEFAULT_BETA : f64 = 0.3;

/// Cutoff of the one-euro derivative pre-filter, Hz. The value from the original paper.
pub const DEFAULT_D_CUTOFF_HZ : f64 = 1.0;

/// What the classifier believes the eye is doing at a sample.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FixationState {
    /// The eye is holding still and the sample has been smoothed. `since_s` is the
    /// timestamp of the fixation's first sample, so its current duration is
    /// `sample.t_s - since_s`.
    Fixating { since_s : f64 },
    /// The eye is in flight, or velocity is not yet known (first sample after a gap).
    /// The sample is raw.
    Saccade,
    /// The provider reported an invalid sample. All filter state has been dropped; the
    /// sample's `point` and `ray` must not be used.
    Lost,
}

/// One classified, possibly smoothed sample.
///
/// Only `sample.point` is filtered. `sample.ray` is passed through untouched because
/// rebuilding a ray from a filtered pixel needs the desk geometry, which this crate
/// deliberately does not depend on.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Filtered {
    pub sample : GazeSample,
    pub state  : FixationState,
}

/// I-VT classification gating a one-euro smoother.
///
/// Feed every sample from the provider in timestamp order. The stack keeps a few
/// samples of raw history for the velocity window and nothing else, so it is cheap to
/// run per sample and safe to reset at any time.
pub struct FilterStack {
    /// Used to express pixel motion in degrees when samples carry no ray.
    scale                    : Box<dyn PxScale>,
    velocity_threshold_deg_s : f64,
    window_s                 : f64,
    max_gap_s                : f64,
    min_cutoff_hz            : f64,
    beta                     : f64,
    d_cutoff_hz              : f64,
    /// Raw samples spanning the velocity window. Classification never sees filtered data.
    history                  : VecDeque<Trace>,
    /// Timestamp of the current fixation's first sample, `None` during a saccade.
    fixation_start_s         : Option<f64>,
    /// One-euro state, `None` until the first sample of a fixation initialises it.
    euro                     : Option<EuroState>,
}

/// Builder for [`FilterStack`]. See the constants in this module for the defaults.
pub struct FilterStackBuilder {
    scale                    : Option<Box<dyn PxScale>>,
    velocity_threshold_deg_s : f64,
    window_s                 : f64,
    max_gap_s                : f64,
    min_cutoff_hz            : f64,
    beta                     : f64,
    d_cutoff_hz              : f64,
}

/// One raw sample, reduced to what velocity estimation needs.
#[derive(Clone, Copy, Debug)]
struct Trace {
    t_s   : f64,
    point : Option<GlobalPx>,
    dir   : Option<DVec3>,
}

/// One-euro filter state, shared across both axes except for the per-axis derivative
/// estimates.
#[derive(Clone, Copy, Debug)]
struct EuroState {
    t_s       : f64,
    /// Previous raw input; the derivative is taken on the raw signal, as in the paper.
    raw       : GlobalPx,
    /// Previous filtered output.
    hat       : GlobalPx,
    /// Smoothed speed estimates in degrees per second, which is what makes `beta`
    /// independent of which output the gaze is on.
    dx_hat_x  : f64,
    dx_hat_y  : f64,
}

// --- FilterStack ---

impl FilterStack {
    /// Starts building a filter stack.
    pub fn create() -> FilterStackBuilder {
        FilterStackBuilder::new()
    }

    /// Classifies and (during fixations) smooths one sample.
    ///
    /// Samples must arrive in non-decreasing timestamp order. Out-of-order or duplicated
    /// timestamps are treated as an unknown velocity, which classifies as a saccade and
    /// passes the sample through raw.
    pub fn push(&mut self, sample: GazeSample) -> Filtered {
        // A dropout invalidates everything: velocity across the gap is meaningless and
        // the smoother must not carry pre-blink position into post-blink samples.
        if !sample.valid {
            self.reset();

            return Filtered { sample: sample, state: FixationState::Lost };
        }

        // Classification runs on raw data, before smoothing, so that the smoother cannot
        // hide a saccade from the classifier that gates it.
        self.history.push_back(Trace::from_sample(&sample));
        self.prune_history(sample.t_s);

        let moving = match self.velocity_deg_s(&sample) {
            Some(v) => v > self.velocity_threshold_deg_s,
            // Unknown velocity (first sample, or a gap) is treated as motion: passing the
            // sample through raw is always safe, smoothing it against stale state is not.
            None    => true,
        };

        if moving {
            self.fixation_start_s = None;
            self.euro             = None;

            return Filtered { sample: sample, state: FixationState::Saccade };
        }

        let since_s = *self.fixation_start_s.get_or_insert(sample.t_s);
        let mut out = sample;

        if let Some(p) = sample.point {
            out.point = Some(self.smooth(sample.t_s, p));
        }

        Filtered { sample: out, state: FixationState::Fixating { since_s: since_s } }
    }

    /// Drops all history and smoothing state. The next sample is classified as a saccade.
    pub fn reset(&mut self) {
        self.history.clear();
        self.fixation_start_s = None;
        self.euro             = None;
    }

    /// Velocity threshold in effect, degrees per second.
    pub fn velocity_threshold_deg_s(&self) -> f64 {
        self.velocity_threshold_deg_s
    }
}

impl FilterStack {
    /// Keeps the newest sample at or before the window start plus everything after it.
    /// That front entry is the anchor the velocity estimate measures against, so the
    /// measured interval is the smallest one that covers the full window.
    fn prune_history(&mut self, now_s: f64) {
        let cutoff = now_s - self.window_s;

        while self.history.len() >= 2 && self.history[1].t_s <= cutoff {
            self.history.pop_front();
        }
    }

    /// Angular velocity of `cur` against the oldest sample in the window, or `None` when
    /// there is no usable anchor (first sample, a gap, or no common representation).
    fn velocity_deg_s(&self, cur: &GazeSample) -> Option<f64> {
        let anchor = self.history.front()?;
        let dt     = cur.t_s - anchor.t_s;

        if dt <= 0.0 || dt > self.max_gap_s {
            return None;
        }

        Some(self.angle_deg(anchor, cur)? / dt)
    }

    /// Visual angle between an earlier sample and the current one.
    ///
    /// Rays are exact and scale-free, so they win when both samples carry one. The pixel
    /// path is the small-angle approximation through the local scale, which is accurate
    /// well past the 30 deg/s threshold at any sane sample rate.
    fn angle_deg(&self, anchor: &Trace, cur: &GazeSample) -> Option<f64> {
        if let (Some(a), Some(b)) = (anchor.dir, cur.ray.map(|r| r.dir)) {
            return Some(a.angle_between(b).to_degrees());
        }

        let (Some(a), Some(b)) = (anchor.point, cur.point) else {
            return None;
        };

        let (ppd_x, ppd_y) = self.scale.px_per_deg(b);

        if ppd_x <= 0.0 || ppd_y <= 0.0 {
            return None;
        }

        Some(((b.x - a.x) / ppd_x).hypot((b.y - a.y) / ppd_y))
    }

    /// One-euro step on a fixation sample. The first sample of a fixation initialises the
    /// state to itself and is returned untouched, which is what keeps a fresh fixation
    /// from being pulled toward the previous one.
    fn smooth(&mut self, t_s: f64, raw: GlobalPx) -> GlobalPx {
        let (ppd_x, ppd_y) = self.scale.px_per_deg(raw);

        let Some(prev) = self.euro else {
            self.euro = Some(EuroState {
                t_s      : t_s,
                raw      : raw,
                hat      : raw,
                dx_hat_x : 0.0,
                dx_hat_y : 0.0,
            });

            return raw;
        };

        let dt = t_s - prev.t_s;

        if dt <= 0.0 || ppd_x <= 0.0 || ppd_y <= 0.0 {
            return prev.hat;
        }

        // Speed estimate in degrees per second, itself low-passed so a single noisy
        // sample cannot open the cutoff wide.
        let a_d      = alpha(self.d_cutoff_hz, dt);
        let dx_x     = (raw.x - prev.raw.x) / ppd_x / dt;
        let dx_y     = (raw.y - prev.raw.y) / ppd_y / dt;
        let dx_hat_x = a_d * dx_x + (1.0 - a_d) * prev.dx_hat_x;
        let dx_hat_y = a_d * dx_y + (1.0 - a_d) * prev.dx_hat_y;

        // Faster motion gets a higher cutoff: precision when still, no lag when moving.
        let a_x = alpha(self.min_cutoff_hz + self.beta * dx_hat_x.abs(), dt);
        let a_y = alpha(self.min_cutoff_hz + self.beta * dx_hat_y.abs(), dt);
        let hat = GlobalPx {
            x : a_x * raw.x + (1.0 - a_x) * prev.hat.x,
            y : a_y * raw.y + (1.0 - a_y) * prev.hat.y,
        };

        self.euro = Some(EuroState {
            t_s      : t_s,
            raw      : raw,
            hat      : hat,
            dx_hat_x : dx_hat_x,
            dx_hat_y : dx_hat_y,
        });

        hat
    }
}

impl fmt::Debug for FilterStack {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("FilterStack")
            .field("velocity_threshold_deg_s" , &self.velocity_threshold_deg_s)
            .field("window_s"                 , &self.window_s)
            .field("max_gap_s"                , &self.max_gap_s)
            .field("min_cutoff_hz"            , &self.min_cutoff_hz)
            .field("beta"                     , &self.beta)
            .field("d_cutoff_hz"              , &self.d_cutoff_hz)
            .field("fixation_start_s"         , &self.fixation_start_s)
            .finish_non_exhaustive()
    }
}

// --- FilterStackBuilder ---

impl FilterStackBuilder {
    /// A builder holding the module defaults.
    pub fn new() -> Self {
        Self {
            scale                    : None,
            velocity_threshold_deg_s : DEFAULT_VELOCITY_THRESHOLD_DEG_S,
            window_s                 : DEFAULT_WINDOW_S,
            max_gap_s                : DEFAULT_MAX_GAP_S,
            min_cutoff_hz            : DEFAULT_MIN_CUTOFF_HZ,
            beta                     : DEFAULT_BETA,
            d_cutoff_hz              : DEFAULT_D_CUTOFF_HZ,
        }
    }

    /// Scale source used to express pixel motion in degrees. Only consulted for samples
    /// without a ray. Defaults to a flat [`crate::scale::FALLBACK_PX_PER_DEG`] world,
    /// which is a placeholder, not a measurement: pass the desk geometry in real use.
    pub fn scale(mut self, scale: Box<dyn PxScale>) -> Self {
        self.scale = Some(scale);

        self
    }

    /// I-VT threshold: above this, the eye counts as in flight.
    pub fn velocity_threshold_deg_s(mut self, deg_s: f64) -> Self {
        self.velocity_threshold_deg_s = deg_s;

        self
    }

    /// I-VT velocity window.
    pub fn window_s(mut self, window_s: f64) -> Self {
        self.window_s = window_s;

        self
    }

    /// Gap beyond which classification restarts rather than measuring across the hole.
    pub fn max_gap_s(mut self, max_gap_s: f64) -> Self {
        self.max_gap_s = max_gap_s;

        self
    }

    /// One-euro parameters: minimum cutoff in Hz and the speed coefficient.
    pub fn one_euro(mut self, min_cutoff_hz: f64, beta: f64) -> Self {
        self.min_cutoff_hz = min_cutoff_hz;
        self.beta          = beta;

        self
    }

    /// Cutoff of the one-euro derivative pre-filter. Rarely worth changing.
    pub fn d_cutoff_hz(mut self, d_cutoff_hz: f64) -> Self {
        self.d_cutoff_hz = d_cutoff_hz;

        self
    }

    /// Builds the stack.
    pub fn build(self) -> FilterStack {
        FilterStack {
            scale                    : self.scale.unwrap_or_else(
                || Box::new(ConstPxScale::default())
            ),
            velocity_threshold_deg_s : self.velocity_threshold_deg_s,
            window_s                 : self.window_s,
            max_gap_s                : self.max_gap_s,
            min_cutoff_hz            : self.min_cutoff_hz,
            beta                     : self.beta,
            d_cutoff_hz              : self.d_cutoff_hz,
            history                  : VecDeque::with_capacity(8),
            fixation_start_s         : None,
            euro                     : None,
        }
    }
}

impl Default for FilterStackBuilder {
    fn default() -> Self {
        Self::new()
    }
}

// --- Trace ---

impl Trace {
    /// Reduces a sample to its velocity-relevant parts.
    fn from_sample(sample: &GazeSample) -> Self {
        Self {
            t_s   : sample.t_s,
            point : sample.point,
            dir   : sample.ray.map(|r| r.dir),
        }
    }
}

/// One-euro smoothing factor for a cutoff frequency and a sample interval.
fn alpha(cutoff_hz: f64, dt: f64) -> f64 {
    let tau = 1.0 / (2.0 * PI * cutoff_hz.max(f64::MIN_POSITIVE));

    1.0 / (1.0 + tau / dt)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::Ray;

    use super::*;

    /// 60 px per degree: a 27 inch 1440p panel at arm's length, near enough.
    const PPD : f64 = 60.0;

    /// Test stack: the documented defaults over a flat 60 px/deg world.
    fn stack() -> FilterStack {
        FilterStack::create()
            .scale(Box::new(ConstPxScale::new(PPD)))
            .velocity_threshold_deg_s(DEFAULT_VELOCITY_THRESHOLD_DEG_S)
            .window_s(DEFAULT_WINDOW_S)
            .one_euro(DEFAULT_MIN_CUTOFF_HZ, DEFAULT_BETA)
            .build()
    }

    /// A valid 2D-only sample at 0.7 deg sigma.
    fn sample(t_s: f64, x: f64, y: f64) -> GazeSample {
        GazeSample {
            t_s       : t_s,
            ray       : None,
            point     : Some(GlobalPx { x: x, y: y }),
            sigma_deg : 0.7,
            valid     : true,
        }
    }

    /// A sample carrying a ray pointing `deg` degrees off the -Z axis in the XZ plane.
    fn ray_sample(t_s: f64, deg: f64) -> GazeSample {
        let r = deg.to_radians();

        GazeSample {
            t_s       : t_s,
            ray       : Some(Ray {
                origin : DVec3::ZERO,
                dir    : DVec3::new(r.sin(), 0.0, -r.cos()),
            }),
            point     : None,
            sigma_deg : 0.7,
            valid     : true,
        }
    }

    #[test]
    fn drift_is_a_fixation() {
        let mut s = stack();

        // 0.5 deg/s for 200 ms at 100 Hz: a textbook fixational drift.
        let mut state = FixationState::Saccade;

        for i in 0..20 {
            let t = i as f64 * 0.01;
            let x = 500.0 + 0.5 * PPD * t;

            state = s.push(sample(t, x, 500.0)).state;
        }

        assert!(matches!(state, FixationState::Fixating { .. }), "{state:?}");
    }

    #[test]
    fn fast_jump_is_a_saccade() {
        let mut s = stack();

        // Settle into a fixation first so the saccade is the only thing being detected.
        for i in 0..10 {
            s.push(sample(i as f64 * 0.01, 500.0, 500.0));
        }

        // 300 deg/s for one 10 ms sample: 3 deg of travel.
        let f = s.push(sample(0.10, 500.0 + 3.0 * PPD, 500.0));

        assert_eq!(f.state, FixationState::Saccade);
    }

    #[test]
    fn first_sample_is_a_saccade() {
        let mut s = stack();

        assert_eq!(s.push(sample(0.0, 100.0, 100.0)).state, FixationState::Saccade);
    }

    #[test]
    fn fixation_start_is_the_first_sample_of_the_fixation() {
        let mut s = stack();
        let mut since = None;

        for i in 0..10 {
            let f = s.push(sample(i as f64 * 0.01, 500.0, 500.0));

            if let FixationState::Fixating { since_s } = f.state {
                since.get_or_insert(since_s);
                assert_eq!(f.state, FixationState::Fixating { since_s: since.unwrap() });
            }
        }

        // Sample 0 has no anchor, sample 1 is the first with a measurable velocity.
        assert_eq!(since, Some(0.01));
    }

    #[test]
    fn ray_samples_classify_without_a_scale() {
        // No scale is configured, so a wrong answer here would mean the pixel path ran.
        let mut s = FilterStack::create().build();

        for i in 0..10 {
            s.push(ray_sample(i as f64 * 0.01, 0.0));
        }

        assert!(matches!(
            s.push(ray_sample(0.10, 0.05)).state,
            FixationState::Fixating { .. }
        ));

        // 10 deg in 10 ms is 1000 deg/s.
        assert_eq!(s.push(ray_sample(0.11, 10.05)).state, FixationState::Saccade);
    }

    #[test]
    fn smoothing_attenuates_fixation_jitter() {
        let mut s = stack();
        let mut lo = f64::INFINITY;
        let mut hi = f64::NEG_INFINITY;

        // Square-wave jitter of +/- 6 px (0.1 deg) around 500: the velocity window spans
        // two samples at 100 Hz, so it measures zero motion and every sample after the
        // first is a fixation. Amplitude is measured once the filter has settled, since a
        // 0.3 Hz minimum cutoff takes a second or so to walk in from its first sample.
        for i in 0..400 {
            let t   = i as f64 * 0.01;
            let raw = 500.0 + if i % 2 == 0 { 6.0 } else { -6.0 };
            let f   = s.push(sample(t, raw, 500.0));

            if i >= 350 {
                let x = f.sample.point.unwrap().x;

                lo = lo.min(x);
                hi = hi.max(x);
            }
        }

        assert!(hi - lo < 1.0, "12 px of raw jitter came through as {}", hi - lo);
    }

    #[test]
    fn smoothing_does_not_cross_a_saccade() {
        let mut s = stack();

        for i in 0..40 {
            s.push(sample(i as f64 * 0.01, 500.0, 500.0));
        }

        // Jump 10 deg right and hold. The jump sample and the samples still measured
        // against pre-jump history are saccades, and the first fixation sample after
        // them must be exactly raw: no trace of the old position.
        let target = 500.0 + 10.0 * PPD;
        let mut first_fixating = None;

        for i in 40..50 {
            let t = i as f64 * 0.01;
            let f = s.push(sample(t, target, 500.0));

            match f.state {
                FixationState::Fixating { .. } => {
                    first_fixating.get_or_insert((t, f.sample.point.unwrap().x));
                }

                _ => {
                    assert_eq!(f.sample.point.unwrap().x, target, "saccade sample was filtered");
                }
            }
        }

        let (t, x) = first_fixating.expect("never returned to a fixation");

        assert_eq!(x, target, "first fixation sample at {t} was dragged back");
    }

    #[test]
    fn invalid_sample_resets_the_stack() {
        let mut s = stack();

        for i in 0..40 {
            s.push(sample(i as f64 * 0.01, 500.0, 500.0));
        }

        let mut lost = sample(0.40, 500.0, 500.0);
        lost.valid   = false;

        assert_eq!(s.push(lost).state, FixationState::Lost);

        // History is gone, so the next sample has no anchor and cannot be smoothed.
        let f = s.push(sample(0.41, 500.0, 500.0));

        assert_eq!(f.state, FixationState::Saccade);
        assert_eq!(f.sample.point.unwrap().x, 500.0);
    }

    #[test]
    fn a_long_gap_restarts_classification() {
        let mut s = stack();

        for i in 0..40 {
            s.push(sample(i as f64 * 0.01, 500.0, 500.0));
        }

        // Same position, but half a second later: velocity is unknowable, not zero.
        assert_eq!(s.push(sample(0.90, 500.0, 500.0)).state, FixationState::Saccade);
    }
}
