//! Fitting the residual model (PLAN-ET5 D2): exported rows in, a [`ResidualModel`] out,
//! with the honest number measured on the way.
//!
//! The fit is the Phase C prototype's, step for step: standardise the kernel columns
//! (imputing the training median), pick inducing points by k-means, whiten them
//! through `K_mm^{-1/2}`, and ridge-regress the residual on the whitened kernel
//! features plus a few raw explicit columns. The linear algebra is written out here
//! rather than pulled in: every matrix is a few hundred square, and a Jacobi
//! eigensolve and a Cholesky are less code than a dependency's feature flags.
//!
//! Training rows are one per click: the exporter's aggregated `is_mean` rows, so a
//! long hold does not outvote a short one and every click is one observation of the
//! eye. Caret clicks are refused (an I-beam on a terminal says nothing about where the
//! eye was; measured at a 10 degree median residual) and so is anything past a residual
//! cap, which is a look-away click rather than a calibration error.
//!
//! Evaluation is leave-one-session-out and nothing else. Frames inside a click are the
//! same observation, and clicks inside a session share a posture and a day.

// Dense matrix code reads as subscripts; iterator rewrites of `a[i][k] * b[k][j]` hide
// which index is which.
#![allow(clippy::needless_range_loop)]

use crate::dataset::Row;
use crate::model::{FEATURE_NAMES, Features, FitReport, MODEL_FORMAT, ResidualModel};

/// Element source refused outright. See the module docs.
const REFUSED_SOURCE: &str = "caret";

/// Ridge on the explicit linear columns. Near-unregularised: the explicit terms are a
/// fixed physical shape, not something meant to shrink like the kernel weights.
const LINEAR_RIDGE: f64 = 1e-6;

/// Jitter added to `K_mm`'s diagonal, relative to its mean diagonal, before the
/// eigendecomposition.
const JITTER: f64 = 1e-6;

/// Eigenvalues of `K_mm` below this are treated as zero when whitening.
const EIGEN_FLOOR: f64 = 1e-10;

/// Lloyd iterations for the inducing points.
const KMEANS_ITERATIONS: usize = 25;

/// Jacobi sweeps allowed before the eigensolve gives up on convergence.
const JACOBI_MAX_SWEEPS: usize = 60;

/// The quantile of the training rows' own predictive variance the fade starts at, and
/// the multiple of it the fade ends at. Inside the data the correction applies in full;
/// a frame three times as uncertain as the most uncertain training row is treated as
/// off the map.
const FADE_LO_QUANTILE: f64 = 0.95;
const FADE_HI_MULTIPLE: f64 = 3.0;

// --- Parameters ---

/// What to fit.
#[derive(Clone, Debug, PartialEq)]
pub struct FitParams {
    /// Kernel feature names, a subset of [`FEATURE_NAMES`].
    pub features     : Vec<String>,
    /// Explicit linear feature names.
    pub explicit     : Vec<String>,
    /// ARD length scale, in standardised units, shared by every kernel column.
    pub length_scale : f64,
    /// Ridge on the whitened kernel weights.
    pub ridge        : f64,
    /// Inducing points.
    pub centers      : usize,
    /// Rows with a firmware residual at or past this many degrees are not trained on.
    pub cap_deg      : f64,
    /// Seed for the k-means initialisation.
    pub seed         : u64,
}

// --- FitParams ---

impl FitParams {
    /// The per-eye direction field with linear pupil and radial terms: the setting the
    /// 2026-09-04 leave-one-session-out sweep picked (per-click median residual 1.83
    /// to 1.44 degrees over five sessions).
    pub fn position_field() -> Self {
        Self {
            features     : names(&[
                "dir_l_yaw_deg", "dir_l_pitch_deg", "dir_r_yaw_deg", "dir_r_pitch_deg",
                "angle_axis_deg",
            ]),
            explicit     : names(&["angle_axis_deg", "pupil_l_mm", "pupil_r_mm"]),
            length_scale : 2.0,
            ridge        : 3.0,
            centers      : 200,
            cap_deg      : 8.0,
            seed         : 0,
        }
    }

