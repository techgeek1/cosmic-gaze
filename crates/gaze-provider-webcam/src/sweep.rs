//! The calibration sweep: show a target, wait for the user to look at it, keep the rays
//! that arrived while they were, and fit `crate::calibration`'s two stages to the result.
//!
//! # Why the sweep needs an uncalibrated provider
//!
//! Every observation is a *raw* ray from the sidecar. Running the sweep against a
//! provider that already has a calibration loaded would fit a correction on top of a
//! correction, and the file it wrote would only be valid when applied twice. `run` takes
//! whatever provider it is given and reads `GazeSample::ray`, so the caller is responsible
//! for handing it one built with `calibration(None)`.
//!
//! # The collection window
//!
//! Each target is shown for `dwell_s` but only the last `collect_s` is kept. The first
//! part is the saccade plus the moment it takes a person to actually settle on a small
//! square, and samples from it are aimed somewhere between the previous target and this
//! one. Keeping them would drag every observation toward the centre of the screen.
//!
//! # Advancing
//!
//! A target advances on its dwell timeout, or early when the user presses Enter. Typing
//! `s` and Enter skips a target the user cannot comfortably look at (behind a bezel, on a
//! panel that has dropped off the compositor's output list), and a skipped target
//! contributes nothing to the fit rather than contributing a wrong observation.

use std::io::{BufRead, IsTerminal};
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, TryRecvError};
use serde::{Deserialize, Serialize};
use gaze_core::{DesktopGeometry, GlobalPx, Ray, Rect};
use gaze_overlay::{OverlayHandle, OverlayState};
use glam::DVec3;

use crate::calibration::{
    CALIBRATION_FORMAT, Calibration, OutputCalibration, OutputGain, TargetDiagnostics,
    TargetResidual, normalise, resolve,
};
use crate::angle::{self, Shape};
use crate::camera::{CameraPose, gaze_yaw_pitch_deg};
use crate::fit::{AngleRow, PolyMap};
use crate::provider::{RawGaze, Reading};

/// Side of the highlighted square drawn at each target, logical pixels.
pub const TARGET_PX: f64 = 40.0;

/// How often the collection loop wakes to drain samples and check for a keypress.
const POLL: Duration = Duration::from_millis(5);

/// Fallback pixels per degree when the geometry cannot supply a local scale. Matches the
/// figure `gaze-snap` falls back to.
const FALLBACK_PX_PER_DEG: f64 = 60.0;

/// Smallest local gain a fitted correction may have anywhere in its working range. Below
/// this the correction has flattened, and a whole region of the screen collapses onto a
/// point.
const MIN_GAIN: f64 = 0.25;

/// Largest local gain a fitted correction may have anywhere in its working range. Above
/// this the correction is more than doubling the reported angle, which says the model is
/// reporting under half the truth there and is past what a polynomial can rescue.
///
/// The band is where the measurements put it. On a real sweep the kept quadratic ran 0.49
/// to 1.17 and behaved; the cubic ran 0.35 to 3.05 and put the pointer at half the distance
/// to the panel edges; against a synthetic model with a 3x saturating gain the quadratic's
/// gain goes negative, which is a fold and is refused by the lower bound.
const MAX_GAIN: f64 = 2.5;

/// Largest ratio between the strongest and weakest local area scale of a per-output pixel
/// map. Looser than the angle gate because this stage only ever adjusts within one panel,
/// and it is measured on an area rather than a length.
const MAX_PIXEL_JACOBIAN_RATIO: f64 = 6.0;

/// Fraction the observed angle range is widened by before the sanity check runs, so a model
/// is judged slightly outside the targets it was fitted on. The user will look there.
const RANGE_MARGIN: f64 = 0.15;

/// Points per axis in the grid the gain check is measured on.
const GAIN_GRID: usize = 11;

/// How far a grid point may sit from the nearest calibration target and still be judged,
/// degrees.
///
/// The targets occupy a diagonal band through the yaw-pitch rectangle rather than filling
/// it, because looking left also means looking at a different height on this desk. Judging
/// a correction in the empty corners measures nothing but extrapolation into places nobody
/// looks, and would reject good fits for it.
const GAIN_REACH_DEG: f64 = 12.0;

/// Called as each target goes up. Boxed rather than generic so `CalibrationSweep` stays a
/// concrete type the CLI can hold without threading a parameter through everything.
pub type TargetHook = Box<dyn FnMut(usize, &SweepTarget)>;

/// One `(observed, target)` pair in an output's normalised coordinates, as `PolyMap::fit`
/// wants them.
type SamplePair = ([f64; 2], [f64; 2]);

/// One place the user is asked to look.
#[derive(Clone, Debug, PartialEq)]
pub struct SweepTarget {
    /// Output the target is drawn on.
    pub output : String,
    /// Where it is drawn, global logical pixels.
    pub px     : GlobalPx,
}

/// One valid sample, reduced to the quantities the angle fit works in.
///
/// `want_*` is computed from **this sample's own** reported eye position, not from a
/// nominal one: the head moves during a sweep, and the direction a target calls for moves
/// with it. Using one averaged eye for the whole sweep would fold that movement into the
/// correction as if it were model error.
#[derive(Clone, Debug, PartialEq)]
pub struct AngleSample {
    /// Reported eye position, camera frame, millimetres.
    pub eye_cam_mm     : DVec3,
    /// Reported gaze direction, camera frame.
    pub gaze_cam       : DVec3,
    /// Reported angles, degrees.
    pub yaw_deg        : f64,
    pub pitch_deg      : f64,
    /// Angles the target called for from this sample's eye, degrees.
    pub want_yaw_deg   : f64,
    pub want_pitch_deg : f64,
    /// True when the raw ray hit no panel. Recorded, never acted on: see `resolve`.
    pub missed         : bool,
    /// Head rotation as the sidecar reported it, uninterpreted. Kept because an
    /// appearance model's error is a function of head pose as much as of gaze angle, and
    /// that is not something you can go back and measure after the sweep.
    pub head_rot       : Option<[f64; 3]>,
    /// The verbatim sidecar line this sample came from. See `Reading::raw`.
    pub raw            : Option<std::sync::Arc<str>>,
}

/// Everything one target contributed. A skipped target produces no observation at all.
#[derive(Clone, Debug)]
pub struct Observation {
    pub target      : SweepTarget,
    /// Every valid sample in the collection window, for the angle fit.
    pub samples     : Vec<AngleSample>,
    /// Mean ray over the collection window: mean origin, mean direction renormalised.
    pub mean_ray    : Ray,
    /// Mean uncorrected landing point, or `None` when every sample missed the desk. Not
    /// clamped, so a target the model could not reach reports nothing rather than a bezel.
    pub observed_px : Option<GlobalPx>,
    /// RMS distance of the landing points from their mean, for the samples that landed.
    pub spread_px   : Option<f64>,
    /// RMS angle between the individual reported gaze directions and their mean. Always
    /// available, panel-independent, and the honest measure of per-sample model jitter.
    pub spread_deg  : f64,
    /// Angle-space diagnostics, measured on the raw stream before any fitting.
    pub diagnostics : TargetDiagnostics,
}

/// Why a candidate model was not eligible, with the number that decided it.
#[derive(Clone, Debug, PartialEq)]
pub enum Rejection {
    /// Not enough targets, or the normal equations were singular at this degree.
    Unfittable,
    /// The angle correction's local gain leaves the sane band between the targets.
    AngleGain { min: f64, max: f64 },
    /// A per-output pixel map folds or collapses between the targets.
    PixelFold { min_det: f64, ratio: f64 },
}

/// One model considered by the fit, and how it scored.
#[derive(Clone, Debug, PartialEq)]
pub struct Candidate {
    /// Which stage-one shape this was.
    pub shape       : Shape,
    /// Whether the per-output pixel polynomial was included.
    pub stage_two   : bool,
    /// In-sample angular RMS, degrees.
    pub rms_deg     : f64,
    /// Leave-one-target-out angular RMS, degrees. What the choice is made on.
    pub rms_loo_deg : f64,
    /// Worst single held-out target, degrees.
    pub worst_loo_deg : f64,
    /// Weakest and strongest local gain of the angle correction over its working range.
    pub gain_bounds : Option<(f64, f64)>,
    /// Set when the model was refused, with the measurement that refused it. A candidate
    /// that is silently dropped is a candidate nobody can argue with.
    pub rejected    : Option<Rejection>,
}

/// The chosen calibration plus every model that was considered, so the choice is auditable
/// rather than a number that fell out of a black box.
#[derive(Clone, Debug)]
pub struct FitReport {
    pub calibration : Calibration,
    pub candidates  : Vec<Candidate>,
    /// Index into `candidates` of the model that was kept.
    pub chosen      : Option<usize>,
}

/// Per-sample scatter across the whole sweep, in the model's own units. The headline
/// "how noisy is this model" figure.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SweepSummary {
    /// Mean over targets of the per-target raw yaw standard deviation, degrees.
    pub yaw_sd_deg     : f64,
    pub pitch_sd_deg   : f64,
    /// Mean over targets of the per-target landing spread, degrees.
    pub spread_deg     : f64,
    /// Mean fraction of readings the sidecar called valid.
    pub valid_fraction : f64,
    /// Total samples across every target whose raw ray hit no panel.
    pub missed         : usize,
    pub targets        : usize,
}

/// What the user did at a target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Advance {
    /// Enter: move on now, keeping what was collected.
    Next,
    /// `s`: move on and throw this target away.
    Skip,
    /// `q`: abandon the sweep.
    Quit,
}

/// How a sweep ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepEnd {
    /// Every target was shown.
    Complete,
    /// The user quit part way through. The observations collected so far are still
    /// returned; whether they are enough to fit is the caller's call.
    Aborted,
}

/// The collected sweep, before fitting.
#[derive(Clone, Debug)]
pub struct SweepOutcome {
    pub end          : SweepEnd,
    pub observations : Vec<Observation>,
    /// Targets shown that produced nothing: skipped, or dwelled through with no valid
    /// sample (the sidecar was down, or the user blinked through the whole window).
    pub missed       : Vec<SweepTarget>,
}

/// Runs a sweep. Entry point is `CalibrationSweep::create()`.
pub struct CalibrationSweep {
    geometry  : DesktopGeometry,
    camera    : Option<CameraPose>,
    targets   : Vec<SweepTarget>,
    dwell_s   : f64,
    collect_s : f64,
    overlay   : Option<OverlayHandle>,
    keys      : Option<Receiver<Advance>>,
    on_target : Option<TargetHook>,
}

/// Builds a `CalibrationSweep`.
pub struct CalibrationSweepBuilder {
    geometry  : Option<DesktopGeometry>,
    camera    : Option<CameraPose>,
    targets   : Option<Vec<SweepTarget>>,
    grid      : usize,
    inset     : f64,
    centre    : bool,
    dwell_s   : f64,
    collect_s : f64,
    overlay   : Option<OverlayHandle>,
    keys      : Option<Receiver<Advance>>,
    on_target : Option<TargetHook>,
}

// --- CalibrationSweep ---

impl CalibrationSweep {
    /// Entry point for the builder.
    pub fn create() -> CalibrationSweepBuilder {
        CalibrationSweepBuilder::new()
    }

