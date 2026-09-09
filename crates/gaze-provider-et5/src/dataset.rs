//! Session files to model rows: the one place that decides what a training example is.
//!
//! The label is a **residual in angle space, before intersection** (PLAN-ET5's third
//! principle). For every frame the firmware produced a combined gaze ray, and the target
//! it should have hit is known, so the label is the yaw and pitch by which the firmware's
//! ray missed the ray from the same origin to the target. Nothing here intersects a
//! panel: the correction exists whether or not the ray lands on glass, and the cone
//! straddles the seam between two of them.
//!
//! The features are the device's own state, not derived quantities:
//!
//! - raw (pre-calibration) eye origins, left and right, tracker millimetres;
//! - per-eye gaze direction as yaw and pitch in tracker space;
//! - the interocular vector from the calibrated origins, which is what encodes head yaw
//!   and roll (the sweep's head-gain fit measured it as the dominant predictor);
//! - pupil diameters, the reason a session runs on two backgrounds;
//! - validity flags, so a monocular frame is a state and not a hole;
//! - angle of the combined ray from the tracker axis, the radial coordinate the firmware's
//!   own error is roughly a function of;
//! - the head features again at 300 ms in the past, because the model's head error trails
//!   the head (cross-validated in the sweep: instantaneous R2 -0.05, 300 ms 0.27).
//!
//! Grouping is by session and by hold, never by frame: frames inside one fixation are the
//! same observation seen ninety times, and a per-sample split reports a number that has
//! nothing to do with tomorrow morning.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use glam::DVec3;
use serde::Deserialize;
use gaze_core::{DesktopGeometry, GlobalPx, OutputGeometry};

use crate::gaze::{Et5Frame, combined_ray};
use crate::model::{Features, or_nan};
use crate::record::{CLICK_PHASE, ClickRecord, SessionEnd, SessionMeta};
use crate::sweep::{
    FALLBACK_PX_PER_DEG, RaySample, StopWindow, TimedFrame, TrajPoint, desk_to_sensor,
    estimate_lag, saccade_mask, target_at, was_moving,
};
use crate::ttp::DisplayArea;

/// How far back the lagged head features look, seconds.
pub const HEAD_LAG_S: f64 = 0.300;

/// Largest gap between the wanted lag time and the frame actually found, seconds. At
/// the ET5's 33 Hz the nearest frame is normally within 15 ms and one dropped frame
/// puts it 30 ms off, so 60 ms tolerates a short dropout and refuses to call a frame
/// from the other side of a blink a head measurement.
const HEAD_LAG_GAP_S: f64 = 0.060;

/// Minimum frames a stop needs before it contributes an aggregated (median) row.
const STOP_MIN_FRAMES: usize = 5;

// --- Rows ---

/// One training example.
///
/// Every feature is `f64` and missing is `f64::NAN`, including the validity flags: a
/// frame that never reported an eye is not the same thing as a frame that reported an
/// untracked one, and the loader on the other side needs to be able to tell.
///
/// Field names and units follow `model/gaze_model/schema.py`, which the Phase C harness
/// reads without a rename shim. The fields that schema has no column for
/// ([`Row::hold_key`], [`Row::session_phase`], the raw origins, the alternative
/// residual and the four passive-click columns) are exported as extra columns;
/// `load_export.py` prints them and drops them.
#[derive(Clone, Debug, PartialEq)]
pub struct Row {
    /// Session this row came from, and the export's `group_key`: leave-one-session-out
    /// is the only honest split, so nothing finer is offered as the grouping column.
    pub session_id          : String,
    /// The hold this row came from: `stop_<n>` for a fixation window, `glide_<n>` for a
    /// segment of smooth pursuit. Rows sharing one are correlated, so this is a
    /// *sub*-split of the session, exported alongside `group_key` and never as it.
    pub hold_key            : String,
    /// `black`, `white`, or `unknown` for imported legacy data.
    pub background          : String,
    /// What kind of row this is: `stop` (a grid fixation), `hold` (a parallax head
    /// sweep, where the head moves and the eyes do not), or `glide` (smooth pursuit).
    pub phase               : String,
    /// Which part of the recording session it came from: `grid_black`, `wander`,
    /// `grid_white`, or `legacy` for imported data.
    pub session_phase       : String,
    /// Host time within the session, seconds.
    pub t_s                 : f64,
    /// True for the one aggregated row per stop. A model that wants one observation per
    /// fixation takes these; a model that wants the noise takes the rest.
    pub is_mean             : bool,

    /// Calibrated left eye position, tracker millimetres.
    pub origin_l_mm         : [f64; 3],
    /// Calibrated right eye position, tracker millimetres.
    pub origin_r_mm         : [f64; 3],
    /// Pre-calibration left eye position, tracker millimetres. Exported as an extra
    /// column: the firmware's own calibration moves the origins by a few millimetres in
    /// a way that is itself a function of the eye model, so the raw pair is a different
    /// (and arguably more honest) head measurement.
    pub origin_raw_l_mm     : [f64; 3],
    /// Pre-calibration right eye position, tracker millimetres.
    pub origin_raw_r_mm     : [f64; 3],
    /// Left eye gaze direction, yaw against the tracker axis, degrees.
    pub dir_l_yaw_deg       : f64,
    /// Left eye gaze direction, pitch against the tracker axis, degrees.
    pub dir_l_pitch_deg     : f64,
    /// Right eye gaze direction, yaw against the tracker axis, degrees.
    pub dir_r_yaw_deg       : f64,
    /// Right eye gaze direction, pitch against the tracker axis, degrees.
    pub dir_r_pitch_deg     : f64,
    /// Right minus left calibrated eye origin, millimetres: head yaw, roll and IPD
    /// foreshortening in one vector.
    pub inter_mm            : [f64; 3],
    /// Left pupil diameter, millimetres.
    pub pupil_l_mm          : f64,
    /// Right pupil diameter, millimetres.
    pub pupil_r_mm          : f64,
    /// 1 when the left eye was tracked, 0 when it was not.
    pub valid_l             : f64,
    /// 1 when the right eye was tracked, 0 when it was not.
    pub valid_r             : f64,
    /// Angle between the firmware's combined ray and the tracker axis, degrees. Zero is
    /// looking straight into the sensor.
    pub angle_axis_deg      : f64,
    /// `origin_l_mm` 300 ms earlier.
    pub lag_origin_l_mm     : [f64; 3],
    /// `origin_r_mm` 300 ms earlier.
    pub lag_origin_r_mm     : [f64; 3],
    /// `inter_mm` 300 ms earlier.
    pub lag_inter_mm        : [f64; 3],

    /// The target's position in tracker space, millimetres.
    pub target_mm           : [f64; 3],
    /// Yaw by which the firmware's combined ray missed the target, degrees.
    pub residual_yaw_deg    : f64,
    /// Pitch by which the firmware's combined ray missed the target, degrees.
    pub residual_pitch_deg  : f64,
    /// The same residual computed from `gaze::combined_ray` (the per-eye midpoint and
    /// mean direction) instead of the firmware's filtered 2D lifted through the declared
    /// plane. Exported as an extra column because the two disagree and the choice is a
    /// real one: the filtered ray is what the runtime corrects, but it inherits any error
    /// in the declared plane, and the per-eye rays do not.
    pub residual_yaw_deg_combined   : f64,
    /// See [`Row::residual_yaw_deg_combined`].
    pub residual_pitch_deg_combined : f64,