    /// Every feature the exporter produces, the Phase C prototype's default.
    pub fn all_features() -> Self {
        Self {
            features : FEATURE_NAMES.iter().map(|n| n.to_string()).collect(),
            ..Self::position_field()
        }
    }
}

/// Owned names from a literal list.
fn names(list: &[&str]) -> Vec<String> {
    list.iter().map(|n| n.to_string()).collect()
}

// --- Selection ---

/// The rows a model trains on: one aggregated row per click or stop, with a finite
/// residual and an admissible source. The cap is applied by the fit, not here, so the
/// evaluation still scores the rows it refuses to learn from.
pub fn select(rows: &[Row]) -> Vec<&Row> {
    rows.iter()
        .filter(|r| r.is_mean)
        .filter(|r| r.source != REFUSED_SOURCE)
        .filter(|r| r.residual_yaw_deg.is_finite() && r.residual_pitch_deg.is_finite())
        .collect()
}

// --- Fit ---

/// Fits a model on `rows` (already selected). `blob_sha256` and `tracker_pitch_deg`
/// are recorded, not used: they are what the provider checks the file against.
pub fn fit(
    rows              : &[&Row],
    params            : &FitParams,
    blob_sha256       : Option<String>,
    tracker_pitch_deg : f64,
)
    -> Result<ResidualModel, TrainError>
{
    let trained = Trained::fit(rows, params)?;
    let mut model = trained.into_model(params, blob_sha256, tracker_pitch_deg);

    // The fade thresholds come from where the training rows themselves sit.
    let mut vars: Vec<f64> = rows.iter()
        .map(|r| model.predict(&Features::from_row(r)).var_deg2)
        .collect();

    vars.sort_by(f64::total_cmp);

    let lo = quantile(&vars, FADE_LO_QUANTILE).max(f64::EPSILON);

    model.var_fade_lo = lo;
    model.var_fade_hi = lo * FADE_HI_MULTIPLE;

    Ok(model)
}

/// Leave-one-session-out evaluation of `params` on `rows`: each session is scored by
/// a model fitted on the others. Per-click medians and 90th percentiles, firmware
/// against model.
pub fn evaluate(rows: &[&Row], params: &FitParams) -> Result<FitReport, TrainError> {
    let mut sessions: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
    sessions.sort_unstable();
    sessions.dedup();

    if sessions.len() < 2 {
        return Err(TrainError::TooFewSessions(sessions.len()));
    }

    let mut firmware    = Vec::with_capacity(rows.len());
    let mut corrected   = Vec::with_capacity(rows.len());
    let mut per_session = Vec::with_capacity(sessions.len());

    for held in &sessions {
        let train: Vec<&Row> = rows.iter().copied().filter(|r| r.session_id != *held).collect();
        let test : Vec<&Row> = rows.iter().copied().filter(|r| r.session_id == *held).collect();

        let model = Trained::fit(&train, params)?.into_model(params, None, 0.0);

        let mut f = Vec::with_capacity(test.len());
        let mut c = Vec::with_capacity(test.len());

        for row in &test {
            let p  = model.predict(&Features::from_row(row));
            let dy = row.residual_yaw_deg   - p.yaw_deg;
            let dp = row.residual_pitch_deg - p.pitch_deg;

            f.push(row.residual_deg());
            c.push((dy * dy + dp * dp).sqrt());
        }

        f.sort_by(f64::total_cmp);
        c.sort_by(f64::total_cmp);
        per_session.push([quantile(&f, 0.5), quantile(&c, 0.5)]);

        firmware.extend(f);
        corrected.extend(c);
    }

    firmware.sort_by(f64::total_cmp);
    corrected.sort_by(f64::total_cmp);

    Ok(FitReport {
        sessions         : sessions.iter().map(|s| s.to_string()).collect(),
        rows             : rows.len(),
        clicks           : rows.iter().map(|r| (&r.session_id, &r.hold_key)).collect::<std::collections::BTreeSet<_>>().len(),
        firmware_p50_deg : quantile(&firmware , 0.5),
        model_p50_deg    : quantile(&corrected, 0.5),
        firmware_p90_deg : quantile(&firmware , 0.9),
        model_p90_deg    : quantile(&corrected, 0.9),
        per_session_p50  : per_session,
    })
}

