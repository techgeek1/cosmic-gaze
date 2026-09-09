//! The flywheel record (PLAN-ET5 E1): every real click the running session attributes,
//! written down with the features it was attributed on, so the day's clicks can be
//! pooled into the next fit (E3) instead of evaporating into the online offset.
//!
//! One record per click offered to the offset, whatever the offset made of it: the
//! verdict is a column, not a filter, because the rejects are the evidence for a
//! retrain and the adopted jumps are the evidence the gate was wrong.
//!
//! # The file
//!
//! One JSONL file per UTC day under the flywheel directory,
//! `<dir>/<YYYY-MM-DD>.jsonl`, appended to across sessions. Each line is one
//! [`ClickRecord`]; a line whose `format` is not [`FLYWHEEL_FORMAT`] is skipped by
//! [`load`] with a warning rather than failing the file.
//!
//! # The row
//!
//! A record carries the median feature vector of the click's window in
//! [`FEATURE_NAMES`] order and the median residual of the firmware's own ray against
//! the clicked point, which is exactly the label a recorded session's stop row
//! carries. [`ClickRecord::row`] rebuilds the [`Row`] so `train` can take flywheel
//! days beside recorded sessions with no second reader.

use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Deserializer, Serialize, Serializer};
use tracing::warn;

use crate::dataset::Row;
use crate::model::{FEATURE_COUNT, FEATURE_NAMES};

/// Record format written by this version.
pub const FLYWHEEL_FORMAT: u32 = 1;

/// Where the session writes its records unless told otherwise.
pub const DEFAULT_FLYWHEEL_DIR: &str = "config/flywheel";

/// Which channel a click came in on.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ClickVia {
    /// A press on the real mouse.
    Mouse,
    /// A commit from the controller's pad.
    Pad,
}

/// What the offset made of the click.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Verdict {
    /// Inside the gate, folded in.
    Accepted,
    /// Past the gate, held.
    Rejected,
    /// Past the gate, and the consensus of recent rejects: the bias jumped to it.
    Adopted,
}

/// One attributed click.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ClickRecord {
    /// [`FLYWHEEL_FORMAT`] at the time of writing.
    pub format             : u32,
    /// Unix time of the press, seconds.
    pub unix_s             : f64,
    /// Host time of the press on the provider's clock, seconds.
    pub t_s                : f64,
    /// Which channel the click came in on.
    pub via                : ClickVia,
    /// Output the click landed on.
    pub display            : String,
    /// The clicked point, global logical pixels.
    pub px                 : [f64; 2],
    /// The clicked point in tracker millimetres.
    pub target_mm          : [f64; 3],
    /// Corrected rays in the click's window.
    pub rays               : usize,
    /// How far before the press the window reached, seconds.
    pub window_s           : f64,
    /// Body hash of the on-device eye model the rays were produced under.
    pub device_blob_sha256 : Option<String>,
    /// Median over the window of each feature, [`FEATURE_NAMES`] order, NaN where no
    /// frame in the window had it (`null` on the wire).
    #[serde(with = "nan_array")]
    pub features           : [f64; FEATURE_COUNT],
    /// Median yaw and pitch by which the firmware's ray missed the target, degrees:
    /// the residual label.
    #[serde(with = "nan_array")]
    pub residual_deg       : [f64; 2],
    /// Median faded model correction over the window, degrees.
    #[serde(with = "nan_array")]
    pub model_deg          : [f64; 2],
    /// Median model fade over the window, 1 inside the training data.
    #[serde(with = "nan_scalar")]
    pub fade               : f64,
    /// Mean posture origin over the window, tracker millimetres: the key the offset
    /// read its bias at.
    #[serde(with = "nan_array")]
    pub posture_mm         : [f64; 3],
    /// The offset that was applied at that posture, before this click moved it.
    #[serde(with = "nan_array")]
    pub offset_deg         : [f64; 2],
    /// Median leftover after model and offset, degrees: what the offset was given.
    #[serde(with = "nan_array")]
    pub leftover_deg       : [f64; 2],
    /// What the offset made of it.
    pub verdict            : Verdict,
}

// --- ClickRecord ---

