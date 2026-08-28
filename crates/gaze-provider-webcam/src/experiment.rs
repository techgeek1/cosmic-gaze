//! Offline experiments on a saved sweep: which correction model actually generalises, and
//! whether the desk config is the thing holding it back.
//!
//! Nothing here runs during a calibration. It exists so a question about the model can be
//! answered against a sweep that has already been captured, in seconds, instead of by
//! asking someone to sit through another forty-five seconds of targets and hoping their
//! head is in the same place. Every trial is scored the same way the real fit is scored,
//! by leave-one-target-out, so the numbers are directly comparable to what `calibrate`
//! prints.
//!
//! # What a feature set is
//!
//! Each trial is a choice of inputs to the angle polynomial. The baseline is the reported
//! gaze angles alone. The others add head information, because an appearance model's error
//! is a function of where the head is as much as of where the eye is pointing: the same
//! reported yaw means something different when the user has turned toward the panel than
//! when they have swivelled their eyes to it. If those trials do not win, the residual
//! structure is coming from somewhere else and the polynomial is not the place to fix it.

use gaze_core::DesktopGeometry;
use glam::DVec3;

use crate::camera::{CameraPose, gaze_yaw_pitch_deg};
use crate::sweep::{AngleSample, Observation};

/// Ridge values swept for every feature set, relative to the normal matrix scale.
pub const RIDGES: [f64; 4] = [1.0e-4, 1.0e-3, 1.0e-2, 1.0e-1];

/// Millimetres the eye position is divided by before entering the basis, so its terms are
/// the same order as the angle terms and one ridge value means the same thing for both.
const EYE_SCALE_MM: f64 = 100.0;

/// Smallest spread, degrees, used as a weight denominator. A target whose samples happen
/// to agree to a thousandth of a degree must not be handed a thousand times the influence
/// of every other target.
const MIN_SPREAD_DEG: f64 = 0.25;

/// Which inputs a trial's polynomial is built on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Features {
    /// Reported yaw and pitch only. The shipping model.
    Gaze,
    /// Gaze angles plus the reported eye position, linearly. Available on every sweep,
    /// since the eye position is what the ray origin is built from anyway.
    GazeEye,
    /// Gaze angles plus head yaw and pitch, linearly.
    GazeHead,
    /// Gaze angles plus head yaw and pitch and their products with the gaze angles. Lets
    /// the *gain* vary with head pose rather than only the offset.
    GazeHeadCross,
    /// Eye-in-head angles: the head rotation is subtracted from the gaze angles before the
    /// fit and added back after. The physically motivated variant, and the one that needs
    /// no extra coefficients at all.
    EyeInHead,
}

/// One trial and how it scored.
#[derive(Clone, Debug)]
pub struct Trial {
    pub features    : Features,
    /// 2 for a quadratic in the gaze angles, 3 for a cubic.
    pub degree      : usize,
    pub ridge       : f64,
    /// Coefficients per axis, so the cost of the extra freedom is visible next to the gain.
    pub terms       : usize,
    pub rms_deg     : f64,
    pub rms_loo_deg : f64,
    pub worst_deg   : f64,
    /// Held-out RMS when the fit weighted each target by `1 / spread`, evaluated with the
    /// same unweighted metric so it is comparable to the row above it.
    pub weighted_loo_deg : f64,
    /// False when the sweep does not carry the inputs this trial needs.
    pub available   : bool,
}

// --- Features ---

impl Features {
    /// Every feature set worth trying.
    pub fn all() -> [Features; 5] {
        [
            Features::Gaze,
            Features::GazeEye,
            Features::GazeHead,
            Features::GazeHeadCross,
            Features::EyeInHead,
        ]
    }

    /// Short name for the report.
    pub fn name(self) -> &'static str {
        match self {
            Features::Gaze          => "gaze",
            Features::GazeEye       => "gaze+eye",
            Features::GazeHead      => "gaze+head",
            Features::GazeHeadCross => "gaze+head*",
            Features::EyeInHead     => "eye-in-head",
        }
    }

    /// True when this feature set needs `head_rot` on every sample.
    pub fn needs_head(self) -> bool {
        matches!(self, Features::GazeHead | Features::GazeHeadCross | Features::EyeInHead)
    }
}

