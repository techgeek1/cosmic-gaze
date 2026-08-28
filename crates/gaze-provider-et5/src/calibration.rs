//! What a calibration sweep produces and `config/calibration-et5.toml` stores: solved
//! display poses in tracker space and a correction field per display, plus the lag
//! estimate and enough metadata to judge staleness.
//!
//! The on-device eye model is not in this file; it lives in the tracker's flash (and
//! its opaque blob is backed up separately). This file is everything client side:
//! where the panels actually are relative to the tracker, and the residual 2D warp
//! left after the device model and the pose mapping have done their part.
//!
//! Applying a calibration means two things, in order:
//!
//! 1. `apply_poses` rewrites the pose fields of a `DesktopGeometry`'s outputs, so ray
//!    intersection happens against the panels where they really are.
//! 2. `correct_point` maps an intersected pixel through the display's fitted
//!    `FieldMap`, in the display's normalised coordinates.

use std::path::Path;

use serde::{Deserialize, Serialize};
use gaze_core::{DesktopGeometry, GlobalPx, OutputGeometry};

use crate::field::FieldMap;
use crate::ttp::{DisplayArea, DisplayRect};

/// Format version written to the file; bump on breaking changes. Version 2 replaced
/// `device_blob_bytes` (a size, checked at startup) with `device_blob_sha256`: the
/// host now uploads the blob on every connect and verifies it, so the file records
/// which model its client-side fits belong to rather than a weak liveness check.
pub const CALIBRATION_FORMAT: u32 = 2;

/// The oversized virtual plane declared for normal operation and the data passes.
/// The firmware clamps `gaze_point_3d` to the declared display area (measured: gaze
/// at another display pegs to the small panel's bounds), which destroys rays to every
/// other display; a plane much larger than any gaze intersection leaves them
/// unclamped. Only the ray matters downstream, so the distorted 2D normalisation
/// this implies is irrelevant.
pub const VIRTUAL_AREA: DisplayRect = DisplayRect {
    w_mm  : 2400.0,
    h_mm  : 1400.0,
    ox_mm : -1200.0,
    oy_mm : -700.0,
    z_mm  : 0.0,
};

// --- Types ---

/// A display's solved pose in tracker space. Mirrors the pose fields of
/// `OutputGeometry`; shape and logical rect stay with the live desk config.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputPose {
    pub position_mm : [f64; 3],
    pub yaw_deg     : f64,
    pub pitch_deg   : f64,
    pub roll_deg    : f64,
}

/// Cap on the head correction per axis, uv units. A regression gone wrong may not
/// move the point further than this, whatever the head does. Sized from measured
/// holds: a ~250 mm lean legitimately needs ~0.3 uv of correction (the interocular
/// depth channel alone contributes ~0.14), and the previous 0.15 cap truncated it
/// (hold v-rms 0.094 clamped vs 0.084 at 0.30, p95 0.184 vs 0.154).
const HEAD_CORRECTION_MAX_UV: f64 = 0.30;