impl ClickRecord {
    /// The record as a training row. `session_id` groups a UTC day's clicks (the
    /// honest split for pooled flywheel data is by day), `hold_key` is `click_<n>` for
    /// the caller's `n`, and the row is the one aggregated observation of its window.
    /// Fields a recorded session has and a click does not are NaN or empty, as the
    /// exporter writes them for a stop.
    pub fn row(&self, n: usize) -> Row {
        let f = &self.features;

        Row {
            session_id          : format!("flywheel-{}", civil_date(self.unix_s)),
            hold_key            : format!("click_{n}"),
            background          : "unknown".to_string(),
            phase               : "click".to_string(),
            session_phase       : "flywheel".to_string(),
            t_s                 : self.t_s,
            is_mean             : true,
            origin_l_mm         : [f[0], f[1], f[2]],
            origin_r_mm         : [f[3], f[4], f[5]],
            origin_raw_l_mm     : [f64::NAN; 3],
            origin_raw_r_mm     : [f64::NAN; 3],
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
            target_mm           : self.target_mm,
            residual_yaw_deg    : self.residual_deg[0],
            residual_pitch_deg  : self.residual_deg[1],
            residual_yaw_deg_combined   : f64::NAN,
            residual_pitch_deg_combined : f64::NAN,
            element_kind        : String::new(),
            element_w_px        : f64::NAN,
            element_h_px        : f64::NAN,
            crop_luma           : f64::NAN,
            source              : {
                match self.via {
                    ClickVia::Mouse => "mouse".to_string(),
                    ClickVia::Pad   => "pad".to_string(),
                }
            },
            posture             : String::new(),
            trainer_task        : f64::NAN,
            trainer_hit         : f64::NAN,
        }
    }

    /// The feature by name, for a reader that does not want to count columns.
    pub fn feature(&self, name: &str) -> Option<f64> {
        FEATURE_NAMES.iter().position(|n| *n == name).map(|i| self.features[i])
    }
}

// --- FlywheelLog ---

/// The open flywheel: a directory and the day file currently being appended to.
#[derive(Debug)]
pub struct FlywheelLog {
    dir     : PathBuf,
    /// The day file open now, by its date, so a session that crosses midnight rolls
    /// to the next file on its own.
    day     : Option<(String, File)>,
    /// Records written by this log.
    written : u64,
}

impl FlywheelLog {
    /// Opens the flywheel in `dir`, creating the directory. No file is touched until
    /// the first record.
    pub fn open(dir: impl Into<PathBuf>) -> Result<Self, FlywheelError> {
        let dir = dir.into();

        std::fs::create_dir_all(&dir)
            .map_err(|e| FlywheelError::Io(dir.display().to_string(), e.to_string()))?;

        Ok(Self { dir: dir, day: None, written: 0 })
    }

    /// Appends one record to the day file its `unix_s` falls on.
    pub fn write(&mut self, record: &ClickRecord) -> Result<(), FlywheelError> {
        let date = civil_date(record.unix_s);

        if self.day.as_ref().is_none_or(|(d, _)| *d != date) {
            let path = self.path_for(&date);
            let file = OpenOptions::new()
                .create(true)
                .append(true)
                .open(&path)
                .map_err(|e| FlywheelError::Io(path.display().to_string(), e.to_string()))?;

            self.day = Some((date.clone(), file));
        }

        let line = serde_json::to_string(record)
            .map_err(|e| FlywheelError::Parse(date.clone(), e.to_string()))?;

        let (_, file) = self.day.as_mut().expect("opened above");

        writeln!(file, "{line}")
            .map_err(|e| FlywheelError::Io(self.path_for(&date).display().to_string(), e.to_string()))?;

        self.written += 1;

        Ok(())
    }

    /// The directory records go to.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Records written by this log so far.
    pub fn written(&self) -> u64 {
        self.written
    }

    /// The day file for a date.
    fn path_for(&self, date: &str) -> PathBuf {
        self.dir.join(format!("{date}.jsonl"))
    }
}

// --- Reading ---

/// Reads every record in one day file, skipping lines of another format with a
/// warning. A missing file is an error; an empty one is an empty vector.
pub fn load(path: &Path) -> Result<Vec<ClickRecord>, FlywheelError> {
    let file = File::open(path)
        .map_err(|e| FlywheelError::Io(path.display().to_string(), e.to_string()))?;

    let mut out = Vec::new();

    for (i, line) in BufReader::new(file).lines().enumerate() {
        let line = line.map_err(|e| FlywheelError::Io(path.display().to_string(), e.to_string()))?;

        if line.trim().is_empty() {
            continue;
        }

        match serde_json::from_str::<ClickRecord>(&line) {
            Ok(record) if record.format == FLYWHEEL_FORMAT => out.push(record),
            Ok(record) => {
                warn!(path = %path.display(), line = i + 1, format = record.format,
                      "flywheel record of another format skipped");
            }
            Err(e) => {
                return Err(FlywheelError::Parse(format!("{}:{}", path.display(), i + 1), e.to_string()));
            }
        }
    }

    Ok(out)
}

