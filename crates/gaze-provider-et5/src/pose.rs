//! Display pose in tracker space, solved two ways from calibration data.
//!
//! [`solve_pose`] is perspective-n-point with the eye as the camera: fixation targets
//! at known surface positions, gaze rays collected while the user looked at them,
//! and Levenberg-Marquardt over the six pose unknowns with angular residuals. Its
//! weakness is structural: with every ray leaving roughly one head position, depth
//! trades off against pitch almost freely.
//!
//! [`solve_pose_points`] fits the same six unknowns to *triangulated* target
//! positions (see `crate::triangulate`): each observation is an absolute point in
//! tracker millimetres, so the residuals are metric and the depth/pitch degeneracy
//! never arises. Three well-spread points fully determine the pose of a panel of
//! known shape; more over-determine it.
//!
//! Both solvers share one LM core over stacked 3-vector residuals with a numeric
//! central-difference Jacobian.

use gaze_core::OutputGeometry;
use glam::DVec3;

/// LM iteration cap. The problem is 6-dimensional and well conditioned; convergence
/// takes far fewer steps in practice.
const MAX_ITERATIONS: usize = 200;

/// Convergence threshold on the relative cost improvement.
const COST_EPS: f64 = 1e-12;

/// Numeric Jacobian step for the position parameters, millimetres.
const STEP_MM: f64 = 0.5;

/// Numeric Jacobian step for the orientation parameters, degrees.
const STEP_DEG: f64 = 0.02;

/// Millimetres per unit of point residual. Keeps point residuals in the same
/// numeric range as the unit-vector ray residuals so the shared LM thresholds and
/// damping behave identically for both solvers.
const POINT_SCALE_MM: f64 = 100.0;

/// Minimum perpendicular spread of a point-observation layout, millimetres: the
/// largest distance from any point to the line through the two farthest points.
/// Below this the layout is effectively collinear and roll about that line is
/// unconstrained.
const MIN_LAYOUT_SPREAD_MM: f64 = 20.0;

// --- Observations ---

/// One fixation: where the target was on the panel, and the gaze ray collected while
/// the user held it. The ray lives in tracker space, the target in the panel's
/// normalised surface coordinates.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PoseObservation {
    /// Target position across the panel width, [0, 1].
    pub u         : f64,
    /// Target position down the panel height, [0, 1].
    pub v         : f64,
    /// Ray origin (the eye), tracker space, millimetres.
    pub origin_mm : DVec3,
    /// Unit ray direction, tracker space.
    pub dir       : DVec3,
}

/// One triangulated fixation: a target's surface position and its measured physical
/// position in tracker space. Constrains the pose by three metric residuals with no
/// viewpoint involved, which is what makes depth and pitch separately observable.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointObservation {
    /// Target position across the panel width, [0, 1].
    pub u        : f64,
    /// Target position down the panel height, [0, 1].
    pub v        : f64,
    /// Triangulated target position, tracker space, millimetres.
    pub point_mm : DVec3,
}

/// A pose solved from rays and its fit quality.
#[derive(Clone, Debug, PartialEq)]
pub struct SolvedPose {
    /// The input geometry with position and orientation replaced by the solution.
    pub output        : OutputGeometry,
    /// Root-mean-square angular residual over the observations, degrees.
    pub rms_deg       : f64,
    /// Worst single-observation angular residual, degrees.
    pub max_deg       : f64,
    /// Per-observation angular residuals, degrees, in input order.
    pub residuals_deg : Vec<f64>,
    /// LM iterations spent.
    pub iterations    : usize,
}

/// A pose solved from triangulated points and its fit quality.
#[derive(Clone, Debug, PartialEq)]
pub struct SolvedPointPose {
    /// The input geometry with position and orientation replaced by the solution.
    pub output       : OutputGeometry,
    /// Root-mean-square metric residual over the points, millimetres.
    pub rms_mm       : f64,
    /// Worst single-point metric residual, millimetres.
    pub max_mm       : f64,
    /// Per-point metric residuals, millimetres, in input order.
    pub residuals_mm : Vec<f64>,
    /// LM iterations spent.
    pub iterations   : usize,
}

