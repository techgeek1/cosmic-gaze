//! Stage one of the calibration: the map from the gaze angles a model reports to the
//! angles it should have reported, both in the camera frame, both in degrees.
//!
//! Two shapes are available and the fit picks between them on held-out error.
//!
//! A **polynomial** ([`crate::fit::AnglePoly`]) is global: every coefficient affects every
//! angle. That is a virtue when the error really is a smooth global distortion and a
//! liability when it is not. This desk's l2cs stream is not: its yaw map has a kink on the
//! negative side, with a slope near 2.3 close to the camera axis falling to 0.4 past -20
//! degrees, while the positive side is nearly linear. A global cubic can only follow that
//! by bending everywhere, which is what put the pointer at half the distance to the panel
//! edges while every fitted number looked healthy.
//!
//! A **thin-plate spline** ([`TpsWarp`]) has local support: each calibration target carries
//! its own basis function, so a bend near the camera axis costs nothing on the far side of
//! the desk. It is the natural fit for a kink. Its smoothing parameter buys back the
//! generality a polynomial gets for free, and it is swept and chosen the same way
//! everything else is.
//!
//! # Extrapolation
//!
//! A thin-plate spline's radial term grows like `r^2 log r`, so a query far outside the
//! calibrated region diverges. The query is therefore clamped into the fitted range before
//! the radial part is evaluated, while the affine part is evaluated at the true angle. The
//! result is that inside the calibrated region the spline is exact, and outside it the
//! correction continues along the linear trend the sweep measured with the edge's local
//! offset frozen on. That is bounded, continuous, and the only honest thing to do where
//! there is no data.

use serde::{Deserialize, Serialize};

use crate::fit::{ANGLE_SCALE_DEG, AngleDegree, AnglePoly, AngleRow, solve_dense};

/// Step used for the numeric derivative when measuring a correction's local gain, degrees.
const GAIN_STEP_DEG: f64 = 1.0;

/// Stage one, in whichever shape won.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "lowercase")]
pub enum AngleCorrection {
    /// A global 2D polynomial.
    Poly(AnglePoly),
    /// A thin-plate spline over the calibration targets.
    Tps(TpsWarp),
}

/// A 2D thin-plate spline from reported angles to intended angles.
///
/// One centre per calibration target. `lambda` is the smoothing: zero interpolates every
/// target exactly, larger values trade that for a flatter surface between them.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TpsWarp {
    /// Centres, in scaled units (degrees divided by `input_scale_deg`), as `[u, v]` pairs.
    pub centres         : Vec<[f64; 2]>,
    /// Radial weights producing the corrected yaw, one per centre.
    pub yaw_weights     : Vec<f64>,
    /// Affine part producing the corrected yaw: `[constant, u, v]`.
    pub yaw_affine      : [f64; 3],
    pub pitch_weights   : Vec<f64>,
    pub pitch_affine    : [f64; 3],
    /// Scale applied to both axes before and after, so the stored numbers are
    /// dimensionless. See `crate::fit::ANGLE_SCALE_DEG`.
    pub input_scale_deg : f64,
    /// Smoothing used for the fit. Recorded so a file explains itself.
    pub lambda          : f64,
    /// Reported-angle range the fit saw, degrees, `[yaw_lo, yaw_hi, pitch_lo, pitch_hi]`.
    /// Queries outside it are clamped for the radial part; see the module docs.
    pub range_deg       : [f64; 4],
}

// --- AngleCorrection ---

impl AngleCorrection {
    /// The correction that changes nothing.
    pub fn identity() -> Self {
        AngleCorrection::Poly(AnglePoly::identity())
    }

    /// Applies the correction to a reported `(yaw, pitch)` in degrees.
    pub fn apply(&self, yaw_deg: f64, pitch_deg: f64) -> (f64, f64) {
        match self {
            AngleCorrection::Poly(p) => p.apply(yaw_deg, pitch_deg),
            AngleCorrection::Tps(t)  => t.apply(yaw_deg, pitch_deg),
        }
    }

    /// True when the stored coefficients are consistent and finite.
    pub fn is_well_formed(&self) -> bool {
        match self {
            AngleCorrection::Poly(p) => p.is_well_formed(),
            AngleCorrection::Tps(t)  => t.is_well_formed(),
        }
    }

    /// Short name for a report.
    pub fn name(&self) -> String {
        match self {
            AngleCorrection::Poly(p) => format!("{:?}", p.degree).to_lowercase(),
            AngleCorrection::Tps(t)  => format!("tps l={}", trim(t.lambda)),
        }
    }