/// Runs every trial over a sweep.
pub fn run(observations: &[Observation]) -> Vec<Trial> {
    let has_head = observations
        .iter()
        .flat_map(|o| o.samples.iter())
        .all(|s| s.head_rot.is_some());

    let mut out = Vec::new();

    for features in Features::all() {
        for degree in [2_usize, 3] {
            for ridge in RIDGES {
                if features.needs_head() && !has_head {
                    out.push(Trial {
                        features         : features,
                        degree           : degree,
                        ridge            : ridge,
                        terms            : 0,
                        rms_deg          : f64::NAN,
                        rms_loo_deg      : f64::NAN,
                        worst_deg        : f64::NAN,
                        weighted_loo_deg : f64::NAN,
                        available        : false,
                    });

                    continue;
                }

                out.push(score(observations, features, degree, ridge));
            }
        }
    }

    out
}

/// Scores one trial.
fn score(observations: &[Observation], features: Features, degree: usize, ridge: f64) -> Trial {
    let terms  = basis_len(features, degree);
    let in_sam = {
        match fit(observations, features, degree, ridge, false) {
            Some(model) => rms(&observations
                .iter()
                .map(|o| residual_deg(&model, features, degree, o))
                .collect::<Vec<_>>()),

            None => f64::NAN,
        }
    };

    let plain    = held_out(observations, features, degree, ridge, false);
    let weighted = held_out(observations, features, degree, ridge, true);

    Trial {
        features         : features,
        degree           : degree,
        ridge            : ridge,
        terms            : terms,
        rms_deg          : in_sam,
        rms_loo_deg      : rms(&plain),
        worst_deg        : plain.iter().copied().filter(|v| v.is_finite()).fold(0.0_f64, f64::max),
        weighted_loo_deg : rms(&weighted),
        available        : true,
    }
}

/// Leave-one-target-out residuals.
fn held_out(
    observations : &[Observation],
    features     : Features,
    degree       : usize,
    ridge        : f64,
    weighted     : bool,
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

        let Some(model) = fit(&rest, features, degree, ridge, weighted) else {
            continue;
        };

        out.push(residual_deg(&model, features, degree, &observations[held]));
    }

    out
}

/// Coefficients for both axes.
struct Model {
    yaw   : Vec<f64>,
    pitch : Vec<f64>,
}

/// Ridge least squares over every sample of every observation.
fn fit(
    observations : &[Observation],
    features     : Features,
    degree       : usize,
    ridge        : f64,
    weighted     : bool,
)
    -> Option<Model>
{
    let n = basis_len(features, degree);

    let mut ata = vec![0.0_f64; n * n];
    let mut aty = vec![0.0_f64; n];
    let mut atp = vec![0.0_f64; n];

    for obs in observations {
        // Weighting by the inverse spread stops a target the user never settled on from
        // pulling the whole surface toward wherever their eyes happened to be.
        let w = {
            if weighted {
                1.0 / obs.spread_deg.max(MIN_SPREAD_DEG)
            }
            else {
                1.0
            }
        };

        for s in &obs.samples {
            let Some(row) = basis(s, features, degree) else {
                continue;
            };

            let (want_yaw, want_pitch) = wanted(s, features);

            for i in 0..n {
                aty[i] += w * row[i] * want_yaw / crate::fit::ANGLE_SCALE_DEG;
                atp[i] += w * row[i] * want_pitch / crate::fit::ANGLE_SCALE_DEG;

                for j in 0..n {
                    ata[i * n + j] += w * row[i] * row[j];
                }
            }
        }
    }

    let scale = ata.iter().fold(0.0_f64, |m, v| m.max(v.abs()));

    if scale <= 0.0 {
        return None;
    }

    for i in 1..n {
        ata[i * n + i] += ridge * scale;
    }

    Some(Model {
        yaw   : solve(&ata, &aty, n)?,
        pitch : solve(&ata, &atp, n)?,
    })
}