    /// Kind of the element a passive click landed on, lowercase (`button`, `text`,
    /// `icon`, ...). Empty for every row that did not come from a click, so a
    /// consumer can filter on it without joining anything.
    pub element_kind    : String,
    /// Width of that element's box, logical pixels. NaN for a non-click row.
    pub element_w_px    : f64,
    /// Height of that element's box, logical pixels. NaN for a non-click row.
    pub element_h_px    : f64,
    /// Mean luminance of the screen crop the element was found in, [0, 1]. The pupil
    /// covariate a passive session gets in place of a driven background. NaN for a
    /// non-click row.
    pub crop_luma       : f64,
    /// Where a click's element came from: `tree`, `vision`, `caret` or `trainer`.
    /// Empty for a non-click row and for click files written before it was recorded.
    pub source          : String,
    /// The posture the trainer had asked for on a trainer click. Empty otherwise.
    pub posture         : String,
    /// The trainer's task index on a trainer click, so tasks can be held out whole.
    /// NaN otherwise.
    pub trainer_task    : f64,
    /// 1 when a trainer click landed on the step's target, 0 when it landed on some
    /// other widget. NaN otherwise.
    pub trainer_hit     : f64,
}

/// The CSV header, in the order [`Row::to_csv`] writes the fields.
///
/// The first 38 are `model/gaze_model/schema.py`'s `COLUMNS`, verbatim and in order, so
/// `load_export.py` needs no aliases. The rest are this exporter's extras.
pub const CSV_COLUMNS: &[&str] = &[
    "session_id", "group_key", "background", "phase", "t_s",
    "origin_l_x_mm", "origin_l_y_mm", "origin_l_z_mm",
    "origin_r_x_mm", "origin_r_y_mm", "origin_r_z_mm",
    "dir_l_yaw_deg", "dir_l_pitch_deg", "dir_r_yaw_deg", "dir_r_pitch_deg",
    "inter_x_mm", "inter_y_mm", "inter_z_mm",
    "pupil_l_mm", "pupil_r_mm",
    "valid_l", "valid_r",
    "angle_axis_deg",
    "origin_l_x_mm_lag300", "origin_l_y_mm_lag300", "origin_l_z_mm_lag300",
    "origin_r_x_mm_lag300", "origin_r_y_mm_lag300", "origin_r_z_mm_lag300",
    "inter_x_mm_lag300", "inter_y_mm_lag300", "inter_z_mm_lag300",
    "target_x_mm", "target_y_mm", "target_z_mm",
    "residual_yaw_deg", "residual_pitch_deg",
    "is_mean",
    // Extras beyond the shared schema.
    "hold_key", "session_phase",
    "origin_raw_l_x_mm", "origin_raw_l_y_mm", "origin_raw_l_z_mm",
    "origin_raw_r_x_mm", "origin_raw_r_y_mm", "origin_raw_r_z_mm",
    "residual_yaw_deg_combined", "residual_pitch_deg_combined",
    "element_kind", "element_w_px", "element_h_px", "crop_luma",
    "source", "posture", "trainer_task", "trainer_hit",
];

/// How many leading columns are the Phase C harness's shared schema.
pub const SHARED_SCHEMA_COLUMNS: usize = 38;

// --- Sessions ---

/// A loaded session file: its meta line and the records it carried.
#[derive(Clone, Debug)]
pub struct Session {
    /// The meta line.
    pub meta   : SessionMeta,
    /// The end line, absent when the session was interrupted before it was written.
    pub end    : Option<SessionEnd>,
    /// Fixation and hold windows in file order.
    pub stops  : Vec<TaggedStop>,
    /// Passive click records by their index `n`, empty for a recorded session.
    pub clicks : BTreeMap<u64, ClickRecord>,
    /// The target trajectory over the whole session, sorted by time. Dense and
    /// continuous, which is what lets a frame be attributed to a phase.
    pub traj   : Vec<TrajPoint>,
    /// Phase and background of each trajectory sample, parallel to `traj`.
    pub tags   : Vec<Tag>,
    /// Every decoded frame, sorted by time.
    pub frames : Vec<TimedFrame>,
}

/// One stop window with the phase it belongs to.
#[derive(Clone, Debug)]
pub struct TaggedStop {
    /// The window itself.
    pub stop    : StopWindow,
    /// Which part of the session it ran in.
    pub tag     : Tag,
    /// Connector this window's target was on, from the record's own `display` field.
    /// A recorded session runs on one display and every stop repeats the meta line's;
    /// a passive click session clicks wherever the pointer is, so the stop's own value
    /// is the authority and the meta line only names the tracker's declared plane.
    pub display : Option<String>,
    /// Index of the click this window belongs to, for a `click` phase record. `None`
    /// for a recorded session's stops, which are identified by file order.
    pub n       : Option<u64>,
}

/// The phase and background labels a record carries.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Tag {
    /// `grid_black`, `wander`, `grid_white`, or `legacy`.
    pub phase      : String,
    /// `black`, `white`, or `unknown`.
    pub background : String,
}

// --- Session ---

impl Session {
    /// Loads one session file.
    ///
    /// A line that does not parse is skipped rather than failing the load: a session
    /// killed mid-write leaves a truncated last line, and the hundred thousand good rows
    /// before it are still data.
    pub fn load(path: &Path) -> Result<Self, DatasetError> {
        let text = std::fs::read_to_string(path)
            .map_err(|e| DatasetError::Io(path.display().to_string(), e.to_string()))?;

        let mut meta   = None;
        let mut end    = None;
        let mut stops  = Vec::new();
        let mut clicks = BTreeMap::new();
        let mut traj   = Vec::new();
        let mut tags   = Vec::new();
        let mut frames = Vec::new();

        for line in text.lines() {
            let Ok(record) = serde_json::from_str::<RawRecord>(line) else {
                continue;
            };

            match record.kind.as_str() {
                "meta"     => meta = serde_json::from_str::<SessionMeta>(line).ok(),
                "meta_end" => end  = serde_json::from_str::<SessionEnd>(line).ok(),
                "stop"     => {
                    if let Some(stop) = record.stop {
                        stops.push(TaggedStop {
                            stop    : stop,
                            tag     : record.tag(),
                            display : record.display.clone(),
                            n       : record.n,
                        });
                    }
                }
                // An unknown kind is skipped by the arm below, so a reader that
                // predates clicks still loads a click session; this arm is what makes
                // the element columns available to one that does not.
                "click"    => {
                    if let Some(click) = record.click {
                        clicks.insert(click.n, click);
                    }
                }
                "traj"     => {
                    if let Some(point) = record.point {
                        traj.push(point);
                        tags.push(record.tag());
                    }
                }
                "frame"    => frames.extend(record.frame),
                _          => {}
            }
        }

        let meta = meta.ok_or_else(|| DatasetError::NoMeta(path.display().to_string()))?;

        // The writer emits one phase at a time, so the file is already in time order;
        // sorting makes that a property of the loader rather than of the writer.
        let mut order: Vec<usize> = (0..traj.len()).collect();
        order.sort_by(|a, b| traj[*a].t_s.total_cmp(&traj[*b].t_s));

        let sorted_traj = order.iter().map(|i| traj[*i]).collect();
        let sorted_tags = order.iter().map(|i| tags[*i].clone()).collect();

        frames.sort_by(|a, b| a.t_s.total_cmp(&b.t_s));

        Ok(Self {
            meta   : meta,
            end    : end,
            stops  : stops,
            clicks : clicks,
            traj   : sorted_traj,
            tags   : sorted_tags,
            frames : frames,
        })
    }

    /// True when the blob was the same at both ends of the session. A session that fails
    /// this describes two different feature extractors and should not be trained on.
    pub fn blob_is_stable(&self) -> bool {
        self.end.as_ref().is_none_or(|e| e.blob_sha256 == self.meta.blob_sha256)
    }