/// An observation kind the shared LM core can evaluate: one 3-vector residual under
/// a hypothesised pose.
trait Residual3 {
    /// Writes the residual of this observation against `posed` into `out`.
    fn eval(&self, posed: &OutputGeometry, out: &mut [f64; 3]);
}

impl Residual3 for PoseObservation {
    // The difference between the observed ray direction and the unit vector from
    // the eye to the hypothesised target. That vector difference is smooth through
    // zero (a plain angle is not), and its norm is the angular error in radians for
    // the small angles that matter here.
    fn eval(&self, posed: &OutputGeometry, out: &mut [f64; 3]) {
        let target = posed.uv_to_world(self.u, self.v);
        let want   = (target - self.origin_mm).normalize();
        let diff   = want - self.dir;

        out[0] = diff.x;
        out[1] = diff.y;
        out[2] = diff.z;
    }
}

impl Residual3 for PointObservation {
    fn eval(&self, posed: &OutputGeometry, out: &mut [f64; 3]) {
        let diff = (posed.uv_to_world(self.u, self.v) - self.point_mm) / POINT_SCALE_MM;

        out[0] = diff.x;
        out[1] = diff.y;
        out[2] = diff.z;
    }
}

// --- Solvers ---

/// Solves the panel pose from ray observations, refining from the pose already in
/// `output`. Needs at least four observations (six unknowns, two angles each, plus
/// slack for noise); more is better.
pub fn solve_pose(output: &OutputGeometry, observations: &[PoseObservation])
    -> Result<SolvedPose, PoseError>
{
    if observations.len() < 4 {
        return Err(PoseError::TooFewObservations(observations.len()));
    }

    for (i, obs) in observations.iter().enumerate() {
        let unit = (obs.dir.length() - 1.0).abs() < 1e-6;

        if !unit || !obs.dir.is_finite() || !obs.origin_mm.is_finite() {
            return Err(PoseError::BadObservation(i));
        }
    }

    let (params, iterations) = refine(output, observations, params_of(output));

    // Report residuals as true angles, which is what the caller reasons in.
    let solved = apply_params(output, &params);
    let mut residuals = Vec::with_capacity(observations.len());

    for obs in observations {
        let target = solved.uv_to_world(obs.u, obs.v);
        let want   = (target - obs.origin_mm).normalize();

        residuals.push(want.angle_between(obs.dir).to_degrees());
    }

    let rms = (residuals.iter().map(|r| r * r).sum::<f64>() / residuals.len() as f64).sqrt();
    let max = residuals.iter().cloned().fold(0.0_f64, f64::max);

    Ok(SolvedPose {
        output        : solved,
        rms_deg       : rms,
        max_deg       : max,
        residuals_deg : residuals,
        iterations    : iterations,
    })
}

/// Solves the panel pose from triangulated points, refining from the pose already in
/// `output`. Needs at least three points (nine residuals over six unknowns) that are
/// not collinear; the panel's shape and size come from `output` and are what let
/// three central points extrapolate to the full surface.
pub fn solve_pose_points(output: &OutputGeometry, points: &[PointObservation])
    -> Result<SolvedPointPose, PoseError>
{
    if points.len() < 3 {
        return Err(PoseError::TooFewObservations(points.len()));
    }

    for (i, p) in points.iter().enumerate() {
        if !p.point_mm.is_finite() {
            return Err(PoseError::BadObservation(i));
        }
    }

    if layout_spread_mm(points) < MIN_LAYOUT_SPREAD_MM {
        return Err(PoseError::DegenerateLayout);
    }

    let (params, iterations) = refine(output, points, params_of(output));

    let solved = apply_params(output, &params);
    let mut residuals = Vec::with_capacity(points.len());

    for p in points {
        residuals.push(solved.uv_to_world(p.u, p.v).distance(p.point_mm));
    }

    let rms = (residuals.iter().map(|r| r * r).sum::<f64>() / residuals.len() as f64).sqrt();
    let max = residuals.iter().cloned().fold(0.0_f64, f64::max);

    Ok(SolvedPointPose {
        output       : solved,
        rms_mm       : rms,
        max_mm       : max,
        residuals_mm : residuals,
        iterations   : iterations,
    })
}

