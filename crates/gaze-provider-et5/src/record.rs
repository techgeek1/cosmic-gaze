//! Recording sessions: a fixed five-minute script that banks raw frames against known
//! targets and fits nothing.
//!
//! The model this feeds (PLAN-ET5 phase C) is one model across sessions conditioned on
//! head state, so what a session has to supply is *state diversity*, not density. Three
//! things vary inside one session and nothing else does:
//!
//! 1. **Pupil diameter**, through the overlay background. The stop grid runs twice, once
//!    on black and once on white, which walks the pupil across most of its range at a
//!    fixed head position and a fixed set of targets. A pupil-radius offset term is only
//!    identifiable if the training set contains both extremes (Tobii [patent reference removed]).
//! 2. **Head position and rotation**, through the prompted wander in the middle: the same
//!    low-discrepancy walk and posture prompts `sweep::run_collect` uses.
//! 3. **Angle from the tracker axis**, through the grid itself, which is laid inside the
//!    device's gaze cone rather than over the whole panel. Outside the cone the glints
//!    leave the cornea and the samples are not error, they are absence of signal.
//!
//! Between-session variation (the thing no per-session calibration can remove) is what
//! the *collection* varies: days, times, lighting, glasses. That is [`RecordConfig`]'s
//! `note` and `glasses` and the operator's diary, not this module's business.
//!
//! # The file
//!
//! One session is one JSONL file under `config/sessions/<unix>-<blobhash8>.jsonl`:
//!
//! - line 1, [`SessionMeta`]: what the device and the desk were at the start.
//! - then `stop`, `traj` and `frame` records in the readings format `sweep::save_pass`
//!   writes, with `phase` and `background` added to the `stop` and `traj` records. Frames
//!   stay byte-identical to the readings format and are attributed to a phase through the
//!   trajectory, which is dense and continuous over the whole session.
//! - last line, [`SessionEnd`]: the blob hash again, so a session that ran while the
//!   firmware mutated its own model can be spotted and thrown away.
//!
//! Client data is keyed to the blob: a retrain orphans every session recorded before it.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use crossbeam_channel::Receiver;
use serde::{Deserialize, Serialize};
use tracing::info;
use gaze_core::{DesktopGeometry, GlobalPx, OutputGeometry, Ray, Rect};
use gaze_overlay::{OverlayHandle, OverlayState};

use crate::blob::{BlobReport, sha256_hex};
use crate::device::Device;
use crate::gaze::Et5Frame;
use crate::sweep::{
    COLLECT_HOLD_S, COLLECT_PROMPTS, COLLECT_U_MAX, COLLECT_U_MIN, COLLECT_V_MAX,
    COLLECT_V_MIN, PassData, StopWindow, SweepError, SweepKey, TimedFrame, glide,
    set_overlay_background, show_target, wait_draining,
};
use crate::ttp::DisplayArea;

/// Format version of the session file; bump on any breaking change to the records.
pub const SESSION_FORMAT: u32 = 1;

/// Fully opaque black, the low-illumination half of the pupil sweep.
pub const BLACK: [u8; 4] = [0, 0, 0, 255];

/// Fully opaque white, the high-illumination half of the pupil sweep.
pub const WHITE: [u8; 4] = [255, 255, 255, 255];

/// How long the eye is given to adapt after a background change before any samples
/// count, seconds. The pupil light reflex constricts in about a second and dilates
/// considerably slower, so this is sized for the slow direction; without it the first
/// stops of each grid carry a pupil that is still moving.
const ADAPT_S: f64 = 4.0;

/// Lattice resolution per axis for the cone search. 41 puts the lattice pitch at about
/// 2% of the panel, which is finer than the grid spacing it feeds.
const CONE_LATTICE: usize = 41;

/// Inset of the cone lattice from the panel edges, uv. Keeps the grid off the bezel.
const CONE_INSET: f64 = 0.04;

/// Margin between the cone rectangle and the outermost grid stops, as a fraction of the
/// rectangle. A stop exactly on the cone boundary is half outside it once the eye
/// overshoots.
const CONE_GRID_MARGIN: f64 = 0.06;

/// Default half-angle of the tracker's usable gaze cone, degrees, measured the way
/// `DesktopGeometry::off_axis_deg` measures it: between the gaze direction and the
/// direction from the eye to the tracker.
///
/// DESIGN §11 puts the envelope at roughly ±25 H / ±15 V about the tracker axis. On this
/// desk the tracker sits under DP-1's bottom bezel and looks up at the face, so the panel
/// is not centred on that axis at all: its bottom centre reads about 4 degrees off and
/// its top corners about 40. A 25 degree limit would keep only the lower band of the
/// panel, where the tracker is actually good; the default is wider than that on purpose.
/// The model has to be trained where the user actually looks, including where the tracker
/// is poor, and every row carries its own angle from the axis, so the cone is something
/// the model and the sigma profile learn rather than something the grid enforces.
/// `--cone-deg 25` narrows the grid to the good band when that is what a session is for.
pub const CONE_MAX_DEG: f64 = 45.0;