/// Every day file in a flywheel directory, oldest first. A missing directory is no
/// files.
pub fn day_files(dir: &Path) -> Result<Vec<PathBuf>, FlywheelError> {
    let entries = {
        match std::fs::read_dir(dir) {
            Ok(entries)                                          => entries,
            Err(e) if e.kind() == std::io::ErrorKind::NotFound  => return Ok(Vec::new()),
            Err(e) => return Err(FlywheelError::Io(dir.display().to_string(), e.to_string())),
        }
    };

    let mut files: Vec<PathBuf> = entries
        .filter_map(|e| e.ok().map(|e| e.path()))
        .filter(|p| p.extension().is_some_and(|x| x == "jsonl"))
        .collect();

    files.sort();

    Ok(files)
}

// --- NaN on the wire ---

/// JSON has no NaN, and `serde_json` writes one as `null` but will not read `null`
/// back as an `f64`. These write a non-finite value as `null` and read `null` as NaN,
/// so a record round-trips.
mod nan_array {
    use super::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer, const N: usize>(v: &[f64; N], s: S) -> Result<S::Ok, S::Error> {
        v.iter()
            .map(|x| x.is_finite().then_some(*x))
            .collect::<Vec<Option<f64>>>()
            .serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>, const N: usize>(d: D) -> Result<[f64; N], D::Error> {
        let values = Vec::<Option<f64>>::deserialize(d)?;
        let n      = values.len();

        values.into_iter()
            .map(|x| x.unwrap_or(f64::NAN))
            .collect::<Vec<f64>>()
            .try_into()
            .map_err(|_| serde::de::Error::invalid_length(n, &format!("{N} numbers").as_str()))
    }
}

/// See [`nan_array`].
mod nan_scalar {
    use super::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(v: &f64, s: S) -> Result<S::Ok, S::Error> {
        v.is_finite().then_some(*v).serialize(s)
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<f64, D::Error> {
        Ok(Option::<f64>::deserialize(d)?.unwrap_or(f64::NAN))
    }
}

// --- Dates ---

/// The UTC calendar date of a Unix time, `YYYY-MM-DD`. Days from the civil
/// calendar by Howard Hinnant's algorithm; no timezone, because a file name that
/// changes with the machine's zone is a file that moves.
pub fn civil_date(unix_s: f64) -> String {
    let days = (unix_s / 86_400.0).floor() as i64;

    // Shift the epoch to 0000-03-01 so leap days fall at the end of the year.
    let z   = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp  = (5 * doy + 2) / 153;
    let d   = doy - (153 * mp + 2) / 5 + 1;
    let m   = if mp < 10 { mp + 3 } else { mp - 9 };
    let y   = yoe + era * 400 + i64::from(m <= 2);

    format!("{y:04}-{m:02}-{d:02}")
}

// --- Error ---

/// Flywheel file failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum FlywheelError {
    #[error("{0}: {1}")]
    Io(String, String),
    #[error("{0}: {1}")]
    Parse(String, String),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn record(unix_s: f64, verdict: Verdict) -> ClickRecord {
        let mut features = [f64::NAN; FEATURE_COUNT];
        features[0] = -30.0;
        features[6] = 1.5;
        features[15] = 1.0;

        ClickRecord {
            format             : FLYWHEEL_FORMAT,
            unix_s             : unix_s,
            t_s                : 12.5,
            via                : ClickVia::Mouse,
            display            : "DP-2".to_string(),
            px                 : [100.0, 200.0],
            target_mm          : [10.0, 20.0, 30.0],
            rays               : 12,
            window_s           : 0.4,
            device_blob_sha256 : Some("abc".to_string()),
            features           : features,
            residual_deg       : [0.7, -0.2],
            model_deg          : [0.4, -0.1],
            fade               : 1.0,
            posture_mm         : [0.0, 0.0, 650.0],
            offset_deg         : [0.2, 0.0],
            leftover_deg       : [0.1, -0.1],
            verdict            : verdict,
        }
    }

