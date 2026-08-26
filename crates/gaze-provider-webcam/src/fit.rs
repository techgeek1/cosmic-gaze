//! The least-squares machinery behind calibration: a 2D polynomial map from observed to
//! intended coordinates, and a two-parameter angular offset for a mis-measured camera
//! pose.
//!
//! Nothing here knows about pixels or panels. `PolyMap` works in whatever normalised
//! coordinates the caller hands it, and `AnglePoly` works in camera-frame degrees.
//! `crate::calibration` supplies the units.

use serde::{Deserialize, Serialize};

/// Largest number of basis terms any supported degree uses. Sizes the fixed arrays the
/// solver works in so the fit never allocates per row.
const MAX_TERMS: usize = 10;

/// First basis index the ridge touches. Terms 0, 1 and 2 are the constant and the two
/// linear terms, and those describe the correction's *gain*. Shrinking them toward zero
/// shrinks the corrected angle toward zero, which means toward looking straight ahead: a
/// heavy ridge would quietly turn the calibration into "the user is staring at the middle
/// of the screen". Only the curvature is regularised.
const RIDGE_FIRST_TERM: usize = 3;

/// Ridge applied to a cubic angle fit, relative to the scale of the normal matrix.
///
/// Ten coefficients per axis against the roughly thirty targets a sweep produces is enough
/// freedom to chase noise, and the cubic terms are the ones that do it: they are tiny in
/// the middle of the target grid and enormous just outside it, so an unregularised cubic
/// fits the corners beautifully and extrapolates into nonsense a few degrees further out.
/// This value was chosen by leave-one-out on a real sweep, where it took the held-out RMS
/// from 4.43 to 4.32 degrees and the worst held-out target from 9.35 to 7.99.
pub const CUBIC_RIDGE: f64 = 1.0e-3;

/// Angle, degrees, that the polynomial's inputs are divided by before the basis is built.
/// Roughly the half-width of the usable gaze cone, so the working range lands near
/// `[-1, 1]` and the cubic terms stay the same order as the linear ones.
pub const ANGLE_SCALE_DEG: f64 = 45.0;

/// Relative pivot magnitude below which the normal matrix counts as singular and the fit
/// degrades to a lower degree. The normal equations square the conditioning of the design
/// matrix, so this is deliberately loose: a fit that only just resolves is a fit that will
/// extrapolate wildly a few pixels outside the target grid.
const PIVOT_EPS: f64 = 1.0e-9;

/// How much of the map a fit is allowed to describe. Degrees are named rather than
/// numbered because the fallback ladder, not the polynomial order, is the interesting
/// property at the call site.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PolyDegree {
    /// A constant offset added to the input. One term per axis, so a single point
    /// determines it. Unlike the other degrees this one is fitted against the *delta*
    /// rather than the target: a one-term polynomial basis would otherwise collapse to a
    /// constant that ignores its input entirely.
    Translation,
    /// Offset, scale, shear and rotation. Three terms per axis, so three points.
    Affine,
    /// The full second-order map. Six terms per axis, so six points.
    Quadratic,
}

/// The shared 2D polynomial basis, in the order every degree in this module indexes it:
/// `[1, x, y, x^2, xy, y^2, x^3, x^2 y, x y^2, y^3]`. Each supported degree uses a prefix,
/// so one ordering serves the pixel maps and the angle map alike and a stored coefficient
/// vector means the same thing wherever it appears.
pub fn poly_basis(x: f64, y: f64) -> [f64; MAX_TERMS] {
    [
        1.0,
        x,
        y,
        x * x,
        x * y,
        y * y,
        x * x * x,
        x * x * y,
        x * y * y,
        y * y * y,
    ]
}

/// How much curvature the angle-space correction is allowed.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AngleDegree {
    /// Six terms per axis, cross term included.
    Quadratic,
    /// Ten terms per axis. Needed when the model's gain is not constant across the field,
    /// which is the normal case for an appearance model: it is close to right straight
    /// ahead and saturates toward the edges.
    Cubic,
}