// --- Configuration ---

/// What one recording session does. `Default` is the intended five-minute script.
#[derive(Clone, Debug)]
pub struct RecordConfig {
    /// Connector name of the display the tracker's plane is declared on.
    pub display           : String,
    /// Total session length, minutes. The two grids take what they take; the wander
    /// gets the rest.
    pub minutes           : f64,
    /// Half-angle of the gaze cone the stop grid is laid inside, degrees.
    pub cone_max_deg      : f64,
    /// Stop grid width.
    pub grid_cols         : usize,
    /// Stop grid height.
    pub grid_rows         : usize,
    /// Settling time at each stop before collection, seconds.
    pub settle_s          : f64,
    /// Collection window at each stop, seconds.
    pub collect_s         : f64,
    /// Sensor-frame pitch from `desk.toml`, degrees. Recorded in the meta line so the
    /// dataset can put desk-frame targets into tracker space without the desk config.
    pub tracker_pitch_deg : f64,
    /// Whether the user was wearing glasses. Prompted on the terminal, not guessed:
    /// Tobii's own advice is a separate profile for glasses, so it is a session label
    /// the model has to see.
    pub glasses           : bool,
    /// Free-text note: lighting, time of day, anything unusual.
    pub note              : String,
    /// Directory sessions are written into.
    pub out_dir           : PathBuf,
}

impl Default for RecordConfig {
    fn default() -> Self {
        Self {
            display           : "DP-1".into(),
            minutes           : 5.0,
            cone_max_deg      : CONE_MAX_DEG,
            grid_cols         : 4,
            grid_rows         : 3,
            settle_s          : 0.5,
            collect_s         : 1.2,
            tracker_pitch_deg : 0.0,
            glasses           : false,
            note              : String::new(),
            out_dir           : PathBuf::from("config/sessions"),
        }
    }
}

/// Which part of the session a record belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Phase {
    /// The stop grid on a black background.
    GridBlack,
    /// The prompted low-discrepancy wander on a white background.
    Wander,
    /// The stop grid again, on a white background.
    GridWhite,
}

// --- Phase ---

impl Phase {
    /// The `phase` value written into the file.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::GridBlack => "grid_black",
            Self::Wander    => "wander",
            Self::GridWhite => "grid_white",
        }
    }

    /// The `background` value written into the file.
    pub fn background_name(&self) -> &'static str {
        match self {
            Self::GridBlack => "black",
            Self::Wander | Self::GridWhite => "white",
        }
    }

    /// The colour the overlay paints for this phase.
    pub fn background(&self) -> [u8; 4] {
        match self {
            Self::GridBlack => BLACK,
            Self::Wander | Self::GridWhite => WHITE,
        }
    }
}

// --- File records ---

/// First line of a session file: everything about the device and the desk that the rows
/// are only meaningful against.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionMeta {
    /// Always `"meta"`.
    pub kind              : String,
    /// [`SESSION_FORMAT`] at write time.
    pub format            : u32,
    /// `<unix secs>-<first 8 hex of the blob hash>`, and the file's stem.
    pub session_id        : String,
    /// Unix time the session started.
    pub created_unix_s    : f64,
    /// SHA-256 of the *body* of the on-device blob retrieved before the first target
    /// was shown. All client-side data is keyed to this: a retrain orphans the
    /// session. The body rather than the whole blob because the blob's result trailer
    /// is re-normalised against the declared plane on every retrieve, so the
    /// whole-blob hash would differ between two sessions on one unchanged model.
    pub blob_sha256       : String,
    /// Size of that whole blob, trailer included, bytes.
    pub blob_bytes        : usize,
    /// Connector name the session ran on.
    pub display           : String,
    /// The display area declared on the device for the whole session. The firmware's 2D
    /// output is only panel uv under this exact plane.
    pub display_area      : DisplayArea,
    /// SHA-256 of the `desk.toml` text in force. A desk change invalidates every target
    /// position in the file, so it has to be detectable.
    pub desk_sha256       : String,
    /// `tracker_pitch_deg` from that desk config.
    pub tracker_pitch_deg : f64,
    /// Whether the user wore glasses.
    pub glasses           : bool,
    /// Free-text note from the operator.
    pub note              : String,
}

/// Last line of a session file.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct SessionEnd {
    /// Always `"meta_end"`.
    pub kind         : String,
    /// Body hash of the blob retrieved again after the last target. Differing from
    /// the meta line's means the firmware mutated its own model mid-session and the
    /// rows are not all describing the same feature extractor.
    pub blob_sha256  : String,
    /// Total frames recorded.
    pub frames       : usize,
    /// How many of them had at least one tracked eye.
    pub valid_frames : usize,
}

/// `phase` value on the records `gaze-clicks` writes. Not a [`Phase`] variant: the
/// three variants there each paint an overlay background, and a passive click session
/// paints nothing at all.
pub const CLICK_PHASE: &str = "click";