/// Angular residual at one target, degrees, evaluated at the target's mean sample.
fn residual_deg(model: &Model, features: Features, degree: usize, obs: &Observation) -> f64 {
    let Some(mean) = mean_sample(obs) else {
        return f64::NAN;
    };

    let Some(row) = basis(&mean, features, degree) else {
        return f64::NAN;
    };

    let scale = crate::fit::ANGLE_SCALE_DEG;

    let mut yaw   = 0.0;
    let mut pitch = 0.0;

    for (i, term) in row.iter().enumerate() {
        yaw   += model.yaw[i] * term;
        pitch += model.pitch[i] * term;
    }

    // Undo whatever the feature set did to the target before comparing.
    let (add_yaw, add_pitch) = restore(&mean, features);

    let got  = crate::camera::gaze_dir_from_yaw_pitch_deg(yaw * scale + add_yaw, pitch * scale + add_pitch);
    let want = crate::camera::gaze_dir_from_yaw_pitch_deg(mean.want_yaw_deg, mean.want_pitch_deg);

    got.angle_between(want).to_degrees()
}

/// The mean of a target's samples, as a synthetic sample.
fn mean_sample(obs: &Observation) -> Option<AngleSample> {
    if obs.samples.is_empty() {
        return None;
    }

    let n = obs.samples.len() as f64;

    let head = {
        let present: Vec<[f64; 3]> = obs.samples.iter().filter_map(|s| s.head_rot).collect();

        if present.is_empty() {
            None
        }
        else {
            let m = present.len() as f64;

            Some([
                present.iter().map(|r| r[0]).sum::<f64>() / m,
                present.iter().map(|r| r[1]).sum::<f64>() / m,
                present.iter().map(|r| r[2]).sum::<f64>() / m,
            ])
        }
    };

    Some(AngleSample {
        eye_cam_mm     : obs.samples.iter().map(|s| s.eye_cam_mm).sum::<DVec3>() / n,
        gaze_cam       : obs.samples.iter().map(|s| s.gaze_cam).sum::<DVec3>() / n,
        yaw_deg        : obs.samples.iter().map(|s| s.yaw_deg).sum::<f64>() / n,
        pitch_deg      : obs.samples.iter().map(|s| s.pitch_deg).sum::<f64>() / n,
        want_yaw_deg   : obs.samples.iter().map(|s| s.want_yaw_deg).sum::<f64>() / n,
        want_pitch_deg : obs.samples.iter().map(|s| s.want_pitch_deg).sum::<f64>() / n,
        missed         : false,
        head_rot       : head,
        raw            : None,
    })
}

/// Head yaw and pitch in degrees, from a Rodrigues rotation vector.
///
/// The rotation is applied to the forward axis and the result read back as angles, which
/// is convention-free: whatever the sidecar means by its axis order, turning the head right
/// moves the forward axis right.
pub fn head_yaw_pitch_deg(rodrigues: [f64; 3]) -> Option<(f64, f64)> {
    let v     = DVec3::from_array(rodrigues);
    let theta = v.length();

    if !theta.is_finite() {
        return None;
    }

    let forward = -DVec3::Z;

    if theta <= 1.0e-12 {
        return gaze_yaw_pitch_deg(forward);
    }

    let axis = v / theta;
    let q    = glam::DQuat::from_axis_angle(axis, theta);

    gaze_yaw_pitch_deg(q * forward)
}

/// Number of basis terms for a feature set.
fn basis_len(features: Features, degree: usize) -> usize {
    let base = if degree >= 3 { 10 } else { 6 };

    match features {
        Features::Gaze | Features::EyeInHead => base,
        Features::GazeEye                    => base + 3,
        Features::GazeHead                   => base + 2,
        Features::GazeHeadCross              => base + 6,
    }
}

