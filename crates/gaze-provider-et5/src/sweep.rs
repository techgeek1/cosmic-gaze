//! The compound calibration sweep: one animated stop-and-go pass per display, with the
//! pauses feeding the on-device eye model (on the tracker's own display) and the
//! pose-from-rays solve, and the glides feeding the dense correction field.
//!
//! # Why stop-and-go
//!
//! The device only accepts calibration points at fixations, and fixation anchors are
//! immune to the lag ambiguity of smooth pursuit; but a static grid alone is sparse
//! exactly where error is largest. The glides between stops add hundreds of samples
//! per display at moderate speed (pursuit gain stays near one below ~20 deg/s), the
//! pauses anchor absolute accuracy, and the same pass yields the ray-to-target
//! correspondences that solve where each panel physically is (see `crate::pose`).
//!
//! # Order matters
//!
//! Committing the eye model changes every subsequent frame, so all client-side
//! fitting data must be collected after the commit. In steady state nothing commits:
//! the on-device model persists in flash, so the data pass is the whole sweep. Only
//! a retrain (first run, `--retrain`) prepends the model-update ceremony: declare
//! the plane implied by the configured desk geometry — the mount is rigid and
//! tape-measurable, which beats every gaze-derived estimate of it — feed the ring
//! of device points, commit, and run the data pass on top of the fresh model. The
//! grid anchors then double as the trained-model health check; the hold
//! triangulation is kept as a diagnostic only.
//!
//! # Lag
//!
//! Gaze trails a moving target by tracker latency plus pursuit latency. The lag is
//! estimated per display by shifting the target trajectory until it best matches the
//! gaze track over the glides, and only lag-shifted glide samples enter the field fit.
//! Catch-up saccades are cut by an angular-velocity gate before any fitting.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use glam::DVec3;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};
use gaze_core::{DesktopGeometry, GlobalPx, OutputGeometry, Ray, Rect};
use gaze_overlay::{OverlayHandle, OverlayState};

use crate::calibration::{
    CALIBRATION_FORMAT, Et5Calibration, HeadGain, OutputCalibration, OutputPose,
    VIRTUAL_AREA, normalise,
};
use crate::device::{Device, DeviceError};
use crate::field::{FieldRow, fit_best};
use crate::gaze::{Et5Frame, EyeCombiner, filtered_ray};
use crate::pose::{PointObservation, PoseObservation, solve_pose, solve_pose_points};
use crate::triangulate::intersect_rays;
use crate::ttp::DisplayArea;

/// Side of the target square drawn at each position, logical pixels.
pub(crate) const TARGET_PX: f64 = 40.0;

/// Animation tick. Short enough that the target motion looks continuous and the gaze
/// channel never backs up.
const TICK: Duration = Duration::from_millis(8);

/// Angular speed of the gliding target, degrees per second. Below this pursuit gain
/// stays near one; faster and the eye falls behind in a way lag shifting cannot fix.
const GLIDE_DEG_S: f64 = 14.0;

/// Pixels per degree fallback when the geometry cannot supply a local scale.
pub(crate) const FALLBACK_PX_PER_DEG: f64 = 60.0;

/// Angular velocity above which a frame is a saccade, degrees per second.
const SACCADE_DEG_S: f64 = 80.0;

/// Window cut around each saccade frame, seconds, covering its tails.
const SACCADE_PAD_S: f64 = 0.06;

/// Largest angular error a glide sample may have and still enter the field fit.
/// Beyond this the user was not looking at the target (a glance away, a blink edge).
const OUTLIER_DEG: f64 = 5.0;

/// Largest angular error a fixation anchor may have and still enter the field fit.
/// Looser than the glide gate: anchors are deliberate fixations, and a ring-trained
/// model legitimately runs 5-6 degrees off at the panel corners — that is exactly
/// the systematic error the field exists to learn. Only mistracks die here.
const ANCHOR_OUTLIER_DEG: f64 = 10.0;

/// Lag search range, seconds.
const LAG_MAX_S: f64 = 0.35;

/// Lag search step, seconds.
const LAG_STEP_S: f64 = 0.01;

/// Lag used when a display's glides yield too few samples to estimate one.
const LAG_DEFAULT_S: f64 = 0.12;

/// Minimum moving samples for a lag estimate.
const LAG_MIN_SAMPLES: usize = 30;

/// Minimum frames a pause window needs to contribute a pose observation.
const STOP_MIN_FRAMES: usize = 5;

/// Length of each head-sweep hold at the end of a data pass, seconds. The user
/// fixates the dot while moving their head through all three axes — the sideways
/// sweep is the triangulation baseline, and the depth leg is what makes the
/// head-gain regression's z column observable at all.
const PARALLAX_HOLD_S: f64 = 10.0;

/// The parallax hold is sliced into chunks this long, each contributing its own pose
/// observation, so the head's path becomes many distinct ray origins instead of one
/// collapsed median.
const PARALLAX_CHUNK_S: f64 = 0.4;

/// Device-pass ring stops around the centre point.
const RING_POINTS: usize = 8;

/// Ring semi-axis across the panel width, uv units. Sized to stay well inside the
/// tracking envelope on an oversized panel.
const RING_RX: f64 = 0.20;

/// Ring semi-axis down the panel height, uv units.
const RING_RY: f64 = 0.30;

/// Lean-in dots appended to the device ring: the central diamond plus the centre,
/// collected while the user leans toward the screen. A single-posture training set
/// leaves the model's head-position terms free to overfit that posture — measured
/// on this unit as large, mostly origin-unpredictable error under head motion. A
/// second posture in the training set constrains them at the source.
const LEAN_DOTS: [(f64, f64); 5] =
    [(0.5, 0.5), (0.35, 0.5), (0.65, 0.5), (0.5, 0.35), (0.5, 0.65)];

/// Hold dots for the plane diagnostic and the head-gain fit: a mid-radius diamond.
/// Triangulation alone would prefer the middle third (tracking noise floor), but the
/// holds also excite the head gain's position-interaction terms, which otherwise
/// extrapolate three times past their fitted range at the panel edges; mid-radius is
/// the compromise, still comfortably inside the tracking envelope.
const TRI_DOTS: [(f64, f64); 4] = [(0.25, 0.5), (0.75, 0.5), (0.5, 0.25), (0.5, 0.75)];

/// Minimum saccade-gated rays before one eye's bundle is triangulated.
const TRI_MIN_RAYS: usize = 40;

/// Minimum lateral spread of one eye's origins over a hold, millimetres. Below this
/// the head sweep did not happen and the bundle only carries vergence baseline.
const TRI_MIN_SPREAD_MM: f64 = 25.0;

/// Largest acceptable RMS ray-to-point distance for a triangulated dot, millimetres.
const TRI_MAX_RMS_MM: f64 = 60.0;

/// Median anchor error beyond which the trained mapping no longer matches the panel
/// (a moved tracker, or a lost on-device model), in the field's normalised [-1, 1]
/// units (0.10 is ~5% of the panel).
const MODEL_HEALTH_WARN_NORM: f64 = 0.10;

/// Largest acceptable point-pose RMS for the triangulated plane to be declared,
/// millimetres. Beyond this the points disagree with the panel's known shape.
const PLANE_MAX_RMS_MM: f64 = 25.0;

/// Viewing distance used to express plane-pass residuals in degrees for the shared
/// summary fields, millimetres.
const NOMINAL_VIEW_MM: f64 = 650.0;

/// Minimum binocular frames for the origin-scale fit.
const SCALE_MIN_FRAMES: usize = 100;

/// Minimum head-depth range before the origin-scale fit is trusted, millimetres.
/// Below this the scale error is constant over the pass, and a constant scale is
/// harmless: uniform scaling about the tracker preserves every gaze direction, and
/// the triangulated plane is measured with the same origins the firmware uses.
const SCALE_MIN_Z_RANGE_MM: f64 = 30.0;

/// Clamp on the origin conditioning factor.
const SCALE_FACTOR_MIN: f64 = 0.9;

/// Clamp on the origin conditioning factor.
const SCALE_FACTOR_MAX: f64 = 1.1;

/// Minimum frames for the head-gain regression and its reference origin.
const HEAD_GAIN_MIN_FRAMES: usize = 120;

/// Minimum head-position spread over the holds for the regression, millimetres.
const HEAD_GAIN_MIN_SPREAD_MM: f64 = 25.0;

/// Cap on a credible head gain, uv per millimetre: more than ~10% of the panel per
/// 40 mm of head motion is fitting noise, not parallax.
const HEAD_GAIN_MAX_UV_PER_MM: f64 = 0.0025;

/// Cap on a credible rotation-channel gain, uv per millimetre of interocular
/// deviation. Rotation deviations are small (a 20-degree yaw is ~22 mm of dz), so
/// the credible gains are an order larger than the origin ones.
const HEAD_GAIN_ROT_MAX_UV_PER_MM: f64 = 0.02;

/// Candidate head-position lags for the gain fit, seconds. The model's head error
/// trails the head: cross-validated on real holds, the instantaneous regression
/// does not generalise at all (leave-one-hold-out R2 -0.05) while ~300 ms does
/// (0.27). The fit keeps whichever lag predicts best.
const HEAD_GAIN_LAGS: [f64; 5] = [0.0, 0.1, 0.2, 0.3, 0.4];

/// Largest origin-to-gaze pairing gap when applying a candidate lag, seconds.
const HEAD_GAIN_PAIR_GAP_S: f64 = 0.06;

// --- Configuration ---

/// Sweep parameters. `Default` is the intended starting point.
#[derive(Clone, Debug)]
pub struct SweepConfig {
    /// Stop grid width per display.
    pub grid_cols      : usize,
    /// Stop grid height per display.
    pub grid_rows      : usize,
    /// Margin of the stop grid from the display edges, fraction of the size.
    pub inset          : f64,
    /// Settling time at each stop before collection, seconds.
    pub settle_s       : f64,
    /// Collection window at each stop, seconds.
    pub collect_s      : f64,
    /// Run the on-device calibration pass first.
    pub device_points  : bool,
    /// Connector name of the display the tracker's display area is declared on.
    pub device_output  : String,
    /// Restrict the sweep to these displays; `None` sweeps every enabled output.
    pub displays       : Option<Vec<String>>,
    /// Cap on glide rows entering one display's field fit (leave-one-out cost grows
    /// with the square of the row count).
    pub max_glide_rows : usize,
    /// Where raw pass data (frames, trajectory, stop windows) is appended as JSONL,
    /// so fits can be re-run and diagnosed offline without another sweep.
    pub raw_out        : Option<PathBuf>,
    /// The tracker display's physical plane by corners (from a previous solved pose),
    /// declared during the device pass instead of the flat first-run guess. The
    /// on-device model is only as good as the plane its points were declared on.
    pub device_corners : Option<DisplayArea>,
    /// Pitch of the sensor frame relative to the desk frame, degrees: the ET5's
    /// mount wedge angles the device up at the face, and it reports (and expects
    /// plane declarations) in its own tilted frame. Measured on this setup as ~13
    /// degrees: the reported eye origins sit ~165 mm below the true seated eye
    /// height, which is exactly this rotation. Comes from `tracker_pitch_deg` in
    /// the desk config.
    pub tracker_pitch_deg : f64,
    /// Force the model-update ceremony (plane pass, device points, commit) even
    /// when a trained plane is already stored. Without this the on-device model is
    /// only trained on the first run, and kept afterwards.
    pub retrain        : bool,
    /// Append the lean-in posture dots to the device ring, so the on-device model
    /// trains at two head positions instead of one.
    pub lean_ring      : bool,
    /// Device pass on the ring layout (centre + `RING_POINTS` stops inside the
    /// tracking envelope) instead of the full stop grid. The eye model needs
    /// angular diversity from clean fixations, not field density, and a ring keeps
    /// degraded envelope-edge samples out of its fit; the dense grid still runs
    /// once, in the data pass, for the field.
    pub device_ring    : bool,
    /// Direct mode: sweep only the tracker's display, keep its trained plane
    /// declared for the data pass, and fit the correction field on the firmware's
    /// trained 2D output instead of reconstructed rays. This is the accurate path
    /// for a single-display setup; the browser-demo behaviour with a field on top.
    pub direct         : bool,
    /// Directory of archived pass files from earlier runs (calibrates and
    /// collects), pooled into the head-gain fit. `None` fits the current pass
    /// alone.
    pub history_dir    : Option<PathBuf>,
    /// Identity of the committed on-device model (a hash of its blob backup),
    /// naming which archived passes are compatible. The head gain corrects that
    /// specific model's posture error, so a retrain orphans the old history.
    pub history_key    : Option<String>,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            grid_cols      : 5,
            grid_rows      : 3,
            inset          : 0.06,
            settle_s       : 0.5,
            collect_s      : 0.8,
            device_points  : true,
            device_output  : "HDMI-A-1".into(),
            displays       : None,
            max_glide_rows : 240,
            raw_out        : None,
            device_corners : None,
            tracker_pitch_deg : 0.0,
            retrain        : false,
            lean_ring      : true,
            device_ring    : true,
            direct         : true,
            history_dir    : None,
            history_key    : None,
        }
    }
}

