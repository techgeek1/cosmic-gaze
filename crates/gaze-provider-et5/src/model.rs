//! The state-conditioned residual model (PLAN-ET5 D1): what the firmware's ray is wrong
//! by, as a function of the device's own state, and how sure the model is of that.
//!
//! The model is the Phase C prototype ported verbatim: a sparse (Nystrom) kernel ridge
//! regression with an ARD RBF kernel over standardised features, plus a few explicit
//! linear terms (angle from the tracker axis, the two pupil diameters) that are summed
//! alongside the kernel rather than left for it to discover. The fit lives in
//! [`crate::train`]; this module is the part that runs on every frame, and the file
//! format that carries a fitted model between the two.
//!
//! Everything a prediction needs is folded at fit time so a frame costs one kernel
//! evaluation per centre and one small quadratic form: the whitening matrix is folded
//! into the kernel weights, and the variance solve into one symmetric matrix. Predictive
//! variance is the subset-of-regressors posterior, in the residual's own units, and it
//! is what fades the correction to zero away from the training data: the snap engine
//! already consumes the sigma it widens.

use std::path::Path;

use glam::DVec3;
use serde::{Deserialize, Serialize};

use crate::dataset::{Head, Row, lagged_head, local_yaw_pitch_deg};
use crate::gaze::Et5Frame;

/// Format version written to the file; bump on breaking changes.
pub const MODEL_FORMAT: u32 = 1;

/// Conventional path of the fitted model.
pub const DEFAULT_MODEL_PATH: &str = "config/model-et5.json";

/// Every feature the exporter produces, in the column order the Phase C harness's
/// `features.py` called `FEATURE_COLS` (the harness is gone; the order stands). A fitted model names the subset it uses, so the
/// runtime and the fit agree on columns by name rather than by position.
pub const FEATURE_NAMES: [&str; FEATURE_COUNT] = [
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
];

/// How many features there are.
pub const FEATURE_COUNT: usize = 27;

/// Newton steps for [`correct_direction`]. The map from tangent offsets to the label's
/// two angles is smooth and nearly linear at the few degrees a residual spans, so
/// the iteration converges well inside this many.
const CORRECT_STEPS: usize = 6;

// --- Features ---

/// One frame's feature vector, in [`FEATURE_NAMES`] order. Missing is NaN, exactly as
/// the exporter writes it; the model imputes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Features {
    pub values : [f64; FEATURE_COUNT],
}

// --- Features ---

impl Features {
    /// The feature vector of an exported row.
    pub fn from_row(row: &Row) -> Self {
        let mut v = [f64::NAN; FEATURE_COUNT];

        v[0..3].copy_from_slice(&row.origin_l_mm);
        v[3..6].copy_from_slice(&row.origin_r_mm);
        v[6]  = row.dir_l_yaw_deg;
        v[7]  = row.dir_l_pitch_deg;
        v[8]  = row.dir_r_yaw_deg;
        v[9]  = row.dir_r_pitch_deg;
        v[10..13].copy_from_slice(&row.inter_mm);
        v[13] = row.pupil_l_mm;
        v[14] = row.pupil_r_mm;
        v[15] = row.valid_l;
        v[16] = row.valid_r;
        v[17] = row.angle_axis_deg;
        v[18..21].copy_from_slice(&row.lag_origin_l_mm);
        v[21..24].copy_from_slice(&row.lag_origin_r_mm);
        v[24..27].copy_from_slice(&row.lag_inter_mm);

        Self { values: v }
    }