/// `background` value on those records. The screen is whatever the user was working on,
/// which is neither of the two controlled pupil extremes a recorded session drives.
pub const CLICK_BACKGROUND: &str = "screen";

/// The element a click landed on, as the recogniser saw it just before the press.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClickElement {
    /// `gaze_core::ElementKind` in lowercase: `button`, `text`, `icon` and so on.
    pub kind  : String,
    /// The element's box in global logical pixels.
    pub bbox  : Rect,
    /// Recognised text, when the box came from OCR or carried a label.
    pub text  : Option<String>,
    /// The recogniser's confidence in [0, 1].
    pub score : f32,
}

/// One accepted click: everything about the press that is not a gaze frame.
///
/// Written as a `"click"` record next to the `"stop"` record that carries the same
/// press's collection window. Readers that do not know the kind skip it, so a session
/// file with clicks in it still loads in every older tool.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ClickRecord {
    /// Index of this click within the session, from zero. The `"stop"` record repeats
    /// it as its own `n`, which is the only key tying the two together.
    pub n           : u64,
    /// `left` or `right`.
    pub button      : String,
    /// Connector the pointer was on.
    pub output      : String,
    /// Where the pointer was at the press, global logical pixels.
    pub px          : GlobalPx,
    /// Host time of the press, seconds since the collector started.
    pub t_press     : f64,
    /// Host time of the release.
    pub t_release   : f64,
    /// How far the pointer moved between press and release, logical pixels.
    pub moved_px    : f64,
    /// 1 for a single click, 2 for the second of a double, and so on.
    pub multi       : u32,
    /// The element under the pointer.
    pub element     : ClickElement,
    /// Where `element` came from: `tree` (the application's accessibility tree, via
    /// `gaze-a11y`), `vision` (the screen recogniser), or `caret` (the I-beam's word
    /// alone). Absent in files written before 2026-08-28, which are all `vision`.
    #[serde(default)]
    pub source      : Option<String>,
    /// Mean luminance of the crop the element was found in, [0, 1]. The pupil
    /// covariate a passive session gets instead of a driven background.
    pub crop_luma   : f64,
    /// Capture-completion time of the frame the element came from, relative to the
    /// press. Positive for the capture fired by the press itself, negative for a
    /// fallback frame taken from the rolling cache.
    pub frame_age_s : f64,
    /// The pointer's shape at the press, as `gaze_clicks::cursor::CursorShape` names it:
    /// `arrow`, `hand`, `text`, `centred` or `other`. Absent when the compositor had not
    /// reported the cursor image, and in files written before it was recorded.
    #[serde(default)]
    pub cursor      : Option<String>,
}

/// What a finished session produced.
#[derive(Clone, Debug)]
pub struct RecordOutcome {
    /// Where the session was written.
    pub path         : PathBuf,
    /// Session id, which is also the file stem.
    pub session_id   : String,
    /// Blob report from the retrieve at the start.
    pub blob_start   : BlobReport,
    /// Blob report from the retrieve at the end.
    pub blob_end     : BlobReport,
    /// Total frames recorded.
    pub frames       : usize,
    /// Frames with at least one tracked eye.
    pub valid_frames : usize,
    /// Stop windows banked across both grids plus the wander holds.
    pub stops        : usize,
}

/// The schedule a session is about to run, without running it. `record --dry-run` prints
/// this; it needs the desk config and nothing else.
#[derive(Clone, Debug)]
pub struct RecordPlan {
    /// Stop positions, in order, shared by both grids.
    pub stops        : Vec<(f64, f64, GlobalPx)>,
    /// The cone rectangle the grid was laid in, as `(u_lo, u_hi, v_lo, v_hi)`.
    pub cone_uv      : (f64, f64, f64, f64),
    /// Largest off-axis angle over the grid, degrees.
    pub cone_max_deg : f64,
    /// Estimated wall time of one grid, seconds.
    pub grid_s       : f64,
    /// Wall time left for the wander, seconds.
    pub wander_s     : f64,
}

// --- Recording ---

/// Plans the session for a display: where the stops go and how the time splits.
///
/// Fails only when the display is not in the desk config, or when no part of it is
/// inside the cone.
pub fn plan(geometry: &DesktopGeometry, config: &RecordConfig)
    -> Result<RecordPlan, SweepError>
{
    let out = geometry.outputs.iter()
        .find(|o| o.name == config.display && o.enabled)
        .ok_or(SweepError::NoDisplays)?;

    let cone_uv = cone_rect(geometry, out, config.cone_max_deg)
        .ok_or(SweepError::NoDisplays)?;
    let stops   = grid_in(out, cone_uv, config.grid_cols, config.grid_rows);

    if stops.is_empty() {
        return Err(SweepError::NoDisplays);
    }


    let worst = stops.iter()
        .map(|(u, v, _)| off_axis_deg(geometry, out, *u, *v))
        .fold(0.0_f64, f64::max);

    // A stop costs its settle plus its collect plus the glide onto it; the glides average
    // about a second at 14 deg/s across a cone this size, and each grid pays one
    // adaptation wait up front.
    let grid_s   = ADAPT_S + stops.len() as f64 * (config.settle_s + config.collect_s + 1.0);
    let wander_s = (config.minutes * 60.0 - 2.0 * grid_s).max(30.0);

    Ok(RecordPlan {
        stops        : stops,
        cone_uv      : cone_uv,
        cone_max_deg : worst,
        grid_s       : grid_s,
        wander_s     : wander_s,
    })
}