/// User input during a sweep.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SweepKey {
    /// End the current collection early.
    Advance,
    /// The user says they are on the target now: the controller's pad click, or `c`.
    /// The retrain treats it as the acceptance itself, checked against the gate's
    /// vote; everything else treats it as `Advance`.
    Commit,
    /// Skip the current stop (behind a bezel, uncomfortable).
    Skip,
    /// Abort the sweep.
    Quit,
}

/// Spawns a stdin reader translating lines into sweep keys: empty line advances,
/// `c` commits, `s` skips, `q` quits.
pub fn terminal_keys() -> Receiver<SweepKey> {
    let (tx, rx) = crossbeam_channel::unbounded();

    terminal_keys_into(tx);

    rx
}

/// The stdin reader behind [`terminal_keys`], feeding a channel the caller owns so
/// another source (a controller) can share it.
pub fn terminal_keys_into(tx: Sender<SweepKey>) {
    std::thread::Builder::new()
        .name("sweep-keys".into())
        .spawn(move || {
            let stdin = std::io::stdin();

            for line in stdin.lock().lines() {
                let Ok(line) = line else {
                    break;
                };

                let key = {
                    match line.trim() {
                        ""  => SweepKey::Advance,
                        "c" => SweepKey::Commit,
                        "s" => SweepKey::Skip,
                        "q" => SweepKey::Quit,
                        _   => continue,
                    }
                };

                if tx.send(key).is_err() {
                    break;
                }
            }
        })
        .expect("spawn key thread");
}

// --- Outcome ---

/// What one display's fit looked like, for the report.
#[derive(Clone, Debug)]
pub struct DisplaySummary {
    pub name           : String,
    /// Stops that contributed pose observations.
    pub targets        : usize,
    /// RMS angular residual of the pose solve, degrees.
    pub pose_rms_deg   : f64,
    /// How far the solved position moved from the configured guess, millimetres.
    pub pose_shift_mm  : f64,
    /// Estimated lag on this display, seconds.
    pub lag_s          : f64,
    /// Glide rows that entered the field fit.
    pub glide_rows     : usize,
    /// Frames dropped by the saccade gate.
    pub saccade_frames : usize,
    /// Leave-one-out RMS of the chosen field, normalised units.
    pub field_rms_norm : f64,
}

/// A completed sweep.
pub struct SweepOutcome {
    /// The fitted client-side calibration.
    pub calibration : Et5Calibration,
    /// Per-display fit reports, in sweep order.
    pub summaries   : Vec<DisplaySummary>,
    /// The on-device calibration blob, when the device pass ran and committed.
    pub device_blob : Option<Vec<u8>>,
}

// --- The sweep ---

/// Runs the whole sweep: optional device pass, then a data pass and fit per display.
pub fn run_sweep(
    device   : &mut Device,
    geometry : &DesktopGeometry,
    overlay  : &OverlayHandle,
    keys     : Option<&Receiver<SweepKey>>,
    config   : &SweepConfig,
)
    -> Result<SweepOutcome, SweepError>
{
    let frames_rx = device.gaze_stream();
    let t0        = Instant::now();

    // Sweep order: the tracker's display first, so the device pass commits its model
    // before any data used for fitting is collected.
    let mut outputs: Vec<&OutputGeometry> = geometry.outputs.iter()
        .filter(|o| o.enabled)
        .filter(|o| {
            // Direct mode fits the firmware's trained 2D, which only exists for the
            // tracker's own display.
            if config.direct {
                return o.name == config.device_output;
            }

            config.displays.as_ref().is_none_or(|names| names.contains(&o.name))
        })
        .collect();

    if outputs.is_empty() {
        return Err(SweepError::NoDisplays);
    }

    outputs.sort_by_key(|o| o.name != config.device_output);

    // Whether this sweep retrains the on-device model. Steady state keeps the
    // committed model and its stored plane; only a first run or `--retrain` trains.
    let stored_area = config.device_corners;

    let retrain = config.direct
        && config.device_points
        && outputs[0].name == config.device_output
        && (config.retrain || stored_area.is_none());

    // The plane the on-device model was (or will be) trained against. A retrain
    // declares the plane implied by the configured desk geometry: the mount is
    // rigid and tape-measurable to a couple of centimetres, which beats every
    // gaze-derived estimate of it (measured: per-eye triangulation lands 120-350 mm
    // off the panel under large head sweeps — the eye model's directions bend
    // systematically away from the trained posture, so rays are the weakest signal
    // in the system). Steady state re-declares the stored plane verbatim: the
    // trained 2D output only applies under the exact plane it was trained on.
    let trained_area = {
        match (retrain, stored_area) {
            (false, Some(area)) => area,
            _                   => {
                // Freshly derived corners are in the desk frame; the device wants
                // its own (pitched) frame. A stored plane is already sensor-frame.
                let c = plane_corners(outputs[0]);

                DisplayArea {
                    tl_mm : desk_to_sensor(c.tl_mm, config.tracker_pitch_deg),
                    tr_mm : desk_to_sensor(c.tr_mm, config.tracker_pitch_deg),
                    bl_mm : desk_to_sensor(c.bl_mm, config.tracker_pitch_deg),
                }
            }
        }
    };

    // The model-update ceremony, retrain only: declare the configured plane and
    // train the on-device model against it. In steady state none of this runs and
    // the data pass is the whole sweep.
    let device_blob = {
        if retrain {
            info!("device pass plane (from desk config): tl=({:.0},{:.0},{:.0})",
                  trained_area.tl_mm[0], trained_area.tl_mm[1], trained_area.tl_mm[2]);
            device.set_display_area_corners(trained_area).map_err(SweepError::Device)?;
            std::thread::sleep(Duration::from_millis(200));

            run_device_pass(device, geometry, outputs[0], overlay, keys, config, t0,
                            &frames_rx)?
        }
        else if !config.direct
            && config.device_points
            && outputs[0].name == config.device_output
        {
            // Legacy ray-mode device pass on the configured plane.
            device.set_display_area_corners(trained_area).map_err(SweepError::Device)?;
            std::thread::sleep(Duration::from_millis(200));

            run_device_pass(device, geometry, outputs[0], overlay, keys, config, t0,
                            &frames_rx)?
        }
        else {
            if config.direct && config.device_points {
                info!("keeping the committed on-device model (retrain with --retrain)");
            }

            None
        }
    };

    if config.direct {
        // Keep the trained plane declared; the data pass fits the trained 2D itself.
        device.set_display_area_corners(trained_area).map_err(SweepError::Device)?;
    }
    else {
        // Multi-display ray fitting needs unclamped rays on every display.
        device.set_display_area(VIRTUAL_AREA).map_err(SweepError::Device)?;
    }

    std::thread::sleep(Duration::from_millis(200));

    // Data passes and fits.
    let mut entries   = Vec::new();
    let mut summaries = Vec::new();

    for out in &outputs {
        info!("sweeping {} for pose and field", out.name);

        let pass = run_pass(None, geometry, out, overlay, keys, config, t0, &frames_rx)?;

        // Persist the raw pass before fitting, so a failed fit still leaves the data
        // for offline diagnosis.
        if let Some(path) = &config.raw_out
            && let Err(e) = save_pass(path, &out.name, &pass) {
                warn!("could not save raw sweep data to {}: {e}", path.display());
            }

        // Archived passes pool into the head gain only in steady state: a retrain
        // just replaced the on-device model, so no earlier pass describes it.
        let history = {
            if config.direct && !retrain && out.name == config.device_output {
                load_history(config, &out.name)
            }
            else {
                Vec::new()
            }
        };

        let fit = {
            if config.direct {
                fit_display_direct(geometry, out, &pass, config,
                                   config.direct.then_some(trained_area), &history)
            }
            else {
                fit_display(geometry, out, &pass, config)
            }
        };

        match fit {
            Ok((entry, summary)) => {
                entries.push(entry);
                summaries.push(summary);
            }
            Err(e)               => {
                warn!("{}: fit failed ({e}); display left uncalibrated", out.name);
            }
        }
    }

    let _ = overlay.set(OverlayState {
        gaze       : None,
        highlight  : None,
        truth      : None,
        label      : None,
        background : None,
        pointer    : None,
    });

    if entries.is_empty() {
        return Err(SweepError::NothingFitted);
    }

    // One file-level lag: the median over displays. Per-display values stay in the
    // summaries for the report.
    let lag_s = median(&mut summaries.iter().map(|s| s.lag_s).collect::<Vec<_>>());

    Ok(SweepOutcome {
        calibration : Et5Calibration {
            format             : CALIBRATION_FORMAT,
            created_unix_s     : Et5Calibration::now_unix_s(),
            lag_s              : lag_s,
            device_output      : config.direct.then(|| config.device_output.clone()),
            device_area        : config.direct.then_some(trained_area),
            device_blob_sha256 : None,
            device_result      : None,
            outputs            : entries,
            health             : Vec::new(),
        },
        summaries   : summaries,
        device_blob : device_blob,
    })
}

// --- Passes ---

/// Target trajectory sample: where the target was drawn and whether it was gliding.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TrajPoint {
    /// Host time since the pass started, seconds.
    pub t_s    : f64,
    /// Where the target square was drawn.
    pub px     : GlobalPx,
    /// True while the target was gliding between stops.
    pub moving : bool,
}

/// One gaze frame with its host-side arrival time.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct TimedFrame {
    /// Host arrival time since the pass started, seconds. The device's own
    /// `timestamp_us` is on a different clock from the target trajectory.
    pub t_s   : f64,
    /// The decoded notification.
    pub frame : Et5Frame,
}

/// One pause's collection window.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
pub struct StopWindow {
    /// Target position across the display, [0, 1].
    pub u        : f64,
    /// Target position down the display, [0, 1].
    pub v        : f64,
    /// The same position in global logical pixels.
    pub px       : GlobalPx,
    /// Start of the collection window, seconds since the pass started.
    pub t_start  : f64,
    /// End of the collection window.
    pub t_end    : f64,
    /// True for the parallax hold: the head was deliberately moving, so this window
    /// is sliced into per-chunk pose observations (nailing the plane depth) and
    /// excluded from the field anchors.
    #[serde(default)]
    pub parallax : bool,
}

/// Everything one pass over one display recorded.
#[derive(Default)]
pub(crate) struct PassData {
    pub(crate) frames : Vec<TimedFrame>,
    pub(crate) traj   : Vec<TrajPoint>,
    pub(crate) stops  : Vec<StopWindow>,
}

/// Runs the device calibration pass: cal session open, a point per stop, commit.
/// Returns the committed blob. A rejected commit is reported but does not abort the
/// sweep; the client-side stages still improve on whatever model the device kept.
#[allow(clippy::too_many_arguments)]
fn run_device_pass(
    device    : &mut Device,
    geometry  : &DesktopGeometry,
    out       : &OutputGeometry,
    overlay   : &OverlayHandle,
    keys      : Option<&Receiver<SweepKey>>,
    config    : &SweepConfig,
    t0        : Instant,
    frames_rx : &Receiver<Et5Frame>,
)
    -> Result<Option<Vec<u8>>, SweepError>
{
    if config.device_ring {
        info!("device calibration pass on {} (ring layout, {} points)",
              out.name, RING_POINTS + 1);
    }
    else {
        info!("device calibration pass on {} ({}x{} grid)",
              out.name, config.grid_cols, config.grid_rows);
    }

    device.cal_begin().map_err(SweepError::Device)?;

    run_pass(Some(device), geometry, out, overlay, keys, config, t0, frames_rx)?;

    match device.cal_finish() {
        Ok(blob) => {
            info!("device calibration committed ({} byte blob)", blob.len());

            Ok(Some(blob))
        }
        Err(e)   => {
            warn!("device calibration commit failed: {e}; continuing with the \
                   previous on-device model");

            Ok(None)
        }
    }
}

