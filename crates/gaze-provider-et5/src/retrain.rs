//! The retrain ceremony: teaching the firmware's own eye model, once, in Talon's
//! shape, and measuring what it learned.
//!
//! # Why this and not the old sweep
//!
//! The device holds a personal eye model (foveal offset, cornea geometry) that turns
//! its raw images into a gaze ray. Everything the host can do downstream — a
//! correction field, a head-gain regression, a learned residual — sits on top of that
//! model, so every host-side fit is keyed to one blob and a retrain orphans the lot
//! (`PLAN-ET5.md`, "retrain the firmware once"). This module is therefore the *only*
//! thing that writes the firmware model, it does nothing else, and it is meant to be
//! run once per mount.
//!
//! # The ceremony
//!
//! 1. **Declare the plane.** The panel's three corners from `desk.toml`, rotated into
//!    the sensor frame by `tracker_pitch_deg` — the same plane the provider re-declares
//!    on every connect through `ConnectOptions.area`. The firmware's trained 2D output
//!    only means panel uv under the exact plane its points were declared on, so this
//!    number is load-bearing. (Every calibration before 2026-08-28 declared it without
//!    the mount pitch; see DESIGN.md §10c.)
//! 2. **Pick a training area.** Not the whole panel: Talon trains on 600x340 mm
//!    bottom-centred on the tracker, which is roughly the envelope where the glints
//!    stay on the cornea. Points sit at 5%, 50% and 95% of that rectangle.
//! 3. **Six rounds, `cal_points_apply` after each.** Centre, then the four mid-edges,
//!    then the four corners — on black, then the same three on white. Two backgrounds
//!    because a pupil-radius term in the firmware fit is only identifiable if it has
//!    seen both extremes (Tobii [patent reference removed]), and the eye needs seconds to adapt
//!    after each flip.
//! 4. **Gaze-gated acceptance.** A point is only added once the device's own reported
//!    gaze has been nearest to it for most of the last window *and* the median of
//!    those samples lands within a few degrees of it. Feeding a point the user was not
//!    actually looking at is worse than not feeding it at all; [`gate_verdict`] is the
//!    whole decision as a pure function, so it can be tested without a device.
//! 5. **Commit, then measure.** `cal_stop` and `cal_retrieve` bank the blob, then a
//!    3x3 health grid reads the firmware's own gaze back against known targets and the
//!    numbers travel with the blob in the calibration file. Nothing is fitted.
//!
//! Nothing here fits a correction field, a head gain, or a pose: those were the old
//! `calibrate`'s client-side stages, and they are replaced by the session recordings
//! of `crate::record` and the model of Phase C/D.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use glam::DVec3;
use tracing::{info, warn};
use gaze_core::{DesktopGeometry, GlobalPx, OutputGeometry};
use gaze_overlay::{OverlayHandle, OverlayState};

use crate::blob::{CalibrationResult, body_sha256_hex, decode_trailer};
use crate::calibration::HealthStop;
use crate::device::Device;
use crate::gaze::Et5Frame;
use crate::record::{BLACK, WHITE};
use crate::sweep::{
    FALLBACK_PX_PER_DEG, SweepError, SweepKey, desk_to_sensor, median, plane_corners,
    set_overlay_background, show_target,
};
use crate::ttp::DisplayArea;

/// Talon's training rectangle width, millimetres. Measured along the panel surface,
/// so a curved panel gets the same arc length a flat one would.
pub const AREA_W_MM: f64 = 600.0;

/// Talon's training rectangle height, millimetres.
pub const AREA_H_MM: f64 = 340.0;

/// Where the three columns and rows sit inside the training rectangle. Talon's
/// numbers: the outer points are inset by 5% so a saccade that overshoots the target
/// still lands on the panel.
const POINT_FRACTIONS: [f64; 3] = [0.05, 0.5, 0.95];

/// Frames the acceptance gate looks back over. Talon's window; at 133 Hz it is about
/// nine tenths of a second.
pub const GATE_WINDOW: usize = 120;

/// How many of [`GATE_WINDOW`] must name the target before it is accepted.
pub const GATE_MIN_HITS: usize = 60;

/// Wall-clock cap on the gate's window, seconds. Frames older than this are dropped
/// whatever the frame count says, so a dropout cannot leave stale samples voting.
pub const GATE_WINDOW_S: f64 = 2.0;

/// Default acceptance radius, degrees of visual angle from the nominal eye.
pub const ACCEPT_DEG: f64 = 3.0;

/// Default per-point patience, seconds. A point that has not tripped the gate by then
/// is skipped with a warning rather than blocking the ceremony.
pub const POINT_TIMEOUT_S: f64 = 15.0;

/// Pupil adaptation wait after a background change, seconds. Same value and reasoning
/// as `crate::record`: sized for dilation, which is the slow direction.
const ADAPT_S: f64 = 4.0;

/// Poll interval of the target loops. Short enough that the gate sees every frame the
/// device sends and the caption stays responsive.
const TICK: Duration = Duration::from_millis(8);

/// Settling time after the device takes a plane declaration, before its 2D output is
/// trusted to be on the new plane.
const PLANE_SETTLE: Duration = Duration::from_millis(200);

/// Health-check dwell per stop, seconds.
const HEALTH_DWELL_S: f64 = 1.0;

/// How much of each health dwell the median is taken over, seconds, counted back from
/// the end. The rest is the saccade and the lock-on.
const HEALTH_MEDIAN_S: f64 = 0.6;

/// Health-grid resolution per axis.
const HEALTH_STEPS: usize = 3;

/// Mid grey: the health check runs on neither pupil extreme, so its numbers describe
/// an ordinary screen rather than the training conditions.
const NEUTRAL: [u8; 4] = [128, 128, 128, 255];

/// Lag written into a fresh calibration file when there is no previous one to inherit
/// from, seconds. Mirrors the sweep's `LAG_DEFAULT_S`; the retrain does not measure
/// lag (that needs glides, which belong to a recording session).
pub const DEFAULT_LAG_S: f64 = 0.12;