    /// Local gain at each of `points`: how far the corrected angle moves per degree the
    /// reported angle moves. Returns `(yaw, pitch, yaw_gain, pitch_gain)` per point.
    ///
    /// This is the shape of the correction *between* the calibration targets, which is
    /// where a person spends almost all of their time and which no residual can see. The
    /// caller chooses the points, and must choose them where the user will actually look:
    /// judging a fit in a corner of the angle range that the sweep never visited measures
    /// nothing but extrapolation.
    pub fn gain_profile(&self, points: &[(f64, f64)]) -> Vec<(f64, f64, f64, f64)> {
        let h = GAIN_STEP_DEG;

        points
            .iter()
            .map(|&(yaw, pitch)| {
                let dy = (self.apply(yaw + h, pitch).0 - self.apply(yaw - h, pitch).0) / (2.0 * h);
                let dp = (self.apply(yaw, pitch + h).1 - self.apply(yaw, pitch - h).1) / (2.0 * h);

                (yaw, pitch, dy, dp)
            })
            .collect()
    }

    /// Weakest and strongest local gain over `points`, or `None` when the correction is not
    /// finite there. Reported so a threshold is a judgement about a measured number rather
    /// than an unexplained rejection.
    pub fn gain_bounds(&self, points: &[(f64, f64)]) -> Option<(f64, f64)> {
        if points.is_empty() {
            return None;
        }

        let gains: Vec<f64> = self
            .gain_profile(points)
            .iter()
            .flat_map(|(_, _, y, p)| [*y, *p])
            .collect();

        if !gains.iter().all(|g| g.is_finite()) {
            return None;
        }

        Some((
            gains.iter().copied().fold(f64::INFINITY, f64::min),
            gains.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ))
    }

    /// Whether the correction behaves like a gaze correction over the region it will be
    /// used on, rather than merely passing through the calibration targets.
    ///
    /// Below `min_gain` the correction has flattened and a region of screen collapses to a
    /// point; a negative value has folded the field over. Above `max_gain` it is amplifying
    /// hard enough that the model must be reporting a small fraction of the true angle
    /// there, which no correction can rescue.
    pub fn is_sane(&self, points: &[(f64, f64)], min_gain: f64, max_gain: f64) -> bool {
        match self.gain_bounds(points) {
            Some((lo, hi)) => lo >= min_gain && hi <= max_gain,
            None           => false,
        }
    }
}

impl Default for AngleCorrection {
    fn default() -> Self {
        Self::identity()
    }
}

// --- TpsWarp ---

impl TpsWarp {
    /// Fits a spline to `rows`, one centre per row.
    ///
    /// `None` when there are too few rows to determine the affine part, or when the system
    /// is singular (every centre in one place).
    pub fn fit(rows: &[AngleRow], lambda: f64) -> Option<Self> {
        let n = rows.len();

        if n < 4 {
            return None;
        }

        let scale = ANGLE_SCALE_DEG;
        let dim   = n + 3;

        let centres: Vec<[f64; 2]> = rows
            .iter()
            .map(|r| [r.yaw_deg / scale, r.pitch_deg / scale])
            .collect();

        // L = [[K + lambda I, P], [P^T, 0]], the standard thin-plate system. The zero block
        // and the P^T rows are what force the radial weights to have no affine component of
        // their own, so the affine part is the surface's global trend.
        let mut l = vec![0.0_f64; dim * dim];

        for i in 0..n {
            for j in 0..n {
                let dx = centres[i][0] - centres[j][0];
                let dy = centres[i][1] - centres[j][1];

                l[i * dim + j] = kernel((dx * dx + dy * dy).sqrt());
            }

            l[i * dim + i] += lambda;

            for (k, value) in [1.0, centres[i][0], centres[i][1]].into_iter().enumerate() {
                l[i * dim + n + k]  = value;
                l[(n + k) * dim + i] = value;
            }
        }

        let solve_axis = |target: &dyn Fn(&AngleRow) -> f64| -> Option<Vec<f64>> {
            let mut rhs = vec![0.0_f64; dim];

            for (i, row) in rows.iter().enumerate() {
                rhs[i] = target(row) / scale;
            }

            solve_dense(&l, &rhs, dim)
        };

        let yaw   = solve_axis(&|r: &AngleRow| r.want_yaw_deg)?;
        let pitch = solve_axis(&|r: &AngleRow| r.want_pitch_deg)?;

        let bound = |f: &dyn Fn(&AngleRow) -> f64| {
            let lo = rows.iter().map(f).fold(f64::INFINITY, f64::min);
            let hi = rows.iter().map(f).fold(f64::NEG_INFINITY, f64::max);

            (lo, hi)
        };

        let (yaw_lo, yaw_hi)     = bound(&|r: &AngleRow| r.yaw_deg);
        let (pitch_lo, pitch_hi) = bound(&|r: &AngleRow| r.pitch_deg);

        Some(Self {
            centres         : centres,
            yaw_weights     : yaw[..n].to_vec(),
            yaw_affine      : [yaw[n], yaw[n + 1], yaw[n + 2]],
            pitch_weights   : pitch[..n].to_vec(),
            pitch_affine    : [pitch[n], pitch[n + 1], pitch[n + 2]],
            input_scale_deg : scale,
            lambda          : lambda,
            range_deg       : [yaw_lo, yaw_hi, pitch_lo, pitch_hi],
        })
    }