/// Runs one animated pass over one display. With `cal` set, each stop also feeds
/// cal_add_point while the user fixates.
#[allow(clippy::too_many_arguments)]
fn run_pass(
    mut cal   : Option<&mut Device>,
    geometry  : &DesktopGeometry,
    out       : &OutputGeometry,
    overlay   : &OverlayHandle,
    keys      : Option<&Receiver<SweepKey>>,
    config    : &SweepConfig,
    t0        : Instant,
    frames_rx : &Receiver<Et5Frame>,
)
    -> Result<PassData, SweepError>
{
    let stops = {
        if cal.is_some() && config.device_ring {
            ring_stops(out, config.lean_ring)
        }
        else {
            grid_stops(out, config)
        }
    };
    let total = stops.len();

    let mut pass = PassData {
        frames : Vec::new(),
        traj   : Vec::new(),
        stops  : Vec::new(),
    };

    // Flush anything queued before the pass so window timestamps stay honest.
    while frames_rx.try_recv().is_ok() {}

    let mut current = stops[0].2;

    for (index, (u, v, px)) in stops.iter().cloned().enumerate() {
        let lean = cal.is_some()
            && config.device_ring
            && config.lean_ring
            && index > RING_POINTS;

        let label = {
            if lean {
                format!("{} cal {}/{} — LEAN IN toward the screen", out.name,
                        index + 1, total)
            }
            else if cal.is_some() {
                format!("{} cal {}/{}", out.name, index + 1, total)
            }
            else {
                format!("{} {}/{}", out.name, index + 1, total)
            }
        };

        // Glide to the stop at constant angular speed.
        glide(geometry, overlay, &label, current, px, t0, frames_rx, &mut pass)?;
        current = px;

        // Settle: the saccade and the moment it takes to lock onto the square.
        show_target(overlay, px, &label)?;
        let mut skip = false;

        // Give the posture change a beat before samples count.
        if lean && index == RING_POINTS + 1 {
            wait_draining(1.5, t0, frames_rx, &mut pass, px, false, keys, &mut skip)?;

            if skip {
                continue;
            }
        }

        wait_draining(config.settle_s, t0, frames_rx, &mut pass, px, false,
                      keys, &mut skip)?;

        if skip {
            continue;
        }

        if let Some(device) = cal.as_deref_mut() {
            // The device collects raw samples for the duration of this call; the user
            // is already settled on the target.
            device.cal_add_point(u, v, 3).map_err(SweepError::Device)?;
        }
        else {
            let t_start = t0.elapsed().as_secs_f64();

            wait_draining(config.collect_s, t0, frames_rx, &mut pass, px, false,
                          keys, &mut skip)?;

            if !skip {
                pass.stops.push(StopWindow {
                    u        : u,
                    v        : v,
                    px       : px,
                    t_start  : t_start,
                    t_end    : t0.elapsed().as_secs_f64(),
                    parallax : false,
                });
            }
        }
    }

    // Head-sweep holds, data passes only: fixating a dot while the head moves is
    // what makes the head-gain regression observable, and the same frames feed the
    // triangulation diagnostic. Lateral motion and turns stay at natural amplitude
    // (the gain should be fitted where it is applied, and the eye model degrades at
    // extreme offsets), but depth wants real range: a sustained lean at the desk
    // extrapolates whatever z span the holds covered.
    if cal.is_none() {
        // The head-sweep holds are the sweep's shared triangulation data: the
        // diamond's per-eye ray bundles re-measure the panel plane under the final
        // model (pose + mount verification), the same frames feed the head-gain
        // regression, and the chunked observations back the ray-solve fallback.
        for (u, v) in TRI_DOTS {
            let target = out.uv_to_px(u, v);
            let label  = format!("{} hold: eyes on the dot — lean well in and back, side to side, up/down, small turns",
                                 out.name);

            glide(geometry, overlay, &label, current, target, t0, frames_rx, &mut pass)?;
            current = target;
            show_target(overlay, target, &label)?;

            let mut skip = false;
            let t_start  = t0.elapsed().as_secs_f64();

            wait_draining(PARALLAX_HOLD_S, t0, frames_rx, &mut pass, target, false,
                          keys, &mut skip)?;

            if !skip {
                pass.stops.push(StopWindow {
                    u        : u,
                    v        : v,
                    px       : target,
                    t_start  : t_start,
                    t_end    : t0.elapsed().as_secs_f64(),
                    parallax : true,
                });
            }
        }
    }

    Ok(pass)
}

/// The serpentine stop grid over a display: row-major, alternating direction, so
/// every glide is between neighbours.
fn grid_stops(out: &OutputGeometry, config: &SweepConfig) -> Vec<(f64, f64, GlobalPx)> {
    let mut stops = Vec::with_capacity(config.grid_cols * config.grid_rows);

    for row in 0..config.grid_rows {
        let mut cols: Vec<usize> = (0..config.grid_cols).collect();

        if row % 2 == 1 {
            cols.reverse();
        }

        for col in cols {
            let u = config.inset
                + (1.0 - 2.0 * config.inset) * col as f64 / (config.grid_cols - 1) as f64;
            let v = config.inset
                + (1.0 - 2.0 * config.inset) * row as f64 / (config.grid_rows - 1) as f64;

            stops.push((u, v, out.uv_to_px(u, v)));
        }
    }

    stops
}

/// The device pass's stop layout: the centre plus a ring of `RING_POINTS` stops on
/// an ellipse inside the tracking envelope. The on-device eye model is a
/// low-dimensional personal fit (foveal offset, cornea geometry), so what it needs
/// is angular diversity from well-tracked fixations; the ring covers every gaze
/// direction at a healthy eccentricity, and staying inside the envelope keeps
/// degraded edge samples out of the fit entirely.
fn ring_stops(out: &OutputGeometry, lean: bool) -> Vec<(f64, f64, GlobalPx)> {
    let mut stops = Vec::with_capacity(RING_POINTS + 1 + LEAN_DOTS.len());

    stops.push((0.5, 0.5, out.uv_to_px(0.5, 0.5)));

    for i in 0..RING_POINTS {
        let theta = std::f64::consts::TAU * i as f64 / RING_POINTS as f64;
        let u     = 0.5 + RING_RX * theta.cos();
        let v     = 0.5 + RING_RY * theta.sin();

        stops.push((u, v, out.uv_to_px(u, v)));
    }

    if lean {
        for (u, v) in LEAN_DOTS {
            stops.push((u, v, out.uv_to_px(u, v)));
        }
    }

    stops
}

/// Animates the target from `from` to `to` at the configured angular speed, recording
/// trajectory and draining frames throughout.
#[allow(clippy::too_many_arguments)]
pub(crate) fn glide(
    geometry  : &DesktopGeometry,
    overlay   : &OverlayHandle,
    label     : &str,
    from      : GlobalPx,
    to        : GlobalPx,
    t0        : Instant,
    frames_rx : &Receiver<Et5Frame>,
    pass      : &mut PassData,
)
    -> Result<(), SweepError>
{
    let dx   = to.x - from.x;
    let dy   = to.y - from.y;
    let dist = (dx * dx + dy * dy).sqrt();

    if dist < 1.0 {
        return Ok(());
    }

    // Angular speed to pixel speed via the local scale at the midpoint.
    let mid    = GlobalPx { x: (from.x + to.x) * 0.5, y: (from.y + to.y) * 0.5 };
    let scale  = geometry.px_per_deg(geometry.eye(), mid)
        .map(|(h, v)| (h + v) * 0.5)
        .unwrap_or(FALLBACK_PX_PER_DEG);
    let px_s   = GLIDE_DEG_S * scale;
    let dur_s  = dist / px_s;
    let start  = Instant::now();

    loop {
        let a = (start.elapsed().as_secs_f64() / dur_s).min(1.0);
        let p = GlobalPx { x: from.x + dx * a, y: from.y + dy * a };

        show_target(overlay, p, label)?;
        drain(frames_rx, t0, &mut pass.frames);
        pass.traj.push(TrajPoint {
            t_s    : t0.elapsed().as_secs_f64(),
            px     : p,
            moving : true,
        });

        if a >= 1.0 {
            return Ok(());
        }

        std::thread::sleep(TICK);
    }
}

/// Sleeps for `duration_s` in ticks, draining frames and recording the stationary
/// target. `Advance` ends the wait early, `Skip` sets the flag, `Quit` aborts.
#[allow(clippy::too_many_arguments)]
pub(crate) fn wait_draining(
    duration_s : f64,
    t0         : Instant,
    frames_rx  : &Receiver<Et5Frame>,
    pass       : &mut PassData,
    target     : GlobalPx,
    moving     : bool,
    keys       : Option<&Receiver<SweepKey>>,
    skip       : &mut bool,
)
    -> Result<(), SweepError>
{
    let start = Instant::now();

    while start.elapsed().as_secs_f64() < duration_s {
        drain(frames_rx, t0, &mut pass.frames);
        pass.traj.push(TrajPoint {
            t_s    : t0.elapsed().as_secs_f64(),
            px     : target,
            moving : moving,
        });

        if let Some(keys) = keys {
            match keys.try_recv() {
                Ok(SweepKey::Quit)    => return Err(SweepError::Aborted),
                Ok(SweepKey::Skip)    => {
                    *skip = true;

                    return Ok(());
                }
                Ok(SweepKey::Advance | SweepKey::Commit) => return Ok(()),
                Err(_)                => {}
            }
        }

        std::thread::sleep(TICK);
    }

    Ok(())
}

/// Moves every queued frame into `frames`, stamped with the current host time.
pub(crate) fn drain(frames_rx: &Receiver<Et5Frame>, t0: Instant, frames: &mut Vec<TimedFrame>) {
    let t_s = t0.elapsed().as_secs_f64();

    while let Ok(frame) = frames_rx.try_recv() {
        frames.push(TimedFrame { t_s: t_s, frame: frame });
    }
}

/// The overlay background every pass currently draws its targets on, packed RGBA with
/// the red byte in the high bits. Zero is the transparent debug overlay `calibrate` and
/// `collect` run under; `record` swaps it between black and white so the session covers
/// both pupil extremes.
///
/// A module static rather than a parameter because `show_target` is called from inside
/// `glide` and `wait_draining` as well, and threading a colour through all three for the
/// benefit of one caller buys nothing: only one pass is ever animating at a time.
static OVERLAY_BACKGROUND: AtomicU32 = AtomicU32::new(0);

/// Sets the background every subsequent target frame is drawn on. `None` restores the
/// transparent overlay.
pub(crate) fn set_overlay_background(color: Option<[u8; 4]>) {
    let packed = {
        match color {
            Some(c) => u32::from_be_bytes(c),
            None    => 0,
        }
    };

    OVERLAY_BACKGROUND.store(packed, Ordering::Relaxed);
}

/// The background `show_target` is currently painting.
fn overlay_background() -> Option<[u8; 4]> {
    let packed = OVERLAY_BACKGROUND.load(Ordering::Relaxed);

    (packed != 0).then(|| packed.to_be_bytes())
}

/// Draws the target square with a centre cross and caption.
pub(crate) fn show_target(overlay: &OverlayHandle, px: GlobalPx, label: &str)
    -> Result<(), SweepError>
{
    overlay.set(OverlayState {
        gaze       : None,
        highlight  : Some(Rect {
            x : px.x - TARGET_PX * 0.5,
            y : px.y - TARGET_PX * 0.5,
            w : TARGET_PX,
            h : TARGET_PX,
        }),
        truth      : Some(px),
        label      : Some(label.to_string()),
        background : overlay_background(),
        pointer    : None,
    }).map_err(|e| SweepError::Overlay(e.to_string()))
}

/// Pose observations for one stop: the median ray over the window, or, for the
/// parallax hold, one median ray per chunk so the moving head contributes many
/// origins on the one target.
fn push_observations(
    stop         : &StopWindow,
    rays         : &[RaySample],
    keep         : &[bool],
    observations : &mut Vec<PoseObservation>,
)
{
    let chunk_s = if stop.parallax { PARALLAX_CHUNK_S } else { f64::INFINITY };

    let mut chunk_start = stop.t_start;

    while chunk_start < stop.t_end {
        let chunk_end = (chunk_start + chunk_s).min(stop.t_end);

        let window: Vec<&RaySample> = rays.iter().zip(keep)
            .filter(|(r, k)| **k && r.t_s >= chunk_start && r.t_s <= chunk_end)
            .map(|(r, _)| r)
            .collect();

        // A parallax chunk is short; three frames are enough for its median.
        let min = if stop.parallax { 3 } else { STOP_MIN_FRAMES };

        if window.len() >= min {
            observations.push(PoseObservation {
                u         : stop.u,
                v         : stop.v,
                origin_mm : median_v3(window.iter().map(|r| r.origin)),
                dir       : median_v3(window.iter().map(|r| r.dir)).normalize(),
            });
        }

        chunk_start = chunk_end;
    }
}