/// The basis row for one sample, or `None` when an input it needs is missing.
fn basis(s: &AngleSample, features: Features, degree: usize) -> Option<Vec<f64>> {
    let scale = crate::fit::ANGLE_SCALE_DEG;

    let (yaw, pitch) = {
        match features {
            Features::EyeInHead => {
                let (hy, hp) = head_yaw_pitch_deg(s.head_rot?)?;

                (s.yaw_deg - hy, s.pitch_deg - hp)
            }

            _ => (s.yaw_deg, s.pitch_deg),
        }
    };

    let (u, v) = (yaw / scale, pitch / scale);
    let poly   = crate::fit::poly_basis(u, v);
    let base   = if degree >= 3 { 10 } else { 6 };

    let mut row: Vec<f64> = poly[..base].to_vec();

    match features {
        Features::Gaze | Features::EyeInHead => {}

        Features::GazeEye => {
            row.push(s.eye_cam_mm.x / EYE_SCALE_MM);
            row.push(s.eye_cam_mm.y / EYE_SCALE_MM);
            row.push(s.eye_cam_mm.z / EYE_SCALE_MM);
        }

        Features::GazeHead => {
            let (hy, hp) = head_yaw_pitch_deg(s.head_rot?)?;

            row.push(hy / scale);
            row.push(hp / scale);
        }

        Features::GazeHeadCross => {
            let (hy, hp) = head_yaw_pitch_deg(s.head_rot?)?;
            let (a, b)   = (hy / scale, hp / scale);

            row.extend_from_slice(&[a, b, a * u, a * v, b * u, b * v]);
        }
    }

    Some(row)
}

/// What the fit is asked to produce for a sample, given the feature set.
fn wanted(s: &AngleSample, features: Features) -> (f64, f64) {
    match features {
        // Eye-in-head fits the eye's contribution alone, so the head's is taken off the
        // target as well and added back at evaluation time.
        Features::EyeInHead => {
            match s.head_rot.and_then(head_yaw_pitch_deg) {
                Some((hy, hp)) => (s.want_yaw_deg - hy, s.want_pitch_deg - hp),
                None           => (s.want_yaw_deg, s.want_pitch_deg),
            }
        }

        _ => (s.want_yaw_deg, s.want_pitch_deg),
    }
}

/// The inverse of `wanted`, applied to a prediction.
fn restore(s: &AngleSample, features: Features) -> (f64, f64) {
    match features {
        Features::EyeInHead => {
            match s.head_rot.and_then(head_yaw_pitch_deg) {
                Some((hy, hp)) => (hy, hp),
                None           => (0.0, 0.0),
            }
        }

        _ => (0.0, 0.0),
    }
}

/// How much the held-out error moves when the camera pose is nudged.
///
/// The wanted angles are built from the camera pose in `desk.toml`, and every one of those
/// numbers is marked MEASURE. If a perturbation makes the fit markedly better, the pose is
/// wrong and correcting it is worth more than any amount of polynomial: the sweep would be
/// asking the model to hit a target that is not where the config says it is.
pub fn pose_sensitivity(
    geometry     : &DesktopGeometry,
    camera       : &CameraPose,
    observations : &[Observation],
    features     : Features,
    degree       : usize,
    ridge        : f64,
)
    -> Vec<(String, f64)>
{
    let mut out = Vec::new();

    let perturbations: Vec<(String, CameraPose)> = [
        ("baseline", 0.0, 0.0, [0.0, 0.0, 0.0]),
        ("yaw +5", 5.0, 0.0, [0.0, 0.0, 0.0]),
        ("yaw -5", -5.0, 0.0, [0.0, 0.0, 0.0]),
        ("pitch +5", 0.0, 5.0, [0.0, 0.0, 0.0]),
        ("pitch -5", 0.0, -5.0, [0.0, 0.0, 0.0]),
        ("x +30mm", 0.0, 0.0, [30.0, 0.0, 0.0]),
        ("x -30mm", 0.0, 0.0, [-30.0, 0.0, 0.0]),
        ("y +30mm", 0.0, 0.0, [0.0, 30.0, 0.0]),
        ("y -30mm", 0.0, 0.0, [0.0, -30.0, 0.0]),
        ("z +30mm", 0.0, 0.0, [0.0, 0.0, 30.0]),
        ("z -30mm", 0.0, 0.0, [0.0, 0.0, -30.0]),
    ]
    .into_iter()
    .map(|(name, dyaw, dpitch, dpos)| {
        (
            name.to_string(),
            CameraPose {
                yaw_deg     : camera.yaw_deg + dyaw,
                pitch_deg   : camera.pitch_deg + dpitch,
                position_mm : [
                    camera.position_mm[0] + dpos[0],
                    camera.position_mm[1] + dpos[1],
                    camera.position_mm[2] + dpos[2],
                ],
                ..camera.clone()
            },
        )
    })
    .collect();

    for (name, pose) in perturbations {
        let moved = with_pose(geometry, &pose, observations);

        out.push((name, rms(&held_out(&moved, features, degree, ridge, false))));
    }

    out
}