    /// The feature vector of a live frame. `lagged` is the head state 300 ms ago (or
    /// the default, all missing, when there was none), `axis` the tracker axis in the
    /// frame the device reports in, and `dir` the firmware's combined ray direction.
    /// This is the one assembly the exporter also uses, so a row and a frame cannot
    /// disagree about what a feature is.
    pub fn of_frame(frame: &Et5Frame, lagged: &Head, axis: DVec3, dir: DVec3) -> Self {
        let head = Head::of(frame);

        let (dir_l_yaw_deg, dir_l_pitch_deg) = eye_angles(
            frame.left_valid(), frame.eye_origin_l_mm, frame.gaze_3d_l_mm, axis,
        );
        let (dir_r_yaw_deg, dir_r_pitch_deg) = eye_angles(
            frame.right_valid(), frame.eye_origin_r_mm, frame.gaze_3d_r_mm, axis,
        );

        let mut v = [f64::NAN; FEATURE_COUNT];

        v[0..3].copy_from_slice(&or_nan(head.cal_l));
        v[3..6].copy_from_slice(&or_nan(head.cal_r));
        v[6]  = dir_l_yaw_deg;
        v[7]  = dir_l_pitch_deg;
        v[8]  = dir_r_yaw_deg;
        v[9]  = dir_r_pitch_deg;
        v[10..13].copy_from_slice(&or_nan(head.inter));
        v[13] = pupil(frame.left_valid(), frame.pupil_l_mm);
        v[14] = pupil(frame.right_valid(), frame.pupil_r_mm);
        v[15] = validity(frame.validity_l);
        v[16] = validity(frame.validity_r);
        v[17] = (-dir).angle_between(axis).to_degrees();
        v[18..21].copy_from_slice(&or_nan(lagged.cal_l));
        v[21..24].copy_from_slice(&or_nan(lagged.cal_r));
        v[24..27].copy_from_slice(&or_nan(lagged.inter));

        Self { values: v }
    }

    /// The value of a feature by name, or `None` for a name that is not a feature.
    pub fn get(&self, name: &str) -> Option<f64> {
        FEATURE_NAMES.iter().position(|n| *n == name).map(|i| self.values[i])
    }
}

/// A vector, or NaNs when it was not reported.
pub(crate) fn or_nan(v: Option<DVec3>) -> [f64; 3] {
    v.map(|v| v.to_array()).unwrap_or([f64::NAN; 3])
}

/// A pupil diameter, or NaN when the eye was not tracked. The device reports -1 for an
/// untracked eye, which would otherwise look like a measurement.
pub(crate) fn pupil(valid: bool, mm: Option<f64>) -> f64 {
    match mm {
        Some(v) if valid && v > 0.0 => v,
        _                           => f64::NAN,
    }
}

/// A validity flag as 1, 0, or NaN when the frame did not carry one.
pub(crate) fn validity(v: Option<u32>) -> f64 {
    match v {
        Some(v) => f64::from(u8::from(v == crate::gaze::VALIDITY_OK)),
        None    => f64::NAN,
    }
}

/// One eye's gaze direction against the tracker axis, or NaNs when the eye was not
/// tracked.
///
/// The axis points tracker to eye and a gaze ray points eye to screen, so they face
/// opposite ways by construction: the direction is negated first, the same negation
/// `DesktopGeometry::off_axis_deg` applies, so a gaze near the tracker reads as a small
/// yaw and pitch rather than as a 180 degree wraparound.
pub(crate) fn eye_angles(
    valid  : bool,
    origin : Option<[f64; 3]>,
    target : Option<[f64; 3]>,
    axis   : DVec3,
)
    -> (f64, f64)
{
    let missing = (f64::NAN, f64::NAN);

    if !valid {
        return missing;
    }

    let (Some(o), Some(t)) = (origin, target) else {
        return missing;
    };

    let delta = DVec3::from_array(t) - DVec3::from_array(o);

    if delta.length_squared() < 1.0 {
        return missing;
    }

    local_yaw_pitch_deg(-delta, axis)
}

// --- Model ---