/// Runs a whole recording session and writes it.
///
/// Declares `area` on the device, retrieves the blob, runs the three phases, retrieves
/// the blob again, and writes the file. Nothing is fitted and nothing is uploaded: the
/// only device state this touches is the declared display area, which every session and
/// the provider itself set anyway.
///
/// `q` ends the session early and still writes everything collected up to that point,
/// which is what makes a session interrupted by a phone call worth having. A short
/// session is a real session; the phases it did not reach simply contribute no rows.
pub fn run_record(
    device   : &mut Device,
    geometry : &DesktopGeometry,
    area     : DisplayArea,
    overlay  : &OverlayHandle,
    keys     : Option<&Receiver<SweepKey>>,
    config   : &RecordConfig,
    desk     : &str,
)
    -> Result<RecordOutcome, SweepError>
{
    let out = geometry.outputs.iter()
        .find(|o| o.name == config.display && o.enabled)
        .ok_or(SweepError::NoDisplays)?
        .clone();

    let plan = plan(geometry, config)?;

    // The blob is the identity of the feature extractor these rows describe, so it is
    // read before anything else happens.
    let blob_start = BlobReport::of(&device.cal_retrieve().map_err(SweepError::Device)?);
    let created    = now_unix_s();
    let session_id = format!("{}-{}", created as u64, blob_start.short());

    info!("recording session {session_id} on {} ({} stops in a {:.0} deg cone, \
           wander {:.0}s)", out.name, plan.stops.len(), plan.cone_max_deg, plan.wander_s);

    // The trained 2D output is panel uv only under the plane it was trained on.
    device.set_display_area_corners(area).map_err(SweepError::Device)?;
    std::thread::sleep(Duration::from_millis(200));

    let frames_rx = device.gaze_stream();
    let t0        = Instant::now();

    let mut phases = [
        (Phase::GridBlack, PassData::default()),
        (Phase::Wander   , PassData::default()),
        (Phase::GridWhite, PassData::default()),
    ];

    // A quit ends the session rather than just the phase, but what has been banked is
    // still written: an interrupted session is short, not invalid.
    for (phase, pass) in &mut phases {
        let result = {
            match phase {
                Phase::Wander => run_wander(geometry, &out, overlay, keys, &frames_rx,
                                            t0, plan.wander_s, pass),
                _             => run_grid(geometry, &out, overlay, keys, &frames_rx, t0,
                                          config, &plan, *phase, pass),
            }
        };

        match result {
            Ok(())                    => {}
            Err(SweepError::Aborted)  => {
                info!("{}: ended early by the user", phase.as_str());
                break;
            }
            Err(e)                    => return Err(e),
        }
    }

    set_overlay_background(None);
    let _ = overlay.set(OverlayState::default());

    let blob_end = BlobReport::of(&device.cal_retrieve().map_err(SweepError::Device)?);

    let frames = phases.iter().map(|(_, p)| p.frames.len()).sum();
    let valid  = phases.iter()
        .flat_map(|(_, p)| p.frames.iter())
        .filter(|f| f.frame.any_valid())
        .count();
    let stops  = phases.iter().map(|(_, p)| p.stops.len()).sum();

    let meta = SessionMeta {
        kind              : "meta".into(),
        format            : SESSION_FORMAT,
        session_id        : session_id.clone(),
        created_unix_s    : created,
        blob_sha256       : blob_start.body_sha256.clone(),
        blob_bytes        : blob_start.len,
        display           : out.name.clone(),
        display_area      : area,
        desk_sha256       : sha256_hex(desk.as_bytes()),
        tracker_pitch_deg : config.tracker_pitch_deg,
        glasses           : config.glasses,
        note              : config.note.clone(),
    };

    let end = SessionEnd {
        kind         : "meta_end".into(),
        blob_sha256  : blob_end.body_sha256.clone(),
        frames       : frames,
        valid_frames : valid,
    };

    std::fs::create_dir_all(&config.out_dir)
        .map_err(|e| SweepError::Readings(e.to_string()))?;

    let path = config.out_dir.join(format!("{session_id}.jsonl"));

    write_session(&path, &meta, &phases, &end)
        .map_err(|e| SweepError::Readings(e.to_string()))?;

    Ok(RecordOutcome {
        path         : path,
        session_id   : session_id,
        blob_start   : blob_start,
        blob_end     : blob_end,
        frames       : frames,
        valid_frames : valid,
        stops        : stops,
    })
}

