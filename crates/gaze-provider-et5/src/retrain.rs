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
//! 3. **Seed the session.** `cal_start`, `cal_clear`, then upload the blob the host
//!    already holds. See "The session order" below: this is the step whose absence
//!    cost the 2026-08-28 01:24 run sixteen of its eighteen points.
//! 4. **Four rounds, `cal_points_apply` after each.** Centre, then the four mid-edges,
//!    then the four corners on black, then the four corners again on white: thirteen
//!    points. Two backgrounds because a pupil-radius term in the firmware fit is only
//!    identifiable if it has seen both extremes (Tobii [patent reference removed]), and the eye
//!    needs seconds to adapt after each flip. Thirteen rather than eighteen because
//!    the device keeps only its newest [`DEVICE_POINT_CAP`] points; see "The point
//!    cap" below.
//! 5. **Gaze-gated acceptance.** A point is only added once the device's own reported
//!    gaze has named it as the nearest of the round's targets for most of the last
//!    window. That is Talon's whole gate, and deliberately not an accuracy test; see
//!    "The gate" below. [`gate_verdict`] is the decision as a pure function, so it can
//!    be tested without a device.
//! 6. **Commit, then measure.** `cal_stop` and `cal_retrieve` bank the blob, then a
//!    3x3 health grid reads the firmware's own gaze back against known targets and the
//!    numbers travel with the blob in the calibration file. Nothing is fitted.
//!
//! # The quick ceremony
//!
//! [`plan_quick`] is the same plane and grid with one round: the centre and the four
//! corners on the desktop dimmed under translucent black, seeded with the current
//! blob, applied once. The daemon runs it from the applet's Calibrate button when a
//! session feels off. Five points so the device's fourteen-point store keeps nine of
//! the seed's beside them, which is how the two-background coverage of the full
//! ceremony survives a top-up; no adaptation wait, so the round takes seconds, and the
//! same health check afterwards. The files are written by [`write_calibration`],
//! shared with the CLI.
//!
//! Nothing here fits a correction field, a head gain, or a pose: those were the old
//! `calibrate`'s client-side stages, and they are replaced by the session recordings
//! of `crate::record` and the model of Phase C/D.
//!
//! # The session order
//!
//! `cal_start` -> `cal_clear` -> `cal_apply(seed)` -> rounds -> `cal_stop` +
//! `cal_retrieve`. The seed upload is nottobii's captured Windows order, and it exists
//! because every gate here reads the device's *own* gaze: after `cal_clear` the
//! firmware has no eye model to report one from, so a ceremony that gates on gaze and
//! does not seed is asking the device a question it cannot answer until the first
//! apply lands. The seed is the blob the run is about to overwrite, so the rounds are
//! collected through a working model and the new one replaces it wholesale at the end.
//! [`resolve_seed`] picks it; `--no-seed` is the experiment that leaves it out.
//!
//! # The gate
//!
//! Talon accepts a point when more than 60 of the last 120 gaze samples (also capped
//! at 2 s) had this target as their *nearest*, and nothing else: no absolute accuracy
//! test, no timeout. That is the right shape, because during a retrain the model being
//! measured is the one being replaced, so "the reported gaze is within 3 degrees of
//! the target" is a statement about the old model rather than about where the user is
//! looking. A round with a single target therefore accepts on any 60 frames that carry
//! a gaze point at all, exactly as Talon does. The ellipse test is still available as
//! `--accept-deg` for a deliberate experiment, off by default.
//!
//! Two things Talon does not have, because this ceremony is unattended in a way its
//! is not. First, frames without a gaze point are counted and reported rather than
//! silently failing to vote: a starving gate and a wandering user look identical from
//! the outside otherwise. Second, when *no* frame has carried a gaze point for
//! `gaze_timeout_s`, the point falls back to a dwell — both eyes tracked for 1.5 s of
//! continuous frames — because a device that is plainly seeing the user but not
//! reporting a direction can still be taught. Nothing times out; a point that will not
//! settle nags until the operator takes it (Enter), skips it (`s`), or aborts (`q`).
//!
//! # Manual acceptance
//!
//! The vote knows the reported gaze is *near* the target; it cannot know the user has
//! settled on it, and on a single-target round it trips on the first sixty frames
//! that carry any gaze at all. With a commit channel in hand (the Daydream pad, or
//! `c`) the ambiguity goes away: under `RetrainConfig::manual` nothing is accepted
//! until the user says so, and the vote becomes the sanity check on the click rather
//! than the decision. A click while the vote names another target is refused with a
//! warning, so a stray press cannot feed the firmware a wrong point. Enter still
//! forces, `s` still skips, and the dwell fallback stays off: the user is the fallback.
//!
//! # The point cap
//!
//! The device's calibration store is a FIFO. The 2026-08-28 11:56 run fed eighteen
//! points (nine on black, the same nine on white) and the committed blob's result
//! trailer held the last fourteen in insertion order, the oldest four — black centre
//! and three black mid-edges — gone without any error reply. That blob was 654,498
//! bytes at roughly 46.6 KB per point, 862 bytes under 640 KiB, so the limit may be
//! the store's size rather than a point count; the two are indistinguishable so far
//! and both say fifteen never fits and fourteen is marginal. The schedule therefore
//! stops at thirteen, ordered so the points that would be evicted first are the ones
//! the health passes rate strongest (the centre column), and [`run_retrain`] reads
//! the trailer back and reports how many points the device actually kept. Talon's
//! nine and Tobii's own five/seven/nine-point ceremonies never approach the cap,
//! which is presumably why nobody documents it.
//!
//! # The 2026-08-28 01:24 failure
//!
//! The first real run accepted 2 of 18 points (the centre of round 1 and one mid-edge)
//! and skipped the other 16 on a 15 s timeout; the firmware then fitted a two-point
//! model over the top of a good one and the health pass read 2 to 40 degrees. Three
//! things had to be true at once: the gate demanded reported gaze within 3 degrees of
//! the target from a device that had just been cleared, a point that never tripped the
//! gate was silently skipped, and a ceremony that accepted almost nothing still
//! committed. All three are fixed here, and [`RetrainConfig::min_points`] is the last
//! line: a ceremony that accepted fewer points than that writes nothing at all.

use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, TryRecvError};
use glam::DVec3;
use tracing::{info, warn};
use gaze_core::{DesktopGeometry, GlobalPx, OutputGeometry};
use gaze_overlay::{Mark, OverlayHandle, OverlayState};

use crate::blob::{CalibrationResult, body, body_sha256_hex, decode_trailer};
use crate::blob::BlobReport;
use crate::calibration::{
    CALIBRATION_FORMAT, CalibrationError, Et5Calibration, FieldFit, HealthStop, OutputCalibration,
    OutputPose, desk_to_sensor, plane_corners,
};
use crate::field::FieldMap;
use crate::device::{Device, DeviceError};
use crate::gaze::{Et5Frame, GAZE_HZ};
use crate::ttp::DisplayArea;

/// Fully opaque black, the low-illumination half of the ceremony's rounds.
pub const BLACK: [u8; 4] = [0, 0, 0, 255];

/// Fully opaque white, the high-illumination half.
pub const WHITE: [u8; 4] = [255, 255, 255, 255];

/// Translucent black over the desktop, the quick ceremony's background on a dark
/// desktop: dark enough that the target reads against anything under it, light enough
/// that the screen still shows through and the pupil stays put.
pub const DIM: [u8; 4] = [0, 0, 0, 176];

/// The same veil for a light desktop: translucent white, so the screen dims towards the
/// colour it already is rather than going dark on a user whose pupil is set for light.
pub const DIM_LIGHT: [u8; 4] = [255, 255, 255, 176];

/// Pixels per degree fallback when the geometry cannot supply a local scale.
const FALLBACK_PX_PER_DEG: f64 = 60.0;

/// Talon's training rectangle width, millimetres. Measured along the panel surface,
/// so a curved panel gets the same arc length a flat one would.
pub const AREA_W_MM: f64 = 600.0;

/// Talon's training rectangle height, millimetres.
pub const AREA_H_MM: f64 = 340.0;

/// Where the three columns and rows sit inside the training rectangle. Talon's
/// numbers: the outer points are inset by 5% so a saccade that overshoots the target
/// still lands on the panel.
const POINT_FRACTIONS: [f64; 3] = [0.05, 0.5, 0.95];

/// How far back the acceptance gate looks, seconds. Talon's 120 frames, which were
/// 1.33 s on the 4C's 90 Hz stream; the ET5 streams gaze at [`GAZE_HZ`], so the
/// count is derived from the time rather than copied.
pub const GATE_WINDOW_S: f64 = 1.33;