    /// The targets this sweep will show, in order.
    pub fn targets(&self) -> &[SweepTarget] {
        &self.targets
    }

    /// Shows every target in turn and collects what the provider reports.
    ///
    /// `provider` must be uncalibrated (see the module docs). Blocks for roughly
    /// `targets().len() * dwell_s` seconds.
    pub fn run(&mut self, provider: &mut dyn RawGaze) -> SweepOutcome {
        let mut observations = Vec::new();
        let mut missed       = Vec::new();
        let total            = self.targets.len();

        for index in 0..total {
            let target = self.targets[index].clone();

            self.show(index, total, &target);

            if let Some(hook) = self.on_target.as_mut() {
                hook(index, &target);
            }

            let (advance, collected) = self.collect(provider);

            if advance == Some(Advance::Quit) {
                self.clear();

                return SweepOutcome {
                    end          : SweepEnd::Aborted,
                    observations : observations,
                    missed       : missed,
                };
            }

            if advance == Some(Advance::Skip) {
                missed.push(target);

                continue;
            }

            match self.summarise(target.clone(), &collected) {
                Some(o) => observations.push(o),
                None    => missed.push(target),
            }
        }

        self.clear();

        SweepOutcome {
            end          : SweepEnd::Complete,
            observations : observations,
            missed       : missed,
        }
    }
}

impl CalibrationSweep {
    /// Puts one target on the overlay.
    fn show(&self, index: usize, total: usize, target: &SweepTarget) {
        let Some(overlay) = self.overlay.as_ref() else {
            return;
        };

        let state = OverlayState {
            gaze      : None,
            highlight : Some(Rect {
                x : target.px.x - TARGET_PX * 0.5,
                y : target.px.y - TARGET_PX * 0.5,
                w : TARGET_PX,
                h : TARGET_PX,
            }),
            truth     : Some(target.px),
            label     : Some(format!("look here {}/{}", index + 1, total)),
        };

        // A dead overlay thread is not a reason to abandon the sweep: the terminal still
        // says which target is which.
        let _ = overlay.set(state);
    }

    /// Blanks the overlay at the end of the sweep.
    fn clear(&self) {
        if let Some(overlay) = self.overlay.as_ref() {
            let _ = overlay.set(OverlayState::default());
        }
    }

    /// Drains samples for one target's dwell, returning the keypress that ended it (if
    /// any) and the rays inside the collection window.
    fn collect(&self, provider: &mut dyn RawGaze) -> (Option<Advance>, Vec<(f64, Reading)>) {
        let start   = Instant::now();
        let mut all = Vec::new();
        let mut key = None;

        loop {
            // Everything is kept, invalid readings included: the fraction of the window the
            // sidecar was actually tracking for is a diagnostic in its own right, and it is
            // lost the moment the invalid readings are filtered out here.
            while let Some(reading) = provider.try_next_reading() {
                all.push((start.elapsed().as_secs_f64(), reading));
            }

            if let Some(advance) = self.poll_key() {
                key = Some(advance);

                break;
            }

            if start.elapsed().as_secs_f64() >= self.dwell_s {
                break;
            }

            thread::sleep(POLL);
        }

        // Keep only the tail of the window, measured back from whenever it actually
        // ended rather than from the nominal dwell: an early Enter shortens it.
        //
        // A caller who asked to keep the whole dwell keeps all of it. The loop always
        // overshoots the dwell by up to one poll, so subtracting the collect window from
        // the real end would otherwise quietly drop the first few milliseconds of a window
        // that was supposed to be complete.
        if self.collect_s < self.dwell_s {
            let end   = start.elapsed().as_secs_f64();
            let floor = end - self.collect_s;

            all.retain(|(t, _)| *t >= floor);
        }

        (key, all)
    }

    /// Non-blocking read of the terminal channel.
    fn poll_key(&self) -> Option<Advance> {
        let keys = self.keys.as_ref()?;

        match keys.try_recv() {
            Ok(a)                       => Some(a),
            Err(TryRecvError::Empty)    => None,

            // Stdin closed. Nothing more will arrive, so fall back to the dwell timer.
            Err(TryRecvError::Disconnected) => None,
        }
    }

    /// Averages one target's collected readings into an observation, or `None` when
    /// nothing usable arrived.
    fn summarise(&self, target: SweepTarget, collected: &[(f64, Reading)]) -> Option<Observation> {
        if collected.is_empty() {
            return None;
        }

        let camera = self.camera.as_ref()?;
        let world  = self.geometry.px_to_world(target.px)?;

        // Where the target sits in the camera's own frame, so each sample's wanted
        // direction can be built against that sample's own reported eye position.
        let target_cam = camera.point_to_camera(world);

        let mut samples: Vec<AngleSample> = Vec::new();
        let mut rays: Vec<Ray>            = Vec::new();
        let mut points: Vec<GlobalPx>     = Vec::new();

        for (_, reading) in collected {
            let Some(message) = reading.message else {
                continue;
            };

            let Some(gaze) = message.gaze() else {
                continue;
            };

            let Some((yaw, pitch)) = gaze_yaw_pitch_deg(gaze.gaze) else {
                continue;
            };

            let want = target_cam - gaze.eye_mm;

            let Some((want_yaw, want_pitch)) = gaze_yaw_pitch_deg(want) else {
                continue;
            };

            // Nothing is clamped here. A sample whose ray leaves the desk still carries a
            // perfectly good pair of angles, and those are what the fit runs on.
            let ray      = Ray {
                origin : camera.point_to_desk(gaze.eye_mm),
                dir    : camera.dir_to_desk(gaze.gaze),
            };
            let resolved = resolve(&self.geometry, camera, None, &ray, false);

            if let Some(p) = resolved.point {
                points.push(p);
            }

            rays.push(ray);
            samples.push(AngleSample {
                eye_cam_mm     : gaze.eye_mm,
                gaze_cam       : gaze.gaze,
                yaw_deg        : yaw,
                pitch_deg      : pitch,
                want_yaw_deg   : want_yaw,
                want_pitch_deg : want_pitch,
                missed         : resolved.missed,
                head_rot       : message.head_rot,
                raw            : reading.raw.clone(),
            });
        }

        if samples.is_empty() {
            return None;
        }

        // Mean ray: the origins are the sidecar's measured eye positions, which move, and
        // the directions are unit vectors, so the mean direction needs renormalising.
        let n      = rays.len() as f64;
        let origin = rays.iter().map(|r| r.origin).sum::<DVec3>() / n;
        let dir    = rays.iter().map(|r| r.dir).sum::<DVec3>() / n;

        if !origin.is_finite() || !dir.is_finite() || dir.length_squared() <= 0.0 {
            return None;
        }

        let mean_ray = Ray { origin: origin, dir: dir.normalize() };

        // Scatter is measured between the reported directions themselves, so it exists
        // whether or not the samples reached a screen and means the same thing on every
        // panel. This is the model's own jitter, with no geometry mixed in.
        let mean_gaze  = samples.iter().map(|s| s.gaze_cam).sum::<DVec3>();
        let spread_deg = {
            if mean_gaze.length_squared() > 0.0 {
                let mean = mean_gaze.normalize();
                let sum: f64 = samples
                    .iter()
                    .map(|s| s.gaze_cam.angle_between(mean).to_degrees().powi(2))
                    .sum();

                finite_or_zero((sum / samples.len() as f64).sqrt())
            }
            else {
                0.0
            }
        };

        // The pixel mean and spread only cover the samples that landed on a panel.
        let (observed_px, spread_px) = {
            if points.is_empty() {
                (None, None)
            }
            else {
                let count  = points.len() as f64;
                let mean_x = points.iter().map(|p| p.x).sum::<f64>() / count;
                let mean_y = points.iter().map(|p| p.y).sum::<f64>() / count;

                let spread = (points
                    .iter()
                    .map(|p| (p.x - mean_x).powi(2) + (p.y - mean_y).powi(2))
                    .sum::<f64>()
                    / count)
                    .sqrt();

                (
                    Some(GlobalPx { x: mean_x, y: mean_y }),
                    Some(finite_or_zero(spread)),
                )
            }
        };

        Some(Observation {
            target      : target.clone(),
            diagnostics : self.diagnose(&samples, collected),
            samples     : samples,
            mean_ray    : mean_ray,
            observed_px : observed_px,
            spread_px   : spread_px,
            spread_deg  : spread_deg,
        })
    }

    /// Builds the angle-space diagnostics for one target.
    fn diagnose(&self, samples: &[AngleSample], collected: &[(f64, Reading)]) -> TargetDiagnostics {
        let yaws: Vec<f64>    = samples.iter().map(|s| s.yaw_deg).collect();
        let pitches: Vec<f64> = samples.iter().map(|s| s.pitch_deg).collect();

        let (yaw_mean, yaw_sd)     = mean_sd(&yaws);
        let (pitch_mean, pitch_sd) = mean_sd(&pitches);

        let n    = samples.len().max(1) as f64;
        let want = (
            samples.iter().map(|s| s.want_yaw_deg).sum::<f64>() / n,
            samples.iter().map(|s| s.want_pitch_deg).sum::<f64>() / n,
        );

        let eye = samples.iter().map(|s| s.eye_cam_mm).sum::<DVec3>() / n;

        // Confidence and head rotation come off the raw messages, including any the angle
        // pass skipped, so the tracked fraction is measured against everything that arrived.
        let mut conf   = 0.0_f64;
        let mut valid  = 0.0_f64;
        let mut head   = [0.0_f64; 3];
        let mut head_n = 0.0_f64;

        for (_, reading) in collected {
            let Some(message) = reading.message else {
                continue;
            };

            if let Some(rot) = message.head_rot {
                head[0] += rot[0];
                head[1] += rot[1];
                head[2] += rot[2];
                head_n  += 1.0;
            }

            if let Some(gaze) = message.gaze() {
                conf  += gaze.conf;
                valid += 1.0;
            }
        }

        let hn   = head_n.max(1.0);
        let vn   = valid.max(1.0);
        let seen = collected.len().max(1) as f64;

        TargetDiagnostics {
            yaw_deg_mean     : yaw_mean,
            yaw_deg_sd       : yaw_sd,
            pitch_deg_mean   : pitch_mean,
            pitch_deg_sd     : pitch_sd,
            target_yaw_deg   : finite_or_zero(want.0),
            target_pitch_deg : finite_or_zero(want.1),
            head_rot_mean    : [head[0] / hn, head[1] / hn, head[2] / hn],
            eye_mm_mean      : eye.to_array(),
            conf_mean        : conf / vn,
            valid_fraction   : valid / seen,
            missed           : samples.iter().filter(|s| s.missed).count(),
            samples_seen     : collected.len(),
        }
    }
}

// --- CalibrationSweepBuilder ---

impl CalibrationSweepBuilder {
    fn new() -> Self {
        Self {
            geometry  : None,
            camera    : None,
            targets   : None,
            grid      : 3,
            inset     : 0.12,
            centre    : true,
            dwell_s   : 1.5,
            collect_s : 1.0,
            overlay   : None,
            keys      : None,
            on_target : None,
        }
    }

    /// Desk geometry. Required; also determines the default target set.
    pub fn geometry(mut self, geometry: DesktopGeometry) -> Self {
        self.geometry = Some(geometry);
        self
    }