/// Converts an old `calibrate`/`collect` readings file into a session file, so the data
/// banked before `record` existed can serve as session zero.
///
/// Everything the readings format does not carry is filled in from outside: the blob hash
/// from its backup file, the plane from the calibration the readings were taken under,
/// and `background: "unknown"` / `phase: "legacy"` on every record, which is honest —
/// that session ran on a transparent overlay over whatever was on screen.
///
/// Returns the path written and how many records it carried over.
pub fn import_readings(
    readings : &Path,
    out_dir  : &Path,
    blob     : &[u8],
    area     : DisplayArea,
    desk     : &str,
    pitch_deg: f64,
    note     : &str,
)
    -> Result<(PathBuf, usize), SweepError>
{
    let text = std::fs::read_to_string(readings)
        .map_err(|e| SweepError::Readings(e.to_string()))?;

    let report  = BlobReport::of(blob);
    let created = std::fs::metadata(readings)
        .and_then(|m| m.modified())
        .ok()
        .and_then(|t| t.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|d| d.as_secs_f64())
        .unwrap_or_else(now_unix_s);

    let session_id = format!("{}-{}", created as u64, report.short());

    // Records name their own display; the plane-pass ones are the sweep's own scratch
    // data under a synthetic name and are not observations of anything.
    let mut records = Vec::new();
    let mut display = String::new();
    let mut frames  = 0usize;
    let mut valid   = 0usize;

    for line in text.lines() {
        let Ok(mut value) = serde_json::from_str::<serde_json::Value>(line) else {
            continue;
        };

        let kind = value.get("kind").and_then(|k| k.as_str()).unwrap_or("").to_string();
        let name = value.get("display").and_then(|d| d.as_str()).unwrap_or("").to_string();

        if !matches!(kind.as_str(), "stop" | "traj" | "frame") || name.ends_with("#tri") {
            continue;
        }

        if display.is_empty() {
            display = name.clone();
        }

        if name != display {
            continue;
        }

        if kind == "frame" {
            frames += 1;

            let any = value.pointer("/frame/frame/validity_l").and_then(|v| v.as_u64()) == Some(0)
                || value.pointer("/frame/frame/validity_r").and_then(|v| v.as_u64()) == Some(0);

            if any {
                valid += 1;
            }
        }
        else if let Some(object) = value.as_object_mut() {
            object.insert("phase".into()     , "legacy".into());
            object.insert("background".into(), "unknown".into());
        }

        records.push(value);
    }

    if display.is_empty() {
        return Err(SweepError::Readings("no usable records in the readings file".into()));
    }

    let meta = SessionMeta {
        kind              : "meta".into(),
        format            : SESSION_FORMAT,
        session_id        : session_id.clone(),
        created_unix_s    : created,
        blob_sha256       : report.body_sha256.clone(),
        blob_bytes        : report.len,
        display           : display,
        display_area      : area,
        desk_sha256       : sha256_hex(desk.as_bytes()),
        tracker_pitch_deg : pitch_deg,
        glasses           : false,
        note              : note.to_string(),
    };

    let end = SessionEnd {
        kind         : "meta_end".into(),
        blob_sha256  : report.body_sha256.clone(),
        frames       : frames,
        valid_frames : valid,
    };

    std::fs::create_dir_all(out_dir).map_err(|e| SweepError::Readings(e.to_string()))?;

    let path  = out_dir.join(format!("{session_id}.jsonl"));
    let count = records.len();

    let file  = std::fs::File::create(&path)
        .map_err(|e| SweepError::Readings(e.to_string()))?;
    let mut w = std::io::BufWriter::new(file);

    let mut write = || -> std::io::Result<()> {
        writeln!(w, "{}", serde_json::json!(meta))?;

        for record in &records {
            writeln!(w, "{record}")?;
        }

        writeln!(w, "{}", serde_json::json!(end))?;

        w.flush()
    };

    write().map_err(|e| SweepError::Readings(e.to_string()))?;

    Ok((path, count))
}

// --- The cone ---

/// Angle between the gaze ray onto `(u, v)` and the tracker axis, degrees.
///
/// The nominal eye from the desk config stands in for the real one: the cone is a
/// property of where the panel is relative to the tracker, and the seated head moves by
/// centimetres inside a cone that is tens of degrees wide.
pub fn off_axis_deg(geometry: &DesktopGeometry, out: &OutputGeometry, u: f64, v: f64) -> f64 {
    let eye    = geometry.eye();
    let target = out.uv_to_world(u, v);
    let delta  = target - eye;

    if delta.length_squared() < 1e-9 {
        return 0.0;
    }

    geometry.off_axis_deg(&Ray { origin: eye, dir: delta.normalize() })
}