// --- Configuration ---

/// What one retrain does. `Default` is the shipped ceremony.
#[derive(Clone, Debug)]
pub struct RetrainConfig {
    /// Connector name of the display the tracker is mounted on and whose plane is
    /// declared to the device.
    pub display           : String,
    /// Sensor-frame pitch from `desk.toml`, degrees. The plane is declared in the
    /// device's own tilted frame, so this is not optional.
    pub tracker_pitch_deg : f64,
    /// Training rectangle width, millimetres along the panel surface.
    pub area_w_mm         : f64,
    /// Training rectangle height, millimetres along the panel surface.
    pub area_h_mm         : f64,
    /// Train over the whole panel instead of the rectangle. The firmware is poor
    /// outside its envelope, so this is an experiment, not a better default.
    pub area_full         : bool,
    /// Acceptance radius, degrees.
    pub accept_deg        : f64,
    /// Per-point patience, seconds.
    pub point_timeout_s   : f64,
    /// Ask the device what point it would like after each round and log the answer.
    /// Exploratory: nothing depends on the reply.
    pub suggest           : bool,
}

impl Default for RetrainConfig {
    fn default() -> Self {
        Self {
            display           : "DP-1".into(),
            tracker_pitch_deg : 0.0,
            area_w_mm         : AREA_W_MM,
            area_h_mm         : AREA_H_MM,
            area_full         : false,
            accept_deg        : ACCEPT_DEG,
            point_timeout_s   : POINT_TIMEOUT_S,
            suggest           : false,
        }
    }
}

// --- Background ---

/// The overlay colour a round runs on. Two rounds of the same schedule on opposite
/// backgrounds walk the pupil across most of its range.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    /// Fully dark: the dilated end.
    Black,
    /// Fully bright: the constricted end.
    White,
}

impl Background {
    /// The colour the overlay paints.
    pub fn color(&self) -> [u8; 4] {
        match self {
            Self::Black => BLACK,
            Self::White => WHITE,
        }
    }

    /// The name used in captions and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::White => "white",
        }
    }
}

// --- Plan ---

/// One training target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct TrainPoint {
    /// Position across the panel, [0, 1]. This is exactly what `cal_add_point`
    /// receives: the firmware's normalised coordinates are the declared plane's.
    pub u     : f64,
    /// Position down the panel, [0, 1].
    pub v     : f64,
    /// The same position in global logical pixels, for the overlay.
    pub px    : GlobalPx,
    /// Human name, for captions and the dry run.
    pub label : &'static str,
}

/// One round: a set of targets fed to the device, then one `cal_points_apply`.
#[derive(Clone, Debug, PartialEq)]
pub struct Round {
    /// Round name, for captions and logs.
    pub name       : &'static str,
    /// The overlay background it runs on.
    pub background : Background,
    /// Indices into [`RetrainPlan::points`], in presentation order.
    pub points     : Vec<usize>,
}

/// The whole ceremony as data, computed without touching the device. `calibrate
/// --dry-run` prints this.
#[derive(Clone, Debug)]
pub struct RetrainPlan {
    /// The display plane declared to the device, sensor frame, millimetres.
    pub area     : DisplayArea,
    /// The training rectangle in panel uv, `(u_lo, u_hi, v_lo, v_hi)`.
    pub train_uv : (f64, f64, f64, f64),
    /// The nine targets, row-major from the top-left of the training rectangle.
    pub points   : Vec<TrainPoint>,
    /// The six rounds.
    pub rounds   : Vec<Round>,
    /// Acceptance tolerance in panel uv, per axis, from `accept_deg`.
    pub tol_uv   : (f64, f64),
}

// --- Gate ---

/// Thresholds the acceptance gate applies. Separated from the plan so the decision
/// itself has no geometry in it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Gate {
    /// How many trailing samples vote.
    pub window   : usize,
    /// How many of them must name the target.
    pub min_hits : usize,
    /// Acceptance radius across the panel, uv.
    pub tol_u    : f64,
    /// Acceptance radius down the panel, uv.
    pub tol_v    : f64,
    /// Panel width divided by height, so "nearest target" is decided in millimetres
    /// rather than in uv, where a horizontal unit is not a vertical one.
    pub aspect   : f64,
}

/// What the gate saw for one target at one moment.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Verdict {
    /// Samples in the window whose nearest target was this one.
    pub hits     : usize,
    /// Median of those samples, panel uv. `None` when there were none.
    pub median   : Option<[f64; 2]>,
    /// Both conditions met: enough hits, and their median close enough.
    pub accepted : bool,
}

/// Decides whether `targets[index]` may be fed to the device, given the trailing
/// window of the firmware's own reported gaze.
///
/// Two conditions, both of them Talon's in spirit: the target has to have been the
/// user's nearest for most of the window (which rejects "on the way there" and "just
/// left"), and the median of those samples has to be close to it (which rejects a
/// steady fixation on something else nearby, the failure a hit count alone cannot
/// see). The median rather than the mean because a blink recovery or a single
/// dropout frame is a wild value, not a small one.
///
/// Samples must already be in panel uv and valid; invalid frames are the caller's to
/// drop, since "no reading" is not the same as "a reading elsewhere" and must not be
/// allowed to satisfy the window.
pub fn gate_verdict(
    samples : &[[f64; 2]],
    targets : &[[f64; 2]],
    index   : usize,
    gate    : &Gate,
)
    -> Verdict
{
    let miss = Verdict { hits: 0, median: None, accepted: false };

    let Some(target) = targets.get(index).copied() else {
        return miss;
    };

    let window = &samples[samples.len().saturating_sub(gate.window)..];

    let mut us = Vec::with_capacity(window.len());
    let mut vs = Vec::with_capacity(window.len());

    for sample in window {
        if nearest(targets, *sample, gate.aspect) == Some(index) {
            us.push(sample[0]);
            vs.push(sample[1]);
        }
    }

    let hits = us.len();

    if hits == 0 {
        return miss;
    }

    let centre = [median(&mut us), median(&mut vs)];

    // A non-positive tolerance would either divide by zero or accept everything; it
    // means the caller has no scale for this panel, so nothing is accepted.
    let accepted = hits >= gate.min_hits
        && gate.tol_u > 0.0
        && gate.tol_v > 0.0
        && {
            let du = (centre[0] - target[0]) / gate.tol_u;
            let dv = (centre[1] - target[1]) / gate.tol_v;

            du * du + dv * dv <= 1.0
        };

    Verdict { hits: hits, median: Some(centre), accepted: accepted }
}