    /// Camera pose, used only to phrase the diagnostics in the camera frame the model
    /// works in. Without it the angular gain cannot be measured, and the fit is unaffected.
    pub fn camera(mut self, camera: CameraPose) -> Self {
        self.camera = Some(camera);
        self
    }

    /// An explicit target list, overriding the generated grid.
    pub fn targets(mut self, targets: Vec<SweepTarget>) -> Self {
        self.targets = Some(targets);
        self
    }

    /// Grid size per output. Defaults to 3, so 3x3 per output.
    pub fn grid(mut self, grid: usize) -> Self {
        self.grid = grid;
        self
    }

    /// How far in from each panel edge the outermost targets sit, as a fraction of the
    /// panel. Defaults to 0.12: far enough in to be comfortably visible, far enough out
    /// that the fit is not extrapolating over most of the screen.
    pub fn inset(mut self, inset: f64) -> Self {
        self.inset = inset;
        self
    }

    /// Whether to add one extra target at the centre of the desk, which is where the user
    /// spends most of their time and the place the fit most needs to be right. Defaults to
    /// true.
    pub fn centre_target(mut self, centre: bool) -> Self {
        self.centre = centre;
        self
    }

    /// How long each target is shown. Defaults to 1.5 s.
    pub fn dwell_s(mut self, dwell_s: f64) -> Self {
        self.dwell_s = dwell_s;
        self
    }

    /// How much of the end of each dwell is kept. Defaults to 1.0 s.
    pub fn collect_s(mut self, collect_s: f64) -> Self {
        self.collect_s = collect_s;
        self
    }

    /// Overlay to draw the targets on. Without one the sweep runs blind, which is only
    /// useful in tests.
    pub fn overlay(mut self, overlay: OverlayHandle) -> Self {
        self.overlay = Some(overlay);
        self
    }

    /// Channel of keypresses for early advance and skip. See `terminal_keys`.
    pub fn keys(mut self, keys: Receiver<Advance>) -> Self {
        self.keys = Some(keys);
        self
    }

    /// Called as each target goes up, before its dwell starts. The fake sidecar mode uses
    /// it to point the synthetic eye at the target that is now on screen.
    pub fn on_target(mut self, hook: impl FnMut(usize, &SweepTarget) + 'static) -> Self {
        self.on_target = Some(Box::new(hook));
        self
    }

    /// Assembles the sweep.
    pub fn build(self) -> Result<CalibrationSweep, SweepError> {
        let geometry = self.geometry.ok_or(SweepError::MissingGeometry)?;

        let targets = self
            .targets
            .unwrap_or_else(|| default_targets(&geometry, self.grid, self.inset, self.centre));

        if targets.is_empty() {
            return Err(SweepError::NoTargets);
        }

        Ok(CalibrationSweep {
            geometry  : geometry,
            camera    : self.camera,
            targets   : targets,
            dwell_s   : self.dwell_s.max(0.0),
            collect_s : self.collect_s.max(0.0).min(self.dwell_s.max(0.0)),
            overlay   : self.overlay,
            keys      : self.keys,
            on_target : self.on_target,
        })
    }
}

/// The default target set: a `grid` by `grid` lattice inset from the edges of every
/// enabled output, plus one at the centre of the desk if `centre` is set.
pub fn default_targets(geometry: &DesktopGeometry, grid: usize, inset: f64, centre: bool)
    -> Vec<SweepTarget>
{
    let mut out = Vec::new();

    for output in geometry.outputs.iter().filter(|o| o.enabled) {
        for row in 0..grid {
            for col in 0..grid {
                let u = lattice(col, grid, inset);
                let v = lattice(row, grid, inset);

                out.push(SweepTarget { output: output.name.clone(), px: output.uv_to_px(u, v) });
            }
        }
    }

    // The centre of the desk is near the seam, where most of the looking happens and where
    // the per-output grids are furthest from their own samples.
    if centre
        && let Some(p) = desk_centre(geometry)
        && let Some(output) = geometry.output_at(p)
    {
        out.push(SweepTarget { output: output.name.clone(), px: p });
    }

    out
}

/// Fits a calibration to a completed sweep, choosing the model by held-out error.
///
/// Four models are tried: a quadratic and a cubic angle correction, each with and without
/// the per-output pixel stage on top. Each is scored by leave-one-target-out RMS, and the
/// best wins. Nothing here is decided by taste: the pixel stage is kept only when it makes
/// held-out targets better, and the cubic is used only when its extra freedom pays for
/// itself.
///
/// In-sample RMS is reported too, but it is not the criterion. Ten coefficients per axis
/// over thirty targets can drive in-sample error to almost nothing while making the model
/// worse everywhere the user actually looks.
pub fn fit(
    geometry     : &DesktopGeometry,
    camera       : &CameraPose,
    observations : &[Observation],
    sigma_deg    : f64,
)
    -> FitReport
{
    let mut candidates = Vec::new();
    let mut best: Option<(usize, Calibration)> = None;

    let range = gain_points(observations);

    for shape in angle::candidates() {
        for stage_two in [false, true] {
            let built = build(geometry, camera, observations, shape, stage_two, sigma_deg);

            let model = {
                match built {
                    Ok(m) => m,

                    Err(reason) => {
                        candidates.push(Candidate {
                            shape         : shape,
                            stage_two     : stage_two,
                            rms_deg       : f64::NAN,
                            rms_loo_deg   : f64::NAN,
                            worst_loo_deg : f64::NAN,
                            gain_bounds   : None,
                            rejected      : Some(reason),
                        });

                        continue;
                    }
                }
            };

            let in_sample = residuals(geometry, camera, &model, observations);
            let held_out  = leave_one_out(geometry, camera, observations, shape, stage_two, sigma_deg);

            candidates.push(Candidate {
                shape         : shape,
                stage_two     : stage_two,
                rms_deg       : rms(&in_sample),
                rms_loo_deg   : rms(&held_out),
                worst_loo_deg : held_out.iter().copied().fold(0.0_f64, f64::max),
                gain_bounds   : model.angle.gain_bounds(&range),
                rejected      : None,
            });

            let index = candidates.len() - 1;

            // Ties go to the simpler model, which is the one already held: the loop runs
            // quadratic before cubic and without the pixel stage before with it.
            if best.as_ref().is_none_or(|(b, _)| {
                candidates[index].rms_loo_deg < candidates[*b].rms_loo_deg
            }) {
                best = Some((index, model));
            }
        }
    }

    let Some((chosen, mut calibration)) = best else {
        // Nothing was fittable, or everything that was fittable misbehaved. The identity
        // is what the system will do, so the identity's error is what has to be reported:
        // a calibration that corrects nothing and claims zero error would sail through the
        // acceptance gate and be written as if it were perfect.
        let mut identity = Calibration { sigma_deg: sigma_deg, ..Calibration::identity() };

        let uncorrected = residuals(geometry, camera, &identity, observations);

        identity.rms_deg     = rms(&uncorrected);
        identity.rms_loo_deg = identity.rms_deg;
        identity.gains       = gains(observations);
        identity.targets     = observations
            .iter()
            .map(|o| residual_record(geometry, camera, &identity, o))
            .collect();

        identity.note = "NO MODEL COULD BE FITTED. Every candidate either could not be \
            solved or misbehaved between the calibration targets, so this file corrects \
            nothing and its error is the raw error of the uncorrected stream."
            .to_string();

        return FitReport { calibration: identity, candidates: candidates, chosen: None };
    };

    calibration.rms_deg     = candidates[chosen].rms_deg;
    calibration.rms_loo_deg = candidates[chosen].rms_loo_deg;
    calibration.gains       = gains(observations);
    calibration.targets     = observations
        .iter()
        .map(|o| residual_record(geometry, camera, &calibration, o))
        .collect();

    // The pixel RMS is the angular one carried onto the screens, so it is only meaningful
    // for targets that produced a landing point.
    let px: Vec<f64> = calibration
        .targets
        .iter()
        .map(|t| t.residual_px)
        .filter(|v| v.is_finite())
        .collect();

    calibration.rms_px = rms(&px);

    FitReport { calibration: calibration, candidates: candidates, chosen: Some(chosen) }
}

/// Builds one candidate model. `None` when the angle fit is underdetermined or singular at
/// this degree.
fn build(
    geometry     : &DesktopGeometry,
    camera       : &CameraPose,
    observations : &[Observation],
    shape        : Shape,
    stage_two    : bool,
    sigma_deg    : f64,
)
    -> Result<Calibration, Rejection>
{
    // Stage one is fitted over every sample of every target at once, not over per-target
    // means: the fit is global, and a target with more samples has genuinely told us more.
    let rows: Vec<AngleRow> = observations
        .iter()
        .flat_map(|o| o.samples.iter())
        .map(|s| AngleRow {
            yaw_deg        : s.yaw_deg,
            pitch_deg      : s.pitch_deg,
            want_yaw_deg   : s.want_yaw_deg,
            want_pitch_deg : s.want_pitch_deg,
        })
        .collect();

    // One row per target for the spline, every sample for the polynomial. See `Shape::fit`.
    let targets: Vec<AngleRow> = observations.iter().filter_map(target_row).collect();
    let angle = shape.fit(&rows, &targets).ok_or(Rejection::Unfittable)?;

    // A correction that passes through every target and misbehaves between them is worse
    // than a simpler one that does neither, and neither the residual table nor
    // leave-one-target-out can tell the difference. This is the only thing that can.
    let points = gain_points_from(&rows);
    let bounds = angle.gain_bounds(&points).ok_or(Rejection::Unfittable)?;

    if bounds.0 < MIN_GAIN || bounds.1 > MAX_GAIN {
        return Err(Rejection::AngleGain { min: bounds.0, max: bounds.1 });
    }

    let mut cal = Calibration {
        format         : CALIBRATION_FORMAT,
        sigma_deg      : sigma_deg,
        created_unix_s : Calibration::now_unix_s(),
        note           : String::new(),
        angle          : angle,
        ..Calibration::identity()
    };

    if !stage_two {
        return Ok(cal);
    }

    // Stage two is fitted on what stage one leaves behind, per output, from the mean ray
    // of each target. Targets whose corrected ray still misses contribute nothing here:
    // there is no pixel to correct, and inventing one is exactly the mistake that made the
    // previous design fail.
    let mut per_output: Vec<(String, Vec<SamplePair>)> = Vec::new();

    for obs in observations {
        let Some(landed) = resolve(geometry, camera, Some(&cal), &obs.mean_ray, false).point else {
            continue;
        };

        let Some(output) = geometry.outputs.iter().find(|o| o.name == obs.target.output) else {
            continue;
        };

        let observed = normalise(output, landed);
        let wanted   = normalise(output, obs.target.px);

        let entry = {
            match per_output.iter_mut().find(|(name, _)| *name == obs.target.output) {
                Some(e) => e,

                None => {
                    per_output.push((obs.target.output.clone(), Vec::new()));
                    per_output.last_mut().expect("just pushed")
                }
            }
        };

        entry.1.push(([observed.0, observed.1], [wanted.0, wanted.1]));
    }

    cal.outputs = per_output
        .into_iter()
        .map(|(name, samples)| OutputCalibration {
            name   : name,
            points : samples.len(),
            map    : PolyMap::fit(&samples),
        })
        .collect();

    // Same reasoning as the angle gate: a pixel map that folds the panel in between its
    // targets is worse than no pixel map at all, and only the Jacobian can see it.
    for out in &cal.outputs {
        let Some((lo, hi)) = out.map.jacobian_bounds() else {
            return Err(Rejection::PixelFold { min_det: f64::NAN, ratio: f64::NAN });
        };

        if lo <= 0.0 || hi / lo > MAX_PIXEL_JACOBIAN_RATIO {
            return Err(Rejection::PixelFold { min_det: lo, ratio: hi / lo });
        }
    }

    Ok(cal)
}