    /// Applies the spline.
    pub fn apply(&self, yaw_deg: f64, pitch_deg: f64) -> (f64, f64) {
        let scale = {
            if self.input_scale_deg.is_finite() && self.input_scale_deg > 0.0 {
                self.input_scale_deg
            }
            else {
                ANGLE_SCALE_DEG
            }
        };

        // The affine part is evaluated where the caller actually is; the radial part is
        // evaluated at the nearest point the sweep covered. See the module docs.
        let u = yaw_deg / scale;
        let v = pitch_deg / scale;

        let cu = yaw_deg.clamp(self.range_deg[0], self.range_deg[1]) / scale;
        let cv = pitch_deg.clamp(self.range_deg[2], self.range_deg[3]) / scale;

        let mut yaw   = self.yaw_affine[0] + self.yaw_affine[1] * u + self.yaw_affine[2] * v;
        let mut pitch = self.pitch_affine[0] + self.pitch_affine[1] * u + self.pitch_affine[2] * v;

        for (i, c) in self.centres.iter().enumerate() {
            let dx = cu - c[0];
            let dy = cv - c[1];
            let k  = kernel((dx * dx + dy * dy).sqrt());

            yaw   += self.yaw_weights[i] * k;
            pitch += self.pitch_weights[i] * k;
        }

        (yaw * scale, pitch * scale)
    }

    /// True when the stored arrays agree in length and every number is finite.
    pub fn is_well_formed(&self) -> bool {
        let n = self.centres.len();

        n >= 4
            && self.yaw_weights.len() == n
            && self.pitch_weights.len() == n
            && self.centres.iter().flatten().all(|c| c.is_finite())
            && self.yaw_weights.iter().all(|w| w.is_finite())
            && self.pitch_weights.iter().all(|w| w.is_finite())
            && self.yaw_affine.iter().all(|a| a.is_finite())
            && self.pitch_affine.iter().all(|a| a.is_finite())
            && self.range_deg.iter().all(|r| r.is_finite())
    }
}

/// Formats a smoothing value compactly for a report: `0.1` rather than `0.100000`.
fn trim(v: f64) -> String {
    let s = format!("{v}");

    s.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// The thin-plate radial basis, `r^2 ln r`, with the removable singularity at zero filled
/// in. This is the function that minimises bending energy in two dimensions, which is what
/// makes the surface between the targets the smoothest one that fits them.
fn kernel(r: f64) -> f64 {
    if r <= 1.0e-12 {
        return 0.0;
    }

    r * r * r.ln()
}

/// Every stage-one shape the fit considers, cheapest first.
///
/// The polynomials come first so that a tie goes to the simpler and more predictable model:
/// a spline carries one basis function per calibration target and a polynomial carries six.
pub fn candidates() -> Vec<Shape> {
    let mut out = vec![
        Shape::Poly(AngleDegree::Quadratic),
        Shape::Poly(AngleDegree::Cubic),
    ];

    // Smoothing is swept rather than guessed. Too little interpolates the sweep's noise;
    // too much flattens the kink the spline was chosen for.
    for lambda in [0.03, 0.1, 0.3, 1.0] {
        out.push(Shape::Tps(lambda));
    }

    out
}

/// One stage-one shape, before it has been fitted.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Shape {
    Poly(AngleDegree),
    Tps(f64),
}

// --- Shape ---

impl Shape {
    /// Fits this shape.
    ///
    /// `samples` is every sample of every target; `targets` is one row per target, the mean
    /// of its samples. A polynomial uses the samples, because averaging is exactly what
    /// least squares does with them and a target with more samples has genuinely told us
    /// more. A spline uses the targets, because it places one basis function per row: given
    /// the samples it would put a basis function on every one and interpolate the sweep's
    /// own jitter, which is not a correction but a recording of the noise.
    pub fn fit(self, samples: &[AngleRow], targets: &[AngleRow]) -> Option<AngleCorrection> {
        match self {
            Shape::Poly(degree) => AnglePoly::fit(samples, degree).map(AngleCorrection::Poly),
            Shape::Tps(lambda)  => TpsWarp::fit(targets, lambda).map(AngleCorrection::Tps),
        }
    }

    /// Short name for a report.
    pub fn name(self) -> String {
        match self {
            Shape::Poly(d) => format!("{d:?}").to_lowercase(),
            Shape::Tps(l)  => format!("tps l={}", trim(l)),
        }
    }
}