/// How long the target must have been the nearest one within the window, seconds.
/// Talon's 60 of 120 frames: half the window.
pub const GATE_HOLD_S: f64 = 0.67;

/// Frames the acceptance gate looks back over: [`GATE_WINDOW_S`] at [`GAZE_HZ`].
pub const GATE_WINDOW: usize = (GATE_WINDOW_S * GAZE_HZ) as usize;

/// How many of [`GATE_WINDOW`] must name the target before it is accepted:
/// [`GATE_HOLD_S`] at [`GAZE_HZ`].
pub const GATE_MIN_HITS: usize = (GATE_HOLD_S * GAZE_HZ) as usize;

/// Default nag interval, seconds. A point that has not tripped the gate by then says
/// so and keeps waiting; nothing is ever skipped without the operator asking.
pub const POINT_TIMEOUT_S: f64 = 15.0;

/// Default patience for the *first gaze point of all*, seconds. Past this with the
/// device reporting no direction whatsoever, the point falls back to a dwell.
pub const GAZE_TIMEOUT_S: f64 = 5.0;

/// Continuous both-eyes-tracked time that carries a point under the dwell fallback,
/// seconds. Long enough that it cannot be satisfied on the way to the target.
pub const DWELL_S: f64 = 1.5;

/// Default floor on accepted points. Below this the ceremony writes nothing: the
/// 2026-08-28 01:24 run committed a two-point model over a good one, and a partial
/// retrain is worse than no retrain because it destroys what it replaces.
pub const MIN_POINTS: usize = 9;

/// The most calibration points the device retains, newest first; older ones are
/// evicted silently. Measured on 2026-08-28: eighteen fed, the last fourteen read
/// back. See "The point cap" in the module docs — the limit may really be
/// [`DEVICE_BLOB_CAP_BYTES`], which the same run came within 862 bytes of.
pub const DEVICE_POINT_CAP: usize = 14;

/// The calibration store size the observed cap lines up with: 640 KiB, at about
/// 46.6 KB of blob per point.
pub const DEVICE_BLOB_CAP_BYTES: usize = 640 * 1024;

/// How often the per-point status line is printed while waiting, seconds.
const STATUS_S: f64 = 1.0;

/// Pupil adaptation wait after a background change, seconds. Same value and reasoning
/// as `crate::record`: sized for dilation, which is the slow direction.
const ADAPT_S: f64 = 4.0;

/// Settle on the anchor dot after the dim background comes up, seconds. No pupil
/// change to wait out; only the saccade to the dot.
const DIM_SETTLE_S: f64 = 1.0;

/// The quick ceremony's targets, indices into the nine-point grid: the centre, then
/// the four corners of the training rectangle in Talon's far-apart order. Five, so
/// that under the [`DEVICE_POINT_CAP`] of fourteen the device keeps nine of the seed's
/// points beside them and the two-background coverage of the full ceremony survives
/// a top-up.
pub const QUICK_POINTS: [usize; 5] = [4, 0, 8, 6, 2];

/// Poll interval of the target loops. Short enough that the gate sees every frame the
/// device sends and the caption stays responsive.
const TICK: Duration = Duration::from_millis(8);

/// Settling time after the device takes a plane declaration, before its 2D output is
/// trusted to be on the new plane.
const PLANE_SETTLE: Duration = Duration::from_millis(200);

/// Settling time after the seed blob goes up, before the first target is shown. The
/// upload is 600 KB and the firmware has to load the model behind it.
const SEED_SETTLE: Duration = Duration::from_millis(500);

/// Pause between closing the tracker and reopening it for the persistence check.
/// Deliberately longer than a re-enumeration takes, so a reboot in progress fails the
/// open instead of racing it.
const RECONNECT_SETTLE: Duration = Duration::from_millis(1000);

/// Health-check dwell per stop, seconds.
const HEALTH_DWELL_S: f64 = 1.0;

/// How much of each health dwell the median is taken over, seconds, counted back from
/// the end. The rest is the saccade and the lock-on.
const HEALTH_MEDIAN_S: f64 = 0.6;

/// Default health-check stops per axis. Four gives sixteen stops: enough for the
/// correction field fitted from them afterwards to hold out a cell and still fit a
/// quadratic, at sixteen seconds of dwelling.
pub const HEALTH_STEPS: usize = 4;

/// Mid grey, [`Background::Neutral`].
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
    /// Optional acceptance radius, degrees. `None` is the shipped gate: Talon's
    /// nearest-target vote with no absolute accuracy test at all. Setting it puts an
    /// ellipse of this radius around the target on top of the vote, which only makes
    /// sense when the device is already reporting a trustworthy gaze.
    pub accept_deg        : Option<f64>,
    /// Nag interval, seconds. Not a timeout: when it elapses the ceremony says which
    /// point it is still waiting on and keeps waiting.
    pub point_timeout_s   : f64,
    /// How long a point waits for the device to report any gaze at all before the
    /// dwell fallback arms, seconds.
    pub gaze_timeout_s    : f64,
    /// The first round that ends in a `cal_points_apply`. 1 applies after every round
    /// (Talon); a larger number holds the earlier rounds' points until it, so the
    /// first fit the firmware runs has more than one point in it.
    pub apply_from_round  : usize,
    /// Accepted points below which the ceremony refuses to commit anything.
    pub min_points        : usize,
    /// Ask the device what point it would like after each round and log the answer.
    /// Exploratory: nothing depends on the reply.
    pub suggest           : bool,
    /// Accept a point only on `RetrainKey::Commit` (or Enter): the vote checks the
    /// click instead of deciding, and the dwell fallback is off. See "Manual
    /// acceptance" in the module docs.
    pub manual            : bool,
    /// Health-check stops per axis (`n` by `n`), at least 2.
    pub health_steps      : usize,
    /// What the health check runs on. Grey by default, so its numbers compare across
    /// runs; the daemon keeps the quick ceremony's dim so the two read as one thing.
    pub health_background : Background,
    /// Whether the desktop is in its dark mode, which picks the veil's colour
    /// (`Background::Dim`). The caller reads it from the theme; true when unknown.
    pub dark              : bool,
}

impl Default for RetrainConfig {
    fn default() -> Self {
        Self {
            display           : "DP-1".into(),
            tracker_pitch_deg : 0.0,
            area_w_mm         : AREA_W_MM,
            area_h_mm         : AREA_H_MM,
            area_full         : false,
            accept_deg        : None,
            point_timeout_s   : POINT_TIMEOUT_S,
            gaze_timeout_s    : GAZE_TIMEOUT_S,
            apply_from_round  : 1,
            min_points        : MIN_POINTS,
            suggest           : false,
            manual            : false,
            health_steps      : HEALTH_STEPS,
            health_background : Background::Neutral,
            dark              : true,
        }
    }
}

// --- Seed ---

/// The blob uploaded into the fresh session after `cal_clear`, so the rounds are
/// collected through a working eye model rather than through a cleared one.
#[derive(Clone)]
pub struct Seed {
    /// Where it was read from, for the log.
    pub path   : PathBuf,
    /// The blob itself, exactly as `cal_apply` wants it.
    pub blob   : Vec<u8>,
    /// Points in its result trailer. A blob with a trailer is a trained model of this
    /// device; one without is a factory default or someone else's file.
    pub points : usize,
}

/// Deliberately not the derived one: the blob is 600 KB and nobody wants it in a log
/// line.
impl std::fmt::Debug for Seed {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Seed")
            .field("path"  , &self.path)
            .field("bytes" , &self.blob.len())
            .field("points", &self.points)
            .finish()
    }
}

/// What [`resolve_seed`] decided, with the sentence explaining it. The reason is
/// printed by `--dry-run` and logged by the real run, because "was the session seeded"
/// is the first question to ask of a ceremony that went wrong.
#[derive(Clone, Debug)]
pub struct SeedChoice {
    /// The blob to upload, if any.
    pub seed : Option<Seed>,
    /// One line saying which file was used, or why none was.
    pub why  : String,
}