/// The largest perpendicular distance from any point to the line through the two
/// farthest-apart points, millimetres. Zero for a collinear layout.
fn layout_spread_mm(points: &[PointObservation]) -> f64 {
    let mut ends = (DVec3::ZERO, DVec3::ZERO);
    let mut best = -1.0_f64;

    for a in points {
        for b in points {
            let d = a.point_mm.distance_squared(b.point_mm);

            if d > best {
                best = d;
                ends = (a.point_mm, b.point_mm);
            }
        }
    }

    let axis = ends.1 - ends.0;

    if axis.length_squared() < 1e-9 {
        return 0.0;
    }

    let axis = axis.normalize();

    points.iter()
        .map(|p| {
            let v = p.point_mm - ends.0;

            (v - axis * v.dot(axis)).length()
        })
        .fold(0.0_f64, f64::max)
}

// --- Parameterisation ---

/// Pose parameter order: position xyz (mm), then yaw, pitch, roll (degrees).
type Params = [f64; 6];

/// Extracts the pose parameters from a geometry.
fn params_of(output: &OutputGeometry) -> Params {
    [
        output.position_mm[0],
        output.position_mm[1],
        output.position_mm[2],
        output.yaw_deg,
        output.pitch_deg,
        output.roll_deg,
    ]
}

/// The input geometry with its pose replaced by `params`.
fn apply_params(output: &OutputGeometry, params: &Params) -> OutputGeometry {
    let mut out = output.clone();
    out.position_mm = [params[0], params[1], params[2]];
    out.yaw_deg     = params[3];
    out.pitch_deg   = params[4];
    out.roll_deg    = params[5];

    out
}

/// Per-parameter numeric differentiation step.
fn param_step(index: usize) -> f64 {
    if index < 3 { STEP_MM } else { STEP_DEG }
}

// --- The LM core ---

/// Standard Levenberg-Marquardt on the stacked 3-vector residuals with a numeric
/// central-difference Jacobian, from `params0`. Parameter units differ (mm vs
/// degrees); the Marquardt damping on the diagonal absorbs the scale difference.
/// Returns the refined parameters and the iterations spent.
fn refine<O: Residual3>(
    output       : &OutputGeometry,
    observations : &[O],
    params0      : Params,
)
    -> (Params, usize)
{
    let mut params = params0;
    let mut cost   = cost_at(output, observations, &params);
    let mut lambda = 1.0e-3;
    let mut iters  = 0usize;

    for _ in 0..MAX_ITERATIONS {
        iters += 1;

        let (jtj, jtr) = normal_equations(output, observations, &params);

        // Try increasingly damped steps until one reduces the cost.
        let mut improved = false;

        for _ in 0..12 {
            let mut damped = jtj;

            for (d, row) in damped.iter_mut().enumerate() {
                row[d] += lambda * jtj[d][d].max(1e-12);
            }

            let Some(step) = solve6(&damped, &jtr) else {
                lambda *= 10.0;
                continue;
            };

            let mut candidate = params;

            for (p, s) in candidate.iter_mut().zip(step.iter()) {
                *p -= s;
            }

            let candidate_cost = cost_at(output, observations, &candidate);

            if candidate_cost < cost {
                let relative = (cost - candidate_cost) / cost.max(1e-30);

                params = candidate;
                cost   = candidate_cost;
                lambda = (lambda * 0.3).max(1e-12);

                improved = true;

                if relative < COST_EPS {
                    iters = MAX_ITERATIONS;
                }

                break;
            }

            lambda *= 10.0;
        }

        if !improved || iters >= MAX_ITERATIONS {
            break;
        }
    }

    (params, iters)
}

/// Writes the 3-vector residual of one observation under `params` into `out`.
fn residual<O: Residual3>(
    output : &OutputGeometry,
    obs    : &O,
    params : &Params,
    out    : &mut [f64; 3],
)
{
    let posed = apply_params(output, params);

    obs.eval(&posed, out);
}