/// A fitted residual model, as written to `config/model-et5.json`.
///
/// Prediction is `sum_i k(x, c_i) w_i + sum_e z_e l_e` over standardised kernel
/// features `x`, the centres `c_i`, and the raw explicit features `z_e`; variance is
/// `[k z] V [k z]^T`. The whitening matrix and the ridge are folded into `w` and `V`
/// at fit time, so a file is what a frame needs and nothing else.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ResidualModel {
    /// Format version, `MODEL_FORMAT` at write time.
    pub format             : u32,
    /// Unix time the fit finished.
    pub created_unix_s     : f64,
    /// Body hash of the on-device model the training sessions ran under. The residual
    /// is the firmware's, so a retrain orphans this file; the provider refuses a
    /// model whose hash is not the blob it uploads.
    pub device_blob_sha256 : Option<String>,
    /// The sensor-frame pitch the training sessions' targets were expressed under,
    /// degrees. The runtime builds its tracker axis and intersects its corrected ray
    /// with the same rotation, so a label and a prediction share one frame.
    pub tracker_pitch_deg  : f64,
    /// Kernel feature names, a subset of [`FEATURE_NAMES`], in column order.
    pub features           : Vec<String>,
    /// Explicit linear feature names. Raw values, NaN read as zero.
    pub explicit           : Vec<String>,
    /// Training median per kernel column, substituted for a missing value.
    pub impute             : Vec<f64>,
    /// Training mean per kernel column.
    pub mean               : Vec<f64>,
    /// Training standard deviation per kernel column (one where it was zero).
    pub std                : Vec<f64>,
    /// ARD length scale per standardised kernel column.
    pub length_scale       : Vec<f64>,
    /// Inducing points, standardised, one row per centre.
    pub centers            : Vec<Vec<f64>>,
    /// Per-centre yaw and pitch weights, whitening folded in.
    pub kernel_weights     : Vec<[f64; 2]>,
    /// Per-explicit-feature yaw and pitch weights.
    pub explicit_weights   : Vec<[f64; 2]>,
    /// The variance form over `[k z]`, `(M + E)` square, ridge and whitening folded.
    pub variance           : Vec<Vec<f64>>,
    /// Predictive variance at or below which the correction applies in full.
    pub var_fade_lo        : f64,
    /// Predictive variance at or above which the correction is faded out entirely.
    pub var_fade_hi        : f64,
    /// What the fit measured about itself, for the record.
    #[serde(default)]
    pub report             : Option<FitReport>,
}

/// One frame's prediction.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Prediction {
    /// The residual the model expects the firmware's ray to carry, degrees: where
    /// the ray is relative to where the eye is, in the tangent frame at the truth.
    pub yaw_deg   : f64,
    pub pitch_deg : f64,
    /// Predictive variance, square degrees, shared by both axes.
    pub var_deg2  : f64,
    /// How much of the correction to apply, 1 inside the training data and 0 far
    /// from it, from the variance against the model's fade thresholds.
    pub fade      : f64,
}

/// What a fit measured about itself: the honest number is leave-one-session-out.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FitReport {
    /// Sessions the model was fitted on.
    pub sessions         : Vec<String>,
    /// Training rows after selection.
    pub rows             : usize,
    /// Distinct clicks (holds) among them.
    pub clicks           : usize,
    /// Median per-click firmware residual, degrees, over every session held out.
    pub firmware_p50_deg : f64,
    /// Median per-click residual after the held-out model's correction.
    pub model_p50_deg    : f64,
    /// 90th percentile per-click firmware residual.
    pub firmware_p90_deg : f64,
    /// 90th percentile per-click residual after correction.
    pub model_p90_deg    : f64,
    /// Per-session medians, firmware then model, in `sessions` order.
    pub per_session_p50  : Vec<[f64; 2]>,
}

// --- ResidualModel ---

impl ResidualModel {
    /// Loads a model file.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, ModelError> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path)
            .map_err(|e| ModelError::Io(path.display().to_string(), e.to_string()))?;
        let model: Self = serde_json::from_str(&text)
            .map_err(|e| ModelError::Parse(path.display().to_string(), e.to_string()))?;

        if model.format != MODEL_FORMAT {
            return Err(ModelError::Format(model.format));
        }

        model.check()?;