    /// The phase and background in force at `t_s`, from the nearest trajectory sample.
    pub fn tag_at(&self, t_s: f64) -> Tag {
        if self.tags.is_empty() {
            return Tag { phase: "unknown".into(), background: "unknown".into() };
        }

        let i = {
            match self.traj.binary_search_by(|p| p.t_s.total_cmp(&t_s)) {
                Ok(i)  => i,
                Err(i) => i.min(self.tags.len() - 1),
            }
        };

        self.tags[i].clone()
    }
}

// --- Loading rows ---

/// Turns one session into rows against the desk geometry it was recorded on.
///
/// The display and the sensor pitch come from the session's own meta line, not from the
/// live desk config, so a session recorded before a remount still resolves its own
/// targets; only the panel's shape and logical rect are read from `geometry`. A session
/// whose display is not in the config produces no rows.
pub fn rows(session: &Session, geometry: &DesktopGeometry) -> Vec<Row> {
    let Some(out) = geometry.outputs.iter().find(|o| o.name == session.meta.display) else {
        return Vec::new();
    };

    let area  = session.meta.display_area;
    let pitch = session.meta.tracker_pitch_deg;

    // The tracker axis in the frame the firmware reports in: the desk config's nominal
    // eye seen from the tracker, rotated out of the desk frame by the mount pitch. A
    // fixed reference, so `angle_axis_deg` moves only when the gaze does.
    let axis = DVec3::from_array(
        desk_to_sensor((geometry.eye() - geometry.tracker()).to_array(), pitch),
    );

    // The firmware's own combined ray per frame, in tracker space. Frames the firmware
    // could not produce one for are absent, so the index back to `frames` is carried.
    let mut indexed = Vec::with_capacity(session.frames.len());

    for (i, f) in session.frames.iter().enumerate() {
        if let Some((origin, dir)) = firmware_ray(&f.frame, &area) {
            indexed.push((i, RaySample { t_s: f.t_s, origin: origin, dir: dir }));
        }
    }

    let samples: Vec<RaySample> = indexed.iter()
        .map(|(_, r)| RaySample { t_s: r.t_s, origin: r.origin, dir: r.dir })
        .collect();
    let keep = saccade_mask(&samples);

    // Pixel scale at the panel centre, for the lag search's degree conversion.
    let scale = geometry.px_per_deg(geometry.eye(), out.uv_to_px(0.5, 0.5))
        .map(|(h, v)| (h + v) * 0.5)
        .unwrap_or(FALLBACK_PX_PER_DEG);

    // Lag: how far the gaze trails the gliding target. Only surviving glide samples vote.
    let moving: Vec<(f64, GlobalPx)> = indexed.iter().zip(&keep)
        .filter(|((_, r), k)| **k && was_moving(&session.traj, r.t_s))
        .filter_map(|((i, r), _)| {
            let [nx, ny] = session.frames[*i].frame.gaze_2d_norm?;

            Some((r.t_s, out.uv_to_px(nx, ny)))
        })
        .collect();

    let lag_s = estimate_lag(&session.traj, &moving, scale);

    // Head features at every frame, so the lag lookup is a search over one array.
    let heads: Vec<(f64, Head)> = session.frames.iter()
        .map(|f| (f.t_s, Head::of(&f.frame)))
        .collect();

    let mut rows = Vec::new();

    // Stops: every surviving frame in the window, then one aggregated row for the window.
    // The parallax holds go in with the grid fixations. They are the head-diverse part of
    // the data and the whole reason the head features exist; `sweep`'s field fit drops
    // them only because a moving head is bad for a static 2D correction field.
    for (index, tagged) in session.stops.iter().enumerate() {
        // A passive click session clicks on whichever panel the pointer is on, so the
        // target is resolved against the stop's own display and only falls back to the
        // meta line's when the record does not name one (every recorded session).
        let stop_out = tagged.display.as_deref()
            .and_then(|name| geometry.outputs.iter().find(|o| o.name == name))
            .unwrap_or(out);

        let click = tagged.n.and_then(|n| session.clicks.get(&n));

        let hold_key = {
            match tagged.n {
                Some(n) if tagged.tag.phase == CLICK_PHASE => format!("click_{n}"),
                _                                          => format!("stop_{index}"),
            }
        };

        let kind  = if tagged.stop.parallax { "hold" } else { "stop" };
        let first = rows.len();

        for ((i, sample), _) in indexed.iter().zip(&keep).filter(|(_, k)| **k) {
            if sample.t_s < tagged.stop.t_start || sample.t_s > tagged.stop.t_end {
                continue;
            }

            let mut row = build_row(
                session, stop_out, pitch, axis, &heads, *i, sample, tagged.stop.px,
                hold_key.clone(), kind, &tagged.tag.phase, &tagged.tag.background,
            );

            if let Some(click) = click {
                row.element_kind = click.element.kind.clone();
                row.element_w_px = click.element.bbox.w;
                row.element_h_px = click.element.bbox.h;
                row.crop_luma    = click.crop_luma;
                row.source       = click.source.clone().unwrap_or_default();

                if let Some(tag) = &click.trainer {
                    row.posture      = tag.posture.clone();
                    row.trainer_task = tag.task as f64;
                    row.trainer_hit  = f64::from(u8::from(tag.hit));
                }
            }

            rows.push(row);
        }

        if rows.len() - first >= STOP_MIN_FRAMES {
            let mean = mean_row(&rows[first..]);

            rows.push(mean);
        }
    }

    // Glides: the lag-shifted target under each surviving sample, grouped by segment.
    let segments = glide_segments(&session.traj);

    for ((i, sample), _) in indexed.iter().zip(&keep).filter(|(_, k)| **k) {
        if !was_moving(&session.traj, sample.t_s) {
            continue;
        }

        let Some(px) = target_at(&session.traj, sample.t_s - lag_s) else {
            continue;
        };

        let Some(segment) = segment_of(&segments, sample.t_s) else {
            continue;
        };

        let tag = session.tag_at(sample.t_s);

        rows.push(build_row(
            session, out, pitch, axis, &heads, *i, sample, px,
            format!("glide_{segment}"), "glide", &tag.phase, &tag.background,
        ));
    }

    rows
}

/// Loads every session file in `paths` (files, or directories of `.jsonl` files) and
/// turns them into rows, in path order.
pub fn load_rows(paths: &[PathBuf], geometry: &DesktopGeometry)
    -> Result<Vec<Row>, DatasetError>
{
    let mut all = Vec::new();

    for path in expand(paths)? {
        let session = Session::load(&path)?;

        if !session.blob_is_stable() {
            tracing::warn!("{}: the blob changed mid-session; rows describe two \
                            different firmware models", path.display());
        }

        all.extend(rows(&session, geometry));
    }

    Ok(all)
}

/// Writes rows as CSV with [`CSV_COLUMNS`] as the header. Missing values are `NaN`.
pub fn write_csv(rows: &[Row], path: &Path) -> Result<(), DatasetError> {
    let file  = std::fs::File::create(path)
        .map_err(|e| DatasetError::Io(path.display().to_string(), e.to_string()))?;
    let mut w = std::io::BufWriter::new(file);

    let mut write = || -> std::io::Result<()> {
        writeln!(w, "{}", CSV_COLUMNS.join(","))?;

        for row in rows {
            writeln!(w, "{}", row.to_csv())?;
        }

        w.flush()
    };

    write().map_err(|e| DatasetError::Io(path.display().to_string(), e.to_string()))
}

// --- Row ---