/// Sum of squared residuals under `params`.
fn cost_at<O: Residual3>(output: &OutputGeometry, observations: &[O], params: &Params)
    -> f64
{
    let mut sum = 0.0;
    let mut r   = [0.0; 3];

    for obs in observations {
        residual(output, obs, params, &mut r);
        sum += r[0] * r[0] + r[1] * r[1] + r[2] * r[2];
    }

    sum
}

/// Builds `J^T J` and `J^T r` with a central-difference Jacobian, without ever
/// materialising the full Jacobian.
// The elimination and accumulation loops index several arrays in lockstep;
// iterator chains would obscure the linear algebra.
#[allow(clippy::needless_range_loop)]
fn normal_equations<O: Residual3>(
    output       : &OutputGeometry,
    observations : &[O],
    params       : &Params,
)
    -> ([[f64; 6]; 6], [f64; 6])
{
    let mut jtj = [[0.0; 6]; 6];
    let mut jtr = [0.0; 6];

    let mut r0    = [0.0; 3];
    let mut plus  = [0.0; 3];
    let mut minus = [0.0; 3];
    let mut jrow  = [[0.0; 3]; 6];

    for obs in observations {
        residual(output, obs, params, &mut r0);

        for p in 0..6 {
            let h = param_step(p);

            let mut pp = *params;
            pp[p] += h;
            residual(output, obs, &pp, &mut plus);

            let mut pm = *params;
            pm[p] -= h;
            residual(output, obs, &pm, &mut minus);

            for c in 0..3 {
                jrow[p][c] = (plus[c] - minus[c]) / (2.0 * h);
            }
        }

        for i in 0..6 {
            for c in 0..3 {
                jtr[i] += jrow[i][c] * r0[c];
            }

            for j in 0..6 {
                let mut dot = 0.0;

                for c in 0..3 {
                    dot += jrow[i][c] * jrow[j][c];
                }

                jtj[i][j] += dot;
            }
        }
    }

    (jtj, jtr)
}

/// Solves a 6x6 linear system by Gaussian elimination with partial pivoting. `None`
/// when the matrix is singular (degenerate target layout).
// The elimination and accumulation loops index several arrays in lockstep;
// iterator chains would obscure the linear algebra.
#[allow(clippy::needless_range_loop)]
fn solve6(a: &[[f64; 6]; 6], b: &[f64; 6]) -> Option<[f64; 6]> {
    let mut m = *a;
    let mut v = *b;

    for col in 0..6 {
        // Pivot on the largest remaining entry in this column.
        let mut pivot = col;

        for row in col + 1..6 {
            if m[row][col].abs() > m[pivot][col].abs() {
                pivot = row;
            }
        }

        if m[pivot][col].abs() < 1e-15 {
            return None;
        }

        m.swap(col, pivot);
        v.swap(col, pivot);

        for row in col + 1..6 {
            let f = m[row][col] / m[col][col];

            for k in col..6 {
                m[row][k] -= f * m[col][k];
            }

            v[row] -= f * v[col];
        }
    }

    let mut x = [0.0; 6];

    for col in (0..6).rev() {
        let mut sum = v[col];

        for k in col + 1..6 {
            sum -= m[col][k] * x[k];
        }

        x[col] = sum / m[col][col];
    }

    Some(x)
}

// --- Errors ---

/// Pose solve failure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, thiserror::Error)]
pub enum PoseError {
    #[error("{0} observations; the solve needs more")]
    TooFewObservations(usize),
    #[error("observation {0} is non-finite or has a non-unit ray")]
    BadObservation(usize),
    #[error("triangulated points are collinear; the pose is under-determined")]
    DegenerateLayout,
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A flat panel shaped like the small display.
    fn flat_panel() -> OutputGeometry {
        OutputGeometry {
            name          : "TEST".into(),
            enabled       : true,
            detect        : true,
            logical_x     : 0.0,
            logical_y     : 0.0,
            logical_w     : 1670.0,
            logical_h     : 1043.0,
            physical_w_mm : 237.0,
            physical_h_mm : 148.0,
            radius_mm     : 0.0,
            position_mm   : [-40.0, -75.0, 100.0],
            yaw_deg       : 3.0,
            pitch_deg     : -15.0,
            roll_deg      : 1.0,
        }
    }