        Ok(model)
    }

    /// Writes the model file.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), ModelError> {
        let path = path.as_ref();
        let text = serde_json::to_string(self)
            .map_err(|e| ModelError::Parse(path.display().to_string(), e.to_string()))?;

        std::fs::write(path, text)
            .map_err(|e| ModelError::Io(path.display().to_string(), e.to_string()))
    }

    /// Number of inducing points.
    pub fn centers(&self) -> usize {
        self.centers.len()
    }

    /// The model's prediction for one feature vector.
    pub fn predict(&self, features: &Features) -> Prediction {
        let m = self.centers.len();
        let e = self.explicit.len();

        // Standardise the kernel columns, imputing what is missing.
        let x: Vec<f64> = self.features.iter().enumerate()
            .map(|(j, name)| {
                let raw = features.get(name).unwrap_or(f64::NAN);
                let v   = if raw.is_finite() { raw } else { self.impute[j] };

                (v - self.mean[j]) / self.std[j]
            })
            .collect();

        // The design row: one kernel value per centre, then the explicit columns.
        let mut d = Vec::with_capacity(m + e);

        for c in &self.centers {
            let s: f64 = x.iter().zip(c).zip(&self.length_scale)
                .map(|((xj, cj), lj)| {
                    let t = (xj - cj) / lj;

                    t * t
                })
                .sum();

            d.push((-0.5 * s).exp());
        }

        for name in &self.explicit {
            let raw = features.get(name).unwrap_or(f64::NAN);

            d.push(if raw.is_finite() { raw } else { 0.0 });
        }

        let mut yaw   = 0.0;
        let mut pitch = 0.0;

        for (k, w) in d[..m].iter().zip(&self.kernel_weights) {
            yaw   += k * w[0];
            pitch += k * w[1];
        }

        for (z, w) in d[m..].iter().zip(&self.explicit_weights) {
            yaw   += z * w[0];
            pitch += z * w[1];
        }

        let var = quadratic_form(&self.variance, &d).max(0.0);

        Prediction {
            yaw_deg   : yaw,
            pitch_deg : pitch,
            var_deg2  : var,
            fade      : self.fade(var),
        }
    }

    /// The fade weight for a predictive variance: 1 at or below the low threshold, 0
    /// at or above the high one, linear between.
    pub fn fade(&self, var: f64) -> f64 {
        if self.var_fade_hi <= self.var_fade_lo {
            return f64::from(u8::from(var <= self.var_fade_lo));
        }

        ((self.var_fade_hi - var) / (self.var_fade_hi - self.var_fade_lo)).clamp(0.0, 1.0)
    }

    /// Structural checks on a loaded file, so a truncated or hand-edited model fails
    /// at load rather than as a panic on the first frame.
    fn check(&self) -> Result<(), ModelError> {
        let d = self.features.len();
        let m = self.centers.len();
        let e = self.explicit.len();

        let bad = |what: &str| Err(ModelError::Shape(what.to_string()));

        for name in self.features.iter().chain(&self.explicit) {
            if !FEATURE_NAMES.contains(&name.as_str()) {
                return Err(ModelError::Feature(name.clone()));
            }
        }

        if self.impute.len() != d || self.mean.len() != d || self.std.len() != d
            || self.length_scale.len() != d
        {
            return bad("per-column vectors do not match the feature count");
        }

        if self.centers.iter().any(|c| c.len() != d) {
            return bad("a centre does not have one value per feature");
        }

        if self.kernel_weights.len() != m || self.explicit_weights.len() != e {
            return bad("weights do not match the centre and explicit counts");
        }

        if self.variance.len() != m + e || self.variance.iter().any(|r| r.len() != m + e) {
            return bad("the variance form is not (centres + explicit) square");
        }

        let positive = |v: &f64| v.is_finite() && *v > 0.0;

        if !self.std.iter().all(positive) || !self.length_scale.iter().all(positive) {
            return bad("a standard deviation or length scale is not positive");
        }

        Ok(())
    }
}

/// `d^T a d` for a square `a`.
fn quadratic_form(a: &[Vec<f64>], d: &[f64]) -> f64 {
    a.iter().zip(d)
        .map(|(row, di)| di * row.iter().zip(d).map(|(aij, dj)| aij * dj).sum::<f64>())
        .sum()
}

// --- Ray correction ---

