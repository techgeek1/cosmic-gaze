//! Replays a JSONL log of `GazeSample`s, either at the recorded pace or as fast as
//! possible. Cheap, deterministic input for filter/snap tests and for re-running a
//! captured session without a live device.

use std::fs;
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use gaze_core::{GazeSample, GlobalPx, Ray};
use glam::DVec3;
use serde::{Deserialize, Serialize};

use crate::provider::GazeProvider;

/// JSON-serialisable mirror of `gaze_core::Ray`. `gaze-core` intentionally does not derive
/// `Serialize`/`Deserialize` on `Ray` and this crate does not modify `gaze-core` (see
/// PLAN.md's agent working rules), so replay files store this local shape and convert.
#[derive(Clone, Copy, Debug, Serialize, Deserialize)]
struct RayRecord {
    origin : [f64; 3],
    dir    : [f64; 3],
}

impl From<Ray> for RayRecord {
    fn from(ray: Ray) -> Self {
        Self { origin: ray.origin.to_array(), dir: ray.dir.to_array() }
    }
}

impl From<RayRecord> for Ray {
    fn from(record: RayRecord) -> Self {
        Self { origin: DVec3::from_array(record.origin), dir: DVec3::from_array(record.dir) }
    }
}

/// JSON-serialisable mirror of `gaze_core::GazeSample`, one per line of a replay file.
#[derive(Clone, Debug, Serialize, Deserialize)]
struct SampleRecord {
    t_s       : f64,
    ray       : Option<RayRecord>,
    point     : Option<GlobalPx>,
    sigma_deg : f64,
    valid     : bool,
}

impl From<GazeSample> for SampleRecord {
    fn from(sample: GazeSample) -> Self {
        Self {
            t_s       : sample.t_s,
            ray       : sample.ray.map(RayRecord::from),
            point     : sample.point,
            sigma_deg : sample.sigma_deg,
            valid     : sample.valid,
        }
    }
}

impl From<SampleRecord> for GazeSample {
    fn from(record: SampleRecord) -> Self {
        Self {
            t_s       : record.t_s,
            ray       : record.ray.map(Ray::from),
            point     : record.point,
            sigma_deg : record.sigma_deg,
            valid     : record.valid,
        }
    }
}

/// Serialises a `GazeSample` to a single JSONL line (no trailing newline). Used by
/// `gaze-provider-cli --record` and by tests that build fixture replay files.
pub fn to_jsonl_line(sample: &GazeSample) -> serde_json::Result<String> {
    serde_json::to_string(&SampleRecord::from(*sample))
}

/// A `GazeProvider` that replays a recorded JSONL log instead of reading a live device.
#[derive(Debug)]
pub struct ReplayProvider {
    samples  : Vec<GazeSample>,
    index    : usize,
    /// When true (the default), `next` sleeps to reproduce the recorded pacing between
    /// samples. When false, samples come back to back as fast as the caller drains them.
    realtime : bool,
    /// `t_s` of the first sample, subtracted from every sample's `t_s` to get a pacing
    /// delay relative to `start`. Recordings need not start at `t_s == 0`.
    t0_s     : f64,
    start    : Instant,
    stopped  : bool,
}

// --- ReplayProvider ---

impl ReplayProvider {
    /// Loads a replay log, one JSON-encoded `GazeSample` per line. Blank lines are
    /// skipped; a malformed line is an error naming its 1-based line number. Defaults to
    /// realtime pacing; call `.realtime(false)` to replay as fast as possible.
    pub fn from_jsonl(path: impl AsRef<Path>) -> Result<Self, ReplayError> {
        __from_jsonl(path.as_ref())
    }

    /// Sets whether `next` reproduces the recorded pacing (`true`, the default) or
    /// returns samples back to back as fast as possible (`false`).
    pub fn realtime(mut self, enabled: bool) -> Self {
        self.realtime = enabled;
        self
    }
}

impl GazeProvider for ReplayProvider {
    fn next(&mut self) -> Option<GazeSample> {
        if self.stopped || self.index >= self.samples.len() {
            return None;
        }

        let sample = self.samples[self.index];
        self.index += 1;

        if self.realtime {
            let target  = Duration::from_secs_f64((sample.t_s - self.t0_s).max(0.0));
            let elapsed = self.start.elapsed();

            if target > elapsed {
                thread::sleep(target - elapsed);
            }
        }

        Some(sample)
    }

    fn try_next(&mut self) -> Option<GazeSample> {
        if self.stopped || self.index >= self.samples.len() {
            return None;
        }

        if self.realtime {
            let target = Duration::from_secs_f64((self.samples[self.index].t_s - self.t0_s).max(0.0));

            if self.start.elapsed() < target {
                return None;
            }
        }

        let sample = self.samples[self.index];
        self.index += 1;

        Some(sample)
    }

    fn stop(&mut self) {
        // No thread or device to release; just make every future call report stopped.
        self.stopped = true;
    }
}