    /// A curved panel shaped like the 27 inch display.
    fn curved_panel() -> OutputGeometry {
        OutputGeometry {
            name          : "CURVED".into(),
            enabled       : true,
            detect        : true,
            logical_x     : 0.0,
            logical_y     : 0.0,
            logical_w     : 2560.0,
            logical_h     : 1440.0,
            physical_w_mm : 600.0,
            physical_h_mm : 340.0,
            radius_mm     : 1500.0,
            position_mm   : [-300.0, 170.0, -60.0],
            yaw_deg       : 20.0,
            pitch_deg     : -2.0,
            roll_deg      : 0.0,
        }
    }

    /// Synthesises exact observations of `truth` from a 4x3 target grid, with the eye
    /// wobbling a little between fixations the way a real head does.
    fn observe(truth: &OutputGeometry) -> Vec<PoseObservation> {
        let mut obs = Vec::new();
        let eye     = DVec3::new(0.0, 180.0, 650.0);

        for row in 0..3 {
            for col in 0..4 {
                let u = 0.1 + 0.8 * col as f64 / 3.0;
                let v = 0.1 + 0.8 * row as f64 / 2.0;

                let wobble = DVec3::new(
                    ((row * 4 + col) as f64 * 0.7).sin() * 5.0,
                    ((row * 4 + col) as f64 * 1.3).cos() * 5.0,
                    ((row * 4 + col) as f64 * 0.5).sin() * 8.0,
                );

                let origin = eye + wobble;
                let target = truth.uv_to_world(u, v);

                obs.push(PoseObservation {
                    u         : u,
                    v         : v,
                    origin_mm : origin,
                    dir       : (target - origin).normalize(),
                });
            }
        }

        obs
    }

    /// The central diamond the plane pass triangulates, as exact points on `truth`.
    fn observe_points(truth: &OutputGeometry) -> Vec<PointObservation> {
        [(0.35, 0.5), (0.65, 0.5), (0.5, 0.35), (0.5, 0.65)]
            .into_iter()
            .map(|(u, v)| PointObservation {
                u        : u,
                v        : v,
                point_mm : truth.uv_to_world(u, v),
            })
            .collect()
    }

    /// Perturbs the pose the way an eyeballed config is wrong.
    fn perturb(truth: &OutputGeometry) -> OutputGeometry {
        let mut init = truth.clone();
        init.position_mm[0] += 25.0;
        init.position_mm[1] -= 18.0;
        init.position_mm[2] += 30.0;
        init.yaw_deg        -= 4.0;
        init.pitch_deg      += 3.0;
        init.roll_deg       -= 2.0;

        init
    }

    #[test]
    fn recovers_flat_pose_from_exact_rays() {
        let truth = flat_panel();
        let obs   = observe(&truth);
        let init  = perturb(&truth);

        let solved = solve_pose(&init, &obs).expect("solve");

        assert!(solved.rms_deg < 0.01, "rms {} deg", solved.rms_deg);

        for k in 0..3 {
            let err = (solved.output.position_mm[k] - truth.position_mm[k]).abs();
            assert!(err < 2.0, "position[{k}] off by {err} mm");
        }

        assert!((solved.output.yaw_deg - truth.yaw_deg).abs() < 0.3);
        assert!((solved.output.pitch_deg - truth.pitch_deg).abs() < 0.3);
    }

    #[test]
    fn recovers_curved_pose_from_exact_rays() {
        let truth = curved_panel();
        let obs   = observe(&truth);
        let init  = perturb(&truth);

        let solved = solve_pose(&init, &obs).expect("solve");

        assert!(solved.rms_deg < 0.01, "rms {} deg", solved.rms_deg);

        for k in 0..3 {
            let err = (solved.output.position_mm[k] - truth.position_mm[k]).abs();
            assert!(err < 3.0, "position[{k}] off by {err} mm");
        }
    }