/// A sorted slice's value at quantile `q` (nearest rank), NaN when empty.
fn quantile(sorted: &[f64], q: f64) -> f64 {
    if sorted.is_empty() {
        return f64::NAN;
    }

    sorted[(((sorted.len() - 1) as f64) * q).round() as usize]
}

// --- Trained ---

/// The unfolded fit, before the whitening and the ridge are baked into the file.
struct Trained {
    impute   : Vec<f64>,
    mean     : Vec<f64>,
    std      : Vec<f64>,
    ls       : Vec<f64>,
    centers  : Vec<Vec<f64>>,
    /// `K_mm^{-1/2}`, symmetric.
    whiten   : Vec<Vec<f64>>,
    /// `(D^T D + reg)^{-1}`, over the whitened kernel columns then the explicit ones.
    a_inv    : Vec<Vec<f64>>,
    /// Ridge coefficients, one yaw/pitch pair per design column.
    coef     : Vec<[f64; 2]>,
}

// --- Trained ---

impl Trained {
    /// The whole fit, on rows that pass the cap.
    fn fit(rows: &[&Row], params: &FitParams) -> Result<Self, TrainError> {
        for name in params.features.iter().chain(&params.explicit) {
            if !FEATURE_NAMES.contains(&name.as_str()) {
                return Err(TrainError::Feature(name.clone()));
            }
        }

        let rows: Vec<&Row> = rows.iter().copied()
            .filter(|r| r.residual_deg() < params.cap_deg)
            .collect();

        let n = rows.len();
        let d = params.features.len();
        let e = params.explicit.len();
        let m = params.centers.min(n);

        if n < 2 * m.max(1) || d == 0 {
            return Err(TrainError::TooFewRows(n));
        }

        let raw: Vec<Vec<f64>> = rows.iter()
            .map(|r| {
                let f = Features::from_row(r);

                params.features.iter().map(|name| f.get(name).unwrap_or(f64::NAN)).collect()
            })
            .collect();

        // Impute with the column median, then standardise.
        let impute: Vec<f64> = (0..d)
            .map(|j| {
                let mut col: Vec<f64> = raw.iter().map(|r| r[j]).filter(|v| v.is_finite()).collect();

                col.sort_by(f64::total_cmp);

                if col.is_empty() { 0.0 } else { quantile(&col, 0.5) }
            })
            .collect();

        let filled: Vec<Vec<f64>> = raw.iter()
            .map(|r| r.iter().zip(&impute).map(|(v, i)| if v.is_finite() { *v } else { *i }).collect())
            .collect();

        let mean: Vec<f64> = (0..d)
            .map(|j| filled.iter().map(|r| r[j]).sum::<f64>() / n as f64)
            .collect();
        let std: Vec<f64> = (0..d)
            .map(|j| {
                let var = filled.iter().map(|r| (r[j] - mean[j]).powi(2)).sum::<f64>() / n as f64;
                let sd  = var.sqrt();

                if sd < 1e-9 { 1.0 } else { sd }
            })
            .collect();

        let xs: Vec<Vec<f64>> = filled.iter()
            .map(|r| r.iter().zip(&mean).zip(&std).map(|((v, m), s)| (v - m) / s).collect())
            .collect();

        let ls = vec![params.length_scale; d];

        // Inducing points, then their whitening.
        let centers = kmeans(&xs, m, params.seed);

        let mut kmm = matrix(m, m);

        for i in 0..m {
            for j in 0..m {
                kmm[i][j] = rbf(&centers[i], &centers[j], &ls);
            }
        }

        let trace: f64 = (0..m).map(|i| kmm[i][i]).sum();

        for (i, row) in kmm.iter_mut().enumerate() {
            row[i] += JITTER * trace / m as f64;
        }

        let (vals, vecs) = symmetric_eigen(&kmm);
        let whiten = {
            // V diag(1/sqrt(lambda)) V^T.
            let mut w = matrix(m, m);

            for i in 0..m {
                for j in 0..m {
                    w[i][j] = (0..m)
                        .map(|k| vecs[i][k] * vecs[j][k] / vals[k].max(EIGEN_FLOOR).sqrt())
                        .sum();
                }
            }

            w
        };

        // The design: whitened kernel features, then the raw explicit columns.
        let explicit_of = |r: &Row| -> Vec<f64> {
            let f = Features::from_row(r);

            params.explicit.iter()
                .map(|name| f.get(name).filter(|v| v.is_finite()).unwrap_or(0.0))
                .collect()
        };

        let design: Vec<Vec<f64>> = xs.iter().zip(&rows)
            .map(|(x, r)| {
                let k: Vec<f64> = centers.iter().map(|c| rbf(x, c, &ls)).collect();
                let mut row = Vec::with_capacity(m + e);

                for j in 0..m {
                    row.push((0..m).map(|i| k[i] * whiten[i][j]).sum());
                }

                row.extend(explicit_of(r));

                row
            })
            .collect();

        // Ridge solve for both outputs at once.
        let p = m + e;
        let mut a = matrix(p, p);

        for row in &design {
            for i in 0..p {
                for j in 0..p {
                    a[i][j] += row[i] * row[j];
                }
            }
        }

        for (i, row) in a.iter_mut().enumerate() {
            row[i] += if i < m { params.ridge } else { LINEAR_RIDGE };
        }

        let a_inv = cholesky_inverse(&a).ok_or(TrainError::Singular)?;

        let mut dty = vec![[0.0; 2]; p];

        for (row, r) in design.iter().zip(&rows) {
            for i in 0..p {
                dty[i][0] += row[i] * r.residual_yaw_deg;
                dty[i][1] += row[i] * r.residual_pitch_deg;
            }
        }

        let coef: Vec<[f64; 2]> = (0..p)
            .map(|i| {
                [
                    (0..p).map(|j| a_inv[i][j] * dty[j][0]).sum(),
                    (0..p).map(|j| a_inv[i][j] * dty[j][1]).sum(),
                ]
            })
            .collect();

        Ok(Self {
            impute  : impute,
            mean    : mean,
            std     : std,
            ls      : ls,
            centers : centers,
            whiten  : whiten,
            a_inv   : a_inv,
            coef    : coef,
        })
    }