/// The largest axis-aligned uv rectangle on `out` every point of which is inside the
/// cone, as `(u_lo, u_hi, v_lo, v_hi)`. `None` when no lattice point qualifies.
///
/// The cone intersected with a panel is not a rectangle and is not even centred on the
/// panel (the tracker looks up at the face from below the bezel here), so this searches
/// rather than assuming: mark a lattice with the test, then take the maximum-area
/// all-true rectangle by the usual largest-rectangle-in-a-histogram sweep.
pub fn cone_rect(geometry: &DesktopGeometry, out: &OutputGeometry, max_deg: f64)
    -> Option<(f64, f64, f64, f64)>
{
    let axis = |i: usize| {
        CONE_INSET + (1.0 - 2.0 * CONE_INSET) * i as f64 / (CONE_LATTICE - 1) as f64
    };

    let ok: Vec<Vec<bool>> = (0..CONE_LATTICE)
        .map(|r| {
            (0..CONE_LATTICE)
                .map(|c| off_axis_deg(geometry, out, axis(c), axis(r)) <= max_deg)
                .collect()
        })
        .collect();

    let mut best    : Option<(usize, usize, usize, usize)> = None;
    let mut best_area = 0usize;
    let mut heights = vec![0usize; CONE_LATTICE];

    for (r, row) in ok.iter().enumerate() {
        for (c, cell) in row.iter().enumerate() {
            heights[c] = if *cell { heights[c] + 1 } else { 0 };
        }

        // Monotonic stack over the histogram: each bar is popped by the first shorter one
        // to its right, and its widest rectangle is known at that moment.
        let mut stack: Vec<(usize, usize)> = Vec::new();

        // The sentinel zero past the end pops whatever is still on the stack.
        for (c, h) in heights.iter().copied().chain(Some(0)).enumerate() {
            let mut left = c;

            while stack.last().is_some_and(|(_, sh)| *sh >= h) {
                let (start, sh) = stack.pop().expect("checked by the loop condition");
                let area        = sh * (c - start);

                if area > best_area {
                    best_area = area;
                    best      = Some((start, c - 1, r + 1 - sh, r));
                }

                left = start;
            }

            stack.push((left, h));
        }
    }

    let (c0, c1, r0, r1) = best?;

    Some((axis(c0), axis(c1), axis(r0), axis(r1)))
}

/// A serpentine stop grid inside a uv rectangle, inset by [`CONE_GRID_MARGIN`] so no stop
/// sits on the cone boundary. Row-major with alternating direction, so every glide is
/// between neighbours.
fn grid_in(out: &OutputGeometry, uv: (f64, f64, f64, f64), cols: usize, rows: usize)
    -> Vec<(f64, f64, GlobalPx)>
{
    let (u_lo, u_hi, v_lo, v_hi) = uv;

    let mu = (u_hi - u_lo) * CONE_GRID_MARGIN;
    let mv = (v_hi - v_lo) * CONE_GRID_MARGIN;

    let at = |lo: f64, hi: f64, m: f64, i: usize, n: usize| {
        if n <= 1 {
            return (lo + hi) * 0.5;
        }

        lo + m + (hi - lo - 2.0 * m) * i as f64 / (n - 1) as f64
    };

    let mut stops = Vec::with_capacity(cols * rows);

    for row in 0..rows {
        let mut order: Vec<usize> = (0..cols).collect();

        if row % 2 == 1 {
            order.reverse();
        }

        for col in order {
            let u = at(u_lo, u_hi, mu, col, cols);
            let v = at(v_lo, v_hi, mv, row, rows);

            stops.push((u, v, out.uv_to_px(u, v)));
        }
    }

    stops
}

// --- Phases ---

/// Runs one stop grid on its phase's background.
#[allow(clippy::too_many_arguments)]
fn run_grid(
    geometry  : &DesktopGeometry,
    out       : &OutputGeometry,
    overlay   : &OverlayHandle,
    keys      : Option<&Receiver<SweepKey>>,
    frames_rx : &Receiver<Et5Frame>,
    t0        : Instant,
    config    : &RecordConfig,
    plan      : &RecordPlan,
    phase     : Phase,
    pass      : &mut PassData,
)
    -> Result<(), SweepError>
{
    set_overlay_background(Some(phase.background()));

    // Flush whatever queued while the previous phase was being written up, so window
    // timestamps stay honest.
    while frames_rx.try_recv().is_ok() {}

    let total = plan.stops.len();

    let Some(&(_, _, first)) = plan.stops.first() else {
        return Err(SweepError::NoDisplays);
    };

    let adapt = format!("{} — adapting to {}, eyes on the dot",
                        phase.as_str(), phase.background_name());

    // The pupil is still moving for seconds after the background flips; nothing collected
    // during that would describe the illumination it is labelled with.
    show_target(overlay, first, &adapt)?;

    let mut skip = false;
    wait_draining(ADAPT_S, t0, frames_rx, pass, first, false, keys, &mut skip)?;

    let mut current = first;

    for (index, (u, v, px)) in plan.stops.iter().cloned().enumerate() {
        let label = format!("{} {}/{} ({} background)",
                            out.name, index + 1, total, phase.background_name());

        glide(geometry, overlay, &label, current, px, t0, frames_rx, pass)?;
        current = px;

        show_target(overlay, px, &label)?;

        let mut skip = false;
        wait_draining(config.settle_s, t0, frames_rx, pass, px, false, keys, &mut skip)?;

        if skip {
            continue;
        }

        let t_start = t0.elapsed().as_secs_f64();

        wait_draining(config.collect_s, t0, frames_rx, pass, px, false, keys, &mut skip)?;

        if skip {
            continue;
        }

        pass.stops.push(StopWindow {
            u        : u,
            v        : v,
            px       : px,
            t_start  : t_start,
            t_end    : t0.elapsed().as_secs_f64(),
            parallax : false,
        });
    }

    Ok(())
}