impl Row {
    /// One CSV line, fields in [`CSV_COLUMNS`] order.
    pub fn to_csv(&self) -> String {
        let mut fields = vec![
            self.session_id.clone(),
            // `group_key`: the session, never the hold. `GroupKFold` on anything finer
            // reports an in-session number and calls it a grouped one.
            self.session_id.clone(),
            self.background.clone(),
            self.phase.clone(),
            self.t_s.to_string(),
        ];

        let push  = |f: &mut Vec<String>, v: f64| f.push(v.to_string());
        let push3 = |f: &mut Vec<String>, v: [f64; 3]| {
            for c in v {
                f.push(c.to_string());
            }
        };

        push3(&mut fields, self.origin_l_mm);
        push3(&mut fields, self.origin_r_mm);
        push(&mut fields, self.dir_l_yaw_deg);
        push(&mut fields, self.dir_l_pitch_deg);
        push(&mut fields, self.dir_r_yaw_deg);
        push(&mut fields, self.dir_r_pitch_deg);
        push3(&mut fields, self.inter_mm);
        push(&mut fields, self.pupil_l_mm);
        push(&mut fields, self.pupil_r_mm);
        push(&mut fields, self.valid_l);
        push(&mut fields, self.valid_r);
        push(&mut fields, self.angle_axis_deg);
        push3(&mut fields, self.lag_origin_l_mm);
        push3(&mut fields, self.lag_origin_r_mm);
        push3(&mut fields, self.lag_inter_mm);
        push3(&mut fields, self.target_mm);
        push(&mut fields, self.residual_yaw_deg);
        push(&mut fields, self.residual_pitch_deg);

        fields.push(u8::from(self.is_mean).to_string());

        fields.push(self.hold_key.clone());
        fields.push(self.session_phase.clone());
        push3(&mut fields, self.origin_raw_l_mm);
        push3(&mut fields, self.origin_raw_r_mm);
        push(&mut fields, self.residual_yaw_deg_combined);
        push(&mut fields, self.residual_pitch_deg_combined);

        fields.push(self.element_kind.clone());
        push(&mut fields, self.element_w_px);
        push(&mut fields, self.element_h_px);
        push(&mut fields, self.crop_luma);
        fields.push(self.source.clone());
        fields.push(self.posture.clone());
        push(&mut fields, self.trainer_task);
        push(&mut fields, self.trainer_hit);

        fields.join(",")
    }

    /// Total angular miss of this row, degrees. Convenience for reporting a baseline.
    pub fn residual_deg(&self) -> f64 {
        (self.residual_yaw_deg.powi(2) + self.residual_pitch_deg.powi(2)).sqrt()
    }
}

// --- Internals ---

/// Head state of one frame: what the 300 ms lookback carries forward.
#[derive(Clone, Copy, Debug, Default)]
pub struct Head {
    /// Calibrated left eye position.
    pub cal_l : Option<DVec3>,
    /// Calibrated right eye position.
    pub cal_r : Option<DVec3>,
    /// Pre-calibration left eye position.
    pub raw_l : Option<DVec3>,
    /// Pre-calibration right eye position.
    pub raw_r : Option<DVec3>,
    /// Right minus left calibrated eye origin.
    pub inter : Option<DVec3>,
}

// --- Head ---

impl Head {
    /// Extracts the head state a frame reports. Origins are kept whatever the validity
    /// flag says only when the flag agrees: an untracked eye's last position is stale by
    /// an unknown amount and is not a measurement.
    pub fn of(frame: &Et5Frame) -> Self {
        let raw_l = frame.left_valid().then_some(frame.eye_origin_raw_l_mm).flatten();
        let raw_r = frame.right_valid().then_some(frame.eye_origin_raw_r_mm).flatten();
        let cal_l = frame.left_valid().then_some(frame.eye_origin_l_mm).flatten();
        let cal_r = frame.right_valid().then_some(frame.eye_origin_r_mm).flatten();

        Self {
            cal_l : cal_l.map(DVec3::from_array),
            cal_r : cal_r.map(DVec3::from_array),
            raw_l : raw_l.map(DVec3::from_array),
            raw_r : raw_r.map(DVec3::from_array),
            inter : match (cal_l, cal_r) {
                (Some(l), Some(r)) => Some(DVec3::from_array(r) - DVec3::from_array(l)),
                _                  => None,
            },
        }
    }
}

/// One line of a session file, as far as the loader cares.
#[derive(Deserialize)]
struct RawRecord {
    /// `meta`, `stop`, `traj`, `frame`, `click` or `meta_end`.
    kind       : String,
    /// Connector the record's target was on. Every writer emits it.
    #[serde(default)]
    display    : Option<String>,
    /// Click index, on the paired `stop` and `click` records of a passive session.
    #[serde(default)]
    n          : Option<u64>,
    /// Present on records written by `record`; absent in imported legacy data.
    #[serde(default)]
    phase      : Option<String>,
    /// See `phase`.
    #[serde(default)]
    background : Option<String>,
    #[serde(default)]
    stop       : Option<StopWindow>,
    #[serde(default)]
    point      : Option<TrajPoint>,
    #[serde(default)]
    frame      : Option<TimedFrame>,
    #[serde(default)]
    click      : Option<ClickRecord>,
}

// --- RawRecord ---

impl RawRecord {
    /// The record's labels, defaulting to `unknown` for a file that predates them.
    fn tag(&self) -> Tag {
        Tag {
            phase      : self.phase.clone().unwrap_or_else(|| "unknown".into()),
            background : self.background.clone().unwrap_or_else(|| "unknown".into()),
        }
    }
}

/// The firmware's combined ray for one frame, in tracker space: origin at the midpoint of
/// the tracked eyes, direction toward where its filtered 2D output lands on the declared
/// plane.
///
/// This is `gaze::filtered_ray` for a three-corner plane rather than an axis-aligned
/// rect: normalised coordinates run from the top-left corner, so the point is
/// `tl + nx (tr - tl) + ny (bl - tl)`.
pub fn firmware_ray(frame: &Et5Frame, area: &DisplayArea) -> Option<(DVec3, DVec3)> {
    let [nx, ny] = frame.gaze_2d_norm?;

    // The firmware reports (-1, -1) for an invalid combined gaze and clamps to the area
    // otherwise, and a clamped point is a wrong direction.
    if !(0.0..1.0).contains(&nx) || !(0.0..1.0).contains(&ny) {
        return None;
    }

    let left  = frame.left_valid().then_some(frame.eye_origin_l_mm).flatten();
    let right = frame.right_valid().then_some(frame.eye_origin_r_mm).flatten();

    let origin = {
        match (left, right) {
            (Some(l), Some(r)) => (DVec3::from_array(l) + DVec3::from_array(r)) * 0.5,
            (Some(l), None)    => DVec3::from_array(l),
            (None, Some(r))    => DVec3::from_array(r),
            (None, None)       => return None,
        }
    };

    let delta = area_point(area, nx, ny) - origin;

    if delta.length_squared() < 1.0 {
        return None;
    }

    Some((origin, delta.normalize()))
}

/// The point at normalised `(nx, ny)` on a three-corner declared plane.
fn area_point(area: &DisplayArea, nx: f64, ny: f64) -> DVec3 {
    let tl = DVec3::from_array(area.tl_mm);
    let tr = DVec3::from_array(area.tr_mm);
    let bl = DVec3::from_array(area.bl_mm);

    tl + (tr - tl) * nx + (bl - tl) * ny
}