/// Residual correction for head translation, learned from the data pass's parallax
/// holds. During a hold the target is fixed, so any correlation between the reported
/// gaze and the eye origin is parallax the trained mapping (and the declared plane)
/// failed to compensate. The correction is linear in head offset with gains that
/// vary linearly across the panel: the dominant geometric residual (the declared
/// chord plane versus the true curved surface, and any plane depth error) scales
/// with eccentricity, so a single global gain can only fix the middle of the
/// screen. Deliberately cause-agnostic beyond that shape.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HeadGain {
    /// Head-neutral reference: the mean eye origin over the grid stops, tracker mm.
    pub origin_mm : [f64; 3],
    /// d(u)/d(origin offset) at panel centre, uv per millimetre.
    pub gain_x    : [f64; 3],
    /// d(v)/d(origin offset) at panel centre, uv per millimetre.
    pub gain_y    : [f64; 3],
    /// Change of `gain_x` per unit of `u - 0.5`.
    #[serde(default)]
    pub gain_x_du : [f64; 3],
    /// Change of `gain_x` per unit of `v - 0.5`.
    #[serde(default)]
    pub gain_x_dv : [f64; 3],
    /// Change of `gain_y` per unit of `u - 0.5`.
    #[serde(default)]
    pub gain_y_du : [f64; 3],
    /// Change of `gain_y` per unit of `v - 0.5`.
    #[serde(default)]
    pub gain_y_dv : [f64; 3],
    /// The head-position lag the gains were fitted at, seconds. Zero means
    /// instantaneous (and is what the fit picks once the rotation channel carries
    /// the signal; without it the origin-only fit preferred ~300 ms).
    #[serde(default)]
    pub lag_s     : f64,
    /// Head-rotation reference: mean interocular (dy, dz, |d|) over the grid
    /// stops, millimetres. The interocular vector encodes head roll and yaw plus
    /// IPD foreshortening — the rotation states the origin position cannot see,
    /// and the dominant predictor of the model's head error (cross-validated
    /// R2 0.64 vs 0.27 for lagged origins alone).
    #[serde(default)]
    pub inter_mm   : [f64; 3],
    /// d(u)/d(interocular deviation), uv per millimetre.
    #[serde(default)]
    pub gain_x_rot : [f64; 3],
    /// d(v)/d(interocular deviation), uv per millimetre.
    #[serde(default)]
    pub gain_y_rot : [f64; 3],
    /// d(reported interocular distance)/d(reported depth) over the fit pass,
    /// millimetres per millimetre. The |d| feature is conditioned by this trend
    /// (about the reference depth) before the rotation gains see it: reported IPD
    /// drifts with distance by a session-dependent amount (measured 1.7 to 6.1 mm
    /// per 100 mm across sessions), so raw |d| is collinear with depth inside a
    /// hold and the regression parks the depth weight on whichever channel it
    /// likes — including the unstable |d| proxy, which then misfires under the
    /// next session's drift. Conditioned, depth can only land on the origin
    /// channel and |d| carries pure rotation (yaw foreshortening, roll).
    #[serde(default)]
    pub inter_z_mm_per_mm : f64,
}

// --- HeadGain ---

impl HeadGain {
    /// The panel uv with the residual for the head at `origin_mm` (and, when both
    /// eyes are tracked, the head rotation encoded by `inter_mm`) removed.
    pub fn apply(&self, u: f64, v: f64, origin_mm: [f64; 3], inter_mm: Option<[f64; 3]>)
        -> (f64, f64)
    {
        let d = [
            origin_mm[0] - self.origin_mm[0],
            origin_mm[1] - self.origin_mm[1],
            origin_mm[2] - self.origin_mm[2],
        ];

        let r = {
            match inter_mm {
                Some(i) => [
                    i[0] - self.inter_mm[0],
                    i[1] - self.inter_mm[1],
                    i[2] - self.inter_mm[2]
                        - self.inter_z_mm_per_mm * (origin_mm[2] - self.origin_mm[2]),
                ],
                None    => [0.0; 3],
            }
        };

        let du  = u - 0.5;
        let dv  = v - 0.5;
        let da  = |g: &[f64; 3]| g[0] * d[0] + g[1] * d[1] + g[2] * d[2];
        let ra  = |g: &[f64; 3]| g[0] * r[0] + g[1] * r[1] + g[2] * r[2];

        let cu = (da(&self.gain_x) + ra(&self.gain_x_rot)
            + du * da(&self.gain_x_du) + dv * da(&self.gain_x_dv))
            .clamp(-HEAD_CORRECTION_MAX_UV, HEAD_CORRECTION_MAX_UV);
        let cv = (da(&self.gain_y) + ra(&self.gain_y_rot)
            + du * da(&self.gain_y_du) + dv * da(&self.gain_y_dv))
            .clamp(-HEAD_CORRECTION_MAX_UV, HEAD_CORRECTION_MAX_UV);

        (u - cu, v - cv)
    }
}