    /// Folds the whitening and the ridge into the runtime form.
    fn into_model(
        self,
        params            : &FitParams,
        blob_sha256       : Option<String>,
        tracker_pitch_deg : f64,
    )
        -> ResidualModel
    {
        let m = self.centers.len();
        let e = params.explicit.len();
        let p = m + e;

        // kernel_weights = whiten @ coef[..m].
        let kernel_weights: Vec<[f64; 2]> = (0..m)
            .map(|i| {
                [
                    (0..m).map(|j| self.whiten[i][j] * self.coef[j][0]).sum(),
                    (0..m).map(|j| self.whiten[i][j] * self.coef[j][1]).sum(),
                ]
            })
            .collect();

        // variance = ridge * S A^{-1} S^T with S = blockdiag(whiten, I).
        let s_at = |i: usize, j: usize| -> f64 {
            match (i < m, j < m) {
                (true , true ) => self.whiten[i][j],
                (false, false) => f64::from(u8::from(i == j)),
                _              => 0.0,
            }
        };

        let mut sa = matrix(p, p);

        for i in 0..p {
            for j in 0..p {
                sa[i][j] = (0..p).map(|k| s_at(i, k) * self.a_inv[k][j]).sum();
            }
        }

        let mut variance = matrix(p, p);

        for i in 0..p {
            for j in 0..p {
                variance[i][j] = params.ridge * (0..p).map(|k| sa[i][k] * s_at(j, k)).sum::<f64>();
            }
        }

        ResidualModel {
            format             : MODEL_FORMAT,
            created_unix_s     : crate::calibration::Et5Calibration::now_unix_s(),
            device_blob_sha256 : blob_sha256,
            tracker_pitch_deg  : tracker_pitch_deg,
            features           : params.features.clone(),
            explicit           : params.explicit.clone(),
            impute             : self.impute,
            mean               : self.mean,
            std                : self.std,
            length_scale       : self.ls,
            centers            : self.centers,
            kernel_weights     : kernel_weights,
            explicit_weights   : self.coef[m..].to_vec(),
            variance           : variance,
            var_fade_lo        : f64::INFINITY,
            var_fade_hi        : f64::INFINITY,
            report             : None,
        }
    }
}