/// Picks the blob that seeds the calibration session.
///
/// `no_seed` wins outright. Otherwise an explicit `--seed` file is used and any
/// problem with it is an error, since silently ignoring the file the operator named is
/// how a run ends up unseeded without anyone noticing. With neither, the blob backup
/// this run is about to overwrite is used when it exists and decodes with a result
/// trailer, and skipped with a reason when it does not.
///
/// Reads the file; the caller does this before opening the device so a bad path costs
/// nothing.
pub fn resolve_seed(explicit: Option<&Path>, blob: &Path, no_seed: bool)
    -> Result<SeedChoice, RetrainError>
{
    if no_seed {
        return Ok(SeedChoice {
            seed : None,
            why  : "--no-seed: the session runs on a cleared model".into(),
        });
    }

    // An explicit file is a request, not a preference: report what went wrong with it.
    if let Some(path) = explicit {
        let bytes = std::fs::read(path)
            .map_err(|e| RetrainError::Seed(format!("reading {}: {e}", path.display())))?;
        let Some((_, table)) = decode_trailer(&bytes) else {
            return Err(RetrainError::Seed(format!(
                "{} has no result trailer, so it is not a trained model of this device",
                path.display())));
        };

        return Ok(SeedChoice {
            why  : format!("seeding from {} ({} bytes, {} trailer points)",
                           path.display(), bytes.len(), table.targets.len()),
            seed : Some(Seed {
                path   : path.to_path_buf(),
                blob   : bytes,
                points : table.targets.len(),
            }),
        });
    }

    if !blob.exists() {
        return Ok(SeedChoice {
            seed : None,
            why  : format!("no seed: {} does not exist yet", blob.display()),
        });
    }

    let bytes = {
        match std::fs::read(blob) {
            Ok(bytes) => bytes,
            Err(e)    => {
                return Ok(SeedChoice {
                    seed : None,
                    why  : format!("no seed: {} could not be read ({e})", blob.display()),
                });
            }
        }
    };

    let Some((_, table)) = decode_trailer(&bytes) else {
        return Ok(SeedChoice {
            seed : None,
            why  : format!("no seed: {} has no result trailer", blob.display()),
        });
    };

    Ok(SeedChoice {
        why  : format!("seeding from the current backup {} ({} bytes, {} trailer \
                        points)", blob.display(), bytes.len(), table.targets.len()),
        seed : Some(Seed {
            path   : blob.to_path_buf(),
            blob   : bytes,
            points : table.targets.len(),
        }),
    })
}

// --- Background ---

/// The overlay colour a round runs on. Two rounds of the same schedule on opposite
/// backgrounds walk the pupil across most of its range; the dim one leaves the pupil
/// where the desktop had it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Background {
    /// Fully dark: the dilated end.
    Black,
    /// Fully bright: the constricted end.
    White,
    /// Opaque mid grey, the health check's default: neither pupil extreme, so its
    /// numbers describe an ordinary screen rather than the training conditions.
    Neutral,
    /// The desktop dimmed under a translucent veil, for the quick ceremony: the
    /// targets are visible, the screen is still there, and the pupil stays near the
    /// state it works at. Black over a dark desktop, white over a light one.
    Dim,
}

impl Background {
    /// The colour the overlay paints. `dark` is the desktop's mode, which only the
    /// veil follows: the extremes and the grey are the same on any desktop.
    pub fn color(&self, dark: bool) -> [u8; 4] {
        match self {
            Self::Black   => BLACK,
            Self::White   => WHITE,
            Self::Neutral => NEUTRAL,
            Self::Dim     => if dark { DIM } else { DIM_LIGHT },
        }
    }

    /// The name used in captions and logs.
    pub fn name(&self) -> &'static str {
        match self {
            Self::Black => "black",
            Self::White   => "white",
            Self::Neutral => "grey",
            Self::Dim     => "dim",
        }
    }

    /// How long the eye is given on the anchor dot after the flip to this
    /// background, seconds. The extremes need the pupil to settle; the dim one only
    /// needs the eye to find the dot.
    pub fn adapt_s(&self) -> f64 {
        match self {
            Self::Black | Self::White | Self::Neutral => ADAPT_S,
            Self::Dim                                => DIM_SETTLE_S,
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
    /// The four rounds.
    pub rounds   : Vec<Round>,
    /// Acceptance tolerance in panel uv, per axis, from `accept_deg`. `None` when no
    /// radius was asked for, which is the default gate.
    pub tol_uv   : Option<(f64, f64)>,
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
    /// Optional acceptance radius across and down the panel, uv. `None` is Talon's
    /// gate: the vote alone decides and the median is reported but not tested.
    pub tol_uv   : Option<(f64, f64)>,
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
/// The condition is Talon's, and only Talon's by default: the target has to have been
/// the nearest of *this round's* targets for at least `min_hits` of the window, which
/// rejects "on the way there" and "just left" without ever asking the device to be
/// accurate. It cannot be, during a retrain: the model reporting the gaze is the one
/// being replaced. A round with a single target is therefore carried by any
/// `min_hits` frames that reported a gaze at all, which is exactly what Talon does and
/// what makes the first round possible on a freshly cleared device.
///
/// `gate.tol_uv` adds an ellipse around the target on top of the vote. It is off by
/// default and exists for a deliberate experiment; a non-positive radius accepts
/// nothing, since it means the caller has no scale for this panel.
///
/// The median (rather than the mean) of the voting samples is always reported, because
/// it says where the old model thought the user was looking and a blink recovery or a
/// dropout frame is a wild value rather than a small one.
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

    // The vote is the whole gate unless a radius was asked for. A non-positive one
    // would either divide by zero or accept everything, and means the caller has no
    // scale for this panel, so it accepts nothing instead.
    let within = {
        match gate.tol_uv {
            None                    => true,
            Some((tol_u, tol_v))    => {
                let du = (centre[0] - target[0]) / tol_u;
                let dv = (centre[1] - target[1]) / tol_v;

                tol_u > 0.0 && tol_v > 0.0 && du * du + dv * dv <= 1.0
            }
        }
    };

    Verdict {
        hits     : hits,
        median   : Some(centre),
        accepted : hits >= gate.min_hits && within,
    }
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
/// targets, and the four rounds.
///
/// Fails only when the display is missing from the desk config or disabled.
pub fn plan(geometry: &DesktopGeometry, config: &RetrainConfig)
    -> Result<RetrainPlan, RetrainError>
{
    let out = geometry.outputs.iter()
        .find(|o| o.name == config.display && o.enabled)
        .ok_or(RetrainError::NoDisplays)?;

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

/// The quick ceremony: the same plane and grid as [`plan`], one round of
/// [`QUICK_POINTS`] on the dimmed desktop, applied once. What the applet's Calibrate
/// button runs when a session merely feels off: seeded with the current blob, it
/// tops the model up with five fresh points in a few seconds instead of retraining it
/// from thirteen over two backgrounds. `config.min_points` should be at most five.
pub fn plan_quick(geometry: &DesktopGeometry, config: &RetrainConfig)
    -> Result<RetrainPlan, RetrainError>
{
    let mut plan = self::plan(geometry, config)?;

    plan.rounds = vec![Round {
        name       : "quick",
        background : Background::Dim,
        points     : QUICK_POINTS.to_vec(),
    }];

    Ok(plan)
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

/// The four rounds: Talon's three schedules on black, then the corners again on
/// white — thirteen points, one under [`DEVICE_POINT_CAP`].
///
/// Small rounds with a commit after each are what Talon does and what the firmware's
/// own fit expects: `cal_points_apply` folds the points collected since the last
/// apply into the model, so a bad point poisons one round rather than the ceremony,
/// and the later rounds are collected through an already-improving model.
///
/// Only the corners repeat on white because the cap allows four more points and the
/// corners are where every health pass so far has been weakest. The order also puts
/// the centre first, so if the store is a byte budget and a heavy run overflows by
/// one, the point the device evicts is the one the model finds easiest.
fn rounds() -> Vec<Round> {
    // Bottom, left, right, top — Talon's order, which keeps consecutive targets far
    // apart so a lingering fixation cannot satisfy the next point.
    vec![
        Round { name: "centre",    background: Background::Black, points: vec![4] },
        Round { name: "mid-edges", background: Background::Black, points: vec![7, 3, 5, 1] },
        Round { name: "corners",   background: Background::Black, points: vec![0, 8, 6, 2] },
        Round { name: "corners",   background: Background::White, points: vec![0, 8, 6, 2] },
    ]
}

/// The acceptance radius in panel uv, per axis, from an angle at the nominal eye.
/// `None` in, `None` out: no radius asked for is the default gate.
fn accept_tolerance_uv(
    geometry   : &DesktopGeometry,
    out        : &OutputGeometry,
    accept_deg : Option<f64>,
)
    -> Option<(f64, f64)>
{
    let accept_deg = accept_deg?;
    let (h, v)     = geometry.px_per_deg(geometry.eye(), out.uv_to_px(0.5, 0.5))
        .unwrap_or((FALLBACK_PX_PER_DEG, FALLBACK_PX_PER_DEG));

    Some((accept_deg * h / out.logical_w, accept_deg * v / out.logical_h))
}

// --- Outcome ---

/// Which rule decided a target. Kept as one enum rather than a pile of booleans so
/// the ceremony summary can count the cases without any of them overlapping.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointOutcome {
    /// The nearest-target vote carried it. The ordinary case.
    Gate,
    /// The user committed it (pad click or `c`) and the vote agreed.
    Clicked,
    /// The device reported no gaze at all, so both eyes tracked for [`DWELL_S`]
    /// carried it instead.
    Dwell,
    /// The operator pressed Enter.
    Forced,
    /// The operator pressed `s`. The only way a point is not added.
    Skipped,
}

// --- PointOutcome ---

impl PointOutcome {
    /// Whether `cal_add_point` was sent for the target.
    pub fn accepted(&self) -> bool {
        !matches!(self, Self::Skipped)
    }

    /// The word used in the per-point table and the logs.
    pub fn label(&self) -> &'static str {
        match self {
            Self::Gate    => "added",
            Self::Clicked => "clicked",
            Self::Dwell   => "dwell",
            Self::Forced  => "forced",
            Self::Skipped => "SKIPPED",
        }
    }
}

/// What happened at one target.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointResult {
    /// Index into [`RetrainPlan::points`].
    pub index   : usize,
    /// Which rule decided it.
    pub outcome : PointOutcome,
    /// Window samples naming this target when the decision was taken.
    pub hits    : usize,
    /// How long the target was shown, seconds.
    pub wait_s  : f64,
    /// Gaze frames seen while the target was up.
    pub frames  : usize,
    /// How many of them carried a usable gaze point. A gate that starves does so here,
    /// and it is the difference between "the user looked away" and "the device stopped
    /// answering".
    pub gaze    : usize,
    /// How many of them had both eyes tracked.
    pub eyes    : usize,
}

// --- PointResult ---

impl PointResult {
    /// Whether the target was fed to the device.
    pub fn accepted(&self) -> bool {
        self.outcome.accepted()
    }
}

/// How one round went, for the ceremony summary.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct RoundSummary {
    /// Round name, matching [`Round::name`].
    pub name       : &'static str,
    /// The background it ran on.
    pub background : Background,
    /// Targets the vote carried.
    pub gate       : usize,
    /// Targets the user clicked in, vote agreeing.
    pub clicked    : usize,
    /// Targets the dwell fallback carried.
    pub dwell      : usize,
    /// Targets the operator forced in.
    pub forced     : usize,
    /// Targets the operator skipped.
    pub skipped    : usize,
    /// Whether the round ended in a `cal_points_apply`.
    pub applied    : bool,
}