/// Everything the sweep learned about one display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputCalibration {
    /// Connector name, matched against `OutputGeometry::name`.
    pub name           : String,
    /// Solved pose in tracker space.
    pub pose           : OutputPose,
    /// Residual correction field in the display's normalised coordinates.
    pub field          : FieldMap,
    /// RMS angular residual of the pose solve, degrees.
    pub pose_rms_deg   : f64,
    /// Leave-one-out RMS of the field fit, normalised units.
    pub field_rms_norm : f64,
    /// Fixation targets that contributed.
    pub targets        : usize,
    /// Head-translation residual correction, when the parallax holds supported one.
    #[serde(default)]
    pub head_gain      : Option<HeadGain>,
}

/// One stop of the post-retrain health check: where the target was, where the
/// firmware said the user was looking, and how far apart those are in degrees.
///
/// Measured immediately after a retrain, on the model the same file's
/// `device_blob_sha256` names, so the numbers and the blob they describe travel
/// together. Nothing consumes them at run time; they exist so that "did this retrain
/// help?" has an answer that is not a memory of how it felt.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct HealthStop {
    /// Target position across the panel, [0, 1].
    pub u         : f64,
    /// Target position down the panel, [0, 1].
    pub v         : f64,
    /// Median reported gaze over the measured window, panel uv.
    pub gaze_u    : f64,
    /// Median reported gaze down the panel.
    pub gaze_v    : f64,
    /// Angle between the target and that median as seen from the nominal eye,
    /// degrees.
    pub error_deg : f64,
    /// Frames the median was taken over.
    pub samples   : usize,
}

/// A complete client-side calibration.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Et5Calibration {
    /// Format version, `CALIBRATION_FORMAT` at write time.
    pub format             : u32,
    /// Unix time the sweep finished.
    pub created_unix_s     : f64,
    /// Estimated tracker-plus-pursuit latency from the glide segments, seconds.
    pub lag_s              : f64,
    /// The display whose plane the on-device model was trained against, when the
    /// sweep ran in direct mode.
    #[serde(default)]
    pub device_output      : Option<String>,
    /// The exact plane declared during that training. The firmware's 2D output is
    /// end-to-end calibrated against this plane; it must be re-declared verbatim at
    /// run time for the trained mapping (rather than the raw ray model) to apply.
    #[serde(default)]
    pub device_area        : Option<DisplayArea>,
    /// SHA-256 (lowercase hex) of the on-device model blob these fits were measured
    /// against, from the backup taken when the model was trained. Every client-side
    /// fit is keyed to one firmware eye model: a retrain orphans the lot. The
    /// provider uploads that same blob on every connect and verifies it, so this is
    /// identity, not a health check.
    #[serde(default)]
    pub device_blob_sha256 : Option<String>,
    /// Per-display results.
    pub outputs            : Vec<OutputCalibration>,
    /// Post-retrain health check: the firmware's own gaze against a grid of known
    /// targets, measured right after the model was committed. Empty when no check
    /// ran. Last in the struct because TOML cannot put a plain value after a table.
    #[serde(default)]
    pub health             : Vec<HealthStop>,
}

// --- Et5Calibration ---

impl Et5Calibration {
    /// Loads a calibration file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CalibrationError> {
        let text = std::fs::read_to_string(path.as_ref())
            .map_err(|e| CalibrationError::Io(e.to_string()))?;
        let cal: Self = toml::from_str(&text)
            .map_err(|e| CalibrationError::Parse(e.to_string()))?;

        if cal.format != CALIBRATION_FORMAT {
            return Err(CalibrationError::Format(cal.format));
        }