/// Fits one display from the firmware's trained 2D output: `gaze_2d_norm` is panel
/// coordinates by construction (the panel is the declared area), so the field learns
/// the residual of the trained mapping directly. The pose is still solved from the
/// fusion rays so the geometry file stays honest, but it does not touch the 2D path.
fn fit_display_direct(
    geometry : &DesktopGeometry,
    out      : &OutputGeometry,
    pass     : &PassData,
    config   : &SweepConfig,
    declared : Option<DisplayArea>,
    history  : &[PassData],
)
    -> Result<(OutputCalibration, DisplaySummary), SweepError>
{
    let _ = geometry;

    let targets = pass.stops.iter().filter(|s| !s.parallax).count();

    // In direct mode the configured desk geometry IS the pose: the declared plane
    // is derived from it, and the trained 2D output is panel uv by construction.
    // The hold triangulation is logged as a diagnostic only — it measures the eye
    // model's head-generalisation error as much as the panel's position (biases of
    // 120-350 mm observed under large sweeps), so it must not steer the geometry.
    if let (Some(fit), Some(area)) = (&triangulated_plane(out, &triangulate_stops(pass)),
                                      declared)
    {
        info!("{}: hold triangulation lands {:.0} mm from the declared plane, \
               self-consistency {:.2} deg (diagnostic; includes eye-model \
               head-generalisation bias)",
              out.name, corner_shift_mm(&plane_corners(&fit.pose), &area),
              fit.rms_deg);
    }

    let (solved_output, pose_rms_deg) = (out.clone(), 0.0);

    // The trained 2D track in pixels. Interior points only; a clamped output is a
    // wrong position.
    let track: Vec<(f64, GlobalPx)> = pass.frames.iter()
        .filter_map(|f| {
            let [nx, ny] = f.frame.gaze_2d_norm?;

            if !(0.001..0.999).contains(&nx) || !(0.001..0.999).contains(&ny) {
                return None;
            }

            Some((f.t_s, out.uv_to_px(nx, ny)))
        })
        .collect();

    let scale = geometry.px_per_deg(geometry.eye(), out.uv_to_px(0.5, 0.5))
        .map(|(h, v)| (h + v) * 0.5)
        .unwrap_or(FALLBACK_PX_PER_DEG);

    // Saccade gate on pixel velocity.
    let mut keep_track = vec![true; track.len()];

    for i in 1..track.len() {
        let dt = track[i].0 - track[i - 1].0;

        if dt <= 0.0 {
            continue;
        }

        let dx    = track[i].1.x - track[i - 1].1.x;
        let dy    = track[i].1.y - track[i - 1].1.y;
        let deg_s = (dx * dx + dy * dy).sqrt() / scale / dt;

        if deg_s > SACCADE_DEG_S {
            keep_track[i]     = false;
            keep_track[i - 1] = false;
        }
    }

    let saccade_frames = keep_track.iter().filter(|k| !**k).count();

    let moving: Vec<(f64, GlobalPx)> = track.iter().zip(&keep_track)
        .filter(|((t, _), k)| **k && was_moving(&pass.traj, *t))
        .map(|((t, p), _)| (*t, *p))
        .collect();

    let lag_s = estimate_lag(&pass.traj, &moving, scale);

    // Field rows: fixation anchors then lag-shifted glides.
    let mut rows = Vec::new();

    for stop in pass.stops.iter().filter(|s| !s.parallax) {
        let window: Vec<GlobalPx> = track.iter().zip(&keep_track)
            .filter(|((t, _), k)| **k && *t >= stop.t_start && *t <= stop.t_end)
            .map(|((_, p), _)| *p)
            .collect();

        if window.len() < STOP_MIN_FRAMES {
            continue;
        }

        let mut xs: Vec<f64> = window.iter().map(|p| p.x).collect();
        let mut ys: Vec<f64> = window.iter().map(|p| p.y).collect();
        let px = GlobalPx { x: median(&mut xs), y: median(&mut ys) };

        // The same sanity gate the glides get: an anchor demanding a correction
        // this large is a mistrack or an envelope casualty, not signal, and a
        // single junk anchor bends the whole field (measured: one top-corner
        // anchor 24 degrees off tripled the LOO).
        let err_deg = ((px.x - stop.px.x).powi(2) + (px.y - stop.px.y).powi(2)).sqrt()
            / scale;

        if err_deg > ANCHOR_OUTLIER_DEG {
            warn!("{}: anchor at ({:.2},{:.2}) is {err_deg:.1} deg off target; dropped",
                  out.name, stop.u, stop.v);
            continue;
        }

        let (nx, ny) = normalise(out, px);
        let (wx, wy) = normalise(out, stop.px);

        rows.push(FieldRow { nx: nx, ny: ny, want_nx: wx, want_ny: wy });
    }

    let anchor_rows = rows.len();

    // Trained-model health: the anchors measure the trained mapping against known
    // fixation truth. A large median error means the mapping no longer matches the
    // panel — a moved tracker, or a lost on-device model — and no correction field
    // should paper over that.
    {
        let mut errs: Vec<f64> = rows.iter()
            .map(|r| ((r.nx - r.want_nx).powi(2) + (r.ny - r.want_ny).powi(2)).sqrt())
            .collect();

        if !errs.is_empty() {
            let med = median(&mut errs);

            if med > MODEL_HEALTH_WARN_NORM {
                warn!("{}: trained mapping is off by {:.0}% of the panel at the \
                       anchors; the tracker moved or the on-device model was lost — \
                       check the mount and rerun `calibrate --retrain`",
                      out.name, med * 50.0);
            }
        }
    }

    let step        = (moving.len() / config.max_glide_rows).max(1);

    for (t_s, gaze_px) in moving.iter().step_by(step) {
        let Some(want) = target_at(&pass.traj, t_s - lag_s) else {
            continue;
        };

        let err_deg = ((gaze_px.x - want.x).powi(2) + (gaze_px.y - want.y).powi(2)).sqrt()
            / scale;

        if err_deg > OUTLIER_DEG {
            continue;
        }

        let (nx, ny) = normalise(out, *gaze_px);
        let (wx, wy) = normalise(out, want);

        rows.push(FieldRow { nx: nx, ny: ny, want_nx: wx, want_ny: wy });
    }

    let glide_rows = rows.len() - anchor_rows;
    let (field, field_rms) = fit_best(&rows);

    let shift = DVec3::from_array(solved_output.position_mm)
        .distance(DVec3::from_array(out.position_mm));

    let entry = OutputCalibration {
        name           : out.name.clone(),
        pose           : OutputPose {
            position_mm : solved_output.position_mm,
            yaw_deg     : solved_output.yaw_deg,
            pitch_deg   : solved_output.pitch_deg,
            roll_deg    : solved_output.roll_deg,
        },
        field          : field,
        pose_rms_deg   : pose_rms_deg,
        field_rms_norm : field_rms,
        targets        : targets,
        head_gain      : fit_head_gain(pass, history),
    };

    let summary = DisplaySummary {
        name           : out.name.clone(),
        targets        : targets,
        pose_rms_deg   : pose_rms_deg,
        pose_shift_mm  : shift,
        lag_s          : lag_s,
        glide_rows     : glide_rows,
        saccade_frames : saccade_frames,
        field_rms_norm : field_rms,
    };

    Ok((entry, summary))
}

// --- The plane pass ---

/// The solved plane-pass geometry: the panel pose fitted to triangulated points,
/// plus its residual expressed in degrees for the shared summary fields.
struct TriFit {
    /// The display geometry with the point-solved pose applied.
    pose    : OutputGeometry,
    /// Point-solve RMS as an angle at the nominal viewing distance, degrees.
    rms_deg : f64,
}

/// Per-pass origin-scale conditioning from the rigid-IPD constraint. The reported
/// inter-origin distance should be constant — the skull is rigid — so any trend of
/// it against reported depth is the tracker's depth-estimation error (measured on
/// this unit: ~+32 mm of reported IPD per metre of depth, i.e. depth motion
/// exaggerated by roughly a third). Origins are rescaled radially from the tracker
/// to hold the reported IPD at its pass median before triangulation. Only the
/// *variation* needs fixing; the absolute scale cancels end-to-end, which is why no
/// user-measured IPD is required.
struct OriginScale {
    /// Reference IPD the pass is normalised to (the pass median), millimetres.
    ipd_ref  : f64,
    /// Mean reported depth of the fitted frames, millimetres.
    mean_z   : f64,
    /// Mean reported IPD of the fitted frames, millimetres.
    mean_ipd : f64,
    /// d(reported IPD)/d(reported depth), millimetres per millimetre.
    slope    : f64,
}

// --- OriginScale ---

impl OriginScale {
    /// Radial rescale factor for an origin at reported depth `z_mm`.
    fn factor(&self, z_mm: f64) -> f64 {
        let predicted = self.mean_ipd + self.slope * (z_mm - self.mean_z);

        (self.ipd_ref / predicted.max(1.0)).clamp(SCALE_FACTOR_MIN, SCALE_FACTOR_MAX)
    }
}

/// Fits the reported-IPD-versus-depth trend over a pass. `None` when the pass has
/// too few binocular frames or too little depth range to see the trend — in which
/// case the scale error is constant over the pass and needs no conditioning.
/// Quiet on purpose: both the triangulation and the head-gain fit call it, and
/// only the triangulation caller reports the trend.
fn fit_origin_scale(frames: &[TimedFrame]) -> Option<OriginScale> {
    let mut rows: Vec<(f64, f64)> = Vec::new();

    for f in frames {
        if !(f.frame.left_valid() && f.frame.right_valid()) {
            continue;
        }

        let (Some(l), Some(r)) = (f.frame.eye_origin_l_mm, f.frame.eye_origin_r_mm)
        else {
            continue;
        };

        let l   = DVec3::from_array(l);
        let r   = DVec3::from_array(r);
        let ipd = l.distance(r);

        // A frame with a wildly implausible IPD is a mistrack, not a measurement.
        if !(40.0..90.0).contains(&ipd) {
            continue;
        }

        rows.push(((l.z + r.z) * 0.5, ipd));
    }

    if rows.len() < SCALE_MIN_FRAMES {
        return None;
    }

    let z_min = rows.iter().map(|(z, _)| *z).fold(f64::INFINITY, f64::min);
    let z_max = rows.iter().map(|(z, _)| *z).fold(f64::NEG_INFINITY, f64::max);

    if z_max - z_min < SCALE_MIN_Z_RANGE_MM {
        return None;
    }

    let n        = rows.len() as f64;
    let mean_z   = rows.iter().map(|(z, _)| z).sum::<f64>() / n;
    let mean_ipd = rows.iter().map(|(_, i)| i).sum::<f64>() / n;

    let cov = rows.iter().map(|(z, i)| (z - mean_z) * (i - mean_ipd)).sum::<f64>();
    let var = rows.iter().map(|(z, _)| (z - mean_z).powi(2)).sum::<f64>();

    if var <= 0.0 {
        return None;
    }

    let slope = cov / var;

    let mut ipds: Vec<f64> = rows.iter().map(|(_, i)| *i).collect();
    let ipd_ref = median(&mut ipds);

    Some(OriginScale {
        ipd_ref  : ipd_ref,
        mean_z   : mean_z,
        mean_ipd : mean_ipd,
        slope    : slope,
    })
}

/// Frames reduced to two per-eye ray tracks (left, right), invalid or degenerate
/// frames dropped from each. The plane pass triangulates each eye alone: a constant
/// per-eye direction bias only translates that eye's triangulated point slightly,
/// while a joint solve would turn the L/R bias *difference* into a vergence depth
/// error.
fn eye_tracks(frames: &[TimedFrame], scale: Option<&OriginScale>)
    -> (Vec<RaySample>, Vec<RaySample>)
{
    let mut left  = Vec::new();
    let mut right = Vec::new();

    for f in frames {
        let push = |valid  : bool,
                        origin : Option<[f64; 3]>,
                        target : Option<[f64; 3]>,
                        track  : &mut Vec<RaySample>| {
            if !valid {
                return;
            }

            let (Some(o), Some(p)) = (origin, target) else {
                return;
            };

            let o = DVec3::from_array(o);
            let d = DVec3::from_array(p) - o;

            if d.length_squared() < 1.0 {
                return;
            }

            // The direction comes from the raw pair (both ends share the frame's
            // scale, so it is scale-exact); the origin is then conditioned so the
            // bundle is metrically consistent across the head sweep.
            let o = {
                match scale {
                    Some(s) => o * s.factor(o.z),
                    None    => o,
                }
            };

            track.push(RaySample { t_s: f.t_s, origin: o, dir: d.normalize() });
        };

        push(f.frame.left_valid(), f.frame.eye_origin_l_mm, f.frame.gaze_3d_l_mm,
             &mut left);
        push(f.frame.right_valid(), f.frame.eye_origin_r_mm, f.frame.gaze_3d_r_mm,
             &mut right);
    }

    (left, right)
}

/// Lateral (xy) extent of a bundle's origins, millimetres: the diagonal of their
/// bounding box, depth ignored. Triangulation quality comes from baseline
/// perpendicular to the rays, which for a screen ahead means sideways or vertical
/// head motion, not in-and-out.
fn lateral_spread_mm(rays: &[(DVec3, DVec3)]) -> f64 {
    if rays.is_empty() {
        return 0.0;
    }

    let mut min = DVec3::splat(f64::INFINITY);
    let mut max = DVec3::splat(f64::NEG_INFINITY);

    for (o, _) in rays {
        min = min.min(*o);
        max = max.max(*o);
    }

    ((max.x - min.x).powi(2) + (max.y - min.y).powi(2)).sqrt()
}