/// The direction the eye was actually looking, given the firmware's direction and
/// the residual it carries: the `want` for which `local_yaw_pitch_deg(dir, want)`
/// returns `(yaw_deg, pitch_deg)`. The inverse of the label, so applying a perfect
/// prediction lands the ray on the target it was measured against.
pub fn correct_direction(dir: DVec3, yaw_deg: f64, pitch_deg: f64) -> DVec3 {
    let dir = dir.normalize();

    if !(yaw_deg.is_finite() && pitch_deg.is_finite()) {
        return dir;
    }

    // Parametrise the answer as tangent offsets at `dir` and Newton-step on the two
    // angles that read back. The label's frame is built at `want`, not at `dir`, so a
    // plain fixed point on the offsets drifts; a finite-difference Jacobian makes the
    // step exact to second order and it converges in two or three iterations.
    let (right, up) = tangent_frame(dir);

    let read = |a: f64, b: f64| -> (f64, f64) {
        local_yaw_pitch_deg(dir, (dir + right * a + up * b).normalize())
    };

    let mut a = -yaw_deg.to_radians().tan();
    let mut b = -pitch_deg.to_radians().tan();

    for _ in 0..CORRECT_STEPS {
        let (y0, p0) = read(a, b);
        let ey = y0 - yaw_deg;
        let ep = p0 - pitch_deg;

        if ey.abs() < 1e-9 && ep.abs() < 1e-9 {
            break;
        }

        let h = 1e-4;
        let (ya, pa) = read(a + h, b);
        let (yb, pb) = read(a, b + h);

        let j = [[(ya - y0) / h, (yb - y0) / h], [(pa - p0) / h, (pb - p0) / h]];
        let det = j[0][0] * j[1][1] - j[0][1] * j[1][0];

        if det.abs() < 1e-12 {
            break;
        }

        a -= ( j[1][1] * ey - j[0][1] * ep) / det;
        b -= (-j[1][0] * ey + j[0][0] * ep) / det;
    }

    (dir + right * a + up * b).normalize()
}

/// The `right` and `local up` axes `local_yaw_pitch_deg` builds at a reference
/// direction, so the correction is expressed in the frame the label was measured in.
fn tangent_frame(reference: DVec3) -> (DVec3, DVec3) {
    let cross = DVec3::Y.cross(reference);

    let right = {
        if cross.length() < 1e-9 {
            DVec3::X
        }
        else {
            cross.normalize()
        }
    };

    (right, reference.cross(right))
}

// --- HeadHistory ---

/// The recent head states, for the 300 ms lagged features at run time. The exporter
/// looks the same lag up in a whole session; this is the streaming equivalent.
#[derive(Debug, Default)]
pub struct HeadHistory {
    /// Oldest first. A second of frames at most, so a plain vector drained from the
    /// front stays cheap.
    entries : Vec<(f64, Head)>,
}

// --- HeadHistory ---

impl HeadHistory {
    /// Records the head state of a frame at host time `t_s`.
    pub fn push(&mut self, t_s: f64, frame: &Et5Frame) {
        self.entries.push((t_s, Head::of(frame)));

        let stale = self.entries.iter().take_while(|(t, _)| t_s - t > 1.0).count();

        if stale > 0 {
            self.entries.drain(..stale);
        }
    }

    /// The head state 300 ms before `t_s`, by the exporter's rule.
    pub fn lagged(&self, t_s: f64) -> Head {
        lagged_head(&self.entries, t_s)
    }

    /// Forgets everything, for a link that went away.
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

// --- Error ---

/// Model file failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum ModelError {
    #[error("{0}: {1}")]
    Io(String, String),
    #[error("{0}: {1}")]
    Parse(String, String),
    #[error("unsupported model format {0}")]
    Format(u32),
    #[error("unknown feature {0}")]
    Feature(String),
    #[error("malformed model: {0}")]
    Shape(String),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn correcting_by_the_label_inverts_the_label() {
        let dirs = [
            DVec3::new(0.1, -0.3, -1.0),
            DVec3::new(-0.4, 0.2, -1.0),
            DVec3::new(0.0, 0.0, -1.0),
            DVec3::new(0.6, 0.5, -0.8),
        ];

        for dir in dirs {
            for (yaw, pitch) in [(2.0, -1.0), (-4.5, 3.2), (0.0, 0.0), (9.0, 9.0), (-0.3, 0.1)] {
                let want   = correct_direction(dir, yaw, pitch);
                let (y, p) = local_yaw_pitch_deg(dir, want);

                assert!((y - yaw).abs() < 1e-6 && (p - pitch).abs() < 1e-6,
                        "dir {dir:?} label ({yaw}, {pitch}) read back ({y}, {p})");
                assert!((want.length() - 1.0).abs() < 1e-12);
            }
        }
    }