/// Index of the target nearest `sample`, comparing distances in millimetres via
/// `aspect`. `None` for an empty target list.
fn nearest(targets: &[[f64; 2]], sample: [f64; 2], aspect: f64) -> Option<usize> {
    let cost = |t: &[f64; 2]| {
        let du = (t[0] - sample[0]) * aspect;
        let dv = t[1] - sample[1];

        du * du + dv * dv
    };

    targets.iter()
        .enumerate()
        .min_by(|(_, a), (_, b)| cost(a).total_cmp(&cost(b)))
        .map(|(i, _)| i)
}

// --- Planning ---

/// Plans the ceremony for a display: the plane, the training rectangle, the nine
/// targets, and the six rounds.
///
/// Fails only when the display is missing from the desk config or disabled.
pub fn plan(geometry: &DesktopGeometry, config: &RetrainConfig)
    -> Result<RetrainPlan, SweepError>
{
    let out = geometry.outputs.iter()
        .find(|o| o.name == config.display && o.enabled)
        .ok_or(SweepError::NoDisplays)?;

    // The corners are derived in the desk frame; the device speaks its own frame,
    // pitched up by the mount wedge.
    let desk = plane_corners(out);
    let area = DisplayArea {
        tl_mm : desk_to_sensor(desk.tl_mm, config.tracker_pitch_deg),
        tr_mm : desk_to_sensor(desk.tr_mm, config.tracker_pitch_deg),
        bl_mm : desk_to_sensor(desk.bl_mm, config.tracker_pitch_deg),
    };

    let train_uv = training_rect(out, geometry.tracker(), config);
    let points   = grid_points(out, train_uv);
    let tol_uv   = accept_tolerance_uv(geometry, out, config.accept_deg);

    Ok(RetrainPlan {
        area     : area,
        train_uv : train_uv,
        points   : points,
        rounds   : rounds(),
        tol_uv   : tol_uv,
    })
}

/// The training rectangle in panel uv: Talon's rectangle, bottom-aligned to the panel
/// and horizontally centred on the tracker rather than on the panel.
///
/// Bottom-aligned because the tracker looks up at the face from under the bottom
/// bezel, so the bottom band of the panel is where its glints are healthiest.
/// Tracker-centred rather than panel-centred because on this desk the panel is offset
/// from the sensor by a couple of centimetres, and the point of the rectangle is to
/// describe the *sensor's* envelope.
fn training_rect(out: &OutputGeometry, tracker: DVec3, config: &RetrainConfig)
    -> (f64, f64, f64, f64)
{
    if config.area_full {
        return (0.0, 1.0, 0.0, 1.0);
    }

    // uv is linear in arc length along the panel, so a width in millimetres is a
    // fraction of the physical size whether the panel is flat or curved.
    let w = safe_fraction(config.area_w_mm, out.physical_w_mm);
    let h = safe_fraction(config.area_h_mm, out.physical_h_mm);

    let v_lo  = 1.0 - h;
    let u_mid = u_at_world_x(out, tracker.x, 1.0 - h * 0.5)
        .clamp(w * 0.5, 1.0 - w * 0.5);

    (u_mid - w * 0.5, u_mid + w * 0.5, v_lo, 1.0)
}

/// `want / have`, clamped into (0, 1]. A panel smaller than the requested rectangle
/// simply contributes all of itself.
fn safe_fraction(want_mm: f64, have_mm: f64) -> f64 {
    if have_mm <= 0.0 || !have_mm.is_finite() || !want_mm.is_finite() {
        return 1.0;
    }

    (want_mm / have_mm).clamp(1e-3, 1.0)
}

/// The `u` whose world point sits at `x_mm`, at height `v`, by bisection.
///
/// Bisection rather than algebra because `uv_to_world` carries the panel's yaw, roll
/// and curvature, and inverting that in closed form for one scalar is not worth the
/// second implementation. Forty halvings put the answer well past f64 relevance.
/// When the panel does not span `x_mm` at all, the nearer edge is returned.
fn u_at_world_x(out: &OutputGeometry, x_mm: f64, v: f64) -> f64 {
    let f  = |u: f64| out.uv_to_world(u, v).x - x_mm;
    let f0 = f(0.0);
    let f1 = f(1.0);

    if f0 == 0.0 {
        return 0.0;
    }

    if f0.signum() == f1.signum() {
        return if f0.abs() <= f1.abs() { 0.0 } else { 1.0 };
    }

    let (mut lo, mut hi) = (0.0_f64, 1.0_f64);

    for _ in 0..40 {
        let mid = 0.5 * (lo + hi);

        if f(mid).signum() == f0.signum() {
            lo = mid;
        }
        else {
            hi = mid;
        }
    }

    0.5 * (lo + hi)
}

/// The nine targets inside the training rectangle, row-major from its top-left.
fn grid_points(out: &OutputGeometry, uv: (f64, f64, f64, f64)) -> Vec<TrainPoint> {
    /// Point names, row-major, as they appear in captions and the dry run.
    const LABELS: [&str; 9] = [
        "top-left"   , "top-centre"   , "top-right",
        "left-centre", "centre"       , "right-centre",
        "bottom-left", "bottom-centre", "bottom-right",
    ];

    let (u_lo, u_hi, v_lo, v_hi) = uv;

    let mut points = Vec::with_capacity(9);

    for (row, fv) in POINT_FRACTIONS.iter().enumerate() {
        for (col, fu) in POINT_FRACTIONS.iter().enumerate() {
            let u = u_lo + (u_hi - u_lo) * fu;
            let v = v_lo + (v_hi - v_lo) * fv;

            points.push(TrainPoint {
                u     : u,
                v     : v,
                px    : out.uv_to_px(u, v),
                label : LABELS[row * 3 + col],
            });
        }
    }

    points
}