/// The ARD RBF kernel between two standardised points.
fn rbf(a: &[f64], b: &[f64], ls: &[f64]) -> f64 {
    let s: f64 = a.iter().zip(b).zip(ls)
        .map(|((x, y), l)| {
            let t = (x - y) / l;

            t * t
        })
        .sum();

    (-0.5 * s).exp()
}

// --- k-means ---

/// k-means++ seeding then Lloyd's iterations. Deterministic for a seed.
fn kmeans(points: &[Vec<f64>], k: usize, seed: u64) -> Vec<Vec<f64>> {
    let mut rng = XorShift(seed.wrapping_mul(0x9E37_79B9_7F4A_7C15) | 1);
    let n       = points.len();

    // Seeding: each next centre drawn with probability proportional to its squared
    // distance from the nearest centre so far.
    let mut centers: Vec<Vec<f64>> = vec![points[rng.below(n)].clone()];
    let mut nearest: Vec<f64>      = points.iter().map(|p| sq_dist(p, &centers[0])).collect();

    while centers.len() < k {
        let total: f64 = nearest.iter().sum();

        let pick = {
            if total <= 0.0 {
                rng.below(n)
            }
            else {
                let mut target = rng.unit() * total;
                let mut chosen = n - 1;

                for (i, d) in nearest.iter().enumerate() {
                    if target < *d {
                        chosen = i;

                        break;
                    }

                    target -= d;
                }

                chosen
            }
        };

        centers.push(points[pick].clone());

        for (p, d) in points.iter().zip(nearest.iter_mut()) {
            *d = d.min(sq_dist(p, &centers[centers.len() - 1]));
        }
    }

    // Lloyd: assign, then move each centre to its members' mean. An emptied centre
    // is re-seeded on the farthest point so the count stays what was asked for.
    let d = points[0].len();

    for _ in 0..KMEANS_ITERATIONS {
        let labels: Vec<usize> = points.iter().map(|p| nearest_center(p, &centers)).collect();

        let mut sums   = vec![vec![0.0; d]; k];
        let mut counts = vec![0usize; k];

        for (p, l) in points.iter().zip(&labels) {
            counts[*l] += 1;

            for (s, v) in sums[*l].iter_mut().zip(p) {
                *s += v;
            }
        }

        let mut moved = 0.0;

        for c in 0..k {
            if counts[c] == 0 {
                let far = (0..n)
                    .max_by(|a, b| {
                        sq_dist(&points[*a], &centers[labels[*a]])
                            .total_cmp(&sq_dist(&points[*b], &centers[labels[*b]]))
                    })
                    .unwrap_or(0);

                centers[c] = points[far].clone();

                continue;
            }

            let fresh: Vec<f64> = sums[c].iter().map(|s| s / counts[c] as f64).collect();

            moved += sq_dist(&fresh, &centers[c]);
            centers[c] = fresh;
        }

        if moved < 1e-12 {
            break;
        }
    }

    centers
}