/// Triangulates each plane-pass hold into a physical point. Per-eye bundles are
/// solved separately and averaged when the eyes carried a real head sweep; a hold
/// without one falls back to the pooled bundle, whose only baseline is the
/// interocular distance (vergence — usable, but noisier and bias-prone).
fn triangulate_stops(pass: &PassData) -> Vec<PointObservation> {
    let scale = fit_origin_scale(&pass.frames);

    if let Some(s) = &scale {
        info!("origin scale: reported ipd {:.1} mm, drift {:+.2} mm per 100 mm \
               of depth; conditioning origins for triangulation",
              s.ipd_ref, s.slope * 100.0);
    }

    let (left, right) = eye_tracks(&pass.frames, scale.as_ref());
    let keep_l        = saccade_mask(&left);
    let keep_r        = saccade_mask(&right);

    let mut points = Vec::new();

    for stop in pass.stops.iter().filter(|s| s.parallax) {
        let bundle = |track: &[RaySample], keep: &[bool]| -> Vec<(DVec3, DVec3)> {
            track.iter().zip(keep)
                .filter(|(r, k)| **k && r.t_s >= stop.t_start && r.t_s <= stop.t_end)
                .map(|(r, _)| (r.origin, r.dir))
                .collect()
        };

        let bundles  = [bundle(&left, &keep_l), bundle(&right, &keep_r)];
        let mut solo = Vec::new();

        for (rays, tag) in bundles.iter().zip(["L", "R"]) {
            if rays.len() < TRI_MIN_RAYS || lateral_spread_mm(rays) < TRI_MIN_SPREAD_MM {
                continue;
            }

            if let Some((p, rms)) = intersect_rays(rays) {
                if rms <= TRI_MAX_RMS_MM {
                    info!("dot ({:.2},{:.2}) eye {tag}: {} rays, rms {:.1} mm",
                          stop.u, stop.v, rays.len(), rms);
                    solo.push(p);
                }
                else {
                    warn!("dot ({:.2},{:.2}) eye {tag}: rms {rms:.1} mm, discarded",
                          stop.u, stop.v);
                }
            }
        }

        let point = {
            match solo.len() {
                2 => {
                    let gap = solo[0].distance(solo[1]);

                    info!("dot ({:.2},{:.2}): eyes agree to {gap:.1} mm",
                          stop.u, stop.v);

                    Some((solo[0] + solo[1]) * 0.5)
                }
                1 => Some(solo[0]),
                _ => {
                    let pooled: Vec<(DVec3, DVec3)> =
                        bundles.iter().flatten().copied().collect();

                    if pooled.len() < TRI_MIN_RAYS {
                        None
                    }
                    else {
                        warn!("dot ({:.2},{:.2}): head sweep too small, \
                               vergence-only triangulation", stop.u, stop.v);

                        intersect_rays(&pooled)
                            .filter(|(_, rms)| *rms <= TRI_MAX_RMS_MM)
                            .map(|(p, _)| p)
                    }
                }
            }
        };

        match point {
            Some(p) => points.push(PointObservation {
                u        : stop.u,
                v        : stop.v,
                point_mm : p,
            }),
            None    => warn!("dot ({:.2},{:.2}) failed to triangulate",
                             stop.u, stop.v),
        }
    }

    points
}

/// Fits the panel pose to the triangulated points. The initial guess is the
/// configured pose translated so the model's dot centroid meets the measured one;
/// the LM point solve does the rest. `None` when there are too few points or they
/// disagree with the panel's known shape.
fn triangulated_plane(out: &OutputGeometry, points: &[PointObservation])
    -> Option<TriFit>
{
    if points.len() < 3 {
        return None;
    }

    let model_c = points.iter().map(|p| out.uv_to_world(p.u, p.v)).sum::<DVec3>()
        / points.len() as f64;
    let meas_c  = points.iter().map(|p| p.point_mm).sum::<DVec3>()
        / points.len() as f64;

    let mut init = out.clone();
    init.position_mm = (DVec3::from_array(init.position_mm) + (meas_c - model_c))
        .to_array();

    let solved = {
        match solve_pose_points(&init, points) {
            Ok(s)  => s,
            Err(e) => {
                warn!("point pose solve failed: {e}");

                return None;
            }
        }
    };

    if solved.rms_mm > PLANE_MAX_RMS_MM {
        warn!("triangulated points disagree with the panel shape by {:.1} mm rms",
              solved.rms_mm);

        return None;
    }

    Some(TriFit {
        pose    : solved.output,
        rms_deg : (solved.rms_mm / NOMINAL_VIEW_MM).atan().to_degrees(),
    })
}

/// The three declared corners of a display's posed surface. For a curved panel the
/// corners are coplanar (the chord plane) while the centre bulges toward the user by
/// the sagitta, which a three-corner declaration cannot express; the trained mapping
/// absorbs the 2D consequences, and only the firmware's internal head-translation
/// compensation sees the residual depth error.
pub fn plane_corners(out: &OutputGeometry) -> DisplayArea {
    DisplayArea {
        tl_mm : out.uv_to_world(0.0, 0.0).to_array(),
        tr_mm : out.uv_to_world(1.0, 0.0).to_array(),
        bl_mm : out.uv_to_world(0.0, 1.0).to_array(),
    }
}

/// Rotates a desk-frame point into the sensor frame: a rotation about +X by the
/// mount pitch, so a point high in the desk frame drops in the sensor frame the
/// way the measured eye origins do (true eye height ~200 mm reads as ~38 mm on
/// this setup — a frame pitched up ~13 degrees).
pub fn desk_to_sensor(p: [f64; 3], pitch_deg: f64) -> [f64; 3] {
    let (sin, cos) = pitch_deg.to_radians().sin_cos();

    [p[0], p[1] * cos - p[2] * sin, p[1] * sin + p[2] * cos]
}

/// The largest corner-to-corner distance between two declared planes, millimetres.
fn corner_shift_mm(a: &DisplayArea, b: &DisplayArea) -> f64 {
    let d = |x: [f64; 3], y: [f64; 3]| DVec3::from_array(x).distance(DVec3::from_array(y));

    d(a.tl_mm, b.tl_mm).max(d(a.tr_mm, b.tr_mm)).max(d(a.bl_mm, b.bl_mm))
}

/// Solves an `n`-dimensional linear system (n <= 12) by Gaussian elimination with
/// partial pivoting, over fixed-size storage.
// The elimination indexes several arrays in lockstep.
#[allow(clippy::needless_range_loop)]
fn solve_lin(mut m: [[f64; 12]; 12], mut v: [f64; 12], n: usize) -> Option<[f64; 12]> {
    for col in 0..n {
        let mut pivot = col;

        for row in col + 1..n {
            if m[row][col].abs() > m[pivot][col].abs() {
                pivot = row;
            }
        }

        if m[pivot][col].abs() < 1e-12 {
            return None;
        }

        m.swap(col, pivot);
        v.swap(col, pivot);

        for row in col + 1..n {
            let f = m[row][col] / m[col][col];

            for k in col..n {
                m[row][k] -= f * m[col][k];
            }

            v[row] -= f * v[col];
        }
    }

    let mut x = [0.0; 12];

    for col in (0..n).rev() {
        let mut sum = v[col];

        for k in col + 1..n {
            sum -= m[col][k] * x[k];
        }

        x[col] = sum / m[col][col];
    }

    Some(x)
}

/// Ridge-regularised least squares over the first `n` regressor components, one
/// solution per uv axis.
// The accumulators index rows and columns in lockstep.
#[allow(clippy::needless_range_loop)]
fn regress_gain(rows: &[([f64; 12], f64, f64)], n: usize)
    -> Option<([f64; 12], [f64; 12])>
{
    let mut ata = [[0.0; 12]; 12];
    let mut atu = [0.0; 12];
    let mut atv = [0.0; 12];

    for (r, ru, rv) in rows {
        for i in 0..n {
            atu[i] += r[i] * ru;
            atv[i] += r[i] * rv;

            for j in 0..n {
                ata[i][j] += r[i] * r[j];
            }
        }
    }

    let ridge = 1e-6 * (0..n).map(|i| ata[i][i]).sum::<f64>().max(1.0);

    for i in 0..n {
        ata[i][i] += ridge;
    }

    let gx = solve_lin(ata, atu, n)?;
    let gy = solve_lin(ata, atv, n)?;

    Some((gx, gy))
}

/// Fits the head residual from the data pass's parallax holds. The regressor is
/// the head offset, the interocular deviation (head roll/yaw plus IPD
/// foreshortening — the dominant predictor: cross-validated R2 0.64 versus 0.27
/// for lagged origins alone), and eccentricity-scaled origin terms when the holds
/// cover three or more distinct dots. Rows are demeaned per hold so each dot's
/// static error stays with the field; a small lag scan remains for the origin
/// pairing, though the rotation channel usually makes zero lag the winner.
fn fit_head_gain(pass: &PassData, history: &[PassData]) -> Option<HeadGain> {
    // A binocular frame's (mean origin, interocular (dy, dz, |d|), interior uv).
    let frame_row = |f: &TimedFrame| -> Option<(DVec3, [f64; 3], [f64; 2])> {
        let [nx, ny] = f.frame.gaze_2d_norm?;

        if !(0.001..0.999).contains(&nx) || !(0.001..0.999).contains(&ny) {
            return None;
        }

        if !(f.frame.left_valid() && f.frame.right_valid()) {
            return None;
        }

        let l = DVec3::from_array(f.frame.eye_origin_l_mm?);
        let r = DVec3::from_array(f.frame.eye_origin_r_mm?);
        let d = r - l;

        Some(((l + r) * 0.5, [d.y, d.z, d.length()], [nx, ny]))
    };

    // Head-neutral references over the grid stops.
    let mut ref_o = DVec3::ZERO;
    let mut ref_i = [0.0; 3];
    let mut ref_n = 0usize;

    for stop in pass.stops.iter().filter(|s| !s.parallax) {
        for f in pass.frames.iter()
            .filter(|f| f.t_s >= stop.t_start && f.t_s <= stop.t_end)
        {
            if let Some((o, i, _)) = frame_row(f) {
                ref_o += o;

                for k in 0..3 {
                    ref_i[k] += i[k];
                }

                ref_n += 1;
            }
        }
    }

    if ref_n < HEAD_GAIN_MIN_FRAMES {
        return None;
    }

    let reference   = ref_o / ref_n as f64;
    let reference_i = ref_i.map(|x| x / ref_n as f64);

    // Per-hold sequences: gaze rows pair with the origin/rotation state from `lag`
    // earlier, so a candidate lag re-pairs the data rather than refitting blind.
    struct Hold {
        states : Vec<(f64, DVec3, [f64; 3])>,
        gaze   : Vec<(f64, [f64; 2])>,
        du     : f64,
        dv     : f64,
    }

    let mut holds: Vec<Hold> = Vec::new();
    let mut dots: Vec<(f64, f64)> = Vec::new();
    let mut pooled = 0usize;

    // The current pass first, then any archived passes recorded under the same
    // on-device model. Rows are demeaned per hold, so passes pool cleanly across
    // sessions. Each pass's |d| states are decorrelated from depth with that
    // pass's own reported-IPD trend before fitting: the trend is session-dependent
    // (measured 1.7 to 6.1 mm per 100 mm), and inside a hold raw |d| is collinear
    // with the origin's z, so least squares would otherwise park the depth weight
    // on the unstable |d| proxy (see `HeadGain::inter_z_mm_per_mm`). Conditioned,
    // depth can only land on the origin channel. The grid-stop reference
    // `inter_mm` is unaffected: its frames average to the reference depth by
    // construction.
    for (idx, p) in std::iter::once(pass).chain(history.iter()).enumerate() {
        let trend_p = fit_origin_scale(&p.frames).map_or(0.0, |s| s.slope);

        for stop in p.stops.iter().filter(|s| s.parallax) {
            let mut states = Vec::new();
            let mut gaze   = Vec::new();

            for f in p.frames.iter()
                .filter(|f| f.t_s >= stop.t_start && f.t_s <= stop.t_end)
            {
                if let Some((o, i, g)) = frame_row(f) {
                    let i = [i[0], i[1], i[2] - trend_p * (o.z - reference.z)];

                    states.push((f.t_s, o, i));
                    gaze.push((f.t_s, g));
                }
            }

            if gaze.len() < STOP_MIN_FRAMES {
                continue;
            }

            if !dots.iter().any(|(a, b)| (a - stop.u).abs() < 1e-9 && (b - stop.v).abs() < 1e-9) {
                dots.push((stop.u, stop.v));
            }

            if idx > 0 {
                pooled += 1;
            }

            holds.push(Hold {
                states : states,
                gaze   : gaze,
                du     : stop.u - 0.5,
                dv     : stop.v - 0.5,
            });
        }
    }

    if pooled > 0 {
        info!("head gain: pooling {pooled} archived holds from {} earlier passes",
              history.len());
    }

    // The stored trend is the current pass's own: it is what the runtime
    // conditioning applies to live frames.
    let trend = fit_origin_scale(&pass.frames).map_or(0.0, |s| s.slope);

    // Demeaned rows for one candidate lag. Regressor layout: origin offset (3),
    // interocular deviation (3), then the eccentricity-scaled origin terms.
    let rows_at = |lag: f64| -> Vec<([f64; 12], f64, f64)> {
        let mut rows = Vec::new();

        for hold in &holds {
            let mut paired: Vec<(DVec3, [f64; 3], [f64; 2])> = Vec::new();

            for (t, g) in &hold.gaze {
                let want = t - lag;

                let Some((tn, o, i)) = hold.states.iter()
                    .min_by(|a, b| {
                        (a.0 - want).abs().partial_cmp(&(b.0 - want).abs()).unwrap()
                    })
                else {
                    continue;
                };

                if (tn - want).abs() <= HEAD_GAIN_PAIR_GAP_S {
                    paired.push((*o, *i, *g));
                }
            }

            if paired.len() < STOP_MIN_FRAMES {
                continue;
            }

            let n  = paired.len() as f64;
            let mo = paired.iter().map(|(o, _, _)| *o).sum::<DVec3>() / n;
            let mut mi = [0.0; 3];

            for (_, i, _) in &paired {
                for k in 0..3 {
                    mi[k] += i[k] / n;
                }
            }

            let mu = paired.iter().map(|(_, _, g)| g[0]).sum::<f64>() / n;
            let mv = paired.iter().map(|(_, _, g)| g[1]).sum::<f64>() / n;

            for (o, i, g) in &paired {
                let o = *o - mo;

                rows.push((
                    [
                        o.x, o.y, o.z,
                        i[0] - mi[0], i[1] - mi[1], i[2] - mi[2],
                        o.x * hold.du, o.y * hold.du, o.z * hold.du,
                        o.x * hold.dv, o.y * hold.dv, o.z * hold.dv,
                    ],
                    g[0] - mu,
                    g[1] - mv,
                ));
            }
        }

        rows
    };

    let rows = rows_at(0.0);

    if rows.len() < HEAD_GAIN_MIN_FRAMES {
        return None;
    }

    // The head must actually have moved for the regression to mean anything.
    let mut min = DVec3::splat(f64::INFINITY);
    let mut max = DVec3::splat(f64::NEG_INFINITY);

    for (r, _, _) in &rows {
        let o = DVec3::new(r[0], r[1], r[2]);

        min = min.min(o);
        max = max.max(o);
    }

    if (max - min).length() < HEAD_GAIN_MIN_SPREAD_MM {
        return None;
    }

    let n = if dots.len() >= 3 { 12 } else { 6 };

    // Pick the lag whose fit predicts its own rows best.
    let mut best: Option<(f64, f64, [f64; 12], [f64; 12])> = None;

    for lag in HEAD_GAIN_LAGS {
        let rows = if lag == 0.0 { rows.clone() } else { rows_at(lag) };

        if rows.len() < HEAD_GAIN_MIN_FRAMES {
            continue;
        }

        let Some((cand_gx, cand_gy)) = regress_gain(&rows, n) else {
            continue;
        };

        let sse: f64 = rows.iter()
            .map(|(r, ru, rv)| {
                let pu: f64 = (0..n).map(|i| cand_gx[i] * r[i]).sum();
                let pv: f64 = (0..n).map(|i| cand_gy[i] * r[i]).sum();

                (ru - pu).powi(2) + (rv - pv).powi(2)
            })
            .sum::<f64>() / rows.len() as f64;

        if best.as_ref().is_none_or(|(_, b, _, _)| sse < *b) {
            best = Some((lag, sse, cand_gx, cand_gy));
        }
    }

    let (lag, _, gx, gy) = best?;

    info!("head gain fitted at {:.0} ms lag", lag * 1000.0);

    let mag = |a: f64, b: f64, c: f64| (a * a + b * b + c * c).sqrt();

    if mag(gx[0], gx[1], gx[2]) > HEAD_GAIN_MAX_UV_PER_MM
        || mag(gy[0], gy[1], gy[2]) > HEAD_GAIN_MAX_UV_PER_MM
        || mag(gx[3], gx[4], gx[5]) > HEAD_GAIN_ROT_MAX_UV_PER_MM
        || mag(gy[3], gy[4], gy[5]) > HEAD_GAIN_ROT_MAX_UV_PER_MM
    {
        warn!("head-gain fit implausibly large; dropped");

        return None;
    }

    Some(HeadGain {
        origin_mm  : reference.to_array(),
        gain_x     : [gx[0], gx[1], gx[2]],
        gain_y     : [gy[0], gy[1], gy[2]],
        gain_x_du  : [gx[6], gx[7], gx[8]],
        gain_x_dv  : [gx[9], gx[10], gx[11]],
        gain_y_du  : [gy[6], gy[7], gy[8]],
        gain_y_dv  : [gy[9], gy[10], gy[11]],
        lag_s             : lag,
        inter_mm          : reference_i,
        gain_x_rot        : [gx[3], gx[4], gx[5]],
        gain_y_rot        : [gy[3], gy[4], gy[5]],
        inter_z_mm_per_mm : trend,
    })
}