/// Yaw and pitch of `dir` relative to `reference`, degrees: azimuth and elevation in the
/// tangent frame at `reference` (right = up x reference, local up = reference x right),
/// both zero exactly when the two are parallel.
///
/// One function does two jobs. With `reference` a target direction it is an angle-space
/// residual; with `reference` the tracker axis it is an absolute direction readout. This
/// is `model/gaze_model/geometry.py::local_yaw_pitch_deg`, ported so the Rust export and
/// the Python harness decompose the same vectors into the same two numbers.
pub fn local_yaw_pitch_deg(dir: DVec3, reference: DVec3) -> (f64, f64) {
    let up = DVec3::Y;

    if dir.length_squared() < 1e-18 || reference.length_squared() < 1e-18 {
        return (f64::NAN, f64::NAN);
    }

    let reference = reference.normalize();
    let cross     = up.cross(reference);

    // Only reachable if the reference points straight up; the tracker axis and every
    // target are roughly forward, so this is a definedness guard, not a real case.
    let right = {
        if cross.length() < 1e-9 {
            DVec3::X
        }
        else {
            cross.normalize()
        }
    };

    let local_up = reference.cross(right);
    let d        = dir.normalize();
    let forward  = d.dot(reference);

    (
        d.dot(right).atan2(forward).to_degrees(),
        d.dot(local_up).atan2(forward).to_degrees(),
    )
}

/// The head state 300 ms before `t_s`: the latest sample at or before that time, or
/// nothing when the nearest one is further away than the gap tolerance.
pub fn lagged_head(heads: &[(f64, Head)], t_s: f64) -> Head {
    let want = t_s - HEAD_LAG_S;

    let i = {
        match heads.binary_search_by(|(t, _)| t.total_cmp(&want)) {
            Ok(i)  => i,
            Err(0) => return Head::default(),
            Err(i) => i - 1,
        }
    };

    let (t, head) = heads[i];

    if want - t > HEAD_LAG_GAP_S {
        return Head::default();
    }

    head
}

/// Assembles one row.
#[allow(clippy::too_many_arguments)]
fn build_row(
    session      : &Session,
    out          : &OutputGeometry,
    pitch        : f64,
    axis         : DVec3,
    heads        : &[(f64, Head)],
    index        : usize,
    sample       : &RaySample,
    target       : GlobalPx,
    hold_key     : String,
    kind         : &str,
    session_phase: &str,
    background   : &str,
)
    -> Row
{
    let frame  = &session.frames[index].frame;
    let head   = Head::of(frame);
    let lagged = lagged_head(heads, sample.t_s);

    // The target sits on the panel surface in the desk frame; the device reports and
    // expects its own frame, pitched up by the mount wedge.
    let target_mm = desk_to_sensor(out.px_to_world(target).to_array(), pitch);
    let want      = DVec3::from_array(target_mm) - sample.origin;

    let residual = |dir: DVec3| {
        if want.length_squared() < 1.0 {
            (f64::NAN, f64::NAN)
        }
        else {
            local_yaw_pitch_deg(dir, want)
        }
    };

    let (residual_yaw_deg, residual_pitch_deg) = residual(sample.dir);

    // The per-eye midpoint ray, for comparison: it needs no declared plane, so it is the
    // residual that survives a plane that was declared wrong.
    let (residual_yaw_deg_combined, residual_pitch_deg_combined) = {
        match combined_ray(frame) {
            Some((origin, dir, _)) => {
                let want = DVec3::from_array(target_mm) - origin;

                if want.length_squared() < 1.0 {
                    (f64::NAN, f64::NAN)
                }
                else {
                    local_yaw_pitch_deg(dir, want)
                }
            }
            None                   => (f64::NAN, f64::NAN),
        }
    };

    // The same assembly the runtime uses on a live frame, so a row and a frame cannot
    // disagree about what a feature is.
    let f = Features::of_frame(frame, &lagged, axis, sample.dir).values;

    Row {
        session_id          : session.meta.session_id.clone(),
        hold_key            : hold_key,
        background          : background.to_string(),
        phase               : kind.to_string(),
        session_phase       : session_phase.to_string(),
        t_s                 : sample.t_s,
        is_mean             : false,
        origin_l_mm         : [f[0], f[1], f[2]],
        origin_r_mm         : [f[3], f[4], f[5]],
        origin_raw_l_mm     : or_nan(head.raw_l),
        origin_raw_r_mm     : or_nan(head.raw_r),
        dir_l_yaw_deg       : f[6],
        dir_l_pitch_deg     : f[7],
        dir_r_yaw_deg       : f[8],
        dir_r_pitch_deg     : f[9],
        inter_mm            : [f[10], f[11], f[12]],
        pupil_l_mm          : f[13],
        pupil_r_mm          : f[14],
        valid_l             : f[15],
        valid_r             : f[16],
        angle_axis_deg      : f[17],
        lag_origin_l_mm     : [f[18], f[19], f[20]],
        lag_origin_r_mm     : [f[21], f[22], f[23]],
        lag_inter_mm        : [f[24], f[25], f[26]],
        target_mm           : target_mm,
        residual_yaw_deg    : residual_yaw_deg,
        residual_pitch_deg  : residual_pitch_deg,
        residual_yaw_deg_combined   : residual_yaw_deg_combined,
        residual_pitch_deg_combined : residual_pitch_deg_combined,
        // Filled in by the caller for a click stop; a row from a recorded session has
        // no element and no measured screen luminance to report.
        element_kind        : String::new(),
        element_w_px        : f64::NAN,
        element_h_px        : f64::NAN,
        crop_luma           : f64::NAN,
        source              : String::new(),
        posture             : String::new(),
        trainer_task        : f64::NAN,
        trainer_hit         : f64::NAN,
    }
}

/// The aggregated row for a stop: every numeric field is the median over the frames
/// that reported it, NaN where none did. Labels come from the first row.
///
/// A median rather than a mean (the flag is still `is_mean`, the schema's name for
/// the one-per-stop row): a click window holds the frames of the eye's arrival and
/// departure as well as the fixation, and on the 2026-09-04 sessions the mean's
/// per-click residual sat 0.15 degrees above the median's, all of it saccade frames
/// the saccade gate did not catch.
fn mean_row(rows: &[Row]) -> Row {
    let mut mean = rows[0].clone();

    let avg = |f: &dyn Fn(&Row) -> f64| {
        let mut values: Vec<f64> = rows.iter().map(f).filter(|v| v.is_finite()).collect();

        if values.is_empty() {
            return f64::NAN;
        }

        values.sort_by(f64::total_cmp);

        let n = values.len();

        if n % 2 == 1 { values[n / 2] } else { (values[n / 2 - 1] + values[n / 2]) * 0.5 }
    };

    let avg3 = |f: &dyn Fn(&Row) -> [f64; 3]| {
        [
            avg(&|r: &Row| f(r)[0]),
            avg(&|r: &Row| f(r)[1]),
            avg(&|r: &Row| f(r)[2]),
        ]
    };

    mean.is_mean             = true;
    mean.t_s                 = avg(&|r| r.t_s);
    mean.origin_l_mm         = avg3(&|r| r.origin_l_mm);
    mean.origin_r_mm         = avg3(&|r| r.origin_r_mm);
    mean.origin_raw_l_mm     = avg3(&|r| r.origin_raw_l_mm);
    mean.origin_raw_r_mm     = avg3(&|r| r.origin_raw_r_mm);
    mean.dir_l_yaw_deg       = avg(&|r| r.dir_l_yaw_deg);
    mean.dir_l_pitch_deg     = avg(&|r| r.dir_l_pitch_deg);
    mean.dir_r_yaw_deg       = avg(&|r| r.dir_r_yaw_deg);
    mean.dir_r_pitch_deg     = avg(&|r| r.dir_r_pitch_deg);
    mean.inter_mm            = avg3(&|r| r.inter_mm);
    mean.pupil_l_mm          = avg(&|r| r.pupil_l_mm);
    mean.pupil_r_mm          = avg(&|r| r.pupil_r_mm);
    mean.valid_l             = avg(&|r| r.valid_l);
    mean.valid_r             = avg(&|r| r.valid_r);
    mean.angle_axis_deg      = avg(&|r| r.angle_axis_deg);
    mean.lag_origin_l_mm     = avg3(&|r| r.lag_origin_l_mm);
    mean.lag_origin_r_mm     = avg3(&|r| r.lag_origin_r_mm);
    mean.lag_inter_mm        = avg3(&|r| r.lag_inter_mm);
    mean.target_mm           = avg3(&|r| r.target_mm);
    mean.residual_yaw_deg    = avg(&|r| r.residual_yaw_deg);
    mean.residual_pitch_deg  = avg(&|r| r.residual_pitch_deg);
    mean.residual_yaw_deg_combined   = avg(&|r| r.residual_yaw_deg_combined);
    mean.residual_pitch_deg_combined = avg(&|r| r.residual_pitch_deg_combined);

    // Constant across a click's frames, so the mean is the value; averaging anyway
    // keeps every numeric field on one rule.
    mean.element_w_px        = avg(&|r| r.element_w_px);
    mean.element_h_px        = avg(&|r| r.element_h_px);
    mean.crop_luma           = avg(&|r| r.crop_luma);
    mean.trainer_task        = avg(&|r| r.trainer_task);
    mean.trainer_hit         = avg(&|r| r.trainer_hit);

    mean
}