/// Index of the nearest centre.
fn nearest_center(p: &[f64], centers: &[Vec<f64>]) -> usize {
    let mut best = 0;
    let mut dist = f64::INFINITY;

    for (i, c) in centers.iter().enumerate() {
        let d = sq_dist(p, c);

        if d < dist {
            dist = d;
            best = i;
        }
    }

    best
}

/// Squared Euclidean distance.
fn sq_dist(a: &[f64], b: &[f64]) -> f64 {
    a.iter().zip(b).map(|(x, y)| (x - y) * (x - y)).sum()
}

/// A small deterministic generator for the seeding: reproducible fits without a
/// dependency.
struct XorShift(u64);

impl XorShift {
    /// The next raw value.
    fn next(&mut self) -> u64 {
        let mut x = self.0;

        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;

        x
    }

    /// Uniform in `[0, 1)`.
    fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1u64 << 53) as f64
    }

    /// Uniform in `0..n`.
    fn below(&mut self, n: usize) -> usize {
        (self.unit() * n as f64) as usize % n.max(1)
    }
}

// --- Linear algebra ---

/// A zero matrix.
fn matrix(rows: usize, cols: usize) -> Vec<Vec<f64>> {
    vec![vec![0.0; cols]; rows]
}

/// Eigendecomposition of a symmetric matrix by cyclic Jacobi rotations: eigenvalues,
/// and eigenvectors as the columns of the returned matrix.
fn symmetric_eigen(a: &[Vec<f64>]) -> (Vec<f64>, Vec<Vec<f64>>) {
    let n     = a.len();
    let mut a = a.to_vec();
    let mut v = matrix(n, n);

    for (i, row) in v.iter_mut().enumerate() {
        row[i] = 1.0;
    }

    for _ in 0..JACOBI_MAX_SWEEPS {
        let off: f64 = (0..n)
            .flat_map(|i| (0..n).filter(move |j| *j != i).map(move |j| (i, j)))
            .map(|(i, j)| a[i][j] * a[i][j])
            .sum();

        if off < 1e-22 {
            break;
        }

        for p in 0..n {
            for q in (p + 1)..n {
                if a[p][q].abs() < 1e-300 {
                    continue;
                }

                // The rotation angle that zeroes a[p][q].
                let theta = (a[q][q] - a[p][p]) / (2.0 * a[p][q]);
                let t     = theta.signum() / (theta.abs() + (theta * theta + 1.0).sqrt());
                let t     = if theta == 0.0 { 1.0 } else { t };
                let c     = 1.0 / (t * t + 1.0).sqrt();
                let s     = t * c;

                for k in 0..n {
                    let akp = a[k][p];
                    let akq = a[k][q];

                    a[k][p] = c * akp - s * akq;
                    a[k][q] = s * akp + c * akq;
                }

                for k in 0..n {
                    let apk = a[p][k];
                    let aqk = a[q][k];

                    a[p][k] = c * apk - s * aqk;
                    a[q][k] = s * apk + c * aqk;
                }

                for row in v.iter_mut() {
                    let vkp = row[p];
                    let vkq = row[q];

                    row[p] = c * vkp - s * vkq;
                    row[q] = s * vkp + c * vkq;
                }
            }
        }
    }

    ((0..n).map(|i| a[i][i]).collect(), v)
}

/// The inverse of a symmetric positive definite matrix through its Cholesky factor,
/// or `None` when it is not positive definite.
fn cholesky_inverse(a: &[Vec<f64>]) -> Option<Vec<Vec<f64>>> {
    let n     = a.len();
    let mut l = matrix(n, n);

    for i in 0..n {
        for j in 0..=i {
            let sum: f64 = (0..j).map(|k| l[i][k] * l[j][k]).sum();

            if i == j {
                let d = a[i][i] - sum;

                if d <= 0.0 || !d.is_finite() {
                    return None;
                }

                l[i][j] = d.sqrt();
            }
            else {
                l[i][j] = (a[i][j] - sum) / l[j][j];
            }
        }
    }

    // Solve L L^T X = I one column at a time.
    let mut inv = matrix(n, n);

    for col in 0..n {
        let mut y = vec![0.0; n];

        for i in 0..n {
            let rhs = f64::from(u8::from(i == col));
            let sum: f64 = (0..i).map(|k| l[i][k] * y[k]).sum();

            y[i] = (rhs - sum) / l[i][i];
        }

        let mut x = vec![0.0; n];

        for i in (0..n).rev() {
            let sum: f64 = ((i + 1)..n).map(|k| l[k][i] * x[k]).sum();

            x[i] = (y[i] - sum) / l[i][i];
        }

        for i in 0..n {
            inv[i][col] = x[i];
        }
    }

    Some(inv)
}