// --- History (pooled passes) ---

/// Maximum archived passes pooled into one fit, newest first. Bounds fit cost and
/// ages out stale postures.
const HISTORY_MAX_PASSES: usize = 8;

/// Loads the archived passes for `display` matching the configured history key.
/// A missing directory, key, or file is just an empty history: the accumulator is
/// an optimisation, never a requirement.
fn load_history(config: &SweepConfig, display: &str) -> Vec<PassData> {
    let (Some(dir), Some(key)) = (&config.history_dir, &config.history_key) else {
        return Vec::new();
    };

    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };

    let prefix = format!("{key}-");

    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(&prefix) && n.ends_with(".jsonl"))
        })
        .collect();

    // Newest first: the timestamp suffix sorts lexicographically within one key.
    files.sort();
    files.reverse();
    files.truncate(HISTORY_MAX_PASSES);

    let mut passes = Vec::new();

    for path in files {
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };

        let mut pass = PassData {
            frames : Vec::new(),
            traj   : Vec::new(),
            stops  : Vec::new(),
        };

        for line in text.lines() {
            let Ok(record) = serde_json::from_str::<RawRecord>(line) else {
                continue;
            };

            if record.display != display {
                continue;
            }

            match record.kind.as_str() {
                "stop"  => pass.stops.extend(record.stop),
                "traj"  => pass.traj.extend(record.point),
                "frame" => pass.frames.extend(record.frame),
                _       => {}
            }
        }

        if !pass.frames.is_empty() {
            passes.push(pass);
        }
    }

    passes
}

// --- Collect (data only, no fit) ---

/// Dwell time at each collect stop, seconds. Long enough for one full prompted
/// head motion at natural speed.
pub(crate) const COLLECT_HOLD_S: f64 = 6.0;

/// Posture prompts cycled through the collect dwells. Together they excite every
/// state the head gain regresses on: depth (in/out), lateral and vertical
/// translation, and the interocular rotation channels (yaw, roll).
pub(crate) const COLLECT_PROMPTS: [&str; 6] = [
    "sit naturally, tiny drifts",
    "lean IN slowly, then back",
    "lean BACK slowly, then in",
    "turn your head left and right",
    "tilt your head side to side",
    "raise and lower your chin",
];

/// Dwell-point bounds, uv. The top strip is excluded: gaze there exits the head
/// box (eyes lost looking up over the tracker), so it can only bank dropouts.
pub(crate) const COLLECT_U_MIN: f64 = 0.08;
/// See `COLLECT_U_MIN`.
pub(crate) const COLLECT_U_MAX: f64 = 0.92;
/// See `COLLECT_U_MIN`.
pub(crate) const COLLECT_V_MIN: f64 = 0.12;
/// See `COLLECT_U_MIN`.
pub(crate) const COLLECT_V_MAX: f64 = 0.90;

/// Runs a free-form data-banking session on one display: the target glides through
/// a low-discrepancy sequence of dwell points, each held under a prompted posture,
/// and the recorded pass is appended to `raw_out` in the standard readings format.
/// Nothing is fitted here — the next `calibrate` or `refit` pools the archive via
/// `SweepConfig::history_dir`. Returns the number of holds banked; quitting early
/// keeps what was collected.
pub fn run_collect(
    device   : &mut Device,
    geometry : &DesktopGeometry,
    out_name : &str,
    overlay  : &OverlayHandle,
    keys     : Option<&Receiver<SweepKey>>,
    minutes  : f64,
    raw_out  : &std::path::Path,
)
    -> Result<usize, SweepError>
{
    let out = geometry.outputs.iter()
        .find(|o| o.name == out_name && o.enabled)
        .ok_or(SweepError::NoDisplays)?;

    let frames_rx = device.gaze_stream();
    let t0        = Instant::now();
    let deadline  = minutes * 60.0;

    let mut pass = PassData {
        frames : Vec::new(),
        traj   : Vec::new(),
        stops  : Vec::new(),
    };

    let mut current = out.uv_to_px(0.5, 0.5);

    // A quit mid-run is not an error: the holds banked so far are the product.
    let mut walk = || -> Result<(), SweepError> {
        let mut k = 0usize;

        while t0.elapsed().as_secs_f64() < deadline {
            // Golden-ratio low-discrepancy walk: well spread over the panel with
            // no RNG and no repeating raster the eye could learn.
            let u = COLLECT_U_MIN
                + (0.5 + k as f64 * 0.618_033_988_749_895).fract()
                    * (COLLECT_U_MAX - COLLECT_U_MIN);
            let v = COLLECT_V_MIN
                + (0.5 + k as f64 * 0.381_966_011_250_105).fract()
                    * (COLLECT_V_MAX - COLLECT_V_MIN);

            let target = out.uv_to_px(u, v);
            let left_s = deadline - t0.elapsed().as_secs_f64();
            let label  = format!("{} collect ({:.0}s left) — eyes on the dot: {}",
                                 out.name, left_s.max(0.0),
                                 COLLECT_PROMPTS[k % COLLECT_PROMPTS.len()]);

            glide(geometry, overlay, &label, current, target, t0, &frames_rx, &mut pass)?;
            current = target;
            show_target(overlay, target, &label)?;

            let mut skip = false;
            let t_start  = t0.elapsed().as_secs_f64();

            wait_draining(COLLECT_HOLD_S, t0, &frames_rx, &mut pass, target, false,
                          keys, &mut skip)?;

            if !skip {
                pass.stops.push(StopWindow {
                    u        : u,
                    v        : v,
                    px       : target,
                    t_start  : t_start,
                    t_end    : t0.elapsed().as_secs_f64(),
                    parallax : true,
                });
            }

            k += 1;
        }

        Ok(())
    };

    match walk() {
        Ok(()) | Err(SweepError::Aborted) => {}
        Err(e)                            => return Err(e),
    }

    let _ = overlay.set(OverlayState {
        gaze       : None,
        highlight  : None,
        truth      : None,
        label      : None,
        background : None,
        pointer    : None,
    });

    save_pass(&raw_out.to_path_buf(), out_name, &pass)
        .map_err(|e| SweepError::Readings(e.to_string()))?;

    Ok(pass.stops.len())
}

// --- Refitting from saved readings ---

/// One line of the raw readings file.
#[derive(Deserialize)]
struct RawRecord {
    kind    : String,
    display : String,
    #[serde(default)]
    stop    : Option<StopWindow>,
    #[serde(default)]
    point   : Option<TrajPoint>,
    #[serde(default)]
    frame   : Option<TimedFrame>,
}

/// Re-runs the per-display fits from a saved readings file, producing a calibration
/// without the device, the compositor, or the user. This is how fit changes (field
/// degrees, gating, weighting) are evaluated against an existing sweep.
pub fn refit(
    geometry : &DesktopGeometry,
    readings : &std::path::Path,
    config   : &SweepConfig,
)
    -> Result<SweepOutcome, SweepError>
{
    let text = std::fs::read_to_string(readings)
        .map_err(|e| SweepError::Readings(e.to_string()))?;

    // Group records per display, preserving first-seen order.
    let mut order  = Vec::new();
    let mut passes : std::collections::HashMap<String, PassData> = Default::default();

    for line in text.lines() {
        let Ok(record) = serde_json::from_str::<RawRecord>(line) else {
            continue;
        };

        if !passes.contains_key(&record.display) {
            order.push(record.display.clone());
            passes.insert(record.display.clone(), PassData {
                frames : Vec::new(),
                traj   : Vec::new(),
                stops  : Vec::new(),
            });
        }

        let pass = passes.get_mut(&record.display).expect("just inserted");

        match record.kind.as_str() {
            "stop"  => pass.stops.extend(record.stop),
            "traj"  => pass.traj.extend(record.point),
            "frame" => pass.frames.extend(record.frame),
            _       => {}
        }
    }

    let mut entries   = Vec::new();
    let mut summaries = Vec::new();

    for name in &order {
        // Plane-pass records are for the sweep's own triangulation, not a refit.
        if name.ends_with("#tri") {
            continue;
        }

        let Some(out) = geometry.outputs.iter().find(|o| o.name == *name) else {
            warn!("{name}: in the readings but not the desk config; skipped");
            continue;
        };

        let history = {
            if config.direct {
                load_history(config, name)
            }
            else {
                Vec::new()
            }
        };

        let fit = {
            if config.direct {
                fit_display_direct(geometry, out, &passes[name], config, None, &history)
            }
            else {
                fit_display(geometry, out, &passes[name], config)
            }
        };

        match fit {
            Ok((entry, summary)) => {
                entries.push(entry);
                summaries.push(summary);
            }
            Err(e)               => {
                warn!("{name}: refit failed ({e}); display left uncalibrated");
            }
        }
    }

    if entries.is_empty() {
        return Err(SweepError::NothingFitted);
    }

    let lag_s = median(&mut summaries.iter().map(|s| s.lag_s).collect::<Vec<_>>());

    Ok(SweepOutcome {
        calibration : Et5Calibration {
            format             : CALIBRATION_FORMAT,
            created_unix_s     : Et5Calibration::now_unix_s(),
            lag_s              : lag_s,
            device_output      : None,
            device_area        : None,
            device_blob_sha256 : None,
            device_result      : None,
            outputs            : entries,
            health             : Vec::new(),
        },
        summaries   : summaries,
        device_blob : None,
    })
}

// --- Fitting ---

/// A frame reduced to its combined ray and time.
pub(crate) struct RaySample {
    pub(crate) t_s    : f64,
    pub(crate) origin : DVec3,
    pub(crate) dir    : DVec3,
}