    #[test]
    fn a_zero_residual_leaves_the_direction_alone() {
        let dir = DVec3::new(0.2, 0.1, -1.0).normalize();

        assert!(correct_direction(dir, 0.0, 0.0).abs_diff_eq(dir, 1e-12));
        assert!(correct_direction(dir, f64::NAN, 1.0).abs_diff_eq(dir, 1e-12));
    }

    #[test]
    fn the_fade_runs_from_one_to_zero_between_the_thresholds() {
        let mut model = tiny_model();
        model.var_fade_lo = 1.0;
        model.var_fade_hi = 3.0;

        assert_eq!(model.fade(0.5), 1.0);
        assert_eq!(model.fade(1.0), 1.0);
        assert!((model.fade(2.0) - 0.5).abs() < 1e-12);
        assert_eq!(model.fade(3.0), 0.0);
        assert_eq!(model.fade(9.0), 0.0);
    }

    #[test]
    fn prediction_is_the_kernel_sum_plus_the_explicit_terms() {
        let model = tiny_model();

        // At the centre itself the kernel is one; the explicit term adds its raw
        // value times its weight.
        let mut f = Features { values: [f64::NAN; FEATURE_COUNT] };
        f.values[17] = 10.0;        // angle_axis_deg: kernel column, standardised to 0
        f.values[13] = 4.0;         // pupil_l_mm: explicit

        let p = model.predict(&f);

        assert!((p.yaw_deg - (1.0 * 0.5 + 4.0 * 0.25)).abs() < 1e-12, "{}", p.yaw_deg);
        assert!((p.pitch_deg - (1.0 * -0.5 + 4.0 * 0.0)).abs() < 1e-12);
        assert!((p.var_deg2 - (1.0 + 4.0 * 4.0)).abs() < 1e-12, "{}", p.var_deg2);
    }

    #[test]
    fn a_missing_kernel_feature_is_imputed_and_a_missing_explicit_one_is_zero() {
        let model = tiny_model();
        let f     = Features { values: [f64::NAN; FEATURE_COUNT] };
        let p     = model.predict(&f);

        // Impute 10 -> standardised 0 -> kernel 1; pupil NaN -> 0.
        assert!((p.yaw_deg - 0.5).abs() < 1e-12);
    }

    #[test]
    fn the_file_round_trips_and_a_bad_shape_is_refused() {
        let dir   = std::env::temp_dir().join("gaze-et5-model-test.json");
        let model = tiny_model();

        model.save(&dir).expect("save");
        let back = ResidualModel::load(&dir).expect("load");
        assert_eq!(back, model);

        let mut broken = model.clone();
        broken.kernel_weights.clear();
        broken.save(&dir).expect("save");
        assert!(matches!(ResidualModel::load(&dir), Err(ModelError::Shape(_))));

        let _ = std::fs::remove_file(&dir);
    }

    /// One centre on one standardised feature, one explicit feature, identity variance.
    fn tiny_model() -> ResidualModel {
        ResidualModel {
            format             : MODEL_FORMAT,
            created_unix_s     : 0.0,
            device_blob_sha256 : None,
            tracker_pitch_deg  : 13.0,
            features           : vec!["angle_axis_deg".into()],
            explicit           : vec!["pupil_l_mm".into()],
            impute             : vec![10.0],
            mean               : vec![10.0],
            std                : vec![2.0],
            length_scale       : vec![1.0],
            centers            : vec![vec![0.0]],
            kernel_weights     : vec![[0.5, -0.5]],
            explicit_weights   : vec![[0.25, 0.0]],
            variance           : vec![vec![1.0, 0.0], vec![0.0, 1.0]],
            var_fade_lo        : 1.0,
            var_fade_hi        : 2.0,
            report             : None,
        }
    }
}