/// The six rounds: Talon's three schedules on black, then the same three on white.
///
/// Small rounds with a commit after each are what Talon does and what the firmware's
/// own fit expects: `cal_points_apply` folds the points collected since the last
/// apply into the model, so a bad point poisons one round rather than the ceremony,
/// and the later rounds are collected through an already-improving model.
fn rounds() -> Vec<Round> {
    let centre = vec![4];
    // Bottom, left, right, top — Talon's order, which keeps consecutive targets far
    // apart so a lingering fixation cannot satisfy the next point.
    let edges  = vec![7, 3, 5, 1];
    let corner = vec![0, 8, 6, 2];

    let mut rounds = Vec::with_capacity(6);

    for background in [Background::Black, Background::White] {
        rounds.push(Round {
            name       : "centre",
            background : background,
            points     : centre.clone(),
        });
        rounds.push(Round {
            name       : "mid-edges",
            background : background,
            points     : edges.clone(),
        });
        rounds.push(Round {
            name       : "corners",
            background : background,
            points     : corner.clone(),
        });
    }

    rounds
}

/// The acceptance radius in panel uv, per axis, from an angle at the nominal eye.
fn accept_tolerance_uv(geometry: &DesktopGeometry, out: &OutputGeometry, accept_deg: f64)
    -> (f64, f64)
{
    let (h, v) = geometry.px_per_deg(geometry.eye(), out.uv_to_px(0.5, 0.5))
        .unwrap_or((FALLBACK_PX_PER_DEG, FALLBACK_PX_PER_DEG));

    (accept_deg * h / out.logical_w, accept_deg * v / out.logical_h)
}

// --- Outcome ---

/// What happened at one target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointResult {
    /// Index into [`RetrainPlan::points`].
    pub index    : usize,
    /// Whether `cal_add_point` was sent for it.
    pub accepted : bool,
    /// Window samples naming this target when the decision was taken.
    pub hits     : usize,
    /// How long the target was shown, seconds.
    pub wait_s   : f64,
    /// True when the operator overrode the gate with Enter.
    pub forced   : bool,
}

/// A completed retrain.
#[derive(Clone, Debug)]
pub struct RetrainOutcome {
    /// The freshly committed on-device model.
    pub blob        : Vec<u8>,
    /// SHA-256 of that blob's body, which is the model's identity everywhere: the
    /// calibration file, session files, the history key. The whole-blob hash is not,
    /// because the result trailer is re-normalised on every retrieve (see `blob`).
    pub body_sha256 : String,
    /// The firmware's own per-point report, decoded off the blob's trailer. `None`
    /// only if the firmware stopped writing one. Normalised against the plane
    /// declared for the ceremony, which is `RetrainPlan::area`.
    pub result      : Option<CalibrationResult>,
    /// Every target, in ceremony order.
    pub results     : Vec<PointResult>,
    /// Targets actually fed to the device.
    pub accepted    : usize,
    /// Rounds that ended in a `cal_points_apply`.
    pub applied     : usize,
    /// Raw `CALIBRATE_GET_POINT_SUGGESTION` replies, one line per round, when
    /// `--suggest` was on.
    pub suggestions : Vec<String>,
}

// --- The ceremony ---

/// Runs the whole ceremony and returns the committed blob.
///
/// Declares the plane, opens one calibration session, walks the six rounds, and
/// closes with `cal_stop` + `cal_retrieve`. `q` at any point aborts: the session is
/// closed and the error propagates, so the caller writes nothing — an aborted retrain
/// leaves the device holding whatever the applied rounds taught it, which is why the
/// caller must not treat a partial ceremony as a success.
pub fn run_retrain(
    device   : &mut Device,
    overlay  : &OverlayHandle,
    keys     : Option<&Receiver<SweepKey>>,
    config   : &RetrainConfig,
    plan     : &RetrainPlan,
)
    -> Result<RetrainOutcome, SweepError>
{
    let gate = Gate {
        window   : GATE_WINDOW,
        min_hits : GATE_MIN_HITS,
        tol_u    : plan.tol_uv.0,
        tol_v    : plan.tol_uv.1,
        aspect   : aspect_of(plan),
    };

    info!("retrain plane (desk config, sensor frame): tl=({:.0},{:.0},{:.0}) \
           tr=({:.0},{:.0},{:.0}) bl=({:.0},{:.0},{:.0})",
          plan.area.tl_mm[0], plan.area.tl_mm[1], plan.area.tl_mm[2],
          plan.area.tr_mm[0], plan.area.tr_mm[1], plan.area.tr_mm[2],
          plan.area.bl_mm[0], plan.area.bl_mm[1], plan.area.bl_mm[2]);

    device.set_display_area_corners(plan.area).map_err(SweepError::Device)?;
    std::thread::sleep(PLANE_SETTLE);

    device.cal_begin().map_err(SweepError::Device)?;

    let frames_rx = device.gaze_stream();

    let mut results     = Vec::new();
    let mut suggestions = Vec::new();
    let mut applied     = 0usize;
    let mut background  = None;

    let total_rounds = plan.rounds.len();

    for (number, round) in plan.rounds.iter().enumerate() {
        let result = run_round(device, overlay, keys, config, plan, &gate, &frames_rx,
                               round, number + 1, total_rounds, &mut background,
                               &mut results, &mut suggestions);

        match result {
            Ok(true)  => applied += 1,
            Ok(false) => {}
            Err(e)    => {
                // Close the session whatever went wrong: a device left in calibration
                // mode streams as if nothing happened and quietly discards points.
                if let Err(stop) = device.cal_stop() {
                    warn!("could not close the calibration session after the abort: {stop}");
                }

                set_overlay_background(None);
                let _ = overlay.set(OverlayState::default());

                return Err(e);
            }
        }
    }

    let accepted = results.iter().filter(|r| r.accepted).count();

    if accepted == 0 {
        let _ = device.cal_stop();
        set_overlay_background(None);
        let _ = overlay.set(OverlayState::default());

        return Err(SweepError::NothingFitted);
    }

    let blob = device.cal_end().map_err(SweepError::Device)?;

    set_overlay_background(None);
    let _ = overlay.set(OverlayState::default());

    info!("retrain committed: {accepted}/{} points over {applied} applied rounds, \
           {} byte blob", results.len(), blob.len());

    Ok(RetrainOutcome {
        body_sha256 : body_sha256_hex(&blob),
        result      : decode_trailer(&blob).map(|(_, table)| table),
        blob        : blob,
        results     : results,
        accepted    : accepted,
        applied     : applied,
        suggestions : suggestions,
    })
}