/// The angle range a fit will be used over: what its rows covered, widened a little.
fn working_range(rows: &[AngleRow]) -> ((f64, f64), (f64, f64)) {
    let widen = |lo: f64, hi: f64| {
        let span = (hi - lo).max(1.0);

        (lo - span * RANGE_MARGIN, hi + span * RANGE_MARGIN)
    };

    let yaw_lo = rows.iter().map(|r| r.yaw_deg).fold(f64::INFINITY, f64::min);
    let yaw_hi = rows.iter().map(|r| r.yaw_deg).fold(f64::NEG_INFINITY, f64::max);
    let pit_lo = rows.iter().map(|r| r.pitch_deg).fold(f64::INFINITY, f64::min);
    let pit_hi = rows.iter().map(|r| r.pitch_deg).fold(f64::NEG_INFINITY, f64::max);

    if !yaw_lo.is_finite() || !yaw_hi.is_finite() || !pit_lo.is_finite() || !pit_hi.is_finite() {
        return ((-45.0, 45.0), (-30.0, 30.0));
    }

    (widen(yaw_lo, yaw_hi), widen(pit_lo, pit_hi))
}

/// The angle range the observations cover, widened a little.
pub fn observed_range(observations: &[Observation]) -> ((f64, f64), (f64, f64)) {
    let rows = rows_of(observations);

    if rows.is_empty() {
        return ((-45.0, 45.0), (-30.0, 30.0));
    }

    working_range(&rows)
}

/// One observation reduced to a single fit row at its mean.
fn target_row(obs: &Observation) -> Option<AngleRow> {
    if obs.samples.is_empty() {
        return None;
    }

    let n = obs.samples.len() as f64;

    Some(AngleRow {
        yaw_deg        : obs.samples.iter().map(|s| s.yaw_deg).sum::<f64>() / n,
        pitch_deg      : obs.samples.iter().map(|s| s.pitch_deg).sum::<f64>() / n,
        want_yaw_deg   : obs.samples.iter().map(|s| s.want_yaw_deg).sum::<f64>() / n,
        want_pitch_deg : obs.samples.iter().map(|s| s.want_pitch_deg).sum::<f64>() / n,
    })
}

/// Every sample of every observation as a fit row.
fn rows_of(observations: &[Observation]) -> Vec<AngleRow> {
    observations
        .iter()
        .flat_map(|o| o.samples.iter())
        .map(|s| AngleRow {
            yaw_deg        : s.yaw_deg,
            pitch_deg      : s.pitch_deg,
            want_yaw_deg   : s.want_yaw_deg,
            want_pitch_deg : s.want_pitch_deg,
        })
        .collect()
}

/// Where a fitted correction should be judged: a grid over the reported-angle range,
/// keeping only the points close enough to a calibration target to be somewhere the user
/// actually looks. See `GAIN_REACH_DEG`.
pub fn gain_points(observations: &[Observation]) -> Vec<(f64, f64)> {
    gain_points_from(&rows_of(observations))
}

/// `gain_points` from fit rows.
fn gain_points_from(rows: &[AngleRow]) -> Vec<(f64, f64)> {
    if rows.is_empty() {
        return Vec::new();
    }

    let (yaw_range, pitch_range) = working_range(rows);
    let mut out = Vec::new();

    for i in 0..GAIN_GRID {
        for j in 0..GAIN_GRID {
            let t = i as f64 / (GAIN_GRID - 1) as f64;
            let u = j as f64 / (GAIN_GRID - 1) as f64;

            let yaw   = yaw_range.0 + t * (yaw_range.1 - yaw_range.0);
            let pitch = pitch_range.0 + u * (pitch_range.1 - pitch_range.0);

            let near = rows.iter().any(|r| {
                let dy = r.yaw_deg - yaw;
                let dp = r.pitch_deg - pitch;

                (dy * dy + dp * dp).sqrt() <= GAIN_REACH_DEG
            });

            if near {
                out.push((yaw, pitch));
            }
        }
    }

    out
}

/// Leave-one-target-out residuals for a model shape: refit without each target in turn and
/// measure it.
fn leave_one_out(
    geometry     : &DesktopGeometry,
    camera       : &CameraPose,
    observations : &[Observation],
    shape        : Shape,
    stage_two    : bool,
    sigma_deg    : f64,
)
    -> Vec<f64>
{
    let mut out = Vec::with_capacity(observations.len());

    for held in 0..observations.len() {
        let rest: Vec<Observation> = observations
            .iter()
            .enumerate()
            .filter(|(i, _)| *i != held)
            .map(|(_, o)| o.clone())
            .collect();

        let Ok(model) = build(geometry, camera, &rest, shape, stage_two, sigma_deg) else {
            continue;
        };

        out.push(residual_deg(geometry, camera, &model, &observations[held]));
    }

    out
}

/// In-sample residual for every observation.
fn residuals(
    geometry     : &DesktopGeometry,
    camera       : &CameraPose,
    model        : &Calibration,
    observations : &[Observation],
)
    -> Vec<f64>
{
    observations
        .iter()
        .map(|o| residual_deg(geometry, camera, model, o))
        .collect()
}

/// Angular residual at one target under `model`, degrees.
///
/// Measured as the angle between the direction the system would end up believing in and
/// the direction the target called for. When the pixel stage moved the point, the believed
/// direction is the one through that moved point, so both stages are scored on the same
/// footing. When the corrected ray misses every panel there is no pixel stage to apply and
/// the angle-space residual stands on its own.
fn residual_deg(
    geometry : &DesktopGeometry,
    camera   : &CameraPose,
    model    : &Calibration,
    obs      : &Observation,
)
    -> f64
{
    let want = {
        let Some(world) = geometry.px_to_world(obs.target.px) else {
            return f64::NAN;
        };

        let v = world - obs.mean_ray.origin;

        if v.length_squared() <= 0.0 {
            return f64::NAN;
        }

        v.normalize()
    };

    let resolved = resolve(geometry, camera, Some(model), &obs.mean_ray, false);

    // A landing point supersedes the raw direction only because stage two may have moved
    // it; without stage two the two agree by construction.
    let believed = {
        match resolved.point.and_then(|p| geometry.px_to_world(p)) {
            Some(world) => {
                let v = world - obs.mean_ray.origin;

                if v.length_squared() > 0.0 { v.normalize() } else { resolved.ray.dir }
            }

            None => resolved.ray.dir,
        }
    };

    believed.angle_between(want).to_degrees()
}

/// Root mean square, ignoring non-finite entries. Zero for an empty set.
fn rms(values: &[f64]) -> f64 {
    let good: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();

    if good.is_empty() {
        return 0.0;
    }

    (good.iter().map(|v| v * v).sum::<f64>() / good.len() as f64).sqrt()
}

/// Least-squares gain per output, measured on the observations before any fitting.
///
/// Two flavours, because they fail differently. The pixel gain is what the polynomial has
/// to invert. The angular gain is measured on the raw sidecar vectors against the angles
/// the targets called for, so it is untouched by the desk geometry and by any error in the
/// camera pose: if that one is well below 1, the model itself is under-reporting.
///
/// Both are single slopes, so neither can describe a gain that varies across the field.
/// When the angle table shows the gain changing with eccentricity, these numbers are a
/// summary of something the summary cannot hold, and the residual plot is the thing to
/// read instead.
pub fn gains(observations: &[Observation]) -> Vec<OutputGain> {
    let mut out: Vec<OutputGain> = Vec::new();

    for name in output_names(observations) {
        let group: Vec<&Observation> = observations
            .iter()
            .filter(|o| o.target.output == name)
            .collect();

        let target_x: Vec<f64> = group.iter().map(|o| o.target.px.x).collect();
        let target_y: Vec<f64> = group.iter().map(|o| o.target.px.y).collect();

        // Only targets that produced a landing point can contribute a pixel gain.
        let landed: Vec<&&Observation> = group.iter().filter(|o| o.observed_px.is_some()).collect();
        let lx: Vec<f64> = landed.iter().map(|o| o.target.px.x).collect();
        let ly: Vec<f64> = landed.iter().map(|o| o.target.px.y).collect();
        let ox: Vec<f64> = landed.iter().filter_map(|o| o.observed_px).map(|p| p.x).collect();
        let oy: Vec<f64> = landed.iter().filter_map(|o| o.observed_px).map(|p| p.y).collect();

        let want_yaw: Vec<f64>   = group.iter().map(|o| o.diagnostics.target_yaw_deg).collect();
        let want_pitch: Vec<f64> = group.iter().map(|o| o.diagnostics.target_pitch_deg).collect();
        let got_yaw: Vec<f64>    = group.iter().map(|o| o.diagnostics.yaw_deg_mean).collect();
        let got_pitch: Vec<f64>  = group.iter().map(|o| o.diagnostics.pitch_deg_mean).collect();

        let _ = (&target_x, &target_y);

        out.push(OutputGain {
            name           : name,
            points         : group.len(),
            gain_px_x      : ls_slope(&lx, &ox),
            gain_px_y      : ls_slope(&ly, &oy),
            gain_deg_yaw   : ls_slope(&want_yaw, &got_yaw),
            gain_deg_pitch : ls_slope(&want_pitch, &got_pitch),
        });
    }

    out
}

/// Per-sample scatter and tracking quality averaged over the whole sweep.
pub fn summarise(observations: &[Observation]) -> SweepSummary {
    if observations.is_empty() {
        return SweepSummary::default();
    }

    let n = observations.len() as f64;

    SweepSummary {
        yaw_sd_deg     : observations.iter().map(|o| o.diagnostics.yaw_deg_sd).sum::<f64>() / n,
        pitch_sd_deg   : observations.iter().map(|o| o.diagnostics.pitch_deg_sd).sum::<f64>() / n,
        spread_deg     : observations.iter().map(|o| o.spread_deg).sum::<f64>() / n,
        valid_fraction : observations.iter().map(|o| o.diagnostics.valid_fraction).sum::<f64>() / n,
        missed         : observations.iter().map(|o| o.diagnostics.missed).sum(),
        targets        : observations.len(),
    }
}

/// Output names in the order they first appear.
fn output_names(observations: &[Observation]) -> Vec<String> {
    let mut names: Vec<String> = Vec::new();

    for o in observations {
        if !names.contains(&o.target.output) {
            names.push(o.target.output.clone());
        }
    }

    names
}