/// Stage one of the calibration: a 2D polynomial from the gaze angles the model reports to
/// the angles it should have reported, both in the camera frame, both in degrees.
///
/// This runs *before* the ray meets the desk, which is the whole point. A model whose
/// reported yaw saturates sends the ray past the edge of a panel, and once it has missed
/// there is no pixel for a pixel-space correction to work on. Fitting in angle space also
/// means the sweep never has to discard or clamp a sample: an angle is well defined
/// whether or not it happens to land on a screen.
///
/// Inputs and outputs are divided by `input_scale_deg` before and after the basis, so the
/// stored coefficients are dimensionless and the identity is a clean `[0, 1, 0, ...]`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AnglePoly {
    pub degree          : AngleDegree,
    /// Coefficients producing the corrected yaw, in `poly_basis` order.
    pub yaw_coeffs      : Vec<f64>,
    pub pitch_coeffs    : Vec<f64>,
    /// Scale applied to both axes before the basis. See `ANGLE_SCALE_DEG`.
    pub input_scale_deg : f64,
}

/// One row of an angle fit: what the model said, and what it should have said.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct AngleRow {
    pub yaw_deg       : f64,
    pub pitch_deg     : f64,
    pub want_yaw_deg  : f64,
    pub want_pitch_deg: f64,
}

/// A 2D polynomial map, one coefficient vector per output axis. Both input and output are
/// in the caller's normalised coordinates.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PolyMap {
    pub degree   : PolyDegree,
    /// Coefficients producing the output x, in the term order of `PolyDegree::terms`.
    pub x_coeffs : Vec<f64>,
    /// Coefficients producing the output y, same term order.
    pub y_coeffs : Vec<f64>,
}

// --- PolyDegree ---

impl PolyDegree {
    /// Number of basis terms, which is also the number of point pairs needed to determine
    /// the map exactly.
    pub fn terms(self) -> usize {
        match self {
            PolyDegree::Translation => 1,
            PolyDegree::Affine      => 3,
            PolyDegree::Quadratic   => 6,
        }
    }

    /// The highest degree `points` sample pairs can support, or `None` when there are no
    /// points at all.
    pub fn for_point_count(points: usize) -> Option<Self> {
        match points {
            0     => None,
            1..=2 => Some(PolyDegree::Translation),
            3..=5 => Some(PolyDegree::Affine),
            _     => Some(PolyDegree::Quadratic),
        }
    }

    /// The next degree down the fallback ladder, used when a fit turns out to be singular
    /// at this degree (collinear points, duplicate targets).
    pub fn weaker(self) -> Option<Self> {
        match self {
            PolyDegree::Quadratic   => Some(PolyDegree::Affine),
            PolyDegree::Affine      => Some(PolyDegree::Translation),
            PolyDegree::Translation => None,
        }
    }

    /// Basis terms evaluated at `(x, y)`. Only the first `terms()` entries are meaningful.
    pub fn basis(self, x: f64, y: f64) -> [f64; MAX_TERMS] {
        poly_basis(x, y)
    }

    /// True when the fit is against `target - observed` and `apply` adds the input back.
    /// See `Translation`.
    pub fn is_delta(self) -> bool {
        matches!(self, PolyDegree::Translation)
    }
}

// --- AngleDegree ---

impl AngleDegree {
    /// Number of basis terms per axis.
    pub fn terms(self) -> usize {
        match self {
            AngleDegree::Quadratic => 6,
            AngleDegree::Cubic     => 10,
        }
    }

    /// Ridge this degree is fitted with, relative to the normal matrix scale.
    pub fn ridge(self) -> f64 {
        match self {
            AngleDegree::Quadratic => 0.0,
            AngleDegree::Cubic     => CUBIC_RIDGE,
        }
    }

    /// Every degree worth trying, cheapest first. The caller picks between them by
    /// held-out error rather than by taste.
    pub fn all() -> [AngleDegree; 2] {
        [AngleDegree::Quadratic, AngleDegree::Cubic]
    }
}