/// Fits one display from its pass: pose from the pauses, lag from the glides, field
/// from both.
fn fit_display(
    geometry : &DesktopGeometry,
    out      : &OutputGeometry,
    pass     : &PassData,
    config   : &SweepConfig,
)
    -> Result<(OutputCalibration, DisplaySummary), SweepError>
{
    // Reduce frames to the two ray tracks and gate saccades on each independently.
    let (fusion, firmware) = ray_samples(&pass.frames);
    let keep_fusion        = saccade_mask(&fusion);
    let keep_firmware      = saccade_mask(&firmware);
    let saccade_frames     = keep_firmware.iter().filter(|k| !**k).count();

    // Pose observations: the median fusion ray per window, with the parallax hold
    // sliced into per-chunk origins.
    let mut observations = Vec::new();

    for stop in &pass.stops {
        push_observations(stop, &fusion, &keep_fusion, &mut observations);
    }

    let targets = pass.stops.iter().filter(|s| !s.parallax).count();

    // Initialise the pose from the observations themselves rather than the config:
    // the configured desk frame need not match the device's frame (axis conventions,
    // the tracker's mounting tilt), and the solver only needs a starting point on the
    // right side of the room.
    let init   = init_pose(geometry, out, &observations);
    let solved = solve_pose(&init, &observations)
        .map_err(|e| SweepError::Pose(out.name.clone(), e))?;

    // A single-output desk for intersecting rays with the solved panel only; a ray
    // that hits a neighbour during this display's pass is not usable here anyway.
    let solved_desk = DesktopGeometry {
        eye_mm     : geometry.eye_mm,
        tracker_mm : geometry.tracker_mm,
        outputs    : vec![solved.output.clone()],
        noise      : None,
    };

    // Angular scale at the panel centre as seen from where the eyes actually were
    // (the configured eye lives in the config frame, which need not match the
    // device's). Only used to express lag costs and the outlier gate in degrees.
    let eye_mm = median_v3(observations.iter().map(|o| o.origin_mm));
    let scale  = solved_desk.px_per_deg(eye_mm, solved.output.uv_to_px(0.5, 0.5))
        .map(|(h, v)| (h + v) * 0.5)
        .unwrap_or(FALLBACK_PX_PER_DEG);

    // Gaze pixels over the glides, for the lag search and the field rows, from the
    // firmware track: this is the signal the corrections will be applied to at run
    // time, its filter latency included (the lag estimate absorbs that).
    let moving: Vec<(f64, GlobalPx)> = firmware.iter().zip(&keep_firmware)
        .filter(|(r, k)| **k && was_moving(&pass.traj, r.t_s))
        .filter_map(|(r, _)| {
            let ray = Ray { origin: r.origin, dir: r.dir };

            solved_desk.intersect(&ray).map(|hit| (r.t_s, hit.px))
        })
        .collect();

    let lag_s = estimate_lag(&pass.traj, &moving, scale);

    // Field rows: one anchor per pause (mean gaze pixel over the window), then the
    // lag-shifted glide samples, outlier gated and subsampled to the cap.
    let mut rows = Vec::new();

    for stop in pass.stops.iter().filter(|s| !s.parallax) {
        // Anchor rows come from the firmware track too: the field corrects the
        // consumed signal, and a firmware-vs-fusion disagreement at a fixation is
        // exactly what it needs to learn.
        let window: Vec<&RaySample> = firmware.iter().zip(&keep_firmware)
            .filter(|(r, k)| **k && r.t_s >= stop.t_start && r.t_s <= stop.t_end)
            .map(|(r, _)| r)
            .collect();

        if window.len() < STOP_MIN_FRAMES {
            continue;
        }

        let ray = Ray {
            origin : median_v3(window.iter().map(|r| r.origin)),
            dir    : median_v3(window.iter().map(|r| r.dir)).normalize(),
        };

        let Some(hit) = solved_desk.intersect(&ray) else {
            continue;
        };

        // Anchors get the glides' outlier gate too; see `fit_display_direct`.
        let err_deg = ((hit.px.x - stop.px.x).powi(2) + (hit.px.y - stop.px.y).powi(2))
            .sqrt() / scale;

        if err_deg > ANCHOR_OUTLIER_DEG {
            warn!("{}: anchor at ({:.2},{:.2}) is {err_deg:.1} deg off target; dropped",
                  out.name, stop.u, stop.v);
            continue;
        }

        let (nx, ny)     = normalise(&solved.output, hit.px);
        let (wx, wy)     = normalise(&solved.output, stop.px);

        rows.push(FieldRow { nx: nx, ny: ny, want_nx: wx, want_ny: wy });
    }

    let anchor_rows = rows.len();

    let step = (moving.len() / config.max_glide_rows).max(1);

    for (t_s, gaze_px) in moving.iter().step_by(step) {
        let Some(want) = target_at(&pass.traj, t_s - lag_s) else {
            continue;
        };

        // Drop samples where the user plainly was not on the target.
        let err_deg = ((gaze_px.x - want.x).powi(2) + (gaze_px.y - want.y).powi(2)).sqrt()
            / scale;

        if err_deg > OUTLIER_DEG {
            continue;
        }

        let (nx, ny) = normalise(&solved.output, *gaze_px);
        let (wx, wy) = normalise(&solved.output, want);

        rows.push(FieldRow { nx: nx, ny: ny, want_nx: wx, want_ny: wy });
    }

    let glide_rows = rows.len() - anchor_rows;
    let (field, field_rms) = fit_best(&rows);

    let shift = DVec3::from_array(solved.output.position_mm)
        .distance(DVec3::from_array(out.position_mm));

    let entry = OutputCalibration {
        name           : out.name.clone(),
        pose           : OutputPose {
            position_mm : solved.output.position_mm,
            yaw_deg     : solved.output.yaw_deg,
            pitch_deg   : solved.output.pitch_deg,
            roll_deg    : solved.output.roll_deg,
        },
        field          : field,
        pose_rms_deg   : solved.rms_deg,
        field_rms_norm : field_rms,
        targets        : targets,
        head_gain      : None,
    };

    let summary = DisplaySummary {
        name           : out.name.clone(),
        targets        : targets,
        pose_rms_deg   : solved.rms_deg,
        pose_shift_mm  : shift,
        lag_s          : lag_s,
        glide_rows     : glide_rows,
        saccade_frames : saccade_frames,
        field_rms_norm : field_rms,
    };

    Ok((entry, summary))
}

/// Appends one display's raw pass to the JSONL file: tagged records for the stop
/// windows, the target trajectory, and every decoded frame.
fn save_pass(path: &PathBuf, display: &str, pass: &PassData) -> std::io::Result<()> {
    let file = std::fs::OpenOptions::new().create(true).append(true).open(path)?;
    let mut w = std::io::BufWriter::new(file);

    for stop in &pass.stops {
        writeln!(w, "{}", serde_json::json!({
            "kind"    : "stop",
            "display" : display,
            "stop"    : stop,
        }))?;
    }

    for point in &pass.traj {
        writeln!(w, "{}", serde_json::json!({
            "kind"    : "traj",
            "display" : display,
            "point"   : point,
        }))?;
    }

    for frame in &pass.frames {
        writeln!(w, "{}", serde_json::json!({
            "kind"    : "frame",
            "display" : display,
            "frame"   : frame,
        }))?;
    }

    w.flush()
}

/// A pose guess built from the observations: the panel centre sits along the mean
/// gaze direction at the configured viewing distance, facing back at the viewer.
/// Config-frame-independent apart from that one distance scalar.
fn init_pose(
    geometry     : &DesktopGeometry,
    out          : &OutputGeometry,
    observations : &[PoseObservation],
)
    -> OutputGeometry
{
    let mut mean_dir    = DVec3::ZERO;
    let mut mean_origin = DVec3::ZERO;

    for obs in observations {
        mean_dir    += obs.dir;
        mean_origin += obs.origin_mm;
    }

    if mean_dir.length_squared() < 1e-12 || observations.is_empty() {
        return out.clone();
    }

    mean_dir     = mean_dir.normalize();
    mean_origin /= observations.len() as f64;

    let distance = (DVec3::from_array(out.position_mm) - geometry.eye()).length();
    let centre   = mean_origin + mean_dir * distance;

    // Face the panel back along the mean gaze: its +Z normal is `-mean_dir`. With
    // the yaw-then-pitch composition (`OutputGeometry` docs), the normal is
    // `(sin yaw cos pitch, -sin pitch, cos yaw cos pitch)`.
    let n     = -mean_dir;
    let pitch = (-n.y).asin();
    let yaw   = n.x.atan2(n.z);

    let mut init = out.clone();
    init.position_mm = centre.to_array();
    init.yaw_deg     = yaw.to_degrees();
    init.pitch_deg   = pitch.to_degrees();
    init.roll_deg    = 0.0;

    init
}

/// Frames reduced to two parallel ray tracks, invalid frames dropped from each.
///
/// The fusion track (per-eye combination) is spatially sharpest at fixations and
/// solves the pose. The firmware track (the device's filtered 2D lifted back to a
/// ray; the data passes run under `VIRTUAL_AREA`, the area it is normalised over) is
/// the signal the provider actually feeds consumers, so the lag estimate and the
/// correction field are fitted on it, temporal filter and all.
pub(crate) fn ray_samples(frames: &[TimedFrame]) -> (Vec<RaySample>, Vec<RaySample>) {
    let mut combiner = EyeCombiner::new();
    let mut fusion   = Vec::new();
    let mut firmware = Vec::new();

    for f in frames {
        if let Some(fused) = combiner.combine(&f.frame) {
            fusion.push(RaySample {
                t_s    : f.t_s,
                origin : fused.origin_mm,
                dir    : fused.dir,
            });
        }

        if let Some((origin, dir, _)) = filtered_ray(&f.frame, &VIRTUAL_AREA) {
            firmware.push(RaySample { t_s: f.t_s, origin: origin, dir: dir });
        }
    }

    (fusion, firmware)
}

/// Marks which samples survive the saccade gate: angular velocity below the threshold
/// and outside the pad window around any frame above it.
pub(crate) fn saccade_mask(rays: &[RaySample]) -> Vec<bool> {
    let mut fast = vec![false; rays.len()];

    for i in 1..rays.len() {
        let dt = rays[i].t_s - rays[i - 1].t_s;

        if dt <= 0.0 {
            continue;
        }

        let deg_s = rays[i].dir.angle_between(rays[i - 1].dir).to_degrees() / dt;

        if deg_s > SACCADE_DEG_S {
            fast[i]     = true;
            fast[i - 1] = true;
        }
    }

    // Expand each saccade by the pad window so its tails go too.
    let mut keep = vec![true; rays.len()];

    for i in 0..rays.len() {
        if !fast[i] {
            continue;
        }

        for (j, k) in keep.iter_mut().enumerate() {
            if (rays[j].t_s - rays[i].t_s).abs() <= SACCADE_PAD_S {
                *k = false;
            }
        }
    }

    keep
}

/// True when the target was gliding at time `t_s`.
pub(crate) fn was_moving(traj: &[TrajPoint], t_s: f64) -> bool {
    match traj.binary_search_by(|p| p.t_s.partial_cmp(&t_s).unwrap()) {
        Ok(i)  => traj[i].moving,
        Err(i) => {
            // Between two trajectory samples: moving only if both neighbours were.
            let before = i.checked_sub(1).map(|k| traj[k].moving).unwrap_or(false);
            let after  = traj.get(i).map(|p| p.moving).unwrap_or(false);

            before && after
        }
    }
}

/// Target position at time `t_s`, linearly interpolated. `None` outside the pass.
pub(crate) fn target_at(traj: &[TrajPoint], t_s: f64) -> Option<GlobalPx> {
    if traj.is_empty() {
        return None;
    }

    let i = {
        match traj.binary_search_by(|p| p.t_s.partial_cmp(&t_s).unwrap()) {
            Ok(i)  => return Some(traj[i].px),
            Err(i) => i,
        }
    };

    if i == 0 || i >= traj.len() {
        return None;
    }

    let a  = &traj[i - 1];
    let b  = &traj[i];
    let dt = b.t_s - a.t_s;

    if dt <= 0.0 {
        return Some(a.px);
    }

    let f = (t_s - a.t_s) / dt;

    Some(GlobalPx {
        x : a.px.x + (b.px.x - a.px.x) * f,
        y : a.px.y + (b.px.y - a.px.y) * f,
    })
}

/// Estimates the gaze-behind-target lag by scanning shifts of the target trajectory
/// and keeping the one that minimises the mean angular distance over the glides.
pub(crate) fn estimate_lag(traj: &[TrajPoint], moving: &[(f64, GlobalPx)], px_per_deg: f64)
    -> f64
{
    if moving.len() < LAG_MIN_SAMPLES {
        return LAG_DEFAULT_S;
    }

    let mut best_lag  = LAG_DEFAULT_S;
    let mut best_cost = f64::INFINITY;

    let steps = (LAG_MAX_S / LAG_STEP_S).round() as usize;

    for step in 0..=steps {
        let lag = step as f64 * LAG_STEP_S;

        let mut sum = 0.0;
        let mut n   = 0usize;

        for (t_s, gaze_px) in moving {
            let Some(want) = target_at(traj, t_s - lag) else {
                continue;
            };

            let dx = gaze_px.x - want.x;
            let dy = gaze_px.y - want.y;

            sum += (dx * dx + dy * dy).sqrt() / px_per_deg;
            n   += 1;
        }

        if n < LAG_MIN_SAMPLES {
            continue;
        }

        let cost = sum / n as f64;

        if cost < best_cost {
            best_cost = cost;
            best_lag  = lag;
        }
    }

    best_lag
}