    #[test]
    fn civil_dates_match_the_calendar() {
        assert_eq!(civil_date(0.0), "1970-01-01");
        assert_eq!(civil_date(86_399.9), "1970-01-01");
        assert_eq!(civil_date(86_400.0), "1970-01-02");
        assert_eq!(civil_date(951_782_400.0), "2000-02-29");
        assert_eq!(civil_date(1_788_926_253.0), "2026-09-09");
        assert_eq!(civil_date(-1.0), "1969-12-31");
    }

    #[test]
    fn records_round_trip_through_day_files_and_roll_at_midnight() {
        let dir = std::env::temp_dir().join(format!("gaze-flywheel-{}", std::process::id()));
        let _   = std::fs::remove_dir_all(&dir);

        let mut log = FlywheelLog::open(&dir).unwrap();
        let day     = 1_788_926_253.0;

        log.write(&record(day, Verdict::Accepted)).unwrap();
        log.write(&record(day + 1.0, Verdict::Rejected)).unwrap();
        log.write(&record(day + 86_400.0, Verdict::Adopted)).unwrap();

        assert_eq!(log.written(), 3);

        let files = day_files(&dir).unwrap();

        assert_eq!(files.len(), 2);
        assert!(files[0].ends_with("2026-09-09.jsonl"), "{files:?}");
        assert!(files[1].ends_with("2026-09-10.jsonl"), "{files:?}");

        let first = load(&files[0]).unwrap();

        assert_eq!(first.len(), 2);
        assert_eq!(first[1].verdict, Verdict::Rejected);

        // Equal in every field; NaN is compared as NaN, which `==` will not do.
        let want = record(day, Verdict::Accepted);
        let same = |a: f64, b: f64| a == b || (a.is_nan() && b.is_nan());

        assert_eq!((first[0].unix_s, first[0].t_s, first[0].via, &first[0].display),
                   (want.unix_s, want.t_s, want.via, &want.display));
        assert_eq!((first[0].px, first[0].target_mm, first[0].rays, first[0].verdict),
                   (want.px, want.target_mm, want.rays, want.verdict));
        assert_eq!(first[0].device_blob_sha256, want.device_blob_sha256);
        assert!(first[0].features.iter().zip(want.features).all(|(a, b)| same(*a, b)));
        assert_eq!((first[0].residual_deg, first[0].model_deg, first[0].offset_deg, first[0].leftover_deg),
                   (want.residual_deg, want.model_deg, want.offset_deg, want.leftover_deg));
        assert_eq!((first[0].fade, first[0].posture_mm), (want.fade, want.posture_mm));

        // NaN features survive as null and come back as NaN.
        assert!(first[0].features[1].is_nan());
        assert_eq!(first[0].feature("origin_l_x_mm"), Some(-30.0));

        // A second log appends rather than truncates.
        let mut again = FlywheelLog::open(&dir).unwrap();
        again.write(&record(day + 2.0, Verdict::Accepted)).unwrap();

        assert_eq!(load(&files[0]).unwrap().len(), 3);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_record_becomes_the_row_the_exporter_would_write() {
        let row = record(1_788_926_253.0, Verdict::Accepted).row(7);

        assert_eq!(row.session_id, "flywheel-2026-09-09");
        assert_eq!(row.hold_key, "click_7");
        assert_eq!(row.phase, "click");
        assert_eq!(row.source, "mouse");
        assert!(row.is_mean);
        assert_eq!(row.origin_l_mm[0], -30.0);
        assert_eq!(row.dir_l_yaw_deg, 1.5);
        assert_eq!(row.valid_l, 1.0);
        assert_eq!(row.residual_yaw_deg, 0.7);
        assert_eq!(row.target_mm, [10.0, 20.0, 30.0]);
        assert!(row.origin_raw_l_mm[0].is_nan());

        // The row's features are the record's, by the runtime's own assembly.
        let back = crate::model::Features::from_row(&row);

        for (i, (a, b)) in back.values.iter().zip(record(0.0, Verdict::Accepted).features).enumerate() {
            assert!(a == &b || (a.is_nan() && b.is_nan()), "feature {i}: {a} vs {b}");
        }
    }

    #[test]
    fn a_missing_directory_has_no_day_files_and_a_missing_file_is_an_error() {
        let dir = std::env::temp_dir().join("gaze-flywheel-does-not-exist");

        assert_eq!(day_files(&dir).unwrap(), Vec::<PathBuf>::new());
        assert!(load(&dir.join("x.jsonl")).is_err());
    }
}