/// Runs the prompted wander: the same golden-ratio walk and posture prompts `collect`
/// uses, on white, for `duration_s`.
#[allow(clippy::too_many_arguments)]
fn run_wander(
    geometry  : &DesktopGeometry,
    out       : &OutputGeometry,
    overlay   : &OverlayHandle,
    keys      : Option<&Receiver<SweepKey>>,
    frames_rx : &Receiver<Et5Frame>,
    t0        : Instant,
    duration_s: f64,
    pass      : &mut PassData,
)
    -> Result<(), SweepError>
{
    set_overlay_background(Some(Phase::Wander.background()));

    while frames_rx.try_recv().is_ok() {}

    let start   = Instant::now();
    let mut cur = out.uv_to_px(0.5, 0.5);

    show_target(overlay, cur, "wander — adapting to white, eyes on the dot")?;

    let mut skip = false;
    wait_draining(ADAPT_S, t0, frames_rx, pass, cur, false, keys, &mut skip)?;

    let mut k = 0usize;

    while start.elapsed().as_secs_f64() < duration_s {
        // Golden-ratio low-discrepancy walk: well spread over the panel with no RNG and
        // no repeating raster the eye could learn.
        let u = COLLECT_U_MIN
            + (0.5 + k as f64 * 0.618_033_988_749_895).fract() * (COLLECT_U_MAX - COLLECT_U_MIN);
        let v = COLLECT_V_MIN
            + (0.5 + k as f64 * 0.381_966_011_250_105).fract() * (COLLECT_V_MAX - COLLECT_V_MIN);

        let target = out.uv_to_px(u, v);
        let left_s = duration_s - start.elapsed().as_secs_f64();
        let label  = format!("{} wander ({:.0}s left) — eyes on the dot: {}",
                             out.name, left_s.max(0.0),
                             COLLECT_PROMPTS[k % COLLECT_PROMPTS.len()]);

        glide(geometry, overlay, &label, cur, target, t0, frames_rx, pass)?;
        cur = target;
        show_target(overlay, target, &label)?;

        let mut skip = false;
        let t_start  = t0.elapsed().as_secs_f64();

        wait_draining(COLLECT_HOLD_S, t0, frames_rx, pass, target, false, keys,
                      &mut skip)?;

        if !skip {
            pass.stops.push(StopWindow {
                u        : u,
                v        : v,
                px       : target,
                t_start  : t_start,
                t_end    : t0.elapsed().as_secs_f64(),
                // The head is deliberately moving through a prompted posture, so this is
                // a parallax hold in the sweep's sense, not a clean fixation anchor.
                parallax : true,
            });
        }

        k += 1;
    }

    Ok(())
}

// --- Lines ---

/// One `"stop"` line. `n` is the click index for a passive click session and `None`
/// for a recorded one, where a stop is identified by its position in the file.
///
/// Every writer of the session format goes through these three builders, so the shape
/// of a line is decided in one place rather than in each producer.
pub fn stop_line(
    display    : &str,
    phase      : &str,
    background : &str,
    stop       : &StopWindow,
    n          : Option<u64>,
)
    -> serde_json::Value
{
    let mut value = serde_json::json!({
        "kind"       : "stop",
        "display"    : display,
        "phase"      : phase,
        "background" : background,
        "stop"       : stop,
    });

    if let (Some(n), Some(object)) = (n, value.as_object_mut()) {
        object.insert("n".into(), serde_json::json!(n));
    }

    value
}

/// One `"frame"` line. Frames stay exactly as the readings format has them, so the
/// archive readers in `sweep` can still parse a session file.
pub fn frame_line(display: &str, frame: &TimedFrame) -> serde_json::Value {
    serde_json::json!({
        "kind"    : "frame",
        "display" : display,
        "frame"   : frame,
    })
}

/// One `"click"` line.
pub fn click_line(display: &str, click: &ClickRecord) -> serde_json::Value {
    serde_json::json!({
        "kind"    : "click",
        "display" : display,
        "phase"   : CLICK_PHASE,
        "n"       : click.n,
        "click"   : click,
    })
}

// --- Writing ---