/// Runs one round. Returns whether it ended in a `cal_points_apply`.
///
/// The gate compares against every target of the round, including ones already added
/// or skipped: a target that has had its turn still competes for "nearest", which can
/// only make a later point harder to accept, never easier.
#[allow(clippy::too_many_arguments)]
fn run_round(
    device      : &mut Device,
    overlay     : &OverlayHandle,
    keys        : Option<&Receiver<SweepKey>>,
    config      : &RetrainConfig,
    plan        : &RetrainPlan,
    gate        : &Gate,
    frames_rx   : &Receiver<Et5Frame>,
    round       : &Round,
    number      : usize,
    total       : usize,
    background  : &mut Option<Background>,
    results     : &mut Vec<PointResult>,
    suggestions : &mut Vec<String>,
)
    -> Result<bool, SweepError>
{
    // The pupil is still moving for seconds after a flip; a round collected during
    // that would be labelled with an illumination the eye had not reached.
    if *background != Some(round.background) {
        *background = Some(round.background);
        set_overlay_background(Some(round.background.color()));

        // The centre point if the plan has one, otherwise wherever the round starts:
        // the adaptation dot only has to give the eye something to hold.
        let anchor = plan.points.get(4)
            .or_else(|| round.points.first().and_then(|i| plan.points.get(*i)))
            .ok_or(SweepError::NoDisplays)?;
        let label  = format!("adapting to {} — eyes on the dot",
                             round.background.name());

        show_target(overlay, anchor.px, &label)?;
        wait(ADAPT_S, frames_rx, keys)?;
    }

    let targets: Vec<[f64; 2]> = round.points.iter()
        .map(|i| [plan.points[*i].u, plan.points[*i].v])
        .collect();

    let mut added = 0usize;

    for (position, index) in round.points.iter().copied().enumerate() {
        let point = plan.points[index];
        let label = format!("round {number}/{total} {} ({}) — {} {}/{}: eyes on the dot",
                            round.name, round.background.name(), point.label,
                            position + 1, round.points.len());

        show_target(overlay, point.px, &label)?;

        let result = run_point(overlay, keys, config, gate, frames_rx, &targets,
                               position, index, point.px, &label)?;

        if result.accepted {
            device.cal_add_point(point.u, point.v, 3).map_err(SweepError::Device)?;
            added += 1;

            info!("round {number} {}: added {} at uv ({:.3}, {:.3}) after {:.1}s \
                   ({} hits{})",
                  round.name, point.label, point.u, point.v, result.wait_s,
                  result.hits, if result.forced { ", forced" } else { "" });
        }
        else {
            warn!("round {number} {}: skipped {} at uv ({:.3}, {:.3}) after {:.1}s \
                   ({} hits of {} needed)",
                  round.name, point.label, point.u, point.v, result.wait_s,
                  result.hits, gate.min_hits);
        }

        results.push(result);
    }

    if added == 0 {
        warn!("round {number} added no points; nothing to apply");

        return Ok(false);
    }

    // Fold this round into the on-device model before the next one is collected.
    device.cal_points_apply().map_err(SweepError::Device)?;
    info!("round {number}/{total} applied ({added} points)");

    if config.suggest {
        suggestions.push(suggestion_line(device, number));
    }

    Ok(true)
}

/// Shows one target until the gate accepts it, the operator forces or skips it, or
/// the patience runs out.
#[allow(clippy::too_many_arguments)]
fn run_point(
    overlay   : &OverlayHandle,
    keys      : Option<&Receiver<SweepKey>>,
    config    : &RetrainConfig,
    gate      : &Gate,
    frames_rx : &Receiver<Et5Frame>,
    targets   : &[[f64; 2]],
    position  : usize,
    index     : usize,
    px        : GlobalPx,
    label     : &str,
)
    -> Result<PointResult, SweepError>
{
    let start = Instant::now();

    // Anything queued from the previous target describes the previous target.
    while frames_rx.try_recv().is_ok() {}

    let mut times  : Vec<f64>      = Vec::new();
    let mut samples: Vec<[f64; 2]> = Vec::new();
    let mut forced = false;

    loop {
        if let Some(keys) = keys {
            match keys.try_recv() {
                Ok(SweepKey::Quit)    => return Err(SweepError::Aborted),
                Ok(SweepKey::Skip)    => {
                    return Ok(PointResult {
                        index    : index,
                        accepted : false,
                        hits     : 0,
                        wait_s   : start.elapsed().as_secs_f64(),
                        forced   : false,
                    });
                }
                // The operator can see the dot and the user; when the gate will not
                // trip but the fixation is plainly good, Enter takes the point.
                Ok(SweepKey::Advance) => forced = true,
                Err(_)                => {}
            }
        }

        let now = start.elapsed().as_secs_f64();

        while let Ok(frame) = frames_rx.try_recv() {
            if let Some(uv) = valid_uv(&frame) {
                times.push(now);
                samples.push(uv);
            }
        }

        // Drop everything older than the wall-clock window, so a dropout cannot leave
        // a stale majority in place.
        let keep = times.iter().position(|t| now - t <= GATE_WINDOW_S)
            .unwrap_or(times.len());

        times.drain(..keep);
        samples.drain(..keep);

        let verdict = gate_verdict(&samples, targets, position, gate);

        if verdict.accepted || forced {
            return Ok(PointResult {
                index    : index,
                accepted : true,
                hits     : verdict.hits,
                wait_s   : now,
                forced   : forced,
            });
        }

        if now >= config.point_timeout_s {
            return Ok(PointResult {
                index    : index,
                accepted : false,
                hits     : verdict.hits,
                wait_s   : now,
                forced   : false,
            });
        }

        // Keep the caption alive with the gate's own progress: a user who cannot tell
        // whether the tracker sees them has no way to fix their posture.
        let live = format!("{label} [{}/{}]", verdict.hits, gate.min_hits);
        show_target(overlay, px, &live)?;

        std::thread::sleep(TICK);
    }
}