/// Recomputes every sample's wanted angles under a different camera pose.
///
/// Only the wanted angles move. The reported gaze and eye are in the camera's own frame
/// and are whatever the sidecar said, no matter where the camera turns out to be.
fn with_pose(
    geometry     : &DesktopGeometry,
    pose         : &CameraPose,
    observations : &[Observation],
)
    -> Vec<Observation>
{
    observations
        .iter()
        .filter_map(|obs| {
            let world      = geometry.px_to_world(obs.target.px)?;
            let target_cam = pose.point_to_camera(world);

            let mut moved = obs.clone();

            for s in &mut moved.samples {
                if let Some((yaw, pitch)) = gaze_yaw_pitch_deg(target_cam - s.eye_cam_mm) {
                    s.want_yaw_deg   = yaw;
                    s.want_pitch_deg = pitch;
                }
            }

            Some(moved)
        })
        .collect()
}

/// Gaussian elimination with partial pivoting on a dense `n x n` system.
fn solve(a: &[f64], b: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut m = a.to_vec();
    let mut y = b.to_vec();

    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| {
            m[i * n + col].abs().partial_cmp(&m[j * n + col].abs()).unwrap_or(std::cmp::Ordering::Equal)
        })?;

        if m[pivot * n + col].abs() < 1.0e-12 {
            return None;
        }

        for k in 0..n {
            m.swap(col * n + k, pivot * n + k);
        }

        y.swap(col, pivot);

        for row in (col + 1)..n {
            let factor = m[row * n + col] / m[col * n + col];

            for k in col..n {
                m[row * n + k] -= factor * m[col * n + k];
            }

            y[row] -= factor * y[col];
        }
    }

    let mut out = vec![0.0; n];

    for row in (0..n).rev() {
        let mut sum = y[row];

        for k in (row + 1)..n {
            sum -= m[row * n + k] * out[k];
        }

        out[row] = sum / m[row * n + row];
    }

    out.iter().all(|c| c.is_finite()).then_some(out)
}

/// Root mean square, ignoring non-finite entries.
fn rms(values: &[f64]) -> f64 {
    let good: Vec<f64> = values.iter().copied().filter(|v| v.is_finite()).collect();

    if good.is_empty() {
        return f64::NAN;
    }

    (good.iter().map(|v| v * v).sum::<f64>() / good.len() as f64).sqrt()
}


// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::camera::gaze_dir_from_yaw_pitch_deg;
    use crate::sweep::{self, SweepTarget};

    /// The frozen 2026-08-25 desk snapshot these assertions were written against;
    /// the live `config/desk.toml` drifts with the physical desk.
    const FIXTURE_TOML: &str = include_str!("../../../config/desk-fixture.toml");

    fn desk() -> (DesktopGeometry, CameraPose) {
        (
            DesktopGeometry::from_toml(FIXTURE_TOML).unwrap(),
            CameraPose::from_desk_toml(FIXTURE_TOML).unwrap(),
        )
    }

    /// A synthetic sweep under a linear angular distortion, optionally carrying head data.
    fn sweep(with_head: bool) -> Vec<Observation> {
        let (g, c) = desk();
        let eye    = c.point_to_camera(g.eye());

        sweep::default_targets(&g, 3, 0.12, true)
            .into_iter()
            .filter_map(|target: SweepTarget| {
                let world = g.px_to_world(target.px)?;
                let want  = gaze_yaw_pitch_deg(c.point_to_camera(world) - eye)?;

                let sample = AngleSample {
                    eye_cam_mm     : eye,
                    gaze_cam       : gaze_dir_from_yaw_pitch_deg(want.0 * 1.3 + 2.0, want.1 * 0.9),
                    yaw_deg        : want.0 * 1.3 + 2.0,
                    pitch_deg      : want.1 * 0.9,
                    want_yaw_deg   : want.0,
                    want_pitch_deg : want.1,
                    missed         : false,
                    head_rot       : with_head.then_some([0.05, -0.1, 0.0]),
                    raw            : None,
                };

                sweep::rebuild(&g, &c, target, vec![sample])
            })
            .collect()
    }

    #[test]
    fn head_angles_come_back_from_a_rodrigues_vector_with_the_expected_signs() {
        // No rotation is straight ahead.
        let (yaw, pitch) = head_yaw_pitch_deg([0.0, 0.0, 0.0]).unwrap();
        assert!(yaw.abs() < 1.0e-9 && pitch.abs() < 1.0e-9);

        // A rotation about +Y turns the forward axis in yaw only.
        let (yaw, pitch) = head_yaw_pitch_deg([0.0, 30.0_f64.to_radians(), 0.0]).unwrap();
        assert!((yaw.abs() - 30.0).abs() < 1.0e-6, "yaw = {yaw}");
        assert!(pitch.abs() < 1.0e-6);

        // A rotation about +X is pitch only.
        let (yaw, pitch) = head_yaw_pitch_deg([20.0_f64.to_radians(), 0.0, 0.0]).unwrap();
        assert!(yaw.abs() < 1.0e-6);
        assert!((pitch.abs() - 20.0).abs() < 1.0e-6, "pitch = {pitch}");

        // A rotation about the view axis moves neither.
        let (yaw, pitch) = head_yaw_pitch_deg([0.0, 0.0, 45.0_f64.to_radians()]).unwrap();
        assert!(yaw.abs() < 1.0e-6 && pitch.abs() < 1.0e-6);

        assert!(head_yaw_pitch_deg([f64::NAN, 0.0, 0.0]).is_none());
    }

    #[test]
    fn trials_needing_head_data_are_marked_unavailable_rather_than_guessed_at() {
        let trials = run(&sweep(false));

        for t in &trials {
            assert_eq!(
                t.available,
                !t.features.needs_head(),
                "{} availability is wrong",
                t.features.name(),
            );
        }

        // And they become available once the sweep carries head rotations.
        assert!(run(&sweep(true)).iter().all(|t| t.available));
    }

    #[test]
    fn every_feature_set_recovers_a_linear_distortion_it_is_capable_of_expressing() {
        let trials = run(&sweep(true));

        for t in trials.iter().filter(|t| t.available && t.ridge <= 1.0e-4) {
            // Every feature set contains the plain gaze basis, so all of them can express
            // this distortion. The eye position is constant across a synthetic sweep, which
            // makes its columns collinear with the intercept, so only the lightest ridge is
            // asked to behave: heavier ones legitimately trade that redundancy for bias.
            assert!(
                t.rms_loo_deg < 1.0,
                "{} degree {} ridge {:.0e} left {:.2} deg",
                t.features.name(), t.degree, t.ridge, t.rms_loo_deg,
            );
        }
    }

    #[test]
    fn a_heavier_ridge_costs_accuracy_on_data_that_does_not_need_it() {
        let trials = run(&sweep(false));

        let at = |ridge: f64| {
            trials
                .iter()
                .find(|t| t.features == Features::Gaze && t.degree == 3 && t.ridge == ridge)
                .map(|t| t.rms_loo_deg)
                .unwrap()
        };

        // The distortion here is exactly representable, so regularisation can only hurt.
        assert!(at(1.0e-4) < at(1.0e-1), "{} vs {}", at(1.0e-4), at(1.0e-1));
    }

    #[test]
    fn the_pose_sweep_reports_a_baseline_and_every_perturbation() {
        let (g, c) = desk();
        let obs    = sweep(false);

        let out = pose_sensitivity(&g, &c, &obs, Features::Gaze, 3, 1.0e-3);

        assert_eq!(out.len(), 11);
        assert_eq!(out[0].0, "baseline");
        assert!(out.iter().all(|(_, v)| v.is_finite()));

        // A pose error changes the wanted angles, so it has to change the score. If this
        // came out flat the perturbation would not be reaching the fit at all, and the
        // whole check would be vacuous.
        let moved = out.iter().skip(1).any(|(_, v)| (v - out[0].1).abs() > 1.0e-6);
        assert!(moved, "perturbing the camera pose must move the score");
    }
}