        Ok(cal)
    }

    /// Writes the calibration file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), CalibrationError> {
        let text = toml::to_string_pretty(self)
            .map_err(|e| CalibrationError::Parse(e.to_string()))?;

        std::fs::write(path.as_ref(), text).map_err(|e| CalibrationError::Io(e.to_string()))
    }

    /// Current unix time, for `created_unix_s`.
    pub fn now_unix_s() -> f64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    /// The entry for a display, if the sweep covered it.
    pub fn output(&self, name: &str) -> Option<&OutputCalibration> {
        self.outputs.iter().find(|o| o.name == name)
    }

    /// Rewrites the pose fields of every matching output in `geometry` with the
    /// solved poses. Outputs the sweep did not cover keep their configured pose.
    pub fn apply_poses(&self, geometry: &mut DesktopGeometry) {
        for out in &mut geometry.outputs {
            let Some(entry) = self.outputs.iter().find(|o| o.name == out.name) else {
                continue;
            };

            out.position_mm = entry.pose.position_mm;
            out.yaw_deg     = entry.pose.yaw_deg;
            out.pitch_deg   = entry.pose.pitch_deg;
            out.roll_deg    = entry.pose.roll_deg;
        }
    }

    /// Maps an intersected pixel through the display's correction field. Points on
    /// displays the sweep did not cover pass through unchanged; the corrected point
    /// is clamped to the display so a correction can never move it onto a neighbour.
    pub fn correct_point(&self, output: &OutputGeometry, p: GlobalPx) -> GlobalPx {
        let Some(entry) = self.output(&output.name) else {
            return p;
        };

        let (nx, ny) = normalise(output, p);
        let (cx, cy) = entry.field.apply(nx, ny);
        let mapped   = denormalise(output, cx, cy);

        GlobalPx {
            x : mapped.x.clamp(output.logical_x, output.logical_x + output.logical_w),
            y : mapped.y.clamp(output.logical_y, output.logical_y + output.logical_h),
        }
    }
}

// --- Coordinate helpers ---

/// A display's normalised coordinates: `[-1, 1]^2` over the visible area.
pub fn normalise(output: &OutputGeometry, p: GlobalPx) -> (f64, f64) {
    (
        2.0 * (p.x - output.logical_x) / output.logical_w - 1.0,
        2.0 * (p.y - output.logical_y) / output.logical_h - 1.0,
    )
}

/// Inverse of `normalise`.
pub fn denormalise(output: &OutputGeometry, nx: f64, ny: f64) -> GlobalPx {
    GlobalPx {
        x : output.logical_x + (nx + 1.0) * 0.5 * output.logical_w,
        y : output.logical_y + (ny + 1.0) * 0.5 * output.logical_h,
    }
}

// --- Errors ---