// --- Error ---

/// Fit failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum TrainError {
    #[error("unknown feature {0}")]
    Feature(String),
    #[error("{0} training rows is too few for the requested centres")]
    TooFewRows(usize),
    #[error("{0} session(s): leave-one-session-out needs at least two")]
    TooFewSessions(usize),
    #[error("the ridge system is singular")]
    Singular,
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jacobi_recovers_a_known_spectrum() {
        // Symmetric with eigenvalues 1, 2, 3 (a diagonal rotated by a known basis).
        let a = vec![
            vec![2.0, 0.5, 0.0],
            vec![0.5, 2.0, 0.5],
            vec![0.0, 0.5, 2.0],
        ];

        let (vals, vecs) = symmetric_eigen(&a);
        let mut sorted = vals.clone();
        sorted.sort_by(f64::total_cmp);

        let want = [2.0 - 0.5 * 2f64.sqrt(), 2.0, 2.0 + 0.5 * 2f64.sqrt()];

        for (v, w) in sorted.iter().zip(want) {
            assert!((v - w).abs() < 1e-9, "{v} vs {w}");
        }

        // A v = lambda v for each column.
        for k in 0..3 {
            for i in 0..3 {
                let av: f64 = (0..3).map(|j| a[i][j] * vecs[j][k]).sum();

                assert!((av - vals[k] * vecs[i][k]).abs() < 1e-9);
            }
        }
    }

    #[test]
    fn cholesky_inverse_inverts() {
        let a = vec![
            vec![4.0, 1.0, 0.5],
            vec![1.0, 3.0, 0.2],
            vec![0.5, 0.2, 2.0],
        ];

        let inv = cholesky_inverse(&a).expect("positive definite");

        for i in 0..3 {
            for j in 0..3 {
                let prod: f64 = (0..3).map(|k| a[i][k] * inv[k][j]).sum();
                let want = f64::from(u8::from(i == j));

                assert!((prod - want).abs() < 1e-9, "({i},{j}) {prod}");
            }
        }

        let bad = vec![vec![1.0, 2.0], vec![2.0, 1.0]];
        assert!(cholesky_inverse(&bad).is_none());
    }

    #[test]
    fn kmeans_finds_separated_clusters() {
        let mut points = Vec::new();

        for i in 0..40 {
            let t = (i as f64) * 0.01;

            points.push(vec![0.0 + t, 0.0 - t]);
            points.push(vec![10.0 - t, 10.0 + t]);
            points.push(vec![-10.0 + t, 5.0 + t]);
        }

        let mut centers = kmeans(&points, 3, 7);
        centers.sort_by(|a, b| a[0].total_cmp(&b[0]));

        assert!((centers[0][0] - -9.8).abs() < 0.5 && (centers[0][1] - 5.2).abs() < 0.5);
        assert!((centers[1][0] - 0.2).abs() < 0.5 && (centers[1][1] - -0.2).abs() < 0.5);
        assert!((centers[2][0] - 9.8).abs() < 0.5 && (centers[2][1] - 10.2).abs() < 0.5);
    }

    #[test]
    fn the_fit_learns_a_smooth_field_and_evaluation_beats_the_firmware() {
        // Three synthetic sessions whose residual is a smooth function of the gaze
        // direction plus a little noise; the held-out sessions should come out well
        // under the raw residual.
        let mut rows = Vec::new();
        let mut rng  = XorShift(42);

        for session in 0..3 {
            for click in 0..120 {
                let yaw   = rng.unit() * 40.0 - 20.0;
                let pitch = rng.unit() * 20.0 - 10.0;
                let n1    = (rng.unit() - 0.5) * 0.2;
                let n2    = (rng.unit() - 0.5) * 0.2;

                let mut row = synthetic_row(session, click, yaw, pitch);
                row.residual_yaw_deg   = 0.05 * yaw + 0.002 * yaw * pitch + n1;
                row.residual_pitch_deg = 0.8 + 0.001 * pitch * pitch - 0.03 * yaw + n2;

                rows.push(row);
            }
        }

        let selected = select(&rows);
        let params   = FitParams { centers: 40, ..FitParams::position_field() };
        let report   = evaluate(&selected, &params).expect("evaluate");

        assert!(report.firmware_p50_deg > 0.8, "{report:?}");
        assert!(report.model_p50_deg < 0.3 * report.firmware_p50_deg, "{report:?}");

        let model = fit(&selected, &params, None, 13.0).expect("fit");

        assert_eq!(model.centers(), 40);
        assert!(model.var_fade_lo.is_finite() && model.var_fade_hi > model.var_fade_lo);

        // In sample, the prediction sits on the field.
        let p = model.predict(&Features::from_row(selected[0]));
        assert!((p.yaw_deg - selected[0].residual_yaw_deg).abs() < 0.3);
        assert!((p.pitch_deg - selected[0].residual_pitch_deg).abs() < 0.3);
        assert!(p.fade > 0.99);
    }

    #[test]
    fn caret_rows_and_frame_rows_are_not_selected() {
        let mut rows = vec![
            synthetic_row(0, 0, 1.0, 1.0),
            synthetic_row(0, 1, 1.0, 1.0),
            synthetic_row(0, 2, 1.0, 1.0),
        ];
        rows[1].source  = "caret".into();
        rows[2].is_mean = false;

        assert_eq!(select(&rows).len(), 1);
    }

    /// A mean row for one click, both eyes looking along `(yaw, pitch)`.
    fn synthetic_row(session: usize, click: usize, yaw: f64, pitch: f64) -> Row {
        Row {
            session_id          : format!("s{session}"),
            hold_key            : format!("click_{click}"),
            background          : "screen".into(),
            phase               : "stop".into(),
            session_phase       : "click".into(),
            t_s                 : click as f64,
            is_mean             : true,
            origin_l_mm         : [-32.0, 40.0, 700.0],
            origin_r_mm         : [32.0, 40.0, 700.0],
            origin_raw_l_mm     : [f64::NAN; 3],
            origin_raw_r_mm     : [f64::NAN; 3],
            dir_l_yaw_deg       : yaw + 0.3,
            dir_l_pitch_deg     : pitch,
            dir_r_yaw_deg       : yaw - 0.3,
            dir_r_pitch_deg     : pitch,
            inter_mm            : [64.0, 0.0, 0.0],
            pupil_l_mm          : 4.0,
            pupil_r_mm          : 4.1,
            valid_l             : 1.0,
            valid_r             : 1.0,
            angle_axis_deg      : (yaw * yaw + pitch * pitch).sqrt(),
            lag_origin_l_mm     : [f64::NAN; 3],
            lag_origin_r_mm     : [f64::NAN; 3],
            lag_inter_mm        : [f64::NAN; 3],
            target_mm           : [0.0, 0.0, 0.0],
            residual_yaw_deg    : 0.0,
            residual_pitch_deg  : 0.0,
            residual_yaw_deg_combined   : f64::NAN,
            residual_pitch_deg_combined : f64::NAN,
            element_kind        : "button".into(),
            element_w_px        : 60.0,
            element_h_px        : 24.0,
            crop_luma           : 0.5,
            source              : "trainer".into(),
            posture             : "normal".into(),
            trainer_task        : 1.0,
            trainer_hit         : 1.0,
        }
    }
}