/// Queries the point suggestion and renders the reply for the log and the report.
fn suggestion_line(device: &mut Device, round: usize) -> String {
    match device.cal_point_suggestion() {
        Ok(payload) => {
            let decoded = crate::ttp::decode_point_suggestion(&payload);
            let points  = decoded.iter()
                .map(|p| format!("({:.4}, {:.4})", p[0], p[1]))
                .collect::<Vec<_>>()
                .join(" ");

            let line = format!(
                "round {round}: {} bytes {}{}",
                payload.len(),
                hex::encode(&payload),
                if points.is_empty() { String::new() } else { format!(" -> {points}") },
            );

            info!("point suggestion {line}");

            line
        }
        Err(e)      => {
            let line = format!("round {round}: query failed ({e})");
            warn!("point suggestion {line}");

            line
        }
    }
}

// --- Health check ---

/// Reads the freshly committed model back: a 3x3 grid over the training area on a
/// neutral background, one second each, the firmware's own gaze against the target.
///
/// This fits nothing and corrects nothing. It exists because a blob with no number
/// attached is unfalsifiable, and because the same numbers on the next retrain are
/// the only honest way to tell whether a change helped.
pub fn run_health(
    device   : &mut Device,
    geometry : &DesktopGeometry,
    overlay  : &OverlayHandle,
    keys     : Option<&Receiver<SweepKey>>,
    config   : &RetrainConfig,
    plan     : &RetrainPlan,
)
    -> Result<Vec<HealthStop>, SweepError>
{
    let out = geometry.outputs.iter()
        .find(|o| o.name == config.display && o.enabled)
        .ok_or(SweepError::NoDisplays)?;

    let eye        = geometry.eye();
    let frames_rx  = device.gaze_stream();
    let (u_lo, u_hi, v_lo, v_hi) = plan.train_uv;

    set_overlay_background(Some(NEUTRAL));

    let mut stops = Vec::with_capacity(HEALTH_STEPS * HEALTH_STEPS);

    for row in 0..HEALTH_STEPS {
        for col in 0..HEALTH_STEPS {
            let fu = col as f64 / (HEALTH_STEPS - 1) as f64;
            let fv = row as f64 / (HEALTH_STEPS - 1) as f64;

            // The same 5%..95% inset as the training points: a stop on the very edge
            // of the rectangle measures the overshoot, not the model.
            let u = u_lo + (u_hi - u_lo) * (POINT_FRACTIONS[0]
                + (POINT_FRACTIONS[2] - POINT_FRACTIONS[0]) * fu);
            let v = v_lo + (v_hi - v_lo) * (POINT_FRACTIONS[0]
                + (POINT_FRACTIONS[2] - POINT_FRACTIONS[0]) * fv);

            let px    = out.uv_to_px(u, v);
            let index = row * HEALTH_STEPS + col + 1;
            let label = format!("health check {index}/{} — eyes on the dot",
                                HEALTH_STEPS * HEALTH_STEPS);

            show_target(overlay, px, &label)?;

            while frames_rx.try_recv().is_ok() {}

            let (mut us, mut vs) = (Vec::new(), Vec::new());
            let start = Instant::now();

            while start.elapsed().as_secs_f64() < HEALTH_DWELL_S {
                if let Some(keys) = keys
                    && matches!(keys.try_recv(), Ok(SweepKey::Quit))
                {
                    set_overlay_background(None);

                    return Err(SweepError::Aborted);
                }

                let t = start.elapsed().as_secs_f64();

                while let Ok(frame) = frames_rx.try_recv() {
                    if let Some(uv) = valid_uv(&frame)
                        && t >= HEALTH_DWELL_S - HEALTH_MEDIAN_S
                    {
                        us.push(uv[0]);
                        vs.push(uv[1]);
                    }
                }

                std::thread::sleep(TICK);
            }

            let samples = us.len();

            if samples == 0 {
                warn!("health stop {index} at uv ({u:.3}, {v:.3}): no valid gaze");

                continue;
            }

            let gu = median(&mut us);
            let gv = median(&mut vs);

            let want = out.uv_to_world(u, v) - eye;
            let got  = out.uv_to_world(gu, gv) - eye;

            stops.push(HealthStop {
                u         : u,
                v         : v,
                gaze_u    : gu,
                gaze_v    : gv,
                error_deg : want.angle_between(got).to_degrees(),
                samples   : samples,
            });
        }
    }

    set_overlay_background(None);
    let _ = overlay.set(OverlayState::default());

    Ok(stops)
}

/// RMS and median of a health pass, degrees. `None` for an empty pass.
pub fn health_summary(stops: &[HealthStop]) -> Option<(f64, f64)> {
    if stops.is_empty() {
        return None;
    }

    let mut errors: Vec<f64> = stops.iter().map(|s| s.error_deg).collect();
    let rms = (errors.iter().map(|e| e * e).sum::<f64>() / errors.len() as f64).sqrt();

    Some((rms, median(&mut errors)))
}

// --- Files ---

/// Moves `path` aside as `<path>.prev-<unix>` before it is overwritten, returning
/// where it went. Nothing to keep is not an error.
///
/// A retrain replaces the one artefact the whole system is keyed to; silently
/// overwriting it would make "put yesterday's model back" impossible.
pub fn keep_previous(path: &Path) -> std::io::Result<Option<PathBuf>> {
    if !path.exists() {
        return Ok(None);
    }

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    let mut name = path.as_os_str().to_os_string();
    name.push(format!(".prev-{stamp}"));

    let kept = PathBuf::from(name);
    std::fs::rename(path, &kept)?;

    Ok(Some(kept))
}