/// Component-wise median of a set of vectors. Robust to the stray frame where the
/// device mistracked mid-fixation.
fn median_v3(vs: impl Iterator<Item = DVec3>) -> DVec3 {
    let collected: Vec<DVec3> = vs.collect();

    let mut xs: Vec<f64> = collected.iter().map(|v| v.x).collect();
    let mut ys: Vec<f64> = collected.iter().map(|v| v.y).collect();
    let mut zs: Vec<f64> = collected.iter().map(|v| v.z).collect();

    DVec3::new(median(&mut xs), median(&mut ys), median(&mut zs))
}

/// Median of a slice; zero when empty.
pub(crate) fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    values.sort_by(|a, b| a.partial_cmp(b).unwrap());

    values[values.len() / 2]
}

// --- Errors ---

/// Sweep failure.
#[derive(Debug, thiserror::Error)]
pub enum SweepError {
    #[error("no enabled displays matched the sweep configuration")]
    NoDisplays,
    #[error("aborted by the user")]
    Aborted,
    #[error("device error during the sweep: {0}")]
    Device(#[source] DeviceError),
    #[error("overlay error: {0}")]
    Overlay(String),
    #[error("{0}: pose solve failed: {1}")]
    Pose(String, #[source] crate::pose::PoseError),
    #[error("no display produced a usable fit")]
    NothingFitted,
    #[error("the retrain accepted only {accepted} of the points it needed ({needed}); \
             nothing was written")]
    TooFewPoints {
        /// Targets that were fed to the device.
        accepted : usize,
        /// The floor the ceremony refused below.
        needed   : usize,
    },
    #[error("calibration seed: {0}")]
    Seed(String),
    #[error("the tracker dropped off the bus mid-ceremony ({0}); on this device that \
             is a firmware reboot, which resets the eye model to the factory blob")]
    TrackerLost(String),
    #[error("the tracker did not keep the model across a reconnect: committed a \
             {committed_len} byte body (sha256 {committed_sha256}), read back a \
             {actual_len} byte body (sha256 {actual_sha256}) after reopening")]
    ModelNotKept {
        /// Body hash of what the ceremony committed.
        committed_sha256 : String,
        /// Body hash of what the reopened device handed back.
        actual_sha256    : String,
        /// Body length committed, bytes.
        committed_len    : usize,
        /// Body length read back, bytes. Around 1478 is the factory blob, which is
        /// what a rebooted device holds.
        actual_len       : usize,
    },
    #[error("could not read the readings file: {0}")]
    Readings(String),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn target_interpolates_between_samples() {
        let traj = vec![
            TrajPoint { t_s: 0.0, px: GlobalPx { x: 0.0, y: 0.0 }, moving: true },
            TrajPoint { t_s: 1.0, px: GlobalPx { x: 100.0, y: 50.0 }, moving: true },
        ];

        let p = target_at(&traj, 0.5).unwrap();
        assert!((p.x - 50.0).abs() < 1e-9);
        assert!((p.y - 25.0).abs() < 1e-9);
        assert!(target_at(&traj, -0.1).is_none());
        assert!(target_at(&traj, 1.1).is_none());
    }

    #[test]
    fn saccade_gate_cuts_fast_motion_and_pads() {
        // 100 Hz samples turning 3 degrees in one step: 300 deg/s, far past the gate.
        let mut rays = Vec::new();

        for i in 0..20 {
            let angle = if i >= 10 { 3.0_f64.to_radians() } else { 0.0 };

            rays.push(RaySample {
                t_s    : i as f64 * 0.01,
                origin : DVec3::ZERO,
                dir    : DVec3::new(angle.sin(), 0.0, angle.cos()),
            });
        }

        let keep = saccade_mask(&rays);
        assert!(!keep[10], "the saccade frame survives");
        assert!(!keep[9], "the frame before it survives");
        // Frames well before and after the pad window survive.
        assert!(keep[0]);
        assert!(keep[19]);
    }

    #[test]
    fn lag_recovers_a_known_shift() {
        // A target moving at constant velocity and a gaze track that is the same
        // trajectory delayed by 120 ms.
        let mut traj   = Vec::new();
        let mut moving = Vec::new();

        for i in 0..200 {
            let t = i as f64 * 0.01;

            traj.push(TrajPoint {
                t_s    : t,
                px     : GlobalPx { x: 100.0 * t, y: 0.0 },
                moving : true,
            });
        }

        for i in 30..190 {
            let t = i as f64 * 0.01;

            moving.push((t, GlobalPx { x: 100.0 * (t - 0.12), y: 0.0 }));
        }

        let lag = estimate_lag(&traj, &moving, 60.0);
        assert!((lag - 0.12).abs() < 0.011, "estimated lag {lag}");
    }

    #[test]
    fn median_is_robust_to_an_outlier() {
        let m = median_v3(
            [
                DVec3::new(1.0, 1.0, 1.0),
                DVec3::new(1.1, 0.9, 1.0),
                DVec3::new(50.0, -20.0, 7.0),
                DVec3::new(0.9, 1.1, 1.0),
                DVec3::new(1.0, 1.0, 1.05),
            ]
            .into_iter(),
        );

        assert!((m.x - 1.0).abs() < 0.11);
        assert!((m.y - 1.0).abs() < 0.11);
    }

    #[test]
    fn ring_stops_form_a_centred_ellipse() {
        let out = OutputGeometry {
            name          : "RING".into(),
            enabled       : true,
            detect        : true,
            logical_x     : 0.0,
            logical_y     : 0.0,
            logical_w     : 4520.0,
            logical_h     : 1920.0,
            physical_w_mm : 880.0,
            physical_h_mm : 370.0,
            radius_mm     : 2300.0,
            position_mm   : [0.0, 200.0, 0.0],
            yaw_deg       : 0.0,
            pitch_deg     : 0.0,
            roll_deg      : 0.0,
        };

        let stops = ring_stops(&out, false);
        assert_eq!(stops.len(), RING_POINTS + 1);
        assert_eq!(ring_stops(&out, true).len(), RING_POINTS + 1 + LEAN_DOTS.len());
        assert_eq!((stops[0].0, stops[0].1), (0.5, 0.5));

        for (u, v, _) in &stops[1..] {
            // On the ellipse, inside the envelope margin.
            let e = ((u - 0.5) / RING_RX).powi(2) + ((v - 0.5) / RING_RY).powi(2);
            assert!((e - 1.0).abs() < 1e-9, "off the ellipse: ({u}, {v})");
        }

        // Both semi-axis extremes are reached.
        assert!(stops.iter().any(|(u, _, _)| (*u - (0.5 + RING_RX)).abs() < 1e-9));
        assert!(stops.iter().any(|(_, v, _)| (*v - (0.5 + RING_RY)).abs() < 1e-9));
    }

    #[test]
    fn desk_to_sensor_matches_the_measured_origin_drop() {
        // True seated eye (y 200, z 687) must read low in a frame pitched up 13
        // degrees, the way the device reports it.
        let p = desk_to_sensor([-38.0, 200.0, 687.0], 13.0);

        assert!((p[0] - -38.0).abs() < 1e-9);
        assert!((p[1] - 40.3).abs() < 1.0, "y {}", p[1]);
        assert!((p[2] - 714.3).abs() < 1.0, "z {}", p[2]);

        // Zero pitch is the identity.
        let q = desk_to_sensor([1.0, 2.0, 3.0], 0.0);
        assert!((q[1] - 2.0).abs() < 1e-12 && (q[2] - 3.0).abs() < 1e-12);
    }

    #[test]
    fn origin_scale_recovers_the_ipd_depth_trend() {
        // Reported IPD grows 0.03 mm per mm of depth around 65 mm at z = 700.
        let mut frames = Vec::new();

        for i in 0..200 {
            let z    = 650.0 + 100.0 * i as f64 / 199.0;
            let ipd  = 65.0 + 0.03 * (z - 700.0);
            let half = ipd * 0.5;

            frames.push(TimedFrame {
                t_s   : i as f64 * 0.01,
                frame : Et5Frame {
                    validity_l      : Some(0),
                    validity_r      : Some(0),
                    eye_origin_l_mm : Some([-half, 150.0, z]),
                    eye_origin_r_mm : Some([half, 150.0, z]),
                    ..Default::default()
                },
            });
        }

        let scale = fit_origin_scale(&frames).expect("fit");

        assert!((scale.slope - 0.03).abs() < 1e-6, "slope {}", scale.slope);
        assert!((scale.ipd_ref - 65.0).abs() < 0.5, "ref {}", scale.ipd_ref);

        // At the far end the reported IPD reads high, so origins shrink back.
        assert!(scale.factor(750.0) < 1.0);
        assert!(scale.factor(650.0) > 1.0);
        assert!((scale.factor(700.0) - 1.0).abs() < 0.01);
    }

    #[test]
    fn head_gain_recovers_an_eccentricity_scaled_residual() {
        // Three holds at distinct dots; the residual's x gain grows with u - 0.5:
        // 2e-4 + 4e-4 * du uv per millimetre of head x offset.
        let mk = |t: f64, ox: f64, u: f64| TimedFrame {
            t_s   : t,
            frame : Et5Frame {
                validity_l      : Some(0),
                validity_r      : Some(0),
                eye_origin_l_mm : Some([ox - 32.0, 150.0, 620.0]),
                eye_origin_r_mm : Some([ox + 32.0, 150.0, 620.0]),
                gaze_2d_norm    : Some([u, 0.5]),
                ..Default::default()
            },
        };

        let mut frames = Vec::new();
        let mut stops  = Vec::new();

        for i in 0..200 {
            frames.push(mk(i as f64 * 0.01, 0.0, 0.4));
        }

        stops.push(StopWindow {
            u        : 0.4,
            v        : 0.5,
            px       : GlobalPx { x: 0.0, y: 0.0 },
            t_start  : 0.0,
            t_end    : 2.0,
            parallax : false,
        });

        for (k, dot_u) in [0.35, 0.65, 0.5].into_iter().enumerate() {
            let t0   = 3.0 + k as f64 * 4.0;
            let gain = 2.0e-4 + 4.0e-4 * (dot_u - 0.5);

            for i in 0..300 {
                let t  = t0 + i as f64 * 0.01;
                let ox = 60.0 * (i as f64 / 299.0 * std::f64::consts::TAU).sin();

                frames.push(mk(t, ox, dot_u + gain * ox));
            }

            stops.push(StopWindow {
                u        : dot_u,
                v        : if dot_u == 0.5 { 0.35 } else { 0.5 },
                px       : GlobalPx { x: 0.0, y: 0.0 },
                t_start  : t0,
                t_end    : t0 + 3.1,
                parallax : true,
            });
        }

        let pass = PassData { frames: frames, traj: Vec::new(), stops: stops };
        let gain = fit_head_gain(&pass, &[]).expect("fit");

        assert!((gain.gain_x[0] - 2.0e-4).abs() < 3.0e-5, "gx {:?}", gain.gain_x);
        assert!((gain.gain_x_du[0] - 4.0e-4).abs() < 6.0e-5,
                "gx_du {:?}", gain.gain_x_du);
        assert!(gain.gain_y[0].abs() < 5.0e-5, "gy {:?}", gain.gain_y);
    }

    #[test]
    fn head_gain_recovers_a_linear_residual() {
        // Grid-stop frames hold the head at the reference; hold frames sweep it in x
        // while the reported u slides by 2e-4 uv per millimetre.
        let mk = |t: f64, ox: f64, u: f64| TimedFrame {
            t_s   : t,
            frame : Et5Frame {
                validity_l      : Some(0),
                validity_r      : Some(0),
                eye_origin_l_mm : Some([ox - 32.0, 150.0, 620.0]),
                eye_origin_r_mm : Some([ox + 32.0, 150.0, 620.0]),
                gaze_2d_norm    : Some([u, 0.5]),
                ..Default::default()
            },
        };

        let mut frames = Vec::new();
        let mut stops  = Vec::new();

        for i in 0..200 {
            frames.push(mk(i as f64 * 0.01, 0.0, 0.4));
        }

        stops.push(StopWindow {
            u        : 0.4,
            v        : 0.5,
            px       : GlobalPx { x: 0.0, y: 0.0 },
            t_start  : 0.0,
            t_end    : 2.0,
            parallax : false,
        });

        for i in 0..300 {
            let t  = 3.0 + i as f64 * 0.01;
            let ox = 60.0 * (i as f64 / 299.0 * std::f64::consts::TAU).sin();

            frames.push(mk(t, ox, 0.5 + 2.0e-4 * ox));
        }

        stops.push(StopWindow {
            u        : 0.5,
            v        : 0.5,
            px       : GlobalPx { x: 0.0, y: 0.0 },
            t_start  : 3.0,
            t_end    : 6.1,
            parallax : true,
        });

        let pass = PassData { frames: frames, traj: Vec::new(), stops: stops };
        let gain = fit_head_gain(&pass, &[]).expect("fit");

        assert!((gain.gain_x[0] - 2.0e-4).abs() < 2.0e-5, "gx {:?}", gain.gain_x);
        assert!(gain.gain_y[0].abs() < 5.0e-5, "gy {:?}", gain.gain_y);
        assert!(gain.origin_mm[0].abs() < 1.0);
    }
}