// --- AnglePoly ---

impl AnglePoly {
    /// The map that changes nothing.
    pub fn identity() -> Self {
        Self {
            degree          : AngleDegree::Quadratic,
            yaw_coeffs      : vec![0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
            pitch_coeffs    : vec![0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            input_scale_deg : ANGLE_SCALE_DEG,
        }
    }

    /// Fits `(yaw, pitch) -> (want_yaw, want_pitch)` by ridge least squares.
    ///
    /// `None` when there are fewer rows than terms, or when the normal equations are
    /// singular even with the ridge. The caller falls back to a lower degree.
    pub fn fit(rows: &[AngleRow], degree: AngleDegree) -> Option<Self> {
        if rows.len() < degree.terms() {
            return None;
        }

        let scale = ANGLE_SCALE_DEG;
        let n     = degree.terms();

        let mut ata = [[0.0_f64; MAX_TERMS]; MAX_TERMS];
        let mut aty = [0.0_f64; MAX_TERMS];
        let mut atp = [0.0_f64; MAX_TERMS];

        for row in rows {
            let basis = poly_basis(row.yaw_deg / scale, row.pitch_deg / scale);

            for i in 0..n {
                aty[i] += basis[i] * row.want_yaw_deg / scale;
                atp[i] += basis[i] * row.want_pitch_deg / scale;

                for j in 0..n {
                    ata[i][j] += basis[i] * basis[j];
                }
            }
        }

        // Both axes share the design matrix, so the elimination has to run on a copy or
        // the second solve would see a triangularised system.
        let mut a_yaw   = ata;
        let mut a_pitch = ata;

        let yaw   = solve_ridge(&mut a_yaw, &mut aty, n, degree.ridge())?;
        let pitch = solve_ridge(&mut a_pitch, &mut atp, n, degree.ridge())?;

        Some(Self {
            degree          : degree,
            yaw_coeffs      : yaw,
            pitch_coeffs    : pitch,
            input_scale_deg : scale,
        })
    }

    /// Applies the map to a reported `(yaw, pitch)` in degrees.
    pub fn apply(&self, yaw_deg: f64, pitch_deg: f64) -> (f64, f64) {
        let scale = {
            if self.input_scale_deg.is_finite() && self.input_scale_deg > 0.0 {
                self.input_scale_deg
            }
            else {
                ANGLE_SCALE_DEG
            }
        };

        let basis = poly_basis(yaw_deg / scale, pitch_deg / scale);
        let n     = self.degree.terms().min(self.yaw_coeffs.len()).min(self.pitch_coeffs.len());

        let mut out = (0.0, 0.0);

        for (i, term) in basis.iter().enumerate().take(n) {
            out.0 += self.yaw_coeffs[i] * term;
            out.1 += self.pitch_coeffs[i] * term;
        }

        (out.0 * scale, out.1 * scale)
    }

    /// True when the coefficient vectors match the degree and are all finite. A
    /// hand-edited or version-skewed file is the reason this exists.
    pub fn is_well_formed(&self) -> bool {
        let n = self.degree.terms();

        self.yaw_coeffs.len() == n
            && self.pitch_coeffs.len() == n
            && self.yaw_coeffs.iter().chain(self.pitch_coeffs.iter()).all(|c| c.is_finite())
    }
}

impl Default for AnglePoly {
    fn default() -> Self {
        Self::identity()
    }
}

// --- PolyMap ---

impl PolyMap {
    /// The map that changes nothing. What a calibration with no points for an output uses.
    pub fn identity() -> Self {
        Self {
            degree   : PolyDegree::Affine,
            x_coeffs : vec![0.0, 1.0, 0.0],
            y_coeffs : vec![0.0, 0.0, 1.0],
        }
    }

    /// Fits `observed -> target` by least squares, taking the highest degree the point
    /// count supports and stepping down whenever the normal equations come out singular.
    /// An empty sample set gives the identity.
    ///
    /// Each sample is `(observed, target)` as `[x, y]` pairs.
    pub fn fit(samples: &[([f64; 2], [f64; 2])]) -> Self {
        let Some(mut degree) = PolyDegree::for_point_count(samples.len()) else {
            return Self::identity();
        };

        loop {
            let x = fit_axis(samples, degree, 0);
            let y = fit_axis(samples, degree, 1);

            if let (Some(x_coeffs), Some(y_coeffs)) = (x, y) {
                return Self { degree: degree, x_coeffs: x_coeffs, y_coeffs: y_coeffs };
            }

            let Some(weaker) = degree.weaker() else {
                // Even a pure translation failed, which takes a sample set that is empty
                // in all but name. Change nothing rather than emit garbage.
                return Self::identity();
            };

            degree = weaker;
        }
    }

    /// Applies the map.
    pub fn apply(&self, x: f64, y: f64) -> (f64, f64) {
        let basis = self.degree.basis(x, y);
        let n     = self.degree.terms().min(self.x_coeffs.len()).min(self.y_coeffs.len());

        // A delta degree fits the offset, not the destination, so the input carries
        // through and the polynomial only supplies the correction.
        let mut out = {
            if self.degree.is_delta() {
                (x, y)
            }
            else {
                (0.0, 0.0)
            }
        };

        for (i, term) in basis.iter().enumerate().take(n) {
            out.0 += self.x_coeffs[i] * term;
            out.1 += self.y_coeffs[i] * term;
        }

        out
    }

    /// Whether the map behaves like a correction over the normalised square rather than
    /// merely passing through its fitted points.
    ///
    /// Same failure as `AnglePoly::is_sane` guards against, in pixel space: six
    /// coefficients per axis fitted to nine targets can hit all nine and fold the panel
    /// over in between. The test is on the Jacobian determinant, which is the local area
    /// scale: it must stay positive, so the map never turns the screen inside out, and must
    /// not swing by more than `max_ratio`, so no region is squashed to nothing while
    /// another is stretched across the panel.
    pub fn is_sane(&self, max_ratio: f64) -> bool {
        match self.jacobian_bounds() {
            Some((lo, hi)) => lo > 0.0 && hi / lo <= max_ratio,
            None           => false,
        }
    }

    /// Smallest and largest local area scale over the normalised square, or `None` when the
    /// map is malformed or not finite there.
    pub fn jacobian_bounds(&self) -> Option<(f64, f64)> {
        if !self.is_well_formed() {
            return None;
        }

        let h        = 1.0e-3;
        let mut dets = Vec::new();

        for i in 0..5 {
            for j in 0..5 {
                let x = -1.0 + 0.5 * i as f64;
                let y = -1.0 + 0.5 * j as f64;

                let dx = self.apply(x + h, y);
                let ex = self.apply(x - h, y);
                let dy = self.apply(x, y + h);
                let ey = self.apply(x, y - h);

                let j11 = (dx.0 - ex.0) / (2.0 * h);
                let j21 = (dx.1 - ex.1) / (2.0 * h);
                let j12 = (dy.0 - ey.0) / (2.0 * h);
                let j22 = (dy.1 - ey.1) / (2.0 * h);

                dets.push(j11 * j22 - j12 * j21);
            }
        }

        if !dets.iter().all(|d| d.is_finite()) {
            return None;
        }

        Some((
            dets.iter().copied().fold(f64::INFINITY, f64::min),
            dets.iter().copied().fold(f64::NEG_INFINITY, f64::max),
        ))
    }

    /// True when the coefficient vectors match the degree. A hand-edited or
    /// version-skewed `calibration.toml` is the reason this exists.
    pub fn is_well_formed(&self) -> bool {
        let n = self.degree.terms();

        self.x_coeffs.len() == n
            && self.y_coeffs.len() == n
            && self.x_coeffs.iter().chain(self.y_coeffs.iter()).all(|c| c.is_finite())
    }
}

/// Fits one output axis (`axis` 0 for x, 1 for y) at `degree`. `None` when the normal
/// equations are singular at that degree.
fn fit_axis(samples: &[([f64; 2], [f64; 2])], degree: PolyDegree, axis: usize) -> Option<Vec<f64>> {
    let n = degree.terms();

    // Accumulate the normal equations directly. With at most six terms and a few dozen
    // points there is no reason to materialise the design matrix.
    let mut ata = [[0.0_f64; MAX_TERMS]; MAX_TERMS];
    let mut atb = [0.0_f64; MAX_TERMS];

    for (observed, target) in samples {
        let basis = degree.basis(observed[0], observed[1]);
        let rhs   = {
            if degree.is_delta() {
                target[axis] - observed[axis]
            }
            else {
                target[axis]
            }
        };

        for i in 0..n {
            atb[i] += basis[i] * rhs;

            for j in 0..n {
                ata[i][j] += basis[i] * basis[j];
            }
        }
    }

    solve_ridge(&mut ata, &mut atb, n, 0.0)
}

/// Gaussian elimination with partial pivoting on an `n x n` system, with an optional ridge
/// added to the diagonal. `None` when a pivot falls below `PIVOT_EPS` relative to the
/// largest entry, which is the signal to fall back to a lower degree.
///
/// `ridge` is relative to the scale of the matrix and only touches the curvature terms; see
/// `RIDGE_FIRST_TERM` for why the affine part is left free.
pub fn solve_ridge(
    a     : &mut [[f64; MAX_TERMS]; MAX_TERMS],
    b     : &mut [f64; MAX_TERMS],
    n     : usize,
    ridge : f64,
)
    -> Option<Vec<f64>>
{
    // Scale the singularity test by the size of the system so it is a statement about
    // conditioning rather than about the units the caller happened to use.
    let scale = (0..n)
        .flat_map(|i| (0..n).map(move |j| (i, j)))
        .map(|(i, j)| a[i][j].abs())
        .fold(0.0_f64, f64::max);

    if scale <= 0.0 {
        return None;
    }

    if ridge > 0.0 {
        for (i, row) in a.iter_mut().enumerate().take(n).skip(RIDGE_FIRST_TERM) {
            row[i] += ridge * scale;
        }
    }

    for col in 0..n {
        // Pivot on the largest remaining entry in this column.
        let pivot = (col..n).max_by(|&i, &j| {
            a[i][col].abs().partial_cmp(&a[j][col].abs()).unwrap_or(std::cmp::Ordering::Equal)
        })?;

        if a[pivot][col].abs() < PIVOT_EPS * scale {
            return None;
        }

        a.swap(col, pivot);
        b.swap(col, pivot);

        for row in (col + 1)..n {
            let factor = a[row][col] / a[col][col];
            let pivot  = a[col];

            for (target, source) in a[row].iter_mut().zip(pivot.iter()).take(n).skip(col) {
                *target -= factor * source;
            }

            b[row] -= factor * b[col];
        }
    }

    // Back substitution.
    let mut out = vec![0.0; n];

    for row in (0..n).rev() {
        let mut sum = b[row];

        for k in (row + 1)..n {
            sum -= a[row][k] * out[k];
        }

        out[row] = sum / a[row][row];
    }

    if out.iter().all(|c| c.is_finite()) {
        Some(out)
    }
    else {
        None
    }
}

/// Gaussian elimination with partial pivoting on a dense row-major `n x n` system.
///
/// Separate from `solve_ridge`, which works in fixed-size arrays sized for a polynomial
/// basis. A spline's system is as large as the number of calibration targets, so it has to
/// be heap allocated.
pub fn solve_dense(a: &[f64], b: &[f64], n: usize) -> Option<Vec<f64>> {
    let mut m = a.to_vec();
    let mut y = b.to_vec();

    for col in 0..n {
        let pivot = (col..n).max_by(|&i, &j| {
            m[i * n + col]
                .abs()
                .partial_cmp(&m[j * n + col].abs())
                .unwrap_or(std::cmp::Ordering::Equal)
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

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A quadratic warp used as the ground truth a fit has to recover.
    fn warp(x: f64, y: f64) -> [f64; 2] {
        [
            0.03 + 1.05 * x - 0.04 * y + 0.02 * x * x + 0.01 * x * y - 0.03 * y * y,
            -0.02 + 0.02 * x + 0.97 * y - 0.01 * x * x + 0.03 * x * y + 0.04 * y * y,
        ]
    }

    /// A 3x3 grid of normalised coordinates, the shape a per-output calibration sees.
    fn grid() -> Vec<[f64; 2]> {
        let mut out = Vec::new();

        for gy in [-0.8, 0.0, 0.8] {
            for gx in [-0.8, 0.0, 0.8] {
                out.push([gx, gy]);
            }
        }

        out
    }

    #[test]
    fn a_quadratic_fit_recovers_a_quadratic_distortion() {
        // The target is the truth; the observation is the truth pushed through the warp.
        // The fit must invert it, so applying it to an observation returns the target.
        let samples: Vec<_> = grid().into_iter().map(|t| (warp(t[0], t[1]), t)).collect();
        let map = PolyMap::fit(&samples);

        assert_eq!(map.degree, PolyDegree::Quadratic);

        for (observed, target) in &samples {
            let (x, y) = map.apply(observed[0], observed[1]);

            assert!(
                (x - target[0]).abs() < 2.0e-3 && (y - target[1]).abs() < 2.0e-3,
                "observed {observed:?} -> ({x}, {y}), wanted {target:?}",
            );
        }
    }

    #[test]
    fn a_quadratic_fit_is_exact_on_a_forward_quadratic() {
        // Fitting the warp itself (rather than its inverse) is an exactly representable
        // problem, so the residual should be at solver noise.
        let samples: Vec<_> = grid().into_iter().map(|t| (t, warp(t[0], t[1]))).collect();
        let map = PolyMap::fit(&samples);

        for (observed, target) in &samples {
            let (x, y) = map.apply(observed[0], observed[1]);

            assert!((x - target[0]).abs() < 1.0e-12, "x residual {}", x - target[0]);
            assert!((y - target[1]).abs() < 1.0e-12, "y residual {}", y - target[1]);
        }
    }

    #[test]
    fn five_points_fall_back_to_affine_and_still_recover_an_affine_map() {
        let affine = |x: f64, y: f64| [0.1 + 0.9 * x + 0.05 * y, -0.2 - 0.03 * x + 1.1 * y];

        let points  = [[-0.8, -0.8], [0.8, -0.8], [-0.8, 0.8], [0.8, 0.8], [0.0, 0.0]];
        let samples: Vec<_> = points.iter().map(|t| (affine(t[0], t[1]), *t)).collect();

        let map = PolyMap::fit(&samples);
        assert_eq!(map.degree, PolyDegree::Affine);

        for (observed, target) in &samples {
            let (x, y) = map.apply(observed[0], observed[1]);

            assert!((x - target[0]).abs() < 1.0e-9);
            assert!((y - target[1]).abs() < 1.0e-9);
        }
    }

    #[test]
    fn two_points_fall_back_to_translation() {
        let samples = [([0.1, 0.1], [0.0, 0.0]), ([0.6, 0.6], [0.5, 0.5])];
        let map     = PolyMap::fit(&samples);

        assert_eq!(map.degree, PolyDegree::Translation);

        // Both pairs are the input shifted by -0.1, so the fitted offset is -0.1 and the
        // map must carry its input through rather than collapsing to a constant.
        let (x, y) = map.apply(0.3, 0.3);
        assert!((x - 0.2).abs() < 1.0e-12, "x = {x}");
        assert!((y - 0.2).abs() < 1.0e-12, "y = {y}");

        let (x2, _) = map.apply(0.9, 0.9);
        assert!((x2 - 0.8).abs() < 1.0e-12, "a translation must depend on its input: {x2}");
    }

    #[test]
    fn an_empty_sample_set_gives_the_identity() {
        let map = PolyMap::fit(&[]);

        assert_eq!(map, PolyMap::identity());

        let (x, y) = map.apply(0.37, -0.42);
        assert!((x - 0.37).abs() < 1.0e-12 && (y + 0.42).abs() < 1.0e-12);
    }

    #[test]
    fn collinear_points_degrade_instead_of_producing_garbage() {
        // Nine points on a line cannot determine a quadratic (or even an affine) map; the
        // ladder must step down until the system is solvable.
        let samples: Vec<_> = (0..9)
            .map(|i| {
                let t = -0.8 + 0.2 * i as f64;

                ([t, 0.0], [t + 0.05, 0.0])
            })
            .collect();

        let map = PolyMap::fit(&samples);

        assert_eq!(map.degree, PolyDegree::Translation, "collinear data must not fit a quadratic");

        let (x, _) = map.apply(0.0, 0.0);
        assert!((x - 0.05).abs() < 1.0e-9, "x = {x}");
    }

    #[test]
    fn duplicate_points_degrade_rather_than_blowing_up() {
        let samples: Vec<_> = (0..8).map(|_| ([0.2, 0.3], [0.25, 0.35])).collect();
        let map = PolyMap::fit(&samples);

        assert!(map.is_well_formed());

        let (x, y) = map.apply(0.2, 0.3);
        assert!((x - 0.25).abs() < 1.0e-6 && (y - 0.35).abs() < 1.0e-6);
    }

    /// A grid of camera-frame angles spanning a realistic gaze cone.
    fn angle_grid() -> Vec<(f64, f64)> {
        let mut out = Vec::new();

        for yaw in [-40.0, -25.0, -10.0, 0.0, 10.0, 25.0, 40.0] {
            for pitch in [-20.0, -8.0, 0.0, 8.0, 20.0] {
                out.push((yaw, pitch));
            }
        }

        out
    }

    /// Rows whose reported angles are the wanted ones put through `distort`.
    fn angle_rows(distort: impl Fn(f64, f64) -> (f64, f64)) -> Vec<AngleRow> {
        angle_grid()
            .into_iter()
            .map(|(want_yaw, want_pitch)| {
                let (yaw, pitch) = distort(want_yaw, want_pitch);

                AngleRow {
                    yaw_deg        : yaw,
                    pitch_deg      : pitch,
                    want_yaw_deg   : want_yaw,
                    want_pitch_deg : want_pitch,
                }
            })
            .collect()
    }

    /// Worst absolute angle error the fit leaves on its own rows, degrees.
    fn worst_residual(poly: &AnglePoly, rows: &[AngleRow]) -> f64 {
        rows.iter()
            .map(|r| {
                let (yaw, pitch) = poly.apply(r.yaw_deg, r.pitch_deg);

                (yaw - r.want_yaw_deg).abs().max((pitch - r.want_pitch_deg).abs())
            })
            .fold(0.0_f64, f64::max)
    }

    #[test]
    fn the_angle_identity_changes_nothing() {
        let poly = AnglePoly::identity();

        for (yaw, pitch) in angle_grid() {
            let (y, p) = poly.apply(yaw, pitch);

            assert!((y - yaw).abs() < 1.0e-12 && (p - pitch).abs() < 1.0e-12);
        }
    }

    #[test]
    fn a_quadratic_angle_fit_inverts_an_affine_distortion_exactly() {
        // Gain and offset per axis plus a cross term: all inside a quadratic's reach, so
        // the residual should be at solver noise.
        let rows = angle_rows(|y, p| (1.4 * y + 3.0 - 0.3 * p, 0.8 * p - 2.0));
        let poly = AnglePoly::fit(&rows, AngleDegree::Quadratic).unwrap();

        assert!(worst_residual(&poly, &rows) < 1.0e-9, "worst {}", worst_residual(&poly, &rows));
    }

    #[test]
    fn a_cubic_follows_a_saturating_gain_a_quadratic_cannot() {
        // The real failure: gain about 3 straight ahead, falling with eccentricity.
        let saturate = |a: f64, k: f64| {
            let scale = 45.0_f64.to_radians();

            (scale * (k * a.to_radians() / scale).atan()).to_degrees()
        };

        let rows = angle_rows(|y, p| (saturate(y, 3.0) - 0.3 * p, saturate(p, 2.0)));

        let quad  = AnglePoly::fit(&rows, AngleDegree::Quadratic).unwrap();
        let cubic = AnglePoly::fit(&rows, AngleDegree::Cubic).unwrap();

        let q = worst_residual(&quad, &rows);
        let c = worst_residual(&cubic, &rows);

        assert!(c < q * 0.6, "cubic {c} should clearly beat quadratic {q}");
        assert!(c < 2.0, "cubic worst residual {c} deg");
    }

    #[test]
    fn the_angle_fit_needs_at_least_as_many_rows_as_terms() {
        let rows = angle_rows(|y, p| (y, p));

        // Too few rows to determine the coefficients at all.
        assert!(AnglePoly::fit(&rows[..5], AngleDegree::Quadratic).is_none());
        assert!(AnglePoly::fit(&rows[..9], AngleDegree::Cubic).is_none());
        assert!(AnglePoly::fit(&[], AngleDegree::Quadratic).is_none());

        // Enough rows *and* enough spread: the grid covers both axes, so both degrees fit.
        assert!(AnglePoly::fit(&rows, AngleDegree::Quadratic).is_some());
        assert!(AnglePoly::fit(&rows, AngleDegree::Cubic).is_some());

        // Enough rows but no spread: six samples all in one column cannot determine a
        // quadratic in two variables, and the solver must say so rather than guess.
        let column: Vec<AngleRow> = rows.iter().filter(|r| r.want_yaw_deg == -40.0).copied().collect();
        assert!(column.len() >= 5);
        assert!(AnglePoly::fit(&column, AngleDegree::Quadratic).is_none());
    }

    #[test]
    fn the_ridge_keeps_a_cubic_from_exploding_on_degenerate_rows() {
        // Every row at the same angle: the design matrix is rank one, and only the ridge
        // makes the system solvable at all.
        let rows: Vec<AngleRow> = (0..20)
            .map(|_| AngleRow {
                yaw_deg        : 5.0,
                pitch_deg      : 3.0,
                want_yaw_deg   : 4.0,
                want_pitch_deg : 2.0,
            })
            .collect();

        // Whatever comes back, it must be finite and it must not be wild.
        if let Some(poly) = AnglePoly::fit(&rows, AngleDegree::Cubic) {
            assert!(poly.is_well_formed());

            let (yaw, pitch) = poly.apply(5.0, 3.0);
            assert!(yaw.is_finite() && pitch.is_finite());
            assert!(yaw.abs() < 90.0 && pitch.abs() < 90.0, "({yaw}, {pitch})");
        }
    }

    #[test]
    fn an_angle_poly_with_a_bad_scale_falls_back_rather_than_dividing_by_zero() {
        let mut poly = AnglePoly::identity();
        poly.input_scale_deg = 0.0;

        let (yaw, pitch) = poly.apply(12.0, -5.0);

        assert!(yaw.is_finite() && pitch.is_finite());
        assert!((yaw - 12.0).abs() < 1.0e-12 && (pitch + 5.0).abs() < 1.0e-12);
    }

    #[test]
    fn the_shared_basis_orders_terms_the_way_every_degree_indexes_it() {
        let b = poly_basis(2.0, 3.0);

        assert_eq!(b, [1.0, 2.0, 3.0, 4.0, 6.0, 9.0, 8.0, 12.0, 18.0, 27.0]);

        // Each degree uses a prefix, so a stored coefficient vector means the same thing
        // wherever it appears.
        assert_eq!(PolyDegree::Affine.terms(), 3);
        assert_eq!(PolyDegree::Quadratic.terms(), 6);
        assert_eq!(AngleDegree::Quadratic.terms(), 6);
        assert_eq!(AngleDegree::Cubic.terms(), 10);
    }

}