/// Calibration file failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum CalibrationError {
    #[error("calibration file io: {0}")]
    Io(String),
    #[error("calibration file parse: {0}")]
    Parse(String),
    #[error("unsupported calibration format {0}")]
    Format(u32),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::field::{FieldMap, FieldRow, fit_best};

    fn panel() -> OutputGeometry {
        OutputGeometry {
            name          : "DP-9".into(),
            enabled       : true,
            logical_x     : 100.0,
            logical_y     : 200.0,
            logical_w     : 1000.0,
            logical_h     : 500.0,
            physical_w_mm : 600.0,
            physical_h_mm : 340.0,
            radius_mm     : 0.0,
            position_mm   : [0.0, 0.0, 0.0],
            yaw_deg       : 0.0,
            pitch_deg     : 0.0,
            roll_deg      : 0.0,
        }
    }

    #[test]
    fn normalise_round_trips() {
        let out = panel();
        let p   = GlobalPx { x: 350.0, y: 640.0 };

        let (nx, ny) = normalise(&out, p);
        let back     = denormalise(&out, nx, ny);

        assert!((back.x - p.x).abs() < 1e-9);
        assert!((back.y - p.y).abs() < 1e-9);
    }

    #[test]
    fn file_round_trips() {
        let rows: Vec<FieldRow> = (0..8)
            .map(|i| {
                let a = i as f64 / 7.0 * 2.0 - 1.0;

                FieldRow { nx: a + 0.05, ny: -a - 0.02, want_nx: a, want_ny: -a }
            })
            .collect();
        let (map, score) = fit_best(&rows);

        let cal = Et5Calibration {
            format             : CALIBRATION_FORMAT,
            created_unix_s     : 1_766_000_000.0,
            lag_s              : 0.12,
            device_output      : None,
            device_area        : None,
            device_blob_sha256 : Some(crate::blob::sha256_hex(b"a blob")),
            outputs            : vec![OutputCalibration {
                name           : "DP-9".into(),
                pose           : OutputPose {
                    position_mm : [-300.0, 170.0, -60.0],
                    yaw_deg     : 20.0,
                    pitch_deg   : -2.0,
                    roll_deg    : 0.1,
                },
                field          : map,
                pose_rms_deg   : 0.4,
                field_rms_norm : score,
                targets        : 8,
                head_gain      : Some(HeadGain {
                    origin_mm         : [0.0, 150.0, 620.0],
                    gain_x            : [1.0e-4, 0.0, -2.0e-4],
                    gain_y            : [0.0, -1.5e-4, 3.0e-4],
                    gain_x_du         : [2.0e-4, 0.0, 0.0],
                    gain_x_dv         : [0.0; 3],
                    gain_y_du         : [0.0; 3],
                    gain_y_dv         : [0.0, 0.0, -1.0e-4],
                    lag_s             : 0.3,
                    inter_mm          : [0.5, -2.0, 65.0],
                    gain_x_rot        : [0.0; 3],
                    gain_y_rot        : [3.0e-3, 0.0, 0.0],
                    inter_z_mm_per_mm : 0.0,
                }),
            }],
            health             : vec![HealthStop {
                u         : 0.5,
                v         : 0.9,
                gaze_u    : 0.51,
                gaze_v    : 0.88,
                error_deg : 0.8,
                samples   : 72,
            }],
        };

        let dir  = std::env::temp_dir().join("gaze-et5-cal-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("calibration.toml");

        cal.save(&path).expect("save");
        let loaded = Et5Calibration::load(&path).expect("load");
        assert_eq!(loaded, cal);
    }

    #[test]
    fn an_older_format_is_refused() {
        // The blob field changed meaning in v2; silently loading a v1 file would
        // apply fits keyed to an eye model nobody can identify any more.
        let dir = std::env::temp_dir().join("gaze-et5-cal-test");
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("old-format.toml");

        std::fs::write(&path, "format = 1\ncreated_unix_s = 0.0\nlag_s = 0.0\n\
                               outputs = []\n").unwrap();

        assert_eq!(Et5Calibration::load(&path), Err(CalibrationError::Format(1)));
    }

    #[test]
    fn poses_apply_by_name() {
        let cal = Et5Calibration {
            format             : CALIBRATION_FORMAT,
            created_unix_s     : 0.0,
            lag_s              : 0.0,
            device_output      : None,
            device_area        : None,
            device_blob_sha256 : None,
            outputs            : vec![OutputCalibration {
                name           : "DP-9".into(),
                pose           : OutputPose {
                    position_mm : [1.0, 2.0, 3.0],
                    yaw_deg     : 4.0,
                    pitch_deg   : 5.0,
                    roll_deg    : 6.0,
                },
                field          : FieldMap::identity(),
                pose_rms_deg   : 0.0,
                field_rms_norm : 0.0,
                targets        : 0,
                head_gain      : None,
            }],
            health             : Vec::new(),
        };

        let mut geometry = DesktopGeometry {
            eye_mm     : [0.0, 180.0, 650.0],
            tracker_mm : [0.0, 0.0, 0.0],
            outputs    : vec![panel()],
            noise      : None,
        };

        cal.apply_poses(&mut geometry);
        assert_eq!(geometry.outputs[0].position_mm, [1.0, 2.0, 3.0]);
        assert_eq!(geometry.outputs[0].yaw_deg, 4.0);
    }

    #[test]
    fn head_gain_subtracts_the_linear_residual() {
        let gain = HeadGain {
            origin_mm : [0.0, 150.0, 620.0],
            gain_x    : [2.0e-4, 0.0, 0.0],
            gain_y    : [0.0, 0.0, -1.0e-4],
            gain_x_du : [2.0e-4, 0.0, 0.0],
            gain_x_dv : [0.0; 3],
            gain_y_du : [0.0; 3],
            gain_y_dv : [0.0; 3],
            lag_s     : 0.0,
            inter_mm          : [0.0, 0.0, 65.0],
            gain_x_rot        : [0.0; 3],
            gain_y_rot        : [0.0, 5.0e-3, 0.0],
            inter_z_mm_per_mm : 0.0,
        };

        // At the reference origin the correction is zero.
        let (u, v) = gain.apply(0.5, 0.5, [0.0, 150.0, 620.0], None);
        assert!((u - 0.5).abs() < 1e-12 && (v - 0.5).abs() < 1e-12);

        // 50 mm right and 40 mm closer: u loses 0.01, v loses 0.004 (the learned
        // residual gain_y[2] * dz = (-1e-4)(-40) is subtracted).
        let (u, v) = gain.apply(0.5, 0.5, [50.0, 150.0, 580.0], None);
        assert!((u - 0.49).abs() < 1e-12, "u {u}");
        assert!((v - 0.496).abs() < 1e-12, "v {v}");

        // Off-centre the gain grows with eccentricity: at u = 0.75 the du term
        // adds 2e-4 * 0.25 * 50 mm = 0.0025 on top of the constant 0.01.
        let (u, _) = gain.apply(0.75, 0.5, [50.0, 150.0, 620.0], None);
        assert!((u - (0.75 - 0.0125)).abs() < 1e-12, "u {u}");

        // Head yaw shows up as interocular dz; the rotation channel corrects it.
        let (_, v) = gain.apply(0.5, 0.5, [0.0, 150.0, 620.0], Some([0.0, 10.0, 65.0]));
        assert!((v - (0.5 - 0.05)).abs() < 1e-12, "v {v}");

        // With the IPD-depth trend set, an |d| change fully predicted by the depth
        // change is invisible to the rotation channel; only the origin channel
        // responds. Untrended, the same inputs also fire the rotation gain.
        let mut conditioned = gain;
        conditioned.gain_y_rot        = [0.0, 0.0, 1.0e-3];
        conditioned.inter_z_mm_per_mm = 0.05;

        let (_, v) = conditioned.apply(0.5, 0.5, [0.0, 150.0, 520.0], Some([0.0, 0.0, 60.0]));
        assert!((v - 0.49).abs() < 1e-12, "v {v}");

        let mut untrended = conditioned;
        untrended.inter_z_mm_per_mm = 0.0;

        let (_, v) = untrended.apply(0.5, 0.5, [0.0, 150.0, 520.0], Some([0.0, 0.0, 60.0]));
        assert!((v - 0.495).abs() < 1e-12, "v {v}");

        // A wild extrapolation is clamped, not amplified.
        let (u, _) = gain.apply(0.5, 0.5, [5000.0, 150.0, 620.0], None);
        assert!((u - (0.5 - 0.30)).abs() < 1e-12, "u {u}");
    }

    #[test]
    fn correction_clamps_to_the_display() {
        // A translation field that pushes points right by half a screen; a corrected
        // point near the right edge must not cross onto a neighbour.
        let mut map = FieldMap::identity();
        map.x_coeffs[0] = 1.0;

        let cal = Et5Calibration {
            format             : CALIBRATION_FORMAT,
            created_unix_s     : 0.0,
            lag_s              : 0.0,
            device_output      : None,
            device_area        : None,
            device_blob_sha256 : None,
            outputs            : vec![OutputCalibration {
                name           : "DP-9".into(),
                pose           : OutputPose {
                    position_mm : [0.0; 3],
                    yaw_deg     : 0.0,
                    pitch_deg   : 0.0,
                    roll_deg    : 0.0,
                },
                field          : map,
                pose_rms_deg   : 0.0,
                field_rms_norm : 0.0,
                targets        : 0,
                head_gain      : None,
            }],
            health             : Vec::new(),
        };

        let out = panel();
        let p   = cal.correct_point(&out, GlobalPx { x: 1050.0, y: 400.0 });
        assert_eq!(p.x, out.logical_x + out.logical_w);
    }
}