/// Time spans over which the target was gliding, in order. One glide is one observation
/// of pursuit, so rows inside one share a group.
fn glide_segments(traj: &[TrajPoint]) -> Vec<(f64, f64)> {
    let mut segments = Vec::new();
    let mut start    = None;

    for point in traj {
        match (point.moving, start) {
            (true , None)    => start = Some(point.t_s),
            (false, Some(s)) => {
                segments.push((s, point.t_s));
                start = None;
            }
            _                => {}
        }
    }

    if let (Some(s), Some(last)) = (start, traj.last()) {
        segments.push((s, last.t_s));
    }

    segments
}

/// Index of the glide segment covering `t_s`.
fn segment_of(segments: &[(f64, f64)], t_s: f64) -> Option<usize> {
    segments.iter().position(|(a, b)| t_s >= *a && t_s <= *b)
}

/// Every `.jsonl` file named by `paths`, expanding directories. Sorted within a
/// directory so a run is reproducible.
pub fn expand(paths: &[PathBuf]) -> Result<Vec<PathBuf>, DatasetError> {
    let mut files = Vec::new();

    for path in paths {
        if !path.is_dir() {
            files.push(path.clone());
            continue;
        }

        let entries = std::fs::read_dir(path)
            .map_err(|e| DatasetError::Io(path.display().to_string(), e.to_string()))?;

        // A BTreeMap sorts by name, which for `<unix>-<hash>.jsonl` is chronological.
        let mut found = BTreeMap::new();

        for entry in entries.flatten() {
            let p = entry.path();

            if p.extension().is_some_and(|e| e == "jsonl") {
                found.insert(p.file_name().map(|n| n.to_owned()).unwrap_or_default(), p);
            }
        }

        files.extend(found.into_values());
    }

    Ok(files)
}

// --- Errors ---