/// Writes a session: meta line, every phase's records, end line.
fn write_session(
    path   : &Path,
    meta   : &SessionMeta,
    phases : &[(Phase, PassData)],
    end    : &SessionEnd,
)
    -> std::io::Result<()>
{
    let file  = std::fs::File::create(path)?;
    let mut w = std::io::BufWriter::new(file);

    writeln!(w, "{}", serde_json::json!(meta))?;

    for (phase, pass) in phases {
        for stop in &pass.stops {
            let line = stop_line(&meta.display, phase.as_str(), phase.background_name(),
                                 stop, None);

            writeln!(w, "{line}")?;
        }

        for point in &pass.traj {
            writeln!(w, "{}", serde_json::json!({
                "kind"       : "traj",
                "display"    : meta.display,
                "phase"      : phase.as_str(),
                "background" : phase.background_name(),
                "point"      : point,
            }))?;
        }

        for frame in &pass.frames {
            writeln!(w, "{}", frame_line(&meta.display, frame))?;
        }
    }

    writeln!(w, "{}", serde_json::json!(end))?;

    w.flush()
}

/// Current unix time, seconds.
fn now_unix_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The real desk, so the cone search runs against the geometry it was sized for.
    fn desk() -> DesktopGeometry {
        DesktopGeometry::from_toml(
            &std::fs::read_to_string("../../config/desk.toml").expect("desk config"),
        ).expect("desk config parses")
    }

    /// The narrow cone the search was designed around. The shipped default is wider (it
    /// covers the panel), so the search's behaviour is tested at the limit that exercises it.
    const NARROW_CONE_DEG: f64 = 25.0;

    #[test]
    fn the_cone_rectangle_is_inside_the_cone() {
        let geometry = desk();
        let out      = geometry.outputs.iter().find(|o| o.name == "DP-1").expect("DP-1");

        let uv = cone_rect(&geometry, out, NARROW_CONE_DEG).expect("a cone rectangle exists");
        let (u_lo, u_hi, v_lo, v_hi) = uv;

        assert!(u_lo < u_hi && v_lo < v_hi, "the rectangle is non-degenerate: {uv:?}");

        for i in 0..=8 {
            for j in 0..=8 {
                let u = u_lo + (u_hi - u_lo) * i as f64 / 8.0;
                let v = v_lo + (v_hi - v_lo) * j as f64 / 8.0;

                let deg = off_axis_deg(&geometry, out, u, v);

                // The lattice is 2% of the panel, so a point between two lattice nodes
                // can be marginally worse than the limit; a degree of slack covers it.
                assert!(deg <= NARROW_CONE_DEG + 1.0,
                        "({u:.2},{v:.2}) is {deg:.1} deg off axis");
            }
        }

        // The tracker sits under the bottom bezel and looks up, so the usable band is the
        // lower part of the panel: this is the fact the whole search exists for.
        assert!(v_hi > 0.8, "the cone reaches the bottom of the panel: {v_hi:.2}");
        assert!(v_lo > 0.2, "the cone excludes the top of the panel: {v_lo:.2}");
    }

    #[test]
    fn the_grid_is_serpentine_and_inside_the_rectangle() {
        let geometry = desk();
        let out      = geometry.outputs.iter().find(|o| o.name == "DP-1").expect("DP-1");

        let uv    = (0.3, 0.7, 0.4, 0.9);
        let stops = grid_in(out, uv, 4, 3);

        assert_eq!(stops.len(), 12);

        for (u, v, px) in &stops {
            assert!(*u >= uv.0 && *u <= uv.1, "u {u} inside {uv:?}");
            assert!(*v >= uv.2 && *v <= uv.3, "v {v} inside {uv:?}");
            assert_eq!(*px, out.uv_to_px(*u, *v));
        }

        // Row 0 runs left to right, row 1 right to left.
        assert!(stops[0].0 < stops[3].0);
        assert!(stops[4].0 > stops[7].0);
    }

    #[test]
    fn the_plan_splits_the_time_it_was_given() {
        let geometry = desk();
        let config   = RecordConfig {
            display : "DP-1".into(),
            minutes : 5.0,
            ..RecordConfig::default()
        };

        let plan = plan(&geometry, &config).expect("a plan for DP-1");

        assert_eq!(plan.stops.len(), 12);
        assert!(plan.wander_s > 60.0, "the wander gets real time: {:.0}s", plan.wander_s);
        assert!(2.0 * plan.grid_s + plan.wander_s <= 5.0 * 60.0 + 1.0);
        assert!(plan.cone_max_deg <= CONE_MAX_DEG + 1.0);
    }

    #[test]
    fn phase_names_match_the_backgrounds() {
        assert_eq!(Phase::GridBlack.as_str()         , "grid_black");
        assert_eq!(Phase::GridBlack.background_name(), "black");
        assert_eq!(Phase::GridBlack.background()     , BLACK);
        assert_eq!(Phase::GridWhite.background()     , WHITE);
        assert_eq!(Phase::Wander.background()        , WHITE);
    }
}