/// Slope of `ys` against `xs` by least squares. `None` when the inputs do not span enough
/// x to define one, which is the honest answer for a single target or a degenerate row.
fn ls_slope(xs: &[f64], ys: &[f64]) -> Option<f64> {
    if xs.len() < 2 || xs.len() != ys.len() {
        return None;
    }

    let n     = xs.len() as f64;
    let x_bar = xs.iter().sum::<f64>() / n;
    let y_bar = ys.iter().sum::<f64>() / n;

    let sxx: f64 = xs.iter().map(|x| (x - x_bar).powi(2)).sum();
    let sxy: f64 = xs.iter().zip(ys.iter()).map(|(x, y)| (x - x_bar) * (y - y_bar)).sum();

    // Scale the degeneracy test by the spread itself so it means "the targets are all at
    // the same place" rather than "the numbers happen to be small".
    if !sxx.is_finite() || sxx <= 1.0e-9 * (1.0 + x_bar.abs()) {
        return None;
    }

    let slope = sxy / sxx;

    slope.is_finite().then_some(slope)
}

/// Mean and population standard deviation, both zero for an empty set.
fn mean_sd(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }

    let n    = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    let var  = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / n;

    (finite_or_zero(mean), finite_or_zero(var.max(0.0).sqrt()))
}

/// Replaces a non-finite figure with zero. Diagnostics are averaged and printed, and one
/// NaN in a column silently destroys every summary built on it.
fn finite_or_zero(v: f64) -> f64 {
    if v.is_finite() { v } else { 0.0 }
}

/// What the corrected stage-one angles come out as at each target, for the text residual
/// plot. Returns `(want_yaw, corrected_yaw, want_pitch, corrected_pitch)`.
pub fn angle_residual(model: &Calibration, obs: &Observation) -> (f64, f64, f64, f64) {
    let (yaw, pitch) = model
        .angle
        .apply(obs.diagnostics.yaw_deg_mean, obs.diagnostics.pitch_deg_mean);

    (obs.diagnostics.target_yaw_deg, yaw, obs.diagnostics.target_pitch_deg, pitch)
}

/// Spawns a thread reading lines from stdin and turning them into advance requests.
///
/// Line buffered, so `s` needs an Enter after it. Raw terminal mode would avoid that but
/// would also mean restoring the termios on every exit path including a panic, which is
/// not worth it for a calibration that already asks the user to sit still for a minute.
/// Returns `None` when stdin is not a terminal, in which case the sweep runs on its dwell
/// timer alone.
pub fn terminal_keys() -> Option<Receiver<Advance>> {
    if !std::io::stdin().is_terminal() {
        return None;
    }

    let (tx, rx) = crossbeam_channel::unbounded();

    // Detached: the thread is parked in a blocking read on stdin, and there is no portable
    // way to interrupt that. It dies with the process.
    thread::Builder::new()
        .name("gaze-webcam-keys".to_string())
        .spawn(move || {
            let stdin = std::io::stdin();

            for line in stdin.lock().lines() {
                let Ok(line) = line else {
                    break;
                };

                let advance = {
                    match line.trim() {
                        "s" | "S" => Advance::Skip,
                        "q" | "Q" => Advance::Quit,
                        _         => Advance::Next,
                    }
                };

                if tx.send(advance).is_err() {
                    break;
                }
            }
        })
        .ok()?;

    Some(rx)
}

/// One target's fraction along an axis, for a `grid` by `grid` lattice inset from the
/// panel edges.
fn lattice(index: usize, grid: usize, inset: f64) -> f64 {
    if grid <= 1 {
        return 0.5;
    }

    let inset = inset.clamp(0.0, 0.49);

    inset + (1.0 - 2.0 * inset) * index as f64 / (grid - 1) as f64
}

/// Centre of the bounding box of every enabled output, moved onto a panel if it landed in
/// a gap. `None` when nothing is enabled.
fn desk_centre(geometry: &DesktopGeometry) -> Option<GlobalPx> {
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);
    let mut any = false;

    for out in geometry.outputs.iter().filter(|o| o.enabled) {
        min = (min.0.min(out.logical_x), min.1.min(out.logical_y));
        max = (max.0.max(out.logical_x + out.logical_w), max.1.max(out.logical_y + out.logical_h));
        any = true;
    }

    if !any {
        return None;
    }

    let centre = GlobalPx { x: 0.5 * (min.0 + max.0), y: 0.5 * (min.1 + max.1) };

    Some(crate::calibration::snap_to_desk(geometry, centre))
}

/// What the finished calibration leaves behind at one target.
///
/// The angular residual is the model's real error and is always defined. The pixel
/// residual only exists when the corrected ray actually reaches a panel, and is left
/// non-finite otherwise rather than being invented from a clamped point.
fn residual_record(
    geometry : &DesktopGeometry,
    camera   : &CameraPose,
    cal      : &Calibration,
    obs      : &Observation,
)
    -> TargetResidual
{
    let corrected    = resolve(geometry, camera, Some(cal), &obs.mean_ray, false).point;
    let residual_deg = residual_deg(geometry, camera, cal, obs);

    let residual_px = {
        match corrected {
            Some(p) => {
                let dx = p.x - obs.target.px.x;
                let dy = p.y - obs.target.px.y;

                (dx * dx + dy * dy).sqrt()
            }

            None => f64::NAN,
        }
    };

    TargetResidual {
        output       : obs.target.output.clone(),
        target_px    : [obs.target.px.x, obs.target.px.y],
        observed_px  : obs.observed_px.map(|p| [p.x, p.y]),
        spread_px    : obs.spread_px,
        spread_deg   : obs.spread_deg,
        residual_px  : residual_px,
        residual_deg : residual_deg,
        samples      : obs.samples.len(),
        diagnostics  : obs.diagnostics,
    }
}

/// An angular figure carried onto each enabled output's pixels, using the local scale at
/// that output's centre.
///
/// An angle is the honest unit for a gaze error, but it is not the unit anyone has an
/// intuition for. The same 1 degree is 55 pixels on the ultrawide and 70 on the small
/// panel, which is the difference between "lands on the right button" and "does not".
pub fn px_equivalent(geometry: &DesktopGeometry, deg: f64) -> Vec<(String, f64)> {
    let eye = geometry.eye();

    geometry
        .outputs
        .iter()
        .filter(|o| o.enabled)
        .map(|o| {
            let centre = o.uv_to_px(0.5, 0.5);
            let scale  = geometry
                .px_per_deg(eye, centre)
                .map(|(h, v)| 0.5 * (h + v))
                .unwrap_or(FALLBACK_PX_PER_DEG);

            (o.name.clone(), deg * scale)
        })
        .collect()
}

/// One sample of a sweep, as written to the raw readings log.
///
/// A calibration can be refitted from these without asking the user to sit through another
/// sweep, which matters because the fitting model is the part most likely to change. Each
/// record is self-contained: the angles are already reduced, so a refit needs only the desk
/// config to interpret them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct SweepRecord {
    pub output         : String,
    pub target_px      : [f64; 2],
    pub eye_cam_mm     : [f64; 3],
    pub gaze_cam       : [f64; 3],
    pub yaw_deg        : f64,
    pub pitch_deg      : f64,
    pub want_yaw_deg   : f64,
    pub want_pitch_deg : f64,
    pub missed         : bool,
    /// Head rotation as reported, uninterpreted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub head_rot       : Option<[f64; 3]>,
    /// The verbatim sidecar line, so a later experiment can use fields this crate has
    /// never heard of. Omitted from the file when it was not captured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub raw            : Option<String>,
}

/// Flattens a sweep into records, one per sample.
pub fn to_records(observations: &[Observation]) -> Vec<SweepRecord> {
    observations
        .iter()
        .flat_map(|o| {
            o.samples.iter().map(move |s| SweepRecord {
                output         : o.target.output.clone(),
                target_px      : [o.target.px.x, o.target.px.y],
                eye_cam_mm     : s.eye_cam_mm.to_array(),
                gaze_cam       : s.gaze_cam.to_array(),
                yaw_deg        : s.yaw_deg,
                pitch_deg      : s.pitch_deg,
                want_yaw_deg   : s.want_yaw_deg,
                want_pitch_deg : s.want_pitch_deg,
                missed         : s.missed,
                head_rot       : s.head_rot,
                raw            : s.raw.as_ref().map(|r| r.to_string()),
            })
        })
        .collect()
}

/// Rebuilds observations from records, grouping by target position.
///
/// The result is fit-ready and scores identically to the original sweep, so a refit is a
/// true replay rather than an approximation.
pub fn from_records(
    geometry : &DesktopGeometry,
    camera   : &CameraPose,
    records  : &[SweepRecord],
)
    -> Vec<Observation>
{
    let mut grouped: Vec<(SweepTarget, Vec<AngleSample>)> = Vec::new();

    for r in records {
        let target = SweepTarget {
            output : r.output.clone(),
            px     : GlobalPx { x: r.target_px[0], y: r.target_px[1] },
        };

        let sample = AngleSample {
            eye_cam_mm     : DVec3::from_array(r.eye_cam_mm),
            gaze_cam       : DVec3::from_array(r.gaze_cam),
            yaw_deg        : r.yaw_deg,
            pitch_deg      : r.pitch_deg,
            want_yaw_deg   : r.want_yaw_deg,
            want_pitch_deg : r.want_pitch_deg,
            missed         : r.missed,
            head_rot       : r.head_rot,
            raw            : r.raw.as_deref().map(std::sync::Arc::from),
        };

        match grouped.iter_mut().find(|(t, _)| *t == target) {
            Some((_, samples)) => samples.push(sample),
            None               => grouped.push((target, vec![sample])),
        }
    }

    grouped
        .into_iter()
        .filter_map(|(target, samples)| rebuild(geometry, camera, target, samples))
        .collect()
}

/// Mean of whatever head rotations the samples carry, zeros when none do.
fn mean_head_rot(samples: &[AngleSample]) -> [f64; 3] {
    let present: Vec<[f64; 3]> = samples.iter().filter_map(|s| s.head_rot).collect();

    if present.is_empty() {
        return [0.0; 3];
    }

    let n = present.len() as f64;

    [
        present.iter().map(|r| r[0]).sum::<f64>() / n,
        present.iter().map(|r| r[1]).sum::<f64>() / n,
        present.iter().map(|r| r[2]).sum::<f64>() / n,
    ]
}