/// Dataset loading failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DatasetError {
    #[error("{0}: {1}")]
    Io(String, String),
    #[error("{0}: no meta line; not a session file")]
    NoMeta(String),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;
    use crate::record::{SESSION_FORMAT, SessionMeta};
    use crate::sweep::plane_corners;

    /// The desk config, which the synthetic sessions place their targets on.
    fn desk() -> DesktopGeometry {
        DesktopGeometry::from_toml(
            &std::fs::read_to_string("../../config/desk.toml").expect("desk config"),
        ).expect("desk config parses")
    }

    /// The flat panel. A curved one would put a real sagitta between the declared chord
    /// plane and the panel surface, which is a residual the tests should not have to
    /// model to check the arithmetic.
    const FLAT: &str = "HDMI-A-1";

    /// The plane the synthetic sessions declare: the flat panel's own corners, which is
    /// what `sweep::run_sweep` declares in direct mode. With zero mount pitch it is the
    /// desk frame, so `uv` on the declared plane and `uv` on the panel are the same
    /// point and an exact hit is exactly representable.
    fn area(geometry: &DesktopGeometry) -> DisplayArea {
        plane_corners(geometry.outputs.iter().find(|o| o.name == FLAT).expect("flat panel"))
    }

    /// A frame that reports both eyes at `origin` looking at normalised `(nx, ny)`.
    fn frame(t_s: f64, origin: [f64; 3], nx: f64, ny: f64) -> serde_json::Value {
        let l = [origin[0] - 32.0, origin[1], origin[2]];
        let r = [origin[0] + 32.0, origin[1], origin[2]];

        json!({
            "kind"    : "frame",
            "display" : FLAT,
            "frame"   : {
                "t_s"   : t_s,
                "frame" : Et5Frame {
                    timestamp_us        : Some((t_s * 1e6) as i64),
                    frame_counter       : Some((t_s * 90.0) as u32),
                    validity_l          : Some(0),
                    validity_r          : Some(0),
                    pupil_l_mm          : Some(3.5),
                    pupil_r_mm          : Some(3.6),
                    gaze_2d_norm        : Some([nx, ny]),
                    gaze_2d_unfiltered  : Some([nx, ny]),
                    gaze_2d_l_norm      : Some([nx, ny]),
                    gaze_2d_r_norm      : Some([nx, ny]),
                    eye_origin_l_mm     : Some(l),
                    eye_origin_r_mm     : Some(r),
                    gaze_3d_l_mm        : Some([0.0, 0.0, -100.0]),
                    gaze_3d_r_mm        : Some([0.0, 0.0, -100.0]),
                    eye_origin_raw_l_mm : Some(l),
                    eye_origin_raw_r_mm : Some(r),
                },
            },
        })
    }

    /// Writes a session file with one stop window and the given frames.
    fn session_file(
        name    : &str,
        geometry: &DesktopGeometry,
        uv      : (f64, f64),
        window  : (f64, f64),
        frames  : Vec<serde_json::Value>,
    )
        -> Session
    {
        let out = geometry.outputs.iter().find(|o| o.name == FLAT).expect("flat panel");
        let px  = out.uv_to_px(uv.0, uv.1);

        let meta = SessionMeta {
            kind              : "meta".into(),
            format            : SESSION_FORMAT,
            session_id        : name.into(),
            created_unix_s    : 0.0,
            blob_sha256       : "0".repeat(64),
            blob_bytes        : 0,
            display           : FLAT.into(),
            display_area      : area(geometry),
            desk_sha256       : "0".repeat(64),
            // The declared plane is built without a mount rotation, so the rows have to
            // read it back the same way.
            tracker_pitch_deg : 0.0,
            glasses           : false,
            note              : "synthetic".into(),
        };

        let mut lines = vec![json!(meta).to_string()];

        lines.push(json!({
            "kind"       : "stop",
            "display"    : FLAT,
            "phase"      : "grid_black",
            "background" : "black",
            "stop"       : {
                "u": uv.0, "v": uv.1, "px": px,
                "t_start": window.0, "t_end": window.1, "parallax": false,
            },
        }).to_string());

        // A stationary trajectory over the whole window, at the tick rate the passes use.
        let mut t = window.0 - 0.2;

        while t <= window.1 + 0.2 {
            lines.push(json!({
                "kind"       : "traj",
                "display"    : FLAT,
                "phase"      : "grid_black",
                "background" : "black",
                "point"      : { "t_s": t, "px": px, "moving": false },
            }).to_string());

            t += 0.008;
        }

        for frame in frames {
            lines.push(frame.to_string());
        }

        let path = std::env::temp_dir().join(format!("gaze-et5-{name}.jsonl"));
        std::fs::write(&path, lines.join("\n")).expect("write the synthetic session");

        let session = Session::load(&path).expect("the synthetic session loads");
        let _ = std::fs::remove_file(&path);

        session
    }

    #[test]
    fn a_ray_through_the_target_has_no_residual() {
        let geometry = desk();
        let uv       = (0.35, 0.6);

        // Every frame looks exactly at the stop's uv, which on a flat panel declared by
        // its own corners is exactly the target point.
        let frames = (0..20)
            .map(|i| frame(1.0 + i as f64 * 0.011, [0.0, 100.0, 600.0], uv.0, uv.1))
            .collect();

        let session = session_file("exact", &geometry, uv, (1.0, 1.2), frames);
        let rows    = rows(&session, &geometry);

        assert!(!rows.is_empty(), "the stop produced rows");

        for row in &rows {
            assert!(row.residual_deg() < 1e-9,
                    "residual {:.6} deg on a ray through the target", row.residual_deg());
            assert_eq!(row.phase        , "stop");
            assert_eq!(row.session_phase, "grid_black");
            assert_eq!(row.background   , "black");
            assert_eq!(row.hold_key     , "stop_0");
        }

        // The window carries its per-frame rows plus exactly one aggregated row.
        assert_eq!(rows.iter().filter(|r| r.is_mean).count(), 1);
        assert!(rows.last().expect("rows exist").is_mean);
    }

    #[test]
    fn the_lagged_head_features_come_from_300_ms_earlier() {
        let geometry = desk();
        let uv       = (0.5, 0.5);

        // Two clusters 300 ms apart at different head positions, slow enough that the
        // saccade gate keeps both.
        let mut frames = Vec::new();

        for i in 0..8 {
            frames.push(frame(1.0 + i as f64 * 0.011, [0.0, 100.0, 600.0], uv.0, uv.1));
        }

        for i in 0..8 {
            frames.push(frame(1.3 + i as f64 * 0.011, [40.0, 100.0, 600.0], uv.0, uv.1));
        }

        let session = session_file("lagged", &geometry, uv, (1.0, 1.4), frames);
        let rows    = rows(&session, &geometry);

        let early = rows.iter().find(|r| r.t_s < 1.05).expect("an early row");
        let late  = rows.iter().find(|r| (r.t_s - 1.3).abs() < 1e-6).expect("a late row");

        // Nothing exists 300 ms before the first cluster.
        assert!(early.lag_origin_l_mm[0].is_nan());

        // The late row sees the early cluster's head, not its own.
        assert!((late.origin_l_mm[0] - 8.0).abs() < 1e-9);
        assert!((late.lag_origin_l_mm[0] + 32.0).abs() < 1e-9,
                "lagged x {}", late.lag_origin_l_mm[0]);
        assert!((late.lag_inter_mm[0] - 64.0).abs() < 1e-9);
    }

    #[test]
    fn a_saccade_frame_is_rejected() {
        let geometry = desk();
        let uv       = (0.5, 0.5);

        let mut frames = Vec::new();

        for i in 0..10 {
            frames.push(frame(1.0 + i as f64 * 0.011, [0.0, 100.0, 600.0], uv.0, uv.1));
        }

        // One frame far away at the tick rate: on a 237 mm panel at 600 mm, 40% of the
        // width in 11 ms is several hundred degrees per second.
        frames.push(frame(1.0 + 10.0 * 0.011, [0.0, 100.0, 600.0], 0.9, 0.5));

        for i in 11..21 {
            frames.push(frame(1.0 + i as f64 * 0.011, [0.0, 100.0, 600.0], uv.0, uv.1));
        }

        let session = session_file("saccade", &geometry, uv, (1.0, 1.3), frames);
        let rows    = rows(&session, &geometry);

        let t_fast = 1.0 + 10.0 * 0.011;

        assert!(!rows.iter().any(|r| !r.is_mean && (r.t_s - t_fast).abs() < 1e-9),
                "the saccade frame is gated out");

        // The pad window takes its tails with it, so the frames either side go too.
        let kept = rows.iter().filter(|r| !r.is_mean).count();
        assert!(kept < 21, "the saccade pad removed neighbours as well: {kept} kept");
        assert!(kept > 0, "the gate did not eat the whole window");

        // And every surviving row still sits on the target.
        for row in rows.iter().filter(|r| !r.is_mean) {
            assert!(row.residual_deg() < 1e-9);
        }
    }

    #[test]
    fn local_yaw_pitch_matches_the_python_harness() {
        // Reference values from `model/gaze_model/geometry.py::local_yaw_pitch_deg`, run
        // on the same vectors. Both implementations have to agree exactly or the Rust
        // export and the Python baselines are labelling different quantities.
        let cases: [(DVec3, DVec3, f64, f64); 5] = [
            (DVec3::new( 0.0,  0.0,   1.0 ), DVec3::new(0.0, 0.0,  1.0 ),
               0.0          ,   0.0          ),
            (DVec3::new( 1.0,  0.0,   1.0 ), DVec3::new(0.0, 0.0,  1.0 ),
              45.0          ,   0.0          ),
            (DVec3::new( 0.0,  1.0,   1.0 ), DVec3::new(0.0, 0.0,  1.0 ),
               0.0          ,  45.0          ),
            (DVec3::new(-0.2,  0.1,   0.95), DVec3::new(0.1, 0.3,  0.9 ),
             -18.516_302_076, -12.140_345_458),
            (DVec3::new( 0.5, -0.4,  -0.75), DVec3::new(0.0, 0.2, -0.98),
             -37.362_147_825, -39.607_107_589),
        ];

        for (dir, reference, yaw, pitch) in cases {
            let (y, p) = local_yaw_pitch_deg(dir, reference);

            assert!((y - yaw).abs()   < 1e-8, "yaw {y} vs {yaw} for {dir:?}");
            assert!((p - pitch).abs() < 1e-8, "pitch {p} vs {pitch} for {dir:?}");
        }

        // A direction parallel to its reference decomposes to zero whatever the
        // reference is, which is what makes the same function serve as a residual.
        let r = DVec3::new(-0.3, 0.7, -0.6);
        let (y, p) = local_yaw_pitch_deg(r * 2.0, r);

        assert!(y.abs() < 1e-12 && p.abs() < 1e-12);
    }

    #[test]
    fn parallax_holds_become_hold_rows() {
        let geometry = desk();
        let uv       = (0.45, 0.5);
        let frames   = (0..12)
            .map(|i| frame(1.0 + i as f64 * 0.011, [0.0, 100.0, 600.0], uv.0, uv.1))
            .collect::<Vec<_>>();

        let mut session = session_file("hold", &geometry, uv, (1.0, 1.2), frames);
        session.stops[0].stop.parallax = true;

        let rows = rows(&session, &geometry);

        assert!(!rows.is_empty());
        assert!(rows.iter().all(|r| r.phase == "hold"),
                "the head-sweep hold is kept, tagged `hold`, not dropped");
    }

    /// The other panel a click can land on, which is never the meta line's display in
    /// the click test: the point of that test is that the stop's own `display` wins.
    const OTHER: &str = "DP-2";

    /// Writes a passive click session: the meta line on `FLAT` (the tracker's declared
    /// plane), one click on `OTHER`, and the frames around it.
    fn click_session_file(
        geometry : &DesktopGeometry,
        name     : &str,
        uv       : (f64, f64),
        window   : (f64, f64),
        frames   : Vec<serde_json::Value>,
    )
        -> Session
    {
        let other = geometry.outputs.iter().find(|o| o.name == OTHER).expect("the second panel");
        let px    = other.uv_to_px(uv.0, uv.1);

        let meta = SessionMeta {
            kind              : "meta".into(),
            format            : SESSION_FORMAT,
            session_id        : name.into(),
            created_unix_s    : 0.0,
            blob_sha256       : "0".repeat(64),
            blob_bytes        : 0,
            display           : FLAT.into(),
            display_area      : area(geometry),
            desk_sha256       : "0".repeat(64),
            tracker_pitch_deg : 0.0,
            glasses           : false,
            note              : "clicks".into(),
        };

        let mut lines = vec![json!(meta).to_string()];

        lines.push(json!({
            "kind"       : "stop",
            "display"    : OTHER,
            "phase"      : "click",
            "background" : "screen",
            "n"          : 0,
            "stop"       : {
                "u": uv.0, "v": uv.1, "px": px,
                "t_start": window.0, "t_end": window.1, "parallax": false,
            },
        }).to_string());

        lines.push(json!({
            "kind"    : "click",
            "display" : OTHER,
            "phase"   : "click",
            "n"       : 0,
            "click"   : {
                "n"           : 0,
                "button"      : "left",
                "output"      : OTHER,
                "px"          : px,
                "t_press"     : window.1 - 0.1,
                "t_release"   : window.1 - 0.02,
                "moved_px"    : 0.4,
                "multi"       : 1,
                "element"     : {
                    "kind"  : "button",
                    "bbox"  : { "x": px.x - 30.0, "y": px.y - 12.0, "w": 60.0, "h": 24.0 },
                    "text"  : "Save",
                    "score" : 0.87,
                },
                "crop_luma"   : 0.21,
                // A trainer click: labelled without a capture, so no frame age. This
                // is `null` on disk and once failed to parse, dropping the click.
                "frame_age_s" : null,
                "source"      : "trainer",
                "trainer"     : {
                    "task": 3, "step": 1, "hit": true,
                    "posture": "lean-left", "theme": "dark",
                },
            },
        }).to_string());

        // A kind no reader knows. Skipping it rather than failing is what lets the
        // format grow without orphaning every tool that reads it.
        lines.push(json!({ "kind": "future", "display": OTHER, "whatever": 1 }).to_string());

        for frame in frames {
            lines.push(frame.to_string());
        }

        let path = std::env::temp_dir().join(format!("gaze-et5-{name}.jsonl"));
        std::fs::write(&path, lines.join("\n")).expect("write the synthetic session");

        let session = Session::load(&path).expect("the synthetic session loads");
        let _ = std::fs::remove_file(&path);

        session
    }

    #[test]
    fn a_click_session_exports_click_rows_with_their_element_columns() {
        let geometry = desk();
        let uv       = (0.4, 0.6);

        // The frames only have to be valid; where they look is not what this checks.
        let frames = (0..20)
            .map(|i| frame(1.0 + i as f64 * 0.011, [0.0, 100.0, 600.0], 0.5, 0.5))
            .collect();

        let session = click_session_file(&geometry, "clicks", uv, (1.0, 1.2), frames);

        assert_eq!(session.clicks.len(), 1, "the click record loaded");
        assert_eq!(session.stops.len() , 1);
        assert_eq!(session.stops[0].n  , Some(0));
        assert_eq!(session.stops[0].display.as_deref(), Some(OTHER));

        let rows = rows(&session, &geometry);

        assert!(!rows.is_empty(), "the click produced rows");

        for row in &rows {
            assert_eq!(row.session_phase, "click");
            assert_eq!(row.background   , "screen");
            assert_eq!(row.hold_key     , "click_0");
            assert_eq!(row.phase        , "stop");

            assert_eq!(row.element_kind, "button");
            assert!((row.element_w_px - 60.0).abs() < 1e-9);
            assert!((row.element_h_px - 24.0).abs() < 1e-9);
            assert!((row.crop_luma - 0.21).abs() < 1e-9);
            assert_eq!(row.source , "trainer");
            assert_eq!(row.posture, "lean-left");
            assert!((row.trainer_task - 3.0).abs() < 1e-9);
            assert!((row.trainer_hit  - 1.0).abs() < 1e-9);
        }

        // The target came off the stop's own display, not the meta line's. The two
        // panels are far apart, so reading the wrong one is unmissable.
        let other = geometry.outputs.iter().find(|o| o.name == OTHER).expect("the second panel");
        let flat  = geometry.outputs.iter().find(|o| o.name == FLAT).expect("the flat panel");
        let px    = other.uv_to_px(uv.0, uv.1);

        let want = desk_to_sensor(other.px_to_world(px).to_array(), 0.0);
        let wrong = desk_to_sensor(flat.px_to_world(px).to_array(), 0.0);

        for (c, axis) in want.iter().enumerate() {
            assert!((rows[0].target_mm[c] - axis).abs() < 1e-9,
                    "target axis {c}: {} vs {axis}", rows[0].target_mm[c]);
        }

        assert!((0..3).any(|c| (want[c] - wrong[c]).abs() > 1.0),
                "the two panels have to disagree for this test to mean anything");

        // And every row still has one field per column.
        for row in &rows {
            assert_eq!(row.to_csv().split(',').count(), CSV_COLUMNS.len());
        }
    }

    #[test]
    fn a_recorded_session_leaves_the_click_columns_empty() {
        let geometry = desk();
        let uv       = (0.5, 0.5);
        let frames   = (0..10)
            .map(|i| frame(1.0 + i as f64 * 0.011, [0.0, 100.0, 600.0], uv.0, uv.1))
            .collect();

        let session = session_file("noclick", &geometry, uv, (1.0, 1.15), frames);
        let rows    = rows(&session, &geometry);

        assert!(!rows.is_empty());

        for row in &rows {
            assert!(row.element_kind.is_empty());
            assert!(row.element_w_px.is_nan());
            assert!(row.crop_luma.is_nan());
        }
    }

    #[test]
    fn csv_has_one_field_per_column() {
        let geometry = desk();
        let uv       = (0.4, 0.55);
        let frames   = (0..10)
            .map(|i| frame(1.0 + i as f64 * 0.011, [0.0, 100.0, 600.0], uv.0, uv.1))
            .collect();

        let session = session_file("csv", &geometry, uv, (1.0, 1.15), frames);
        let rows    = rows(&session, &geometry);

        assert!(!rows.is_empty());

        for row in &rows {
            assert_eq!(row.to_csv().split(',').count(), CSV_COLUMNS.len());
        }

        // The shared prefix is the Phase C harness's contract: `load_export.py` fails
        // loudly on a missing column, so this pins the count and the two positions the
        // Rust side is most likely to get wrong.
        assert_eq!(CSV_COLUMNS[..SHARED_SCHEMA_COLUMNS].len(), 38);
        assert_eq!(CSV_COLUMNS[0]                            , "session_id");
        assert_eq!(CSV_COLUMNS[1]                            , "group_key");
        assert_eq!(CSV_COLUMNS[SHARED_SCHEMA_COLUMNS - 1]    , "is_mean");

        // `group_key` is the session, never the finer hold key: grouped CV on anything
        // narrower reports an in-session number under a grouped label.
        let line              = rows[0].to_csv();
        let fields: Vec<&str> = line.split(',').collect();

        assert_eq!(fields[1], rows[0].session_id);
        assert_ne!(fields[1], rows[0].hold_key);
    }
}