/// Non-generic implementation behind `ReplayProvider::from_jsonl`.
fn __from_jsonl(path: &Path) -> Result<ReplayProvider, ReplayError> {
    let text = fs::read_to_string(path)
        .map_err(|source| ReplayError::Io { path: path.to_path_buf(), source })?;

    let mut samples = Vec::new();

    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        let record: SampleRecord = serde_json::from_str(line)
            .map_err(|source| ReplayError::Parse { path: path.to_path_buf(), line: i + 1, source })?;

        samples.push(GazeSample::from(record));
    }

    let t0_s = samples.first().map_or(0.0, |s| s.t_s);

    Ok(ReplayProvider { samples, index: 0, realtime: true, t0_s, start: Instant::now(), stopped: false })
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum ReplayError {
    #[error("cannot read replay file {}: {source}", path.display())]
    Io { path: PathBuf, #[source] source: std::io::Error },

    #[error("replay file {} line {line}: {source}", path.display())]
    Parse { path: PathBuf, line: usize, #[source] source: serde_json::Error },
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use std::io::Write;

    use glam::DVec3;

    use super::*;

    fn sample(t_s: f64, valid: bool) -> GazeSample {
        if valid {
            GazeSample {
                t_s       : t_s,
                ray       : Some(Ray { origin: DVec3::ZERO, dir: DVec3::new(0.0, 0.0, 1.0) }),
                point     : Some(GlobalPx { x: 100.0, y: 200.0 }),
                sigma_deg : 0.7,
                valid     : true,
            }
        }
        else {
            GazeSample { t_s: t_s, ray: None, point: None, sigma_deg: f64::MAX, valid: false }
        }
    }

    /// Writes `samples` as JSONL, with a blank line inserted between entries to prove
    /// blank lines are tolerated.
    fn write_jsonl(path: &Path, samples: &[GazeSample]) {
        let mut file = fs::File::create(path).unwrap();

        for sample in samples {
            writeln!(file, "{}", to_jsonl_line(sample).unwrap()).unwrap();
            writeln!(file).unwrap();
        }
    }

    #[test]
    fn round_trips_valid_and_invalid_samples_through_jsonl() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");
        let original = vec![sample(0.0, true), sample(0.1, false), sample(0.2, true)];

        write_jsonl(&path, &original);

        let mut provider = ReplayProvider::from_jsonl(&path).unwrap().realtime(false);
        let mut replayed = Vec::new();

        while let Some(s) = provider.next() {
            replayed.push(s);
        }

        assert_eq!(replayed, original);
    }

    #[test]
    fn next_returns_none_once_exhausted_and_stays_stopped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        write_jsonl(&path, &[sample(0.0, true)]);

        let mut provider = ReplayProvider::from_jsonl(&path).unwrap().realtime(false);

        assert!(provider.next().is_some());
        assert!(provider.next().is_none());
        assert!(provider.next().is_none());
    }

    #[test]
    fn stop_makes_next_and_try_next_report_stopped_even_with_samples_left() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        write_jsonl(&path, &[sample(0.0, true), sample(0.1, true)]);

        let mut provider = ReplayProvider::from_jsonl(&path).unwrap().realtime(false);
        provider.stop();

        assert!(provider.next().is_none());
        assert!(provider.try_next().is_none());
    }

    #[test]
    fn non_realtime_mode_does_not_sleep_for_the_recorded_pacing() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        // Recorded 2 seconds apart; non-realtime replay must not wait for that gap.
        write_jsonl(&path, &[sample(0.0, true), sample(2.0, true)]);

        let mut provider = ReplayProvider::from_jsonl(&path).unwrap().realtime(false);
        let start = std::time::Instant::now();

        provider.next();
        provider.next();

        assert!(start.elapsed() < Duration::from_millis(200));
    }

    #[test]
    fn realtime_try_next_withholds_a_sample_until_its_recorded_time() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        write_jsonl(&path, &[sample(0.0, true), sample(0.15, true)]);

        // Default is realtime; take the first sample immediately, then poll for the
        // second before and after its recorded offset has elapsed.
        let mut provider = ReplayProvider::from_jsonl(&path).unwrap();

        assert!(provider.next().is_some());
        assert!(provider.try_next().is_none(), "second sample should not be ready yet");

        thread::sleep(Duration::from_millis(200));

        assert!(provider.try_next().is_some(), "second sample should be ready by now");
    }

    #[test]
    fn blank_lines_are_skipped() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        fs::write(&path, "\n\n").unwrap();

        let mut provider = ReplayProvider::from_jsonl(&path).unwrap().realtime(false);

        assert!(provider.next().is_none());
    }

    #[test]
    fn a_malformed_line_reports_its_one_based_line_number() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("session.jsonl");

        let good = to_jsonl_line(&sample(0.0, true)).unwrap();
        fs::write(&path, format!("{good}\nnot json\n")).unwrap();

        let err = ReplayProvider::from_jsonl(&path).unwrap_err();

        assert!(matches!(err, ReplayError::Parse { line: 2, .. }));
    }

    #[test]
    fn a_missing_file_is_an_io_error() {
        let err = ReplayProvider::from_jsonl("/nonexistent/definitely-not-a-real-path.jsonl").unwrap_err();

        assert!(matches!(err, ReplayError::Io { .. }));
    }
}