/// Builds one observation from already-reduced samples.
pub fn rebuild(
    geometry : &DesktopGeometry,
    camera   : &CameraPose,
    target   : SweepTarget,
    samples  : Vec<AngleSample>,
)
    -> Option<Observation>
{
    if samples.is_empty() {
        return None;
    }

    let n      = samples.len() as f64;
    let origin = samples.iter().map(|s| camera.point_to_desk(s.eye_cam_mm)).sum::<DVec3>() / n;
    let dir    = samples.iter().map(|s| camera.dir_to_desk(s.gaze_cam)).sum::<DVec3>() / n;

    if !origin.is_finite() || !dir.is_finite() || dir.length_squared() <= 0.0 {
        return None;
    }

    let mean_ray = Ray { origin: origin, dir: dir.normalize() };

    let points: Vec<GlobalPx> = samples
        .iter()
        .filter_map(|s| {
            let ray = Ray {
                origin : camera.point_to_desk(s.eye_cam_mm),
                dir    : camera.dir_to_desk(s.gaze_cam),
            };

            resolve(geometry, camera, None, &ray, false).point
        })
        .collect();

    let (observed_px, spread_px) = {
        if points.is_empty() {
            (None, None)
        }
        else {
            let count  = points.len() as f64;
            let mean_x = points.iter().map(|p| p.x).sum::<f64>() / count;
            let mean_y = points.iter().map(|p| p.y).sum::<f64>() / count;

            let spread = (points
                .iter()
                .map(|p| (p.x - mean_x).powi(2) + (p.y - mean_y).powi(2))
                .sum::<f64>()
                / count)
                .sqrt();

            (Some(GlobalPx { x: mean_x, y: mean_y }), Some(finite_or_zero(spread)))
        }
    };

    let mean_gaze  = samples.iter().map(|s| s.gaze_cam).sum::<DVec3>();
    let spread_deg = {
        if mean_gaze.length_squared() > 0.0 {
            let mean     = mean_gaze.normalize();
            let sum: f64 = samples
                .iter()
                .map(|s| s.gaze_cam.angle_between(mean).to_degrees().powi(2))
                .sum();

            finite_or_zero((sum / n).sqrt())
        }
        else {
            0.0
        }
    };

    let (yaw_mean, yaw_sd)     = mean_sd(&samples.iter().map(|s| s.yaw_deg).collect::<Vec<_>>());
    let (pitch_mean, pitch_sd) = mean_sd(&samples.iter().map(|s| s.pitch_deg).collect::<Vec<_>>());

    let diagnostics = TargetDiagnostics {
        yaw_deg_mean     : yaw_mean,
        yaw_deg_sd       : yaw_sd,
        pitch_deg_mean   : pitch_mean,
        pitch_deg_sd     : pitch_sd,
        target_yaw_deg   : samples.iter().map(|s| s.want_yaw_deg).sum::<f64>() / n,
        target_pitch_deg : samples.iter().map(|s| s.want_pitch_deg).sum::<f64>() / n,
        head_rot_mean    : mean_head_rot(&samples),
        eye_mm_mean      : (samples.iter().map(|s| s.eye_cam_mm).sum::<DVec3>() / n).to_array(),
        conf_mean        : 0.0,
        valid_fraction   : 1.0,
        missed           : samples.iter().filter(|s| s.missed).count(),
        samples_seen     : samples.len(),
    };

    Some(Observation {
        target      : target,
        samples     : samples,
        mean_ray    : mean_ray,
        observed_px : observed_px,
        spread_px   : spread_px,
        spread_deg  : spread_deg,
        diagnostics : diagnostics,
    })
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error("calibration sweep needs a desk geometry")]
    MissingGeometry,

    #[error("calibration sweep has no targets: are any outputs enabled?")]
    NoTargets,
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::angle::{AngleCorrection, TpsWarp};
    use crate::camera::gaze_dir_from_yaw_pitch_deg;
    use crate::fit::{AngleDegree, AnglePoly};

    const DESK_TOML: &str = include_str!("../../../config/desk.toml");

    fn desk() -> DesktopGeometry {
        DesktopGeometry::from_toml(DESK_TOML).unwrap()
    }

    fn cam() -> CameraPose {
        CameraPose::from_desk_toml(DESK_TOML).unwrap()
    }

    /// Builds an observation for `target` whose reported gaze is the true gaze put through
    /// `distort`, a camera-frame `(yaw, pitch) -> (yaw, pitch)` map. `jitter` samples are
    /// generated, spread deterministically about the reported angles.
    fn observation_with(
        g       : &DesktopGeometry,
        c       : &CameraPose,
        target  : SweepTarget,
        count   : usize,
        jitter  : f64,
        distort : impl Fn(f64, f64) -> (f64, f64),
    )
        -> Option<Observation>
    {
        let world   = g.px_to_world(target.px)?;
        let eye_cam = c.point_to_camera(g.eye());
        let want    = gaze_yaw_pitch_deg(c.point_to_camera(world) - eye_cam)?;

        let mut samples = Vec::new();

        for i in 0..count.max(1) {
            // A deterministic zero-mean wobble: the fit must see scatter without the test
            // depending on an RNG.
            let phase = i as f64 * std::f64::consts::TAU / count.max(1) as f64;
            let (yaw, pitch) = distort(want.0, want.1);

            let yaw   = yaw + jitter * phase.sin();
            let pitch = pitch + jitter * phase.cos();

            let gaze = gaze_dir_from_yaw_pitch_deg(yaw, pitch);
            let ray  = Ray { origin: g.eye(), dir: c.dir_to_desk(gaze) };

            samples.push(AngleSample {
                eye_cam_mm     : eye_cam,
                gaze_cam       : gaze,
                yaw_deg        : yaw,
                pitch_deg      : pitch,
                want_yaw_deg   : want.0,
                want_pitch_deg : want.1,
                missed         : resolve(g, c, None, &ray, false).missed,
                head_rot       : None,
                raw            : None,
            });
        }

        rebuild(g, c, target, samples)
    }

    /// A full sweep's worth of observations under one distortion.
    fn sweep_with(
        g       : &DesktopGeometry,
        c       : &CameraPose,
        jitter  : f64,
        distort : impl Fn(f64, f64) -> (f64, f64) + Copy,
    )
        -> Vec<Observation>
    {
        default_targets(g, 3, 0.12, true)
            .into_iter()
            .filter_map(|t| observation_with(g, c, t, 8, jitter, distort))
            .collect()
    }

    /// A `RawGaze` that hands back a canned list of readings, so the collection path can be
    /// exercised without a socket or a sleep.
    struct StubGaze {
        readings : std::collections::VecDeque<Reading>,
    }

    impl RawGaze for StubGaze {
        fn try_next_reading(&mut self) -> Option<Reading> {
            self.readings.pop_front()
        }
    }

    /// One reading looking along `dir` (desk frame) from the nominal eye, phrased in the
    /// camera frame the way the sidecar would.
    fn reading(g: &DesktopGeometry, c: &CameraPose, dir: DVec3, seq: u64) -> Reading {
        let ray      = Ray { origin: g.eye(), dir: dir.normalize() };
        let resolved = resolve(g, c, None, &ray, true);

        Reading {
            sample  : gaze_core::GazeSample {
                t_s       : seq as f64 / 30.0,
                ray       : Some(ray),
                point     : resolved.point,
                sigma_deg : 2.5,
                valid     : resolved.point.is_some(),
            },
            message : Some(crate::protocol::SidecarMessage::from_gaze(&crate::protocol::SidecarGaze {
                t      : seq as f64 / 30.0,
                seq    : seq,
                eye_mm : c.point_to_camera(g.eye()),
                gaze   : c.dir_to_camera(ray.dir),
                conf   : 0.8,
                lat_ms : 40.0,
            })),
            raw     : None,
            meta    : Some(crate::provider::SampleMeta {
                seq          : seq,
                sidecar_t_s  : seq as f64 / 30.0,
                conf         : 0.8,
                lat_ms       : 40.0,
                off_axis_deg : g.off_axis_deg(&ray),
                clamped      : resolved.clamped,
            }),
        }
    }

    /// Runs a one-target sweep over a canned set of readings.
    fn sweep_once(g: &DesktopGeometry, c: &CameraPose, target: SweepTarget, readings: Vec<Reading>)
        -> SweepOutcome
    {
        let mut sweep = CalibrationSweep::create()
            .geometry(g.clone())
            .camera(c.clone())
            .targets(vec![target])
            .dwell_s(0.0)
            .collect_s(0.0)
            .build()
            .unwrap();

        let mut stub = StubGaze { readings: readings.into() };

        sweep.run(&mut stub)
    }

    #[test]
    fn the_default_target_set_covers_every_output_plus_the_centre() {
        let g = desk();
        let t = default_targets(&g, 3, 0.12, true);

        assert_eq!(t.len(), 28);

        for output in &g.outputs {
            let count = t.iter().filter(|x| x.output == output.name).count();

            assert!(count >= 9, "{} got only {count} targets", output.name);
        }

        for target in &t {
            let at = g.output_at(target.px).expect("target off the desk");

            assert_eq!(at.name, target.output, "target {target:?} is on the wrong output");
        }
    }

    #[test]
    fn a_one_by_one_grid_is_the_panel_centre_and_does_not_divide_by_zero() {
        let g = desk();
        let t = default_targets(&g, 1, 0.12, false);

        assert_eq!(t.len(), 3);

        let dp1 = &g.outputs[0];
        assert!((t[0].px.x - (dp1.logical_x + dp1.logical_w * 0.5)).abs() < 1.0e-9);
    }

    #[test]
    fn the_fit_recovers_a_constant_angular_offset() {
        let g = desk();
        let c = cam();

        let obs    = sweep_with(&g, &c, 0.0, |y, p| (y + 3.0, p - 2.0));
        let report = fit(&g, &c, &obs, 2.5);

        assert!(report.chosen.is_some());
        assert!(report.calibration.rms_deg < 0.05, "in-sample {}", report.calibration.rms_deg);
        assert!(report.calibration.rms_loo_deg < 0.1, "held-out {}", report.calibration.rms_loo_deg);
    }

    #[test]
    fn a_kept_model_is_always_sane_between_its_targets() {
        let g = desk();
        let c = cam();

        // A gain of about 3 straight ahead falling off with eccentricity: the shape that
        // produced the live "reaches half way to the edges" symptom. Whatever is kept, its
        // gain must not swing wildly, because that is what the symptom *was*.
        let saturate = |a: f64, k: f64| {
            let scale = 45.0_f64.to_radians();

            (scale * (k * a.to_radians() / scale).atan()).to_degrees()
        };

        let obs    = sweep_with(&g, &c, 0.0, move |y, p| (saturate(y, 3.0) - 0.3 * p, saturate(p, 2.0)));
        let report = fit(&g, &c, &obs, 2.5);
        let points = gain_points(&obs);

        if let Some(i) = report.chosen {
            assert!(report.candidates[i].rejected.is_none());
            assert!(
                report.calibration.angle.is_sane(&points, MIN_GAIN, MAX_GAIN),
                "a kept model must be sane: gain {:?}",
                report.calibration.angle.gain_bounds(&points),
            );
        }

        // Every rejection has to name a measured reason rather than vanishing.
        for c in report.candidates.iter().filter(|c| c.rejected.is_some()) {
            assert!(!matches!(c.rejected, Some(Rejection::Unfittable)) || obs.len() < 10);
        }
    }

    #[test]
    fn an_overfitted_correction_is_rejected_even_though_it_scores_well() {
        let g = desk();
        let c = cam();

        // The exact failure from the field: a cubic that passes through every target and
        // swings its gain between them. It must not be kept however good its residual is.
        let saturate = |a: f64, k: f64| {
            let scale = 45.0_f64.to_radians();

            (scale * (k * a.to_radians() / scale).atan()).to_degrees()
        };

        let obs   = sweep_with(&g, &c, 0.0, move |y, p| (saturate(y, 3.0) - 0.3 * p, saturate(p, 2.0)));
        let rows: Vec<AngleRow> = obs
            .iter()
            .flat_map(|o| o.samples.iter())
            .map(|s| AngleRow {
                yaw_deg        : s.yaw_deg,
                pitch_deg      : s.pitch_deg,
                want_yaw_deg   : s.want_yaw_deg,
                want_pitch_deg : s.want_pitch_deg,
            })
            .collect();

        let cubic  = AngleCorrection::Poly(AnglePoly::fit(&rows, AngleDegree::Cubic).unwrap());
        let points = gain_points(&obs);

        // It fits its targets well...
        let worst = rows
            .iter()
            .map(|r| {
                let (y, p) = cubic.apply(r.yaw_deg, r.pitch_deg);

                (y - r.want_yaw_deg).abs().max((p - r.want_pitch_deg).abs())
            })
            .fold(0.0_f64, f64::max);

        assert!(worst < 6.0, "the overfitted cubic should still hit its targets: {worst}");

        // ...and is refused anyway, because of what it does in between them.
        let (lo, hi) = cubic.gain_bounds(&points).unwrap();

        assert!(
            !cubic.is_sane(&points, MIN_GAIN, MAX_GAIN),
            "gain {lo:.2} to {hi:.2} should have been refused",
        );

        assert!(!report_keeps_cubic(&g, &c, &obs), "the fit must not keep it either");
    }

    /// True when a fit over `obs` kept a cubic angle model.
    fn report_keeps_cubic(g: &DesktopGeometry, c: &CameraPose, obs: &[Observation]) -> bool {
        let report = fit(g, c, obs, 2.5);

        report
            .chosen
            .map(|i| report.candidates[i].shape == Shape::Poly(AngleDegree::Cubic))
            .unwrap_or(false)
    }

    #[test]
    fn the_fit_follows_a_gentle_saturating_gain_no_single_offset_could() {
        let g = desk();
        let c = cam();

        // The failure that motivated the angle-space design: gain of about 3 straight
        // ahead, falling off with eccentricity, plus a yaw error that depends on pitch.
        let saturate = |a: f64, k: f64| {
            let scale = 45.0_f64.to_radians();

            (scale * (k * a.to_radians() / scale).atan()).to_degrees()
        };

        // Gentle enough that a sane correction exists, which is the regime a usable
        // calibration lives in.
        let distort = move |y: f64, p: f64| (saturate(y, 1.4) - 0.15 * p, saturate(p, 1.3));

        let obs = sweep_with(&g, &c, 0.0, distort);

        // Establish that the raw error really is large, so the numbers below mean something.
        let raw = (obs
            .iter()
            .map(|o| {
                let d = &o.diagnostics;

                (d.yaw_deg_mean - d.target_yaw_deg).powi(2)
                    + (d.pitch_deg_mean - d.target_pitch_deg).powi(2)
            })
            .sum::<f64>()
            / obs.len() as f64)
            .sqrt();

        assert!(raw > 5.0, "the test distortion is too gentle to be interesting: {raw}");

        let report = fit(&g, &c, &obs, 2.5);

        // A sane quadratic cannot fully invert an arctangent across a hundred and fifty
        // degrees of desk, and it is not allowed to try: what it must do is take a clear
        // bite out of the error while staying well behaved. Recovering the rest needs more
        // calibration targets, not more polynomial.
        assert!(report.chosen.is_some(), "a sane model must be found for a gentle curve");
        assert!(
            report.calibration.rms_loo_deg < raw * 0.7,
            "held-out {} deg against a raw error of {raw} deg",
            report.calibration.rms_loo_deg,
        );

        assert!(report.calibration.angle.is_sane(&gain_points(&obs), MIN_GAIN, MAX_GAIN));
    }

    #[test]
    fn a_spline_follows_a_kink_a_polynomial_has_to_smear() {
        let g = desk();
        let c = cam();

        // A yaw map with a hinge: steep near the camera axis on one side, gentle past it.
        // This is the shape of the real l2cs stream, and the reason a spline is in the
        // candidate set at all: a global polynomial can only follow a local bend by
        // bending everywhere.
        let kinked = |y: f64, p: f64| {
            let bent = if y < -8.0 { -8.0 + (y + 8.0) * 0.45 } else { y * 2.1 };

            (bent - 0.25 * p, p * 1.1)
        };

        let obs    = sweep_with(&g, &c, 0.0, kinked);
        let rows: Vec<AngleRow> = obs.iter().filter_map(target_row).collect();
        let points = gain_points(&obs);

        let quad = AngleCorrection::Poly(AnglePoly::fit(&rows, AngleDegree::Quadratic).unwrap());
        let tps  = AngleCorrection::Tps(TpsWarp::fit(&rows, 0.1).unwrap());

        let worst = |m: &AngleCorrection| {
            rows.iter()
                .map(|r| {
                    let (y, p) = m.apply(r.yaw_deg, r.pitch_deg);

                    (y - r.want_yaw_deg).abs().max((p - r.want_pitch_deg).abs())
                })
                .fold(0.0_f64, f64::max)
        };

        assert!(
            worst(&tps) < worst(&quad) * 0.5,
            "spline {:.2} should clearly beat quadratic {:.2} on a kink",
            worst(&tps), worst(&quad),
        );

        // And it must do so without misbehaving between the targets, which is the whole
        // reason the gate exists.
        assert!(
            tps.is_sane(&points, MIN_GAIN, MAX_GAIN),
            "spline gain {:?}",
            tps.gain_bounds(&points),
        );
    }

    #[test]
    fn a_spline_puts_one_centre_per_target_not_one_per_sample() {
        let g = desk();
        let c = cam();

        // Given a centre per sample a spline interpolates the sweep's own jitter, which
        // is a recording of the noise rather than a correction. It also turns a 79 by 79
        // solve into a 3419 by 3419 one.
        let obs  = sweep_with(&g, &c, 1.0, |y, p| (y * 1.2, p));
        let rows: Vec<AngleRow> = obs.iter().filter_map(target_row).collect();

        let samples: usize = obs.iter().map(|o| o.samples.len()).sum();
        assert!(samples > rows.len() * 4, "the test sweep must have many samples per target");

        let report = fit(&g, &c, &obs, 2.5);

        if let AngleCorrection::Tps(t) = &report.calibration.angle {
            assert_eq!(t.centres.len(), rows.len());
        }
    }

    #[test]
    fn a_spline_extrapolates_along_its_trend_instead_of_diverging() {
        let g = desk();
        let c = cam();

        let obs  = sweep_with(&g, &c, 0.0, |y, p| (y * 1.3 + 2.0, p * 0.9));
        let rows: Vec<AngleRow> = obs.iter().filter_map(target_row).collect();
        let tps  = TpsWarp::fit(&rows, 0.1).unwrap();

        // The radial term grows like r^2 log r, so an unclamped query far outside the
        // calibrated region would run away. Well outside it, the correction must stay
        // finite and stay roughly on the linear trend the sweep measured.
        for yaw in [-200.0, -120.0, 120.0, 200.0] {
            let (y, p) = tps.apply(yaw, 0.0);

            assert!(y.is_finite() && p.is_finite(), "diverged at yaw {yaw}: ({y}, {p})");
            assert!(
                y.abs() < yaw.abs() * 2.0 + 90.0,
                "runaway extrapolation at yaw {yaw}: {y}",
            );
        }
    }

    #[test]
    fn the_model_is_chosen_on_held_out_error_and_every_candidate_is_reported() {
        let g = desk();
        let c = cam();

        let obs    = sweep_with(&g, &c, 0.4, |y, p| (y * 1.2 + 2.0, p * 0.9 - 1.0));
        let report = fit(&g, &c, &obs, 2.5);

        // Every shape, each with and without the pixel stage.
        assert_eq!(report.candidates.len(), crate::angle::candidates().len() * 2);

        let chosen = &report.candidates[report.chosen.expect("a model must be chosen")];
        let best   = report
            .candidates
            .iter()
            .map(|c| c.rms_loo_deg)
            .fold(f64::INFINITY, f64::min);

        assert!((chosen.rms_loo_deg - best).abs() < 1.0e-12, "the best model must win");

        // In-sample error is never worse than held-out for the model that was kept, which
        // is the whole reason the gate uses the latter.
        assert!(chosen.rms_deg <= chosen.rms_loo_deg + 1.0e-9);

        // The stored numbers agree with the candidate that won.
        assert!((report.calibration.rms_loo_deg - chosen.rms_loo_deg).abs() < 1.0e-12);
        assert!((report.calibration.rms_deg - chosen.rms_deg).abs() < 1.0e-12);
    }

    #[test]
    fn the_pixel_stage_is_dropped_when_it_does_not_help() {
        let g = desk();
        let c = cam();

        // A pure angular distortion is fully described by stage one. Stage two can then
        // only add free parameters, so held-out error must reject it.
        let obs    = sweep_with(&g, &c, 0.0, |y, p| (y + 2.0, p - 1.5));
        let report = fit(&g, &c, &obs, 2.5);

        let chosen = &report.candidates[report.chosen.unwrap()];
        assert!(!chosen.stage_two, "the pixel stage should not have been kept");
        assert!(report.calibration.outputs.is_empty());
    }

    #[test]
    fn a_fit_with_no_observations_is_the_identity_and_reports_nothing_chosen() {
        let g      = desk();
        let c      = cam();
        let report = fit(&g, &c, &[], 2.5);

        assert!(report.chosen.is_none());
        assert_eq!(report.calibration.angle, AngleCorrection::identity());

        // Every candidate is accounted for, none silently dropped.
        assert_eq!(report.candidates.len(), crate::angle::candidates().len() * 2);
        assert!(report.candidates.iter().all(|c| c.rejected == Some(Rejection::Unfittable)));

        // Nothing to measure, so nothing is claimed.
        assert_eq!(report.calibration.rms_deg, 0.0);
    }

    #[test]
    fn a_sweep_no_model_survives_reports_the_uncorrected_error_not_zero() {
        let g = desk();
        let c = cam();

        // A gain so aggressive that no sane polynomial inverts it. The fit must report
        // what the system will actually do, which is nothing, at the raw error. Claiming
        // zero would sail through the acceptance gate and ship an identity as perfect.
        let saturate = |a: f64| {
            let scale = 20.0_f64.to_radians();

            (scale * (5.0 * a.to_radians() / scale).atan()).to_degrees()
        };

        let obs    = sweep_with(&g, &c, 0.0, move |y, p| (saturate(y), saturate(p)));
        let report = fit(&g, &c, &obs, 2.5);

        if report.chosen.is_none() {
            assert!(
                report.calibration.rms_deg > 5.0,
                "an uncorrected calibration must report its real error, got {}",
                report.calibration.rms_deg,
            );
            assert_eq!(report.calibration.rms_loo_deg, report.calibration.rms_deg);
            assert!(report.calibration.note.contains("NO MODEL"));
        }
    }

    #[test]
    fn a_sweep_needs_a_geometry_and_at_least_one_target() {
        assert!(matches!(
            CalibrationSweep::create().build(),
            Err(SweepError::MissingGeometry),
        ));

        assert!(matches!(
            CalibrationSweep::create().geometry(desk()).targets(Vec::new()).build(),
            Err(SweepError::NoTargets),
        ));
    }

    #[test]
    fn the_collect_window_is_never_longer_than_the_dwell() {
        let sweep = CalibrationSweep::create()
            .geometry(desk())
            .dwell_s(0.5)
            .collect_s(3.0)
            .build()
            .unwrap();

        assert_eq!(sweep.collect_s, 0.5);
    }

    #[test]
    fn collection_records_a_miss_instead_of_clamping_it() {
        let g      = desk();
        let c      = cam();
        let target = SweepTarget { output: "DP-1".to_string(), px: GlobalPx { x: 4479.0, y: 800.0 } };

        // Aim well above the panel: every sample leaves the desk. The sweep must keep the
        // angles and record the misses, not manufacture points on a bezel.
        let truth = g.px_to_ray(target.px).unwrap();
        let off   = DesktopGeometry::perturb_ray(&truth, 0.0, 40.0);

        let readings: Vec<Reading> = (0..6).map(|i| reading(&g, &c, off.dir, i)).collect();
        let outcome  = sweep_once(&g, &c, target, readings);
        let obs      = &outcome.observations[0];

        assert_eq!(obs.samples.len(), 6, "the samples must be kept");
        assert_eq!(obs.diagnostics.missed, 6, "and every one recorded as a miss");
        assert_eq!(obs.observed_px, None, "no landing point may be invented");
        assert_eq!(obs.spread_px, None);
        assert!(obs.spread_deg.is_finite());
    }

    #[test]
    fn the_spread_is_reported_in_degrees_and_survives_identical_samples() {
        let g      = desk();
        let c      = cam();
        let target = SweepTarget { output: "DP-1".to_string(), px: GlobalPx { x: 4479.0, y: 800.0 } };
        let truth  = g.px_to_ray(target.px).unwrap();

        // A 1 degree scatter either side of the mean.
        let readings: Vec<Reading> = (0..8)
            .map(|i| {
                let sign = if i % 2 == 0 { 1.0 } else { -1.0 };

                reading(&g, &c, DesktopGeometry::perturb_ray(&truth, sign * 1.0, 0.0).dir, i)
            })
            .collect();

        let scattered = sweep_once(&g, &c, target.clone(), readings);
        let spread    = scattered.observations[0].spread_deg;

        assert!(spread.is_finite(), "spread_deg must never be NaN");
        assert!((spread - 1.0).abs() < 0.05, "spread_deg = {spread}");

        // The angle between a direction and itself is the case a careless acos turns into
        // a NaN, and a perfectly still stream hits it every sample.
        let still: Vec<Reading> = (0..8).map(|i| reading(&g, &c, truth.dir, i)).collect();
        let obs   = sweep_once(&g, &c, target, still);

        assert_eq!(obs.observations[0].spread_deg, 0.0);
        assert!(obs.observations[0].spread_px.unwrap() < 1.0e-6);
        assert!(obs.observations[0].diagnostics.yaw_deg_sd < 1.0e-9);
    }

    #[test]
    fn the_diagnostics_record_the_raw_angles_validity_and_misses() {
        let g      = desk();
        let c      = cam();
        let target = SweepTarget { output: "DP-1".to_string(), px: GlobalPx { x: 4479.0, y: 800.0 } };
        let truth  = g.px_to_ray(target.px).unwrap();

        let mut readings: Vec<Reading> = (0..6).map(|i| reading(&g, &c, truth.dir, i)).collect();

        let off_desk = DesktopGeometry::perturb_ray(&truth, 0.0, 40.0);
        readings.push(reading(&g, &c, off_desk.dir, 6));
        readings.push(reading(&g, &c, off_desk.dir, 7));

        readings.push(Reading {
            sample  : gaze_core::GazeSample {
                t_s: 0.3, ray: None, point: None, sigma_deg: f64::MAX, valid: false,
            },
            message : Some(crate::protocol::SidecarMessage::invalid(0.3, 8)),
            raw     : None,
            meta    : None,
        });

        let outcome = sweep_once(&g, &c, target, readings);
        let d       = &outcome.observations[0].diagnostics;

        assert_eq!(d.samples_seen, 9);
        assert_eq!(d.missed, 2, "the two off-desk rays must be counted as misses");
        assert!((d.valid_fraction - 8.0 / 9.0).abs() < 1.0e-9, "valid = {}", d.valid_fraction);
        assert!((d.conf_mean - 0.8).abs() < 1.0e-9);

        let want = gaze_yaw_pitch_deg(c.dir_to_camera(truth.dir)).unwrap();
        assert!((d.target_yaw_deg - want.0).abs() < 0.5, "{} vs {}", d.target_yaw_deg, want.0);

        let eye = c.point_to_camera(g.eye());
        assert!((d.eye_mm_mean[2] - eye.z).abs() < 1.0e-6);
    }

    #[test]
    fn the_angular_gain_recovers_a_model_that_under_reports() {
        let g    = desk();
        let c    = cam();
        let gain = 0.6_f64;

        let observations: Vec<Observation> = default_targets(&g, 3, 0.12, false)
            .into_iter()
            .filter(|t| t.output == "DP-1")
            .filter_map(|t| observation_with(&g, &c, t, 2, 0.0, |y, p| (y * gain, p * gain)))
            .collect();

        assert_eq!(observations.len(), 9);

        let gains = gains(&observations);
        assert_eq!(gains.len(), 1);

        let yaw_gain   = gains[0].gain_deg_yaw.expect("a nine point grid must yield a yaw gain");
        let pitch_gain = gains[0].gain_deg_pitch.expect("and a pitch gain");

        assert!((yaw_gain - gain).abs() < 0.02, "yaw gain = {yaw_gain}");
        assert!((pitch_gain - gain).abs() < 0.05, "pitch gain = {pitch_gain}");
    }

    #[test]
    fn a_gain_of_one_reads_as_one() {
        let g = desk();
        let c = cam();

        let observations: Vec<Observation> = default_targets(&g, 3, 0.12, false)
            .into_iter()
            .filter(|t| t.output == "DP-2")
            .filter_map(|t| observation_with(&g, &c, t, 1, 0.0, |y, p| (y, p)))
            .collect();

        let gains = gains(&observations);

        assert!((gains[0].gain_deg_yaw.unwrap() - 1.0).abs() < 0.02);
        assert!((gains[0].gain_px_x.unwrap() - 1.0).abs() < 0.02);
        assert!((gains[0].gain_px_y.unwrap() - 1.0).abs() < 0.02);
    }

    #[test]
    fn a_slope_needs_two_distinct_points_and_says_so_otherwise() {
        assert_eq!(ls_slope(&[], &[]), None);
        assert_eq!(ls_slope(&[1.0], &[2.0]), None);
        assert_eq!(ls_slope(&[3.0, 3.0, 3.0], &[1.0, 2.0, 3.0]), None, "no spread in x");
        assert_eq!(ls_slope(&[0.0, 1.0], &[0.0, 1.0, 2.0]), None, "mismatched lengths");

        let slope = ls_slope(&[0.0, 1.0, 2.0], &[1.0, 1.5, 2.0]).unwrap();
        assert!((slope - 0.5).abs() < 1.0e-12);
    }

    #[test]
    fn the_summary_averages_the_per_target_scatter() {
        assert_eq!(summarise(&[]), SweepSummary::default());

        let g   = desk();
        let c   = cam();
        let obs = sweep_with(&g, &c, 2.0, |y, p| (y, p));
        let s   = summarise(&obs);

        assert_eq!(s.targets, obs.len());
        assert!((s.valid_fraction - 1.0).abs() < 1.0e-9);
        assert!(s.spread_deg > 1.0, "the 2 degree wobble must show up: {}", s.spread_deg);
        assert!(s.yaw_sd_deg > 0.5);
    }

    #[test]
    fn a_sweep_round_trips_through_its_readings_log_and_scores_the_same() {
        let g = desk();
        let c = cam();

        let obs     = sweep_with(&g, &c, 0.5, |y, p| (y * 1.3 + 2.0, p * 0.85 - 1.0));
        let records = to_records(&obs);

        assert_eq!(records.len(), obs.iter().map(|o| o.samples.len()).sum::<usize>());

        // A refit from the log must be a true replay, not an approximation: this is what
        // makes it possible to try a new fitting model without asking the user to sit
        // through another sweep.
        let back = from_records(&g, &c, &records);

        assert_eq!(back.len(), obs.len());

        let before = fit(&g, &c, &obs, 2.5);
        let after  = fit(&g, &c, &back, 2.5);

        assert!(
            (before.calibration.rms_loo_deg - after.calibration.rms_loo_deg).abs() < 1.0e-9,
            "{} vs {}",
            before.calibration.rms_loo_deg,
            after.calibration.rms_loo_deg,
        );
        assert_eq!(before.calibration.angle.name(), after.calibration.angle.name());
    }

    #[test]
    fn the_readings_log_carries_head_rotation_and_the_verbatim_line() {
        let g = desk();
        let c = cam();

        let target = SweepTarget { output: "DP-1".to_string(), px: GlobalPx { x: 4479.0, y: 800.0 } };
        let raw    = r#"{"seq":3,"valid":true,"gaze_iris":[0,0,-1]}"#;

        let sample = AngleSample {
            eye_cam_mm     : c.point_to_camera(g.eye()),
            gaze_cam       : gaze_dir_from_yaw_pitch_deg(5.0, 2.0),
            yaw_deg        : 5.0,
            pitch_deg      : 2.0,
            want_yaw_deg   : 4.0,
            want_pitch_deg : 1.0,
            missed         : false,
            head_rot       : Some([0.1, -0.2, 0.05]),
            raw            : Some(std::sync::Arc::from(raw)),
        };

        let obs     = rebuild(&g, &c, target, vec![sample]).unwrap();
        let records = to_records(&[obs]);

        assert_eq!(records[0].head_rot, Some([0.1, -0.2, 0.05]));
        assert_eq!(records[0].raw.as_deref(), Some(raw));

        // And both survive the trip back, so an experiment run months later sees them.
        let back = from_records(&g, &c, &records);
        assert_eq!(back[0].samples[0].head_rot, Some([0.1, -0.2, 0.05]));
        assert_eq!(back[0].samples[0].raw.as_deref(), Some(raw));

        // The per-target head rotation makes it into the diagnostics too.
        assert!((back[0].diagnostics.head_rot_mean[0] - 0.1).abs() < 1.0e-12);
    }

    #[test]
    fn a_sweep_without_head_rotation_still_round_trips() {
        let g = desk();
        let c = cam();

        // Readings written before those fields existed must keep working.
        let obs     = sweep_with(&g, &c, 0.0, |y, p| (y + 1.0, p));
        let records = to_records(&obs);

        assert!(records.iter().all(|r| r.head_rot.is_none() && r.raw.is_none()));

        let json = serde_json::to_string(&records[0]).unwrap();
        assert!(!json.contains("head_rot"), "absent fields must not bloat the log: {json}");

        let back = from_records(&g, &c, &records);
        assert_eq!(back.len(), obs.len());
    }

    #[test]
    fn the_px_equivalent_reports_a_figure_for_every_enabled_output() {
        let g  = desk();
        let px = px_equivalent(&g, 1.0);

        assert_eq!(px.len(), 3);

        for (name, value) in &px {
            assert!(*value > 20.0 && *value < 200.0, "{name} reports {value} px per degree");
        }

        // Zero degrees is zero pixels everywhere.
        assert!(px_equivalent(&g, 0.0).iter().all(|(_, v)| *v == 0.0));
    }
}