    #[test]
    fn survives_noisy_rays() {
        let truth   = flat_panel();
        let mut obs = observe(&truth);

        // Half a degree of deterministic angular noise, roughly the device class.
        for (i, o) in obs.iter_mut().enumerate() {
            let axis  = DVec3::new((i as f64).sin(), (i as f64 * 2.0).cos(), 0.3).normalize();
            let quat  = glam::DQuat::from_axis_angle(axis, 0.5_f64.to_radians());
            o.dir = (quat * o.dir).normalize();
        }

        let solved = solve_pose(&perturb(&truth), &obs).expect("solve");

        // Noise this size should leave the pose close and the residual near the noise
        // floor rather than absorbing it into a wild pose.
        assert!(solved.rms_deg < 0.6, "rms {} deg", solved.rms_deg);
        assert!((solved.output.pitch_deg - truth.pitch_deg).abs() < 2.5);
    }

    #[test]
    fn rejects_underdetermined_input() {
        let truth = flat_panel();
        let obs   = observe(&truth);

        let err = solve_pose(&truth, &obs[..3]).unwrap_err();
        assert_eq!(err, PoseError::TooFewObservations(3));
    }

    #[test]
    fn recovers_flat_pose_from_exact_points() {
        let truth  = flat_panel();
        let points = observe_points(&truth);
        let init   = perturb(&truth);

        let solved = solve_pose_points(&init, &points).expect("solve");

        assert!(solved.rms_mm < 0.01, "rms {} mm", solved.rms_mm);

        for k in 0..3 {
            let err = (solved.output.position_mm[k] - truth.position_mm[k]).abs();
            assert!(err < 0.5, "position[{k}] off by {err} mm");
        }

        assert!((solved.output.pitch_deg - truth.pitch_deg).abs() < 0.2);
        assert!((solved.output.yaw_deg - truth.yaw_deg).abs() < 0.2);
    }

    #[test]
    fn recovers_curved_pose_from_exact_points() {
        let truth  = curved_panel();
        let points = observe_points(&truth);
        let init   = perturb(&truth);

        let solved = solve_pose_points(&init, &points).expect("solve");

        assert!(solved.rms_mm < 0.01, "rms {} mm", solved.rms_mm);

        for k in 0..3 {
            let err = (solved.output.position_mm[k] - truth.position_mm[k]).abs();
            assert!(err < 1.0, "position[{k}] off by {err} mm");
        }
    }

    #[test]
    fn noisy_points_stay_bounded() {
        let truth      = flat_panel();
        let mut points = observe_points(&truth);

        // A few millimetres of deterministic triangulation noise per point.
        for (i, p) in points.iter_mut().enumerate() {
            p.point_mm += DVec3::new(
                (i as f64 * 1.1).sin() * 3.0,
                (i as f64 * 2.3).cos() * 3.0,
                (i as f64 * 0.7).sin() * 3.0,
            );
        }

        let solved = solve_pose_points(&perturb(&truth), &points).expect("solve");

        for k in 0..3 {
            let err = (solved.output.position_mm[k] - truth.position_mm[k]).abs();
            assert!(err < 15.0, "position[{k}] off by {err} mm");
        }

        // The lever arm is the central diamond, so a few mm of noise may cost a
        // degree or two of orientation, not more.
        assert!((solved.output.pitch_deg - truth.pitch_deg).abs() < 3.5);
    }

    #[test]
    fn rejects_collinear_points() {
        let truth = flat_panel();

        let points: Vec<PointObservation> = [(0.2, 0.5), (0.5, 0.5), (0.8, 0.5)]
            .into_iter()
            .map(|(u, v)| PointObservation {
                u        : u,
                v        : v,
                point_mm : truth.uv_to_world(u, v),
            })
            .collect();

        let err = solve_pose_points(&truth, &points).unwrap_err();
        assert_eq!(err, PoseError::DegenerateLayout);
    }

    #[test]
    fn rejects_too_few_points() {
        let truth  = flat_panel();
        let points = observe_points(&truth);

        let err = solve_pose_points(&truth, &points[..2]).unwrap_err();
        assert_eq!(err, PoseError::TooFewObservations(2));
    }
}