// --- RoundSummary ---

impl RoundSummary {
    /// Targets fed to the device in this round, however they were decided.
    pub fn accepted(&self) -> usize {
        self.gate + self.clicked + self.dwell + self.forced
    }
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
    /// Points the device actually retained, read off the result trailer; fewer than
    /// `accepted` means the store evicted the oldest (see "The point cap").
    pub kept        : Option<usize>,
    /// Every target, in ceremony order.
    pub results     : Vec<PointResult>,
    /// Every round, in ceremony order.
    pub rounds      : Vec<RoundSummary>,
    /// The tracker's `(bus, address)` when the ceremony opened it, so a
    /// re-enumeration during the ceremony is visible against the address the
    /// persistence check reads back.
    pub usb_before  : (u8, u8),
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
/// Declares the plane, opens one calibration session in the order documented at the
/// top of this module (`cal_start`, `cal_clear`, `cal_apply(seed)`), walks the six
/// rounds, and closes with `cal_stop` + `cal_retrieve`.
///
/// Three ways this ends without a blob. `q` aborts; a ceremony that accepted fewer
/// than `config.min_points` targets refuses to commit; and the tracker leaving the USB
/// bus is reported as [`RetrainError::TrackerLost`], because on this device that is a
/// firmware reboot and a reboot resets the eye model to the factory blob. The first
/// two close the session on the way out. In every case the caller writes nothing, and
/// in none of them is the device as it was: it holds whatever the applied rounds
/// taught it, or the factory model. That is why the caller must also run
/// [`verify_persistence`] before it writes anything.
pub fn run_retrain(
    device   : &mut Device,
    overlay  : &OverlayHandle,
    keys     : Option<&Receiver<RetrainKey>>,
    config   : &RetrainConfig,
    plan     : &RetrainPlan,
    seed     : Option<&Seed>,
)
    -> Result<RetrainOutcome, RetrainError>
{
    let gate = Gate {
        window   : GATE_WINDOW,
        min_hits : GATE_MIN_HITS,
        tol_uv   : plan.tol_uv,
        aspect   : aspect_of(plan),
    };

    info!("retrain plane (desk config, sensor frame): tl=({:.0},{:.0},{:.0}) \
           tr=({:.0},{:.0},{:.0}) bl=({:.0},{:.0},{:.0})",
          plan.area.tl_mm[0], plan.area.tl_mm[1], plan.area.tl_mm[2],
          plan.area.tr_mm[0], plan.area.tr_mm[1], plan.area.tr_mm[2],
          plan.area.bl_mm[0], plan.area.bl_mm[1], plan.area.bl_mm[2]);

    let usb_before = device.usb_address();
    info!("tracker on bus {}.{} at the start of the ceremony",
          usb_before.0, usb_before.1);

    // The first round's background and its anchor dot go up before the plane and the
    // seed, so the screen answers the request at once and the seconds the upload
    // takes count as the eye's adaptation rather than adding to it.
    let shown = {
        match plan.rounds.first() {
            Some(round) => {
                let anchor = anchor_point(plan, round)?;

                set_overlay_background(Some(round.background.color(config.dark)));
                show_target(overlay, anchor.px, 0.0)?;

                Some((round.background, anchor.px, Instant::now()))
            }
            None => None,
        }
    };

    device.set_display_area_corners(plan.area).map_err(device_error)?;
    std::thread::sleep(PLANE_SETTLE);

    device.cal_begin().map_err(device_error)?;

    // Straight after the clear, before a single point is collected: the rounds are
    // gated on the device's own gaze and a cleared model does not report one.
    match seed {
        Some(seed) => {
            device.cal_seed(&seed.blob).map_err(device_error)?;
            info!("seeded the session with {} ({} bytes, {} trailer points)",
                  seed.path.display(), seed.blob.len(), seed.points);
            std::thread::sleep(SEED_SETTLE);
        }
        None       => warn!("no seed blob: the rounds run on a cleared model, so the \
                             device may report no gaze until the first apply"),
    }

    let frames_rx = device.gaze_stream();

    let mut results     = Vec::new();
    let mut summaries   = Vec::new();
    let mut suggestions = Vec::new();
    let mut background  = None;
    let mut pending     = 0usize;

    // Whatever of the adaptation the seeding did not already cover.
    if let Some((first, anchor_px, since)) = shown {
        let remaining = first.adapt_s() - since.elapsed().as_secs_f64();

        info!("adapting to {}: eyes on the dot", first.name());

        if remaining > 0.0 {
            wait(overlay, anchor_px, remaining, &frames_rx, keys)?;
        }

        background = Some(first);
    }

    let total_rounds = plan.rounds.len();

    for (number, round) in plan.rounds.iter().enumerate() {
        let result = run_round(device, overlay, keys, config, plan, &gate, &frames_rx,
                               round, number + 1, total_rounds, &mut background,
                               &mut pending, &mut results, &mut suggestions);

        match result {
            Ok(summary) => summaries.push(summary),
            Err(e)      => {
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

    let accepted = results.iter().filter(|r| r.accepted()).count();
    let applied  = summaries.iter().filter(|s| s.applied).count();

    log_summary(&summaries);

    // A model fitted from a handful of points is not a worse model, it is a broken
    // one, and committing it destroys the one it replaced.
    let needed = config.min_points.max(1);

    if accepted < needed {
        if let Err(stop) = device.cal_stop() {
            warn!("could not close the calibration session after the refusal: {stop}");
        }

        set_overlay_background(None);
        let _ = overlay.set(OverlayState::default());

        return Err(RetrainError::TooFewPoints { accepted: accepted, needed: needed });
    }

    // `cal_end` fits nothing, so anything still unapplied would be collected and then
    // thrown away. Only reachable with `apply_from_round` past the last round.
    if pending > 0 {
        device.cal_points_apply().map_err(device_error)?;
        info!("final apply ({pending} points held past the last round)");
    }

    let blob = device.cal_end().map_err(device_error)?;

    // The background is left up for the health check; its end clears the overlay,
    // and a caller that runs no health check stops the overlay anyway.
    info!("retrain committed: {accepted}/{} points over {applied} applied rounds, \
           {} byte blob", results.len(), blob.len());

    // The device's store is a FIFO (see "The point cap"): what it kept is in the
    // trailer, and a shortfall is the only sign that anything was evicted.
    let result = decode_trailer(&blob).map(|(_, table)| table);
    let kept   = result.as_ref().map(|table| table.targets.len());

    match kept {
        Some(kept) if kept < accepted => {
            warn!("the device kept {kept} of the {accepted} accepted points; the oldest \
                   {} were evicted (cap {DEVICE_POINT_CAP} points or \
                   {DEVICE_BLOB_CAP_BYTES} bytes; this blob is {} bytes)",
                  accepted - kept, blob.len());
        }
        Some(kept) => {
            info!("the device kept all {kept} accepted points ({} of {} bytes)",
                  blob.len(), DEVICE_BLOB_CAP_BYTES);
        }
        None => {
            warn!("the committed blob carries no result trailer, so how many points \
                   the device kept is unknown");
        }
    }

    Ok(RetrainOutcome {
        body_sha256 : body_sha256_hex(&blob),
        result      : result,
        kept        : kept,
        blob        : blob,
        results     : results,
        rounds      : summaries,
        usb_before  : usb_before,
        accepted    : accepted,
        applied     : applied,
        suggestions : suggestions,
    })
}

/// Logs the per-round tally. Runs before the commit decision, so a ceremony that
/// refuses to commit still says what it saw.
fn log_summary(summaries: &[RoundSummary]) {
    for (number, summary) in summaries.iter().enumerate() {
        info!("round {}/{} {} ({}): {} accepted ({} gate, {} clicked, {} dwell, \
               {} forced), {} skipped, {}",
              number + 1, summaries.len(), summary.name, summary.background.name(),
              summary.accepted(), summary.gate, summary.clicked, summary.dwell,
              summary.forced,
              summary.skipped,
              if summary.applied { "applied" } else { "not applied" });
    }
}

/// Runs one round and reports how it went.
///
/// The gate compares against every target of the round, including ones already added
/// or skipped: a target that has had its turn still competes for "nearest", which can
/// only make a later point harder to accept, never easier.
///
/// `pending` counts points added since the last apply and spans rounds, so deferring
/// the first apply with `apply_from_round` holds the earlier points rather than
/// losing them.
#[allow(clippy::too_many_arguments)]
fn run_round(
    device      : &mut Device,
    overlay     : &OverlayHandle,
    keys        : Option<&Receiver<RetrainKey>>,
    config      : &RetrainConfig,
    plan        : &RetrainPlan,
    gate        : &Gate,
    frames_rx   : &Receiver<Et5Frame>,
    round       : &Round,
    number      : usize,
    total       : usize,
    background  : &mut Option<Background>,
    pending     : &mut usize,
    results     : &mut Vec<PointResult>,
    suggestions : &mut Vec<String>,
)
    -> Result<RoundSummary, RetrainError>
{
    // The pupil is still moving for seconds after a flip; a round collected during
    // that would be labelled with an illumination the eye had not reached.
    if *background != Some(round.background) {
        *background = Some(round.background);
        set_overlay_background(Some(round.background.color(config.dark)));

        let anchor = anchor_point(plan, round)?;

        info!("adapting to {}: eyes on the dot", round.background.name());

        show_target(overlay, anchor.px, 0.0)?;
        wait(overlay, anchor.px, round.background.adapt_s(), frames_rx, keys)?;
    }

    let targets: Vec<[f64; 2]> = round.points.iter()
        .map(|i| [plan.points[*i].u, plan.points[*i].v])
        .collect();

    let mut summary = RoundSummary {
        name       : round.name,
        background : round.background,
        gate       : 0,
        clicked    : 0,
        dwell      : 0,
        forced     : 0,
        skipped    : 0,
        applied    : false,
    };

    for (position, index) in round.points.iter().copied().enumerate() {
        let point = plan.points[index];

        info!("round {number}/{total} {} ({}): {} {}/{}, eyes on the dot",
              round.name, round.background.name(), point.label,
              position + 1, round.points.len());

        show_target(overlay, point.px, 0.0)?;

        let result = run_point(overlay, keys, config, gate, frames_rx, &targets,
                               position, index, point.px, point.label)?;

        match result.outcome {
            PointOutcome::Gate    => summary.gate    += 1,
            PointOutcome::Clicked => summary.clicked += 1,
            PointOutcome::Dwell   => summary.dwell   += 1,
            PointOutcome::Forced  => summary.forced  += 1,
            PointOutcome::Skipped => summary.skipped += 1,
        }

        if result.accepted() {
            device.cal_add_point(point.u, point.v, 3).map_err(device_error)?;
            *pending += 1;

            info!("round {number} {}: {} {} at uv ({:.3}, {:.3}) after {:.1}s \
                   ({} hits, {}/{} frames with gaze)",
                  round.name, result.outcome.label(), point.label, point.u, point.v,
                  result.wait_s, result.hits, result.gaze, result.frames);
        }
        else {
            warn!("round {number} {}: skipped {} at uv ({:.3}, {:.3}) after {:.1}s \
                   ({} hits of {} needed, {}/{} frames with gaze)",
                  round.name, point.label, point.u, point.v, result.wait_s,
                  result.hits, gate.min_hits, result.gaze, result.frames);
        }

        results.push(result);
    }

    // Fold the collected points into the on-device model before the next round is
    // collected. `apply_from_round` can hold the first fit back; the points wait in
    // the device until then rather than being lost.
    if *pending == 0 {
        warn!("round {number} has nothing unapplied; skipping the apply");

        return Ok(summary);
    }

    if number < config.apply_from_round {
        info!("round {number}/{total} not applied (--apply-from-round {}); \
               {} points held", config.apply_from_round, *pending);

        return Ok(summary);
    }

    // The last apply is the slow one: the firmware refits the whole model, which
    // takes seconds. The dot goes so the eye is not asked to hold a target through
    // it; the background stays, because the health check that follows runs on it and
    // the two should read as one operation.
    if number == total {
        let _ = overlay.set(OverlayState { background: overlay_background(), ..OverlayState::default() });
    }

    device.cal_points_apply().map_err(device_error)?;
    info!("round {number}/{total} applied ({} points)", *pending);

    *pending        = 0;
    summary.applied = true;

    if config.suggest {
        suggestions.push(suggestion_line(device, number));
    }

    Ok(summary)
}

/// Shows one target until the gate accepts it, the dwell fallback carries it, or the
/// operator forces or skips it. Nothing here times out.
///
/// The two things this does beyond running the gate are both about the failure mode
/// the 2026-08-28 01:24 run hit, where the gate starved on a device that was reporting
/// no gaze and every point was silently dropped after fifteen seconds. Frames are
/// counted three ways (seen, carrying a gaze point, both eyes tracked) and the tally
/// is printed once a second, and a target that has seen no gaze at all for
/// `gaze_timeout_s` arms the dwell: both eyes tracked for [`DWELL_S`] of continuous
/// frames adds it. Once armed the dwell stays armed, since a device that lost its
/// answer for five seconds has not proved anything by finding it again.
///
/// `name` is the target's short name for the nag line. The mark on screen shows the
/// gate's progress and nothing else; the numbers go to the log.
#[allow(clippy::too_many_arguments)]
fn run_point(
    overlay   : &OverlayHandle,
    keys      : Option<&Receiver<RetrainKey>>,
    config    : &RetrainConfig,
    gate      : &Gate,
    frames_rx : &Receiver<Et5Frame>,
    targets   : &[[f64; 2]],
    position  : usize,
    index     : usize,
    px        : GlobalPx,
    name      : &str,
)
    -> Result<PointResult, RetrainError>
{
    let start = Instant::now();

    // Anything queued from the previous target describes the previous target.
    while next_frame(frames_rx)?.is_some() {}

    let mut times  : Vec<f64>      = Vec::new();
    let mut samples: Vec<[f64; 2]> = Vec::new();
    let mut tally                  = Tally::default();

    let mut dwell_since : Option<f64> = None;
    let mut dwell_armed = false;
    let mut forced      = false;
    let mut clicked     = false;
    let mut next_status = STATUS_S;
    let mut next_nag    = config.point_timeout_s;

    loop {
        let now = start.elapsed().as_secs_f64();

        if let Some(keys) = keys {
            match keys.try_recv() {
                Ok(RetrainKey::Quit)    => return Err(RetrainError::Aborted),
                Ok(RetrainKey::Skip)    => {
                    return Ok(tally.result(index, PointOutcome::Skipped, 0, now));
                }
                // The operator can see the dot and the user; when the gate will not
                // trip but the fixation is plainly good, Enter takes the point.
                Ok(RetrainKey::Advance) => forced = true,
                // The user says they are on it; the vote below gets to disagree.
                Ok(RetrainKey::Commit)  => clicked = true,
                Err(_)                => {}
            }
        }

        // One pass over everything that arrived since the last tick, counting what the
        // device did and did not report.
        while let Some(frame) = next_frame(frames_rx)? {
            tally.frames += 1;

            if frame.left_valid() && frame.right_valid() {
                tally.eyes += 1;
                dwell_since.get_or_insert(now);
            }
            else {
                dwell_since = None;
            }

            if let Some(uv) = valid_uv(&frame) {
                tally.gaze += 1;
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

        if forced {
            return Ok(tally.result(index, PointOutcome::Forced, verdict.hits, now));
        }

        if clicked {
            clicked = false;

            // A click is the decision; the vote is the check. Naming this target for
            // at least half the window is a low bar on purpose: the model reporting
            // the gaze is the one being replaced, so all it can refuse is a click
            // taken while plainly looking at another target of this round.
            if verdict.hits > 0 && verdict.hits * 2 >= samples.len() {
                return Ok(tally.result(index, PointOutcome::Clicked, verdict.hits, now));
            }

            warn!("click refused at {name}: the reported gaze names this target in {}/{} \
                   recent frames; look at the dot and click again (Enter forces it)",
                  verdict.hits, samples.len());
        }

        if verdict.accepted && !config.manual {
            return Ok(tally.result(index, PointOutcome::Gate, verdict.hits, now));
        }

        // The device has answered nothing for long enough that waiting on the gate is
        // waiting on something that is not coming. Under manual acceptance the user
        // is the fallback.
        if !config.manual && !dwell_armed && tally.gaze == 0 && now >= config.gaze_timeout_s {
            dwell_armed = true;

            warn!("no gaze reported in {now:.1}s at this target ({}/{} frames had both \
                   eyes): falling back to a {DWELL_S:.1}s dwell",
                  tally.eyes, tally.frames);
        }

        if dwell_armed && dwell_since.is_some_and(|t| now - t >= DWELL_S) {
            return Ok(tally.result(index, PointOutcome::Dwell, verdict.hits, now));
        }

        // A gate that starves and a user who is looking elsewhere are the same picture
        // from outside; the three counts are what tells them apart.
        if now >= next_status {
            next_status = now + STATUS_S;

            info!("hits {}/{}, gaze {}/{} frames, eyes ok {}/{}",
                  verdict.hits, gate.min_hits, tally.gaze, tally.frames, tally.eyes,
                  tally.frames);
        }

        // Never a skip: patience elapsing is a prompt to the operator, not a decision.
        if now >= next_nag {
            next_nag = now + config.point_timeout_s;

            match config.manual {
                true  => warn!("still waiting on {name}: click when you are on it, Enter \
                                forces it, s skips, q aborts"),
                false => warn!("still waiting on {name}: Enter adds it now, s skips, q aborts"),
            }
        }

        // Keep the mark alive with the gate's own progress: a user who cannot tell
        // whether the tracker sees them has no way to fix their posture. Under manual
        // acceptance it is how much of the window names this target; on the dwell
        // fallback, how much of the dwell has run; otherwise the vote against its gate.
        let progress = {
            match (config.manual, dwell_armed) {
                (true, _)      => match samples.len() {
                    0 => 0.0,
                    n => verdict.hits as f64 / n as f64,
                },
                (false, true)  => match dwell_since {
                    Some(since) => (now - since) / DWELL_S,
                    None        => 0.0,
                },
                (false, false) => verdict.hits as f64 / gate.min_hits.max(1) as f64,
            }
        };

        show_target(overlay, px, progress.clamp(0.0, 1.0) as f32)?;

        std::thread::sleep(TICK);
    }
}

/// Gaze frames seen at one target, split by what the device actually reported. The
/// split is the diagnostic the 2026-08-28 01:24 run did not have: `frames` without
/// `gaze` is a device that sees a face and will not say where it is looking.
#[derive(Clone, Copy, Debug, Default)]
struct Tally {
    /// Frames seen at all.
    frames : usize,
    /// Of those, the ones carrying a usable gaze point (`valid_uv`). A clamped or
    /// sentinel reading is not usable and does not count here.
    gaze   : usize,
    /// Of those, the ones with both eyes tracked.
    eyes   : usize,
}

// --- Tally ---

impl Tally {
    /// Closes out a target with this tally behind it.
    fn result(&self, index: usize, outcome: PointOutcome, hits: usize, wait_s: f64)
        -> PointResult
    {
        PointResult {
            index   : index,
            outcome : outcome,
            hits    : hits,
            wait_s  : wait_s,
            frames  : self.frames,
            gaze    : self.gaze,
            eyes    : self.eyes,
        }
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
    keys     : Option<&Receiver<RetrainKey>>,
    config   : &RetrainConfig,
    plan     : &RetrainPlan,
)
    -> Result<Vec<HealthStop>, RetrainError>
{
    let out = geometry.outputs.iter()
        .find(|o| o.name == config.display && o.enabled)
        .ok_or(RetrainError::NoDisplays)?;

    let eye        = geometry.eye();
    let frames_rx  = device.gaze_stream();
    let (u_lo, u_hi, v_lo, v_hi) = plan.train_uv;

    set_overlay_background(Some(config.health_background.color(config.dark)));

    let steps     = config.health_steps.max(2);
    let mut stops = Vec::with_capacity(steps * steps);

    for row in 0..steps {
        for col in 0..steps {
            let fu = col as f64 / (steps - 1) as f64;
            let fv = row as f64 / (steps - 1) as f64;

            // The same 5%..95% inset as the training points: a stop on the very edge
            // of the rectangle measures the overshoot, not the model.
            let u = u_lo + (u_hi - u_lo) * (POINT_FRACTIONS[0]
                + (POINT_FRACTIONS[2] - POINT_FRACTIONS[0]) * fu);
            let v = v_lo + (v_hi - v_lo) * (POINT_FRACTIONS[0]
                + (POINT_FRACTIONS[2] - POINT_FRACTIONS[0]) * fv);

            let px    = out.uv_to_px(u, v);
            let index = row * steps + col + 1;

            info!("health check {index}/{}: eyes on the dot", steps * steps);

            show_target(overlay, px, 0.0)?;

            while next_frame(&frames_rx)?.is_some() {}

            let (mut us, mut vs) = (Vec::new(), Vec::new());
            let start = Instant::now();

            while start.elapsed().as_secs_f64() < HEALTH_DWELL_S {
                if let Some(keys) = keys
                    && matches!(keys.try_recv(), Ok(RetrainKey::Quit))
                {
                    set_overlay_background(None);

                    return Err(RetrainError::Aborted);
                }

                let t = start.elapsed().as_secs_f64();

                show_target(overlay, px, (t / HEALTH_DWELL_S) as f32)?;

                while let Some(frame) = next_frame(&frames_rx)? {
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

// --- Writing the files ---

/// Where a committed ceremony is written.
#[derive(Clone, Copy, Debug)]
pub struct CalibrationFiles<'a> {
    /// The device blob backup the provider re-declares on every connect.
    pub blob        : &'a Path,
    /// The calibration file.
    pub calibration : &'a Path,
}

/// What [`write_calibration`] did.
#[derive(Debug)]
pub struct Written {
    /// The committed blob's identity.
    pub report           : BlobReport,
    /// The correction field fit from the health stops, `None` when there were too
    /// few to fit one (the field is then the identity).
    pub field            : Option<FieldFit>,
    /// Where the previous blob was kept, when there was one.
    pub blob_kept        : Option<PathBuf>,
    /// Where the previous calibration was kept, when there was one.
    pub calibration_kept : Option<PathBuf>,
}

/// Writes a verified ceremony out: the blob first (the one artefact that cannot be
/// recreated without the user sitting down again), then the calibration file
/// describing it, each previous file kept as `*.prev-<unix>`. The calibration
/// carries the display's configured pose (the ceremony declares the measured plane
/// and fits nothing), the health stops, and the correction field fitted from them
/// when it beats leaving the firmware's mapping alone. `lag_s` is inherited from the
/// previous calibration when there is one.
///
/// Only call this after [`verify_persistence`] passed: the files must describe a
/// model the device still holds.
pub fn write_calibration(
    files    : CalibrationFiles<'_>,
    geometry : &DesktopGeometry,
    config   : &RetrainConfig,
    plan     : &RetrainPlan,
    outcome  : &RetrainOutcome,
    health   : Vec<HealthStop>,
)
    -> Result<Written, RetrainError>
{
    let report = BlobReport::of(&outcome.blob);
    let lag_s  = Et5Calibration::load(files.calibration).map(|c| c.lag_s).unwrap_or(DEFAULT_LAG_S);

    let out_geometry = geometry.outputs.iter()
        .find(|o| o.name == config.display)
        .ok_or_else(|| RetrainError::DisplayVanished(config.display.clone()))?;

    let blob_kept = keep_previous(files.blob).map_err(|source| {
        RetrainError::Write { path: files.blob.to_path_buf(), source: source }
    })?;

    std::fs::write(files.blob, &outcome.blob).map_err(|source| {
        RetrainError::Write { path: files.blob.to_path_buf(), source: source }
    })?;

    let mut calibration = Et5Calibration {
        format             : CALIBRATION_FORMAT,
        created_unix_s     : Et5Calibration::now_unix_s(),
        lag_s              : lag_s,
        device_output      : Some(config.display.clone()),
        device_area        : Some(plan.area),
        device_blob_sha256 : Some(report.body_sha256.clone()),
        device_result      : outcome.result.clone(),
        outputs            : vec![OutputCalibration {
            name           : config.display.clone(),
            pose           : OutputPose {
                position_mm : out_geometry.position_mm,
                yaw_deg     : out_geometry.yaw_deg,
                pitch_deg   : out_geometry.pitch_deg,
                roll_deg    : out_geometry.roll_deg,
            },
            field          : FieldMap::identity(),
            pose_rms_deg   : 0.0,
            field_rms_norm : 0.0,
            targets        : outcome.accepted,
            head_gain      : None,
        }],
        health             : health,
    };

    let field = calibration.fit_field_from_health();

    let calibration_kept = keep_previous(files.calibration).map_err(|source| {
        RetrainError::Write { path: files.calibration.to_path_buf(), source: source }
    })?;

    calibration.save(files.calibration).map_err(RetrainError::Calibration)?;

    Ok(Written {
        report           : report,
        field            : field,
        blob_kept        : blob_kept,
        calibration_kept : calibration_kept,
    })
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

// --- Persistence check ---

/// What the post-ceremony reconnect saw.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Persistence {
    /// The tracker's `(bus, address)` when the ceremony opened it.
    pub before        : (u8, u8),
    /// Its `(bus, address)` after the reconnect. A different pair is a
    /// re-enumeration: the firmware rebooted at some point in between.
    pub after         : (u8, u8),
    /// Body SHA-256 of the blob the reopened device handed back. Equal to the
    /// committed one, or this is an error rather than a value.
    pub body_sha256   : String,
    /// Length of the retrieved blob, bytes.
    pub retrieved_len : usize,
}

// --- Persistence ---

impl Persistence {
    /// Whether the tracker came back on a different bus address than it left on.
    pub fn re_enumerated(&self) -> bool {
        self.before != self.after
    }
}

/// Closes nothing and opens the tracker again, then checks it still holds the model
/// the ceremony just committed.
///
/// The caller must have dropped its own [`Device`] first; the interface is claimed
/// exclusively, so a second open fails while the first is alive.
///
/// This exists because of the 2026-08-28 01:24 run: the tracker re-enumerated on the
/// bus at 01:24:55 as the ceremony finished, and an ET5 reboot resets the eye model to
/// the 1478-byte factory blob. The run wrote a calibration file describing a model the
/// device no longer had. A blob that has not survived a close and a reopen is not
/// committed, whatever `cal_retrieve` said while the session was still warm, so
/// nothing is written until this passes.
pub fn verify_persistence(committed: &[u8], before: (u8, u8))
    -> Result<Persistence, RetrainError>
{
    // Long enough that a reboot in progress fails the open rather than racing it.
    std::thread::sleep(RECONNECT_SETTLE);

    let mut device = Device::connect().map_err(device_error)?;
    let after      = device.usb_address();
    let retrieved  = device.cal_retrieve().map_err(device_error);

    device.close();
    drop(device);

    let retrieved = retrieved?;

    // Worth saying out loud even when the model survived: it means the firmware
    // rebooted at some point and the next one may not be so lucky.
    if after != before {
        warn!("the tracker re-enumerated during the ceremony: bus {}.{} -> bus {}.{}. \
               The firmware rebooted, which resets the on-device eye model.",
              before.0, before.1, after.0, after.1);
    }

    let want = body_sha256_hex(committed);
    let got  = body_sha256_hex(&retrieved);

    if want != got {
        return Err(RetrainError::ModelNotKept {
            committed_sha256 : want,
            actual_sha256    : got,
            committed_len    : body(committed).len(),
            actual_len       : body(&retrieved).len(),
        });
    }

    info!("the tracker still holds the committed model after a reconnect \
           ({} bytes, body {}); bus {}.{} -> bus {}.{}",
          retrieved.len(), &got[..16], before.0, before.1, after.0, after.1);

    Ok(Persistence {
        before        : before,
        after         : after,
        body_sha256   : got,
        retrieved_len : retrieved.len(),
    })
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

/// Turns a device error into the ceremony's, separating out the one failure that is
/// not the ceremony's fault.
///
/// A transport error means the tracker left the bus. On this device that is a firmware
/// reboot, and a reboot resets the eye model to the factory blob, so everything the
/// ceremony has done up to that point is gone: it must abort and write nothing rather
/// than retry into a device that is no longer the one it was talking to.
fn device_error(e: DeviceError) -> RetrainError {
    match e {
        DeviceError::Transport(e) => RetrainError::TrackerLost(e.to_string()),
        e                         => RetrainError::Device(e),
    }
}

/// Pulls one gaze frame if there is one.
///
/// `Err` only when the reader thread has dropped its sender, which it does when the
/// USB transport fails under it. That is the same reboot [`device_error`] names, seen
/// from the stream side instead of from a request.
fn next_frame(frames_rx: &Receiver<Et5Frame>) -> Result<Option<Et5Frame>, RetrainError> {
    match frames_rx.try_recv() {
        Ok(frame)                       => Ok(Some(frame)),
        Err(TryRecvError::Empty)        => Ok(None),
        Err(TryRecvError::Disconnected) => Err(RetrainError::TrackerLost(
            "the gaze stream ended: the reader thread lost the USB transport".into())),
    }
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
fn wait(
    overlay    : &OverlayHandle,
    px         : GlobalPx,
    duration_s : f64,
    frames_rx  : &Receiver<Et5Frame>,
    keys       : Option<&Receiver<RetrainKey>>,
)
    -> Result<(), RetrainError>
{
    let start = Instant::now();

    while start.elapsed().as_secs_f64() < duration_s {
        let progress = (start.elapsed().as_secs_f64() / duration_s.max(f64::EPSILON)) as f32;

        show_target(overlay, px, progress)?;

        while next_frame(frames_rx)?.is_some() {}

        if let Some(keys) = keys {
            match keys.try_recv() {
                Ok(RetrainKey::Quit) => return Err(RetrainError::Aborted),
                Ok(RetrainKey::Advance | RetrainKey::Commit | RetrainKey::Skip) => return Ok(()),
                Err(_)             => {}
            }
        }

        std::thread::sleep(TICK);
    }

    Ok(())
}

// --- Keys ---

/// User input during a ceremony.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RetrainKey {
    /// End the current collection early.
    Advance,
    /// The user says they are on the target now: the controller's pad click, or `c`.
    /// The retrain treats it as the acceptance itself, checked against the gate's
    /// vote; the health check treats it as `Advance`.
    Commit,
    /// Skip the current point (behind a bezel, uncomfortable).
    Skip,
    /// Abort the ceremony.
    Quit,
}

/// Spawns a stdin reader translating lines into keys: empty line advances, `c`
/// commits, `s` skips, `q` quits.
pub fn terminal_keys() -> Receiver<RetrainKey> {
    let (tx, rx) = crossbeam_channel::unbounded();

    terminal_keys_into(tx);

    rx
}

/// The stdin reader behind [`terminal_keys`], feeding a channel the caller owns so
/// another source (a controller) can share it.
pub fn terminal_keys_into(tx: Sender<RetrainKey>) {
    std::thread::Builder::new()
        .name("retrain-keys".into())
        .spawn(move || {
            let stdin = std::io::stdin();

            for line in stdin.lock().lines() {
                let Ok(line) = line else {
                    break;
                };

                let key = {
                    match line.trim() {
                        ""  => RetrainKey::Advance,
                        "c" => RetrainKey::Commit,
                        "s" => RetrainKey::Skip,
                        "q" => RetrainKey::Quit,
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

// --- Target drawing ---

/// The overlay background the ceremony currently draws its targets on, packed RGBA
/// with the red byte in the high bits. Zero is the transparent overlay.
///
/// A module static rather than a parameter because [`show_target`] is called from
/// every wait in the ceremony, and threading a colour through all of them for the
/// benefit of the round loop buys nothing: only one target is ever animating at a time.
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

/// The background [`show_target`] is currently painting.
fn overlay_background() -> Option<[u8; 4]> {
    let packed = OVERLAY_BACKGROUND.load(Ordering::Relaxed);

    (packed != 0).then(|| packed.to_be_bytes())
}

/// The dot a round adapts on: the centre point if the plan has one, otherwise
/// wherever the round starts. It only has to give the eye something to hold.
fn anchor_point<'a>(plan: &'a RetrainPlan, round: &Round) -> Result<&'a TrainPoint, RetrainError> {
    plan.points.get(4)
        .or_else(|| round.points.first().and_then(|i| plan.points.get(*i)))
        .ok_or(RetrainError::NoDisplays)
}

/// Draws the target: the pointer look's mark, a ring with the dot at its centre, its
/// interior filling with `progress` (0 to 1) as the hold at it runs. No caption: what
/// the user needs to know goes to the terminal.
pub(crate) fn show_target(overlay: &OverlayHandle, px: GlobalPx, progress: f32)
    -> Result<(), RetrainError>
{
    overlay.set(OverlayState {
        background : overlay_background(),
        mark       : Some(Mark { at: px, progress: progress.clamp(0.0, 1.0) }),
        ..OverlayState::default()
    }).map_err(|e| RetrainError::Overlay(e.to_string()))
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

/// Ceremony failure.
#[derive(Debug, thiserror::Error)]
pub enum RetrainError {
    #[error("no enabled display matched the configured tracker display")]
    NoDisplays,
    #[error("aborted by the user")]
    Aborted,
    #[error("device error during the ceremony: {0}")]
    Device(#[source] DeviceError),
    #[error("overlay error: {0}")]
    Overlay(String),
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
    #[error("writing {path}: {source}")]
    Write {
        path   : PathBuf,
        #[source]
        source : std::io::Error,
    },
    #[error("the retrained display {0} vanished from the desk config")]
    DisplayVanished(String),
    #[error("writing the calibration file: {0}")]
    Calibration(#[source] CalibrationError),
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

    /// The shipped gate: Talon's vote and no ellipse at all.
    fn talon_gate() -> Gate {
        Gate {
            window   : 120,
            min_hits : 60,
            tol_uv   : None,
            aspect   : 1.0,
        }
    }

    /// The opt-in gate with a generous radius, for the tests that are about the
    /// ellipse rather than the vote.
    fn loose_gate() -> Gate {
        Gate { tol_uv: Some((0.05, 0.05)), ..talon_gate() }
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
    fn the_schedule_is_thirteen_points_in_four_rounds() {
        let geometry = desk();
        let plan     = plan(&geometry, &config()).expect("plan");

        assert_eq!(plan.points.len(), 9);
        assert_eq!(plan.rounds.len(), 4);
        assert_eq!(plan.rounds.iter().map(|r| r.points.len()).sum::<usize>(), 13);

        // Centre first, then the mid-edges, then the corners, then the corners again.
        assert_eq!(plan.rounds[0].points, vec![4]);
        assert_eq!(plan.rounds[1].points, vec![7, 3, 5, 1]);
        assert_eq!(plan.rounds[2].points, vec![0, 8, 6, 2]);
        assert_eq!(plan.rounds[3].points, vec![0, 8, 6, 2]);

        for round in &plan.rounds[..3] {
            assert_eq!(round.background, Background::Black);
        }

        assert_eq!(plan.rounds[3].background, Background::White);

        // Every point is used at least once; the corners twice.
        for index in 0..9 {
            let uses = plan.rounds.iter()
                .flat_map(|r| r.points.iter())
                .filter(|i| **i == index)
                .count();
            let want = if [0, 2, 6, 8].contains(&index) { 2 } else { 1 };

            assert_eq!(uses, want, "point {index} appears {uses} times");
        }
    }

    #[test]
    fn the_schedule_stays_under_the_device_point_cap() {
        // The device evicts the oldest points past the cap without a word (2026-08-28
        // 11:56: eighteen fed, fourteen kept), and the cap may be a byte budget the
        // fourteenth point only just fit, so the schedule leaves one point of margin.
        let geometry = desk();
        let plan     = plan(&geometry, &config()).expect("plan");
        let total    = plan.rounds.iter().map(|r| r.points.len()).sum::<usize>();

        assert!(total < DEVICE_POINT_CAP, "{total} points is not under the cap");
        assert!(total >= MIN_POINTS);
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
        let gate    = Gate { tol_uv: Some((0.0, 0.05)), ..loose_gate() };

        assert!(!gate_verdict(&[[0.2, 0.2]; 120], &targets, 0, &gate).accepted);
    }

    #[test]
    fn a_one_target_round_accepts_on_frames_alone() {
        // Round 1 is the centre point on a device that has just been cleared: with one
        // target every frame names it, wherever it landed, so the vote is Talon's
        // "did the user look at the screen for two thirds of a second". This is the
        // case the 2026-08-28 01:24 gate could not pass on sixteen of eighteen points.
        let targets = [[0.5, 0.5]];
        let samples: Vec<[f64; 2]> = (0..60)
            .map(|i| [0.05 + i as f64 * 0.01, 0.9 - i as f64 * 0.01])
            .collect();

        let verdict = gate_verdict(&samples, &targets, 0, &talon_gate());

        assert_eq!(verdict.hits, 60);
        assert!(verdict.accepted, "{verdict:?}");

        // One frame short is still one frame short.
        assert!(!gate_verdict(&samples[..59], &targets, 0, &talon_gate()).accepted);
    }

    #[test]
    fn without_a_tolerance_a_distant_vote_is_accepted() {
        // The same samples the median test rejects under a radius: with no ellipse the
        // nearest-target vote carries them, which is the point of the default gate.
        // A tenth of the panel is far more than ten degrees of visual angle here.
        let targets = [[0.2, 0.2], [0.8, 0.8]];
        let samples = vec![[0.35, 0.35]; 120];

        assert!(!gate_verdict(&samples, &targets, 0, &loose_gate()).accepted);

        let verdict = gate_verdict(&samples, &targets, 0, &talon_gate());

        assert!(verdict.accepted);
        assert_eq!(verdict.median, Some([0.35, 0.35]));
    }

    #[test]
    fn the_gate_has_no_radius_unless_one_is_asked_for() {
        let geometry = desk();

        assert_eq!(plan(&geometry, &config()).expect("plan").tol_uv, None);

        let plan = plan(&geometry, &RetrainConfig {
            accept_deg : Some(3.0),
            ..config()
        }).expect("plan");

        // Three degrees on a 3840 px wide, 880 mm panel at ~690 mm: a few percent of
        // the panel, not a pixel and not half a screen.
        let (tol_u, tol_v) = plan.tol_uv.expect("a radius was asked for");

        assert!(tol_u > 0.01 && tol_u < 0.15, "{tol_u}");
        assert!(tol_v > 0.02 && tol_v < 0.40, "{tol_v}");
    }

    #[test]
    fn the_seed_defaults_to_the_blob_it_is_about_to_overwrite() {
        let dir = std::env::temp_dir().join("gaze-et5-retrain-seed-test");
        std::fs::create_dir_all(&dir).expect("temp dir");

        let blob    = dir.join("calibration-et5.bin");
        let trained = std::fs::read("tests/fixtures/blob-tail-committed.bin")
            .expect("a real blob tail with a result trailer");

        // Nothing to seed from is a reason, not a failure.
        std::fs::remove_file(&blob).ok();
        let choice = resolve_seed(None, &blob, false).expect("resolve");
        assert!(choice.seed.is_none());
        assert!(choice.why.contains("does not exist"), "{}", choice.why);

        // A file with no result trailer is not one of this device's models.
        std::fs::write(&blob, vec![0x5au8; 4096]).expect("write");
        let choice = resolve_seed(None, &blob, false).expect("resolve");
        assert!(choice.seed.is_none());
        assert!(choice.why.contains("no result trailer"), "{}", choice.why);

        // The ordinary case: the backup this run is about to replace.
        std::fs::write(&blob, &trained).expect("write");
        let seed = resolve_seed(None, &blob, false).expect("resolve")
            .seed.expect("the trained blob seeds");
        assert_eq!(seed.blob, trained);
        assert!(seed.points > 0);

        // --no-seed wins over anything on disk.
        let choice = resolve_seed(None, &blob, true).expect("resolve");
        assert!(choice.seed.is_none());
        assert!(choice.why.contains("--no-seed"), "{}", choice.why);

        // An explicit file that cannot be used is an error, never a silent skip.
        let missing = dir.join("nope.bin");
        assert!(resolve_seed(Some(&missing), &blob, false).is_err());

        let untrained = dir.join("untrained.bin");
        std::fs::write(&untrained, vec![0x5au8; 4096]).expect("write");
        assert!(resolve_seed(Some(&untrained), &blob, false).is_err());

        std::fs::remove_dir_all(&dir).ok();
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