// --- Helpers ---

/// The panel's uv aspect: how many vertical uv units one horizontal unit is worth in
/// millimetres.
fn aspect_of(plan: &RetrainPlan) -> f64 {
    // Derived from the declared plane rather than the config so it describes the same
    // surface the firmware normalises against.
    let tl = DVec3::from_array(plan.area.tl_mm);
    let tr = DVec3::from_array(plan.area.tr_mm);
    let bl = DVec3::from_array(plan.area.bl_mm);

    let h = tl.distance(bl);

    if h <= 0.0 { 1.0 } else { tl.distance(tr) / h }
}

/// The firmware's combined 2D gaze as panel uv, when it is a real reading.
///
/// The device reports (-1, -1) for "no combined gaze" and clamps to the plane bounds
/// otherwise, so an exact 0 or 1 is a direction that was thrown away, not a direction
/// at the edge; both are dropped rather than allowed to vote in the gate.
fn valid_uv(frame: &Et5Frame) -> Option<[f64; 2]> {
    let [u, v] = frame.gaze_2d_norm?;

    ((0.0..1.0).contains(&u) && (0.0..1.0).contains(&v)).then_some([u, v])
}

/// Sleeps for `duration_s` in ticks, honouring the abort key and keeping the gaze
/// queue drained (a full queue would otherwise hand the next target stale frames).
fn wait(duration_s: f64, frames_rx: &Receiver<Et5Frame>, keys: Option<&Receiver<SweepKey>>)
    -> Result<(), SweepError>
{
    let start = Instant::now();

    while start.elapsed().as_secs_f64() < duration_s {
        while frames_rx.try_recv().is_ok() {}

        if let Some(keys) = keys {
            match keys.try_recv() {
                Ok(SweepKey::Quit)                     => return Err(SweepError::Aborted),
                Ok(SweepKey::Advance | SweepKey::Skip) => return Ok(()),
                Err(_)                                 => {}
            }
        }

        std::thread::sleep(TICK);
    }

    Ok(())
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The real desk, so the plan is exercised against the geometry it was sized for.
    fn desk() -> DesktopGeometry {
        DesktopGeometry::from_toml(
            &std::fs::read_to_string("../../config/desk.toml").expect("desk config"),
        ).expect("desk config parses")
    }

    /// The shipped ceremony on this desk, mount pitch included.
    fn config() -> RetrainConfig {
        RetrainConfig { tracker_pitch_deg: 13.0, ..RetrainConfig::default() }
    }

    /// A gate with generous tolerances, for the tests that are about hit counting.
    fn loose_gate() -> Gate {
        Gate {
            window   : 120,
            min_hits : 60,
            tol_u    : 0.05,
            tol_v    : 0.05,
            aspect   : 1.0,
        }
    }

    #[test]
    fn the_plane_carries_the_mount_pitch() {
        let geometry = desk();
        let flat     = plan(&geometry, &RetrainConfig::default()).expect("plan");
        let pitched  = plan(&geometry, &config()).expect("plan");

        // The whole point of the 2026-08-28 fix: a non-zero pitch must move the
        // declared corners out of the desk frame.
        assert_ne!(flat.area.tl_mm, pitched.area.tl_mm);

        let expected = desk_to_sensor(flat.area.tl_mm, 13.0);
        assert_eq!(pitched.area.tl_mm, expected);
    }

    #[test]
    fn the_training_area_is_talons_rectangle_at_the_bottom() {
        let geometry = desk();
        let plan     = plan(&geometry, &config()).expect("plan");
        let out      = geometry.outputs.iter().find(|o| o.name == "DP-1").expect("DP-1");

        let (u_lo, u_hi, v_lo, v_hi) = plan.train_uv;

        assert!((v_hi - 1.0).abs() < 1e-12, "bottom aligned: {v_hi}");
        assert!(((v_hi - v_lo) * out.physical_h_mm - AREA_H_MM).abs() < 1e-6);
        assert!(((u_hi - u_lo) * out.physical_w_mm - AREA_W_MM).abs() < 1e-6);

        // Centred on the tracker (world x = 0), not on the panel: DP-1 sits 25 mm to
        // the right of the sensor, so the window is left of the panel's middle.
        let mid = 0.5 * (u_lo + u_hi);
        assert!(mid < 0.5, "the window is left of the panel centre: {mid:.4}");
        assert!(out.uv_to_world(mid, 0.5 * (v_lo + v_hi)).x.abs() < 1.0,
                "the window's middle sits over the tracker");
    }

    #[test]
    fn the_full_area_option_covers_the_panel() {
        let geometry = desk();
        let plan     = plan(&geometry, &RetrainConfig {
            area_full : true,
            ..config()
        }).expect("plan");

        assert_eq!(plan.train_uv, (0.0, 1.0, 0.0, 1.0));

        // The inset still keeps every target off the bezel.
        for point in &plan.points {
            assert!(point.u >= 0.05 - 1e-12 && point.u <= 0.95 + 1e-12);
            assert!(point.v >= 0.05 - 1e-12 && point.v <= 0.95 + 1e-12);
        }
    }

    #[test]
    fn the_schedule_is_eighteen_points_in_six_rounds() {
        let geometry = desk();
        let plan     = plan(&geometry, &config()).expect("plan");

        assert_eq!(plan.points.len(), 9);
        assert_eq!(plan.rounds.len(), 6);
        assert_eq!(plan.rounds.iter().map(|r| r.points.len()).sum::<usize>(), 18);

        // Centre first, then the mid-edges, then the corners, on each background.
        assert_eq!(plan.rounds[0].points, vec![4]);
        assert_eq!(plan.rounds[1].points, vec![7, 3, 5, 1]);
        assert_eq!(plan.rounds[2].points, vec![0, 8, 6, 2]);

        for round in &plan.rounds[..3] {
            assert_eq!(round.background, Background::Black);
        }

        for round in &plan.rounds[3..] {
            assert_eq!(round.background, Background::White);
        }

        // Every point is used exactly twice over the ceremony.
        for index in 0..9 {
            let uses = plan.rounds.iter()
                .flat_map(|r| r.points.iter())
                .filter(|i| **i == index)
                .count();

            assert_eq!(uses, 2, "point {index} appears {uses} times");
        }
    }

    #[test]
    fn a_steady_fixation_on_the_target_is_accepted() {
        let targets = [[0.2, 0.2], [0.8, 0.8]];
        let samples: Vec<[f64; 2]> = (0..120)
            .map(|i| [0.2 + (i % 3) as f64 * 0.001, 0.2 - (i % 5) as f64 * 0.001])
            .collect();

        let verdict = gate_verdict(&samples, &targets, 0, &loose_gate());

        assert!(verdict.accepted);
        assert_eq!(verdict.hits, 120);
    }

    #[test]
    fn too_few_hits_is_refused_however_good_they_are() {
        let targets = [[0.2, 0.2], [0.8, 0.8]];

        // Fifty perfect samples on the target, seventy on the other one.
        let mut samples = vec![[0.8, 0.8]; 70];
        samples.extend(std::iter::repeat_n([0.2, 0.2], 50));

        let verdict = gate_verdict(&samples, &targets, 0, &loose_gate());

        assert_eq!(verdict.hits, 50);
        assert!(!verdict.accepted);
    }

    #[test]
    fn a_near_miss_is_refused_even_with_every_frame() {
        // Every sample names the target (it is much nearer than the other one) but
        // the fixation sits a long way off it: the hit count alone cannot see this,
        // which is why the median test exists.
        let targets = [[0.2, 0.2], [0.8, 0.8]];
        let samples = vec![[0.35, 0.35]; 120];

        let verdict = gate_verdict(&samples, &targets, 0, &loose_gate());

        assert_eq!(verdict.hits, 120);
        assert!(!verdict.accepted);
        assert_eq!(verdict.median, Some([0.35, 0.35]));
    }

    #[test]
    fn only_the_last_window_votes() {
        let targets = [[0.2, 0.2], [0.8, 0.8]];

        // A long stale run on the target followed by a full window elsewhere.
        let mut samples = vec![[0.2, 0.2]; 500];
        samples.extend(std::iter::repeat_n([0.8, 0.8], 120));

        assert!(!gate_verdict(&samples, &targets, 0, &loose_gate()).accepted);
        assert!(gate_verdict(&samples, &targets, 1, &loose_gate()).accepted);
    }

    #[test]
    fn nearest_is_measured_in_millimetres_not_uv() {
        // A target 0.05 to the right and one 0.08 below. In raw uv the right-hand one
        // is nearer; on a 880x370 panel a horizontal uv unit is 2.4 vertical ones, so
        // in millimetres — which is what the eye moved — the lower one is.
        let targets = [[0.60, 0.55], [0.55, 0.63]];
        let sample  = [0.55, 0.55];

        assert_eq!(nearest(&targets, sample, 1.0), Some(0));
        assert_eq!(nearest(&targets, sample, 880.0 / 370.0), Some(1));
        assert_eq!(nearest(&[], sample, 1.0), None);
    }

    #[test]
    fn an_empty_window_is_never_accepted() {
        let targets = [[0.2, 0.2]];
        let verdict = gate_verdict(&[], &targets, 0, &loose_gate());

        assert_eq!(verdict, Verdict { hits: 0, median: None, accepted: false });

        // So is an index nobody planned.
        assert!(!gate_verdict(&[[0.2, 0.2]; 120], &targets, 7, &loose_gate()).accepted);
    }

    #[test]
    fn a_degenerate_tolerance_accepts_nothing() {
        let targets = [[0.2, 0.2]];
        let gate    = Gate { tol_u: 0.0, ..loose_gate() };

        assert!(!gate_verdict(&[[0.2, 0.2]; 120], &targets, 0, &gate).accepted);
    }

    #[test]
    fn the_acceptance_radius_is_a_real_angle() {
        let geometry = desk();
        let plan     = plan(&geometry, &config()).expect("plan");

        // Three degrees on a 3840 px wide, 880 mm panel at ~690 mm: a few percent of
        // the panel, not a pixel and not half a screen.
        assert!(plan.tol_uv.0 > 0.01 && plan.tol_uv.0 < 0.15, "{:?}", plan.tol_uv);
        assert!(plan.tol_uv.1 > 0.02 && plan.tol_uv.1 < 0.40, "{:?}", plan.tol_uv);
    }

    #[test]
    fn the_health_summary_is_rms_and_median() {
        let stop = |deg: f64| HealthStop {
            u         : 0.5,
            v         : 0.5,
            gaze_u    : 0.5,
            gaze_v    : 0.5,
            error_deg : deg,
            samples   : 60,
        };

        let (rms, p50) = health_summary(&[stop(1.0), stop(2.0), stop(3.0)])
            .expect("three stops");

        assert!((rms - (14.0_f64 / 3.0).sqrt()).abs() < 1e-12);
        assert!((p50 - 2.0).abs() < 1e-12);
        assert!(health_summary(&[]).is_none());
    }

    #[test]
    fn a_backup_is_kept_before_an_overwrite() {
        let dir = std::env::temp_dir().join("gaze-et5-retrain-test");
        std::fs::create_dir_all(&dir).expect("temp dir");

        let path = dir.join("blob.bin");
        std::fs::write(&path, b"old").expect("write");

        let kept = keep_previous(&path).expect("keep").expect("something to keep");

        assert!(!path.exists(), "the original was moved aside");
        assert_eq!(std::fs::read(&kept).expect("read"), b"old");
        assert!(kept.to_string_lossy().contains(".prev-"));

        std::fs::remove_file(&kept).ok();
        assert_eq!(keep_previous(&path).expect("keep"), None);
    }
}
