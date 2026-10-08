//! The per-display correction field: a low-order 2D polynomial from where the
//! (device-calibrated) gaze landed to where it should have landed, fitted
//! on the post-retrain health check and applied to every sample at run time.
//!
//! Everything here works in a display's normalised coordinates, `[-1, 1]^2` over the
//! visible area, so coefficients are dimensionless, comparable between panels, and the
//! normal equations stay conditioned. `crate::calibration` supplies the conversion.
//!
//! Model selection is by leave-one-out error, not taste: every degree the sample count
//! admits is scored on held-out points, and the identity (no correction) competes on
//! the same footing. A correction ships only when it beats leaving the data alone.

use serde::{Deserialize, Serialize};

/// Maximum basis terms of any degree.
const MAX_TERMS: usize = 10;

/// Ridge added to the normal-matrix diagonal, relative to its trace, for the quadratic
/// fit. Keeps a nearly collinear target layout from producing a wild extrapolation.
const QUADRATIC_RIDGE: f64 = 1e-9;

/// Ridge for the cubic fit, much larger because ten terms on a sweep's worth of rows
/// can chase noise at the extrapolation margins.
const CUBIC_RIDGE: f64 = 1e-6;

// --- Basis ---

/// The shared polynomial basis, in the order every degree indexes it:
/// `[1, x, y, x^2, xy, y^2, x^3, x^2 y, x y^2, y^3]`.
fn basis(x: f64, y: f64) -> [f64; MAX_TERMS] {
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

/// How much of the basis a fit uses.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FieldDegree {
    /// A constant offset. Fitted against the delta so one point determines it.
    Translation,
    /// Offset, scale, shear, rotation. Three terms per axis.
    Affine,
    /// The full second-order map. Six terms per axis.
    Quadratic,
    /// The full third-order map, for error that grows toward the display edges
    /// faster than a quadratic can express. Ten terms per axis, ridge regularised.
    Cubic,
}

// --- FieldDegree ---

impl FieldDegree {
    /// Basis terms per axis, which is also the minimum sample count.
    pub fn terms(self) -> usize {
        match self {
            FieldDegree::Translation => 1,
            FieldDegree::Affine      => 3,
            FieldDegree::Quadratic   => 6,
            FieldDegree::Cubic       => 10,
        }
    }

    /// Every degree, cheapest first, for the selection ladder.
    pub fn all() -> [FieldDegree; 4] {
        [
            FieldDegree::Translation,
            FieldDegree::Affine,
            FieldDegree::Quadratic,
            FieldDegree::Cubic,
        ]
    }

    /// Ridge applied to this degree's normal matrix, relative to its trace.
    fn ridge(self) -> f64 {
        match self {
            FieldDegree::Quadratic => QUADRATIC_RIDGE,
            FieldDegree::Cubic     => CUBIC_RIDGE,
            _                      => 0.0,
        }
    }

    /// True when the fit target is `want - observed` and `apply` adds the input back.
    /// A one-term basis fitted against the absolute target would collapse to a
    /// constant that ignores its input.
    fn is_delta(self) -> bool {
        matches!(self, FieldDegree::Translation)
    }
}

// --- FieldMap ---

/// One observation: where a sample landed and where its target was, both normalised.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FieldRow {
    pub nx      : f64,
    pub ny      : f64,
    pub want_nx : f64,
    pub want_ny : f64,
}

/// A fitted correction map for one display.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FieldMap {
    pub degree   : FieldDegree,
    /// Coefficients producing the corrected x, in `basis` order.
    pub x_coeffs : Vec<f64>,
    /// Coefficients producing the corrected y, same order.
    pub y_coeffs : Vec<f64>,
}

impl FieldMap {
    /// The map that changes nothing.
    pub fn identity() -> Self {
        Self {
            degree   : FieldDegree::Translation,
            x_coeffs : vec![0.0],
            y_coeffs : vec![0.0],
        }
    }

    /// Fits one degree by least squares. `None` with fewer rows than terms or a
    /// singular system (collinear targets); the caller steps down the ladder.
    // The accumulation loops index basis and target arrays in lockstep; iterator
    // chains would obscure the normal equations.
    #[allow(clippy::needless_range_loop)]
    pub fn fit(rows: &[FieldRow], degree: FieldDegree) -> Option<Self> {
        let n = degree.terms();

        if rows.len() < n {
            return None;
        }

        let mut ata = [[0.0; MAX_TERMS]; MAX_TERMS];
        let mut atx = [0.0; MAX_TERMS];
        let mut aty = [0.0; MAX_TERMS];

        for row in rows {
            let b = basis(row.nx, row.ny);

            let (tx, ty) = {
                if degree.is_delta() {
                    (row.want_nx - row.nx, row.want_ny - row.ny)
                }
                else {
                    (row.want_nx, row.want_ny)
                }
            };

            for i in 0..n {
                atx[i] += b[i] * tx;
                aty[i] += b[i] * ty;

                for j in 0..n {
                    ata[i][j] += b[i] * b[j];
                }
            }
        }

        let ridge = degree.ridge();

        if ridge > 0.0 {
            let trace: f64 = (0..n).map(|i| ata[i][i]).sum();

            for (i, row) in ata.iter_mut().enumerate().take(n) {
                row[i] += ridge * trace;
            }
        }

        let x_coeffs = solve(&ata, &atx, n)?;
        let y_coeffs = solve(&ata, &aty, n)?;

        Some(Self {
            degree   : degree,
            x_coeffs : x_coeffs,
            y_coeffs : y_coeffs,
        })
    }

    /// Applies the correction to a normalised point.
    pub fn apply(&self, nx: f64, ny: f64) -> (f64, f64) {
        let b = basis(nx, ny);

        let mut x = 0.0;
        let mut y = 0.0;

        // The coefficient vectors hold exactly `terms()` entries, so the zip stops at
        // the degree's basis prefix.
        for ((cx, cy), bi) in self.x_coeffs.iter().zip(&self.y_coeffs).zip(&b) {
            x += cx * bi;
            y += cy * bi;
        }

        if self.degree.is_delta() {
            (nx + x, ny + y)
        }
        else {
            (x, y)
        }
    }
}

// --- Model selection ---

/// Spatial cell size for cross-validation grouping, normalised units. Consecutive
/// sweep samples share a target and its noise, so holding out single rows leaks the
/// held-out answer into the training set and systematically flatters the highest
/// degree (measured: row-wise folds kept a cubic whose leave-one-target-out error
/// was worse than the quadratic's). Held out a spatial cell at a time, a fold is
/// genuinely unseen; the cell is sized comfortably above the gaze noise.
const CV_CELL_NORM: f64 = 0.15;

/// Fits the best-scoring map for these rows, judged by held-out RMS (a spatial
/// region at a time) against the identity. Returns the winner and its score
/// (normalised units); the identity wins on data that is pure noise, so applying
/// the result is always at least as good as not.
pub fn fit_best(rows: &[FieldRow]) -> (FieldMap, f64) {
    let identity_score = rms_identity(rows);

    let mut best       = FieldMap::identity();
    let mut best_score = identity_score;

    for degree in FieldDegree::all() {
        // Held-out scoring needs one more row than the fit itself.
        if rows.len() < degree.terms() + 1 {
            continue;
        }

        let Some(score) = cv_rms(rows, degree) else {
            continue;
        };

        if score < best_score {
            let Some(map) = FieldMap::fit(rows, degree) else {
                continue;
            };

            best       = map;
            best_score = score;
        }
    }

    (best, best_score)
}

/// Held-out RMS of a degree over the rows, one spatial cell at a time. Falls back
/// to row-wise folds when every row shares one cell (a single-target fit). `None`
/// when any fold fails to fit, which rules the degree out of selection.
fn cv_rms(rows: &[FieldRow], degree: FieldDegree) -> Option<f64> {
    let cell = |r: &FieldRow| -> (i64, i64) {
        (
            (r.want_nx / CV_CELL_NORM).floor() as i64,
            (r.want_ny / CV_CELL_NORM).floor() as i64,
        )
    };

    let mut cells: Vec<(i64, i64)> = rows.iter().map(cell).collect();

    cells.sort_unstable();
    cells.dedup();

    let mut sum = 0.0;

    if cells.len() < 2 {
        for held in 0..rows.len() {
            let fold: Vec<FieldRow> = rows.iter().enumerate()
                .filter(|(i, _)| *i != held)
                .map(|(_, r)| *r)
                .collect();

            let map = FieldMap::fit(&fold, degree)?;

            sum += held_out_sq(&map, &rows[held]);
        }

        return Some((sum / rows.len() as f64).sqrt());
    }

    for held in &cells {
        let fold: Vec<FieldRow> = rows.iter()
            .filter(|r| cell(r) != *held)
            .copied()
            .collect();

        let map = FieldMap::fit(&fold, degree)?;

        for row in rows.iter().filter(|r| cell(r) == *held) {
            sum += held_out_sq(&map, row);
        }
    }

    Some((sum / rows.len() as f64).sqrt())
}

/// Squared held-out residual of one row under a fitted map.
fn held_out_sq(map: &FieldMap, row: &FieldRow) -> f64 {
    let (px, py) = map.apply(row.nx, row.ny);

    let dx = px - row.want_nx;
    let dy = py - row.want_ny;

    dx * dx + dy * dy
}

/// RMS of the raw deltas: what the error is if no correction is applied.
fn rms_identity(rows: &[FieldRow]) -> f64 {
    if rows.is_empty() {
        return 0.0;
    }

    let sum: f64 = rows.iter()
        .map(|r| {
            let dx = r.nx - r.want_nx;
            let dy = r.ny - r.want_ny;

            dx * dx + dy * dy
        })
        .sum();

    (sum / rows.len() as f64).sqrt()
}

/// Solves the leading `n`-by-`n` block by Gaussian elimination with partial pivoting.
// The elimination and accumulation loops index several arrays in lockstep;
// iterator chains would obscure the linear algebra.
#[allow(clippy::needless_range_loop)]
fn solve(a: &[[f64; MAX_TERMS]; MAX_TERMS], b: &[f64; MAX_TERMS], n: usize) -> Option<Vec<f64>> {
    let mut m = *a;
    let mut v = *b;

    for col in 0..n {
        let mut pivot = col;

        for row in col + 1..n {
            if m[row][col].abs() > m[pivot][col].abs() {
                pivot = row;
            }
        }

        if m[pivot][col].abs() < 1e-13 {
            return None;
        }

        m.swap(col, pivot);
        v.swap(col, pivot);

        for row in col + 1..n {
            let f = m[row][col] / m[col][col];

            for k in col..n {
                m[row][k] -= f * m[col][k];
            }

            v[row] -= f * v[col];
        }
    }

    let mut x = vec![0.0; n];

    for col in (0..n).rev() {
        let mut sum = v[col];

        for k in col + 1..n {
            sum -= m[col][k] * x[k];
        }

        x[col] = sum / m[col][col];
    }

    Some(x)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Rows sampled from a known warp over a grid.
    fn warped_rows(warp: impl Fn(f64, f64) -> (f64, f64)) -> Vec<FieldRow> {
        let mut rows = Vec::new();

        for row in 0..4 {
            for col in 0..4 {
                let wx = -0.9 + 1.8 * col as f64 / 3.0;
                let wy = -0.9 + 1.8 * row as f64 / 3.0;

                // The warp maps true target position to observed position; the fit
                // learns the inverse direction (observed to intended).
                let (nx, ny) = warp(wx, wy);

                rows.push(FieldRow { nx: nx, ny: ny, want_nx: wx, want_ny: wy });
            }
        }

        rows
    }

    #[test]
    fn recovers_a_translation() {
        let rows = warped_rows(|x, y| (x + 0.05, y - 0.03));
        let (map, score) = fit_best(&rows);

        assert!(score < 1e-9, "loo score {score}");

        let (cx, cy) = map.apply(0.35, -0.15);
        assert!((cx - 0.30).abs() < 1e-9);
        assert!((cy - -0.12).abs() < 1e-9);
    }

    #[test]
    fn recovers_a_quadratic_warp() {
        // The fit learns the inverse of the warp, and the inverse of a quadratic map
        // is not itself a polynomial, so the achievable floor is a small residual
        // rather than machine epsilon. What matters is that it lands within a small
        // fraction of a percent of the display, far below the device's own error.
        let rows = warped_rows(|x, y| (x + 0.04 * x * x - 0.02 * y, y + 0.03 * x * y));
        let (map, score) = fit_best(&rows);

        // The inverse of a quadratic warp is not a polynomial, so the cubic can
        // legitimately edge out the quadratic on held-out points; either is fine.
        assert!(matches!(map.degree, FieldDegree::Quadratic | FieldDegree::Cubic));
        assert!(score < 5e-3, "loo score {score}");

        // A point off the grid still lands close to the intended position.
        let (wx, wy) = (0.25, -0.4);
        let (nx, ny) = (wx + 0.04 * wx * wx - 0.02 * wy, wy + 0.03 * wx * wy);
        let (cx, cy) = map.apply(nx, ny);
        assert!((cx - wx).abs() < 5e-3);
        assert!((cy - wy).abs() < 5e-3);
    }

    #[test]
    fn noise_never_yields_a_wild_map() {
        // Fixed pseudo-random offsets with no spatial structure. With this few
        // samples a low-order fit can still edge out the identity by chance, so the
        // guarantee to test is not "identity always wins" but "the winner never does
        // worse than nothing, and never distorts beyond the noise scale".
        let deltas = [
            ( 0.013, -0.007), (-0.019,  0.011), ( 0.004,  0.016), (-0.012, -0.015),
            ( 0.017,  0.002), (-0.006,  0.019), ( 0.009, -0.013), (-0.016,  0.005),
            ( 0.001, -0.018), ( 0.015,  0.008), (-0.011, -0.003), ( 0.007,  0.014),
        ];

        let rows: Vec<FieldRow> = deltas.iter().enumerate()
            .map(|(i, (dx, dy))| {
                let nx = -0.9 + 1.8 * (i % 4) as f64 / 3.0;
                let ny = -0.9 + 1.8 * (i / 4) as f64 / 2.0;

                FieldRow { nx: nx, ny: ny, want_nx: nx + dx, want_ny: ny + dy }
            })
            .collect();

        let (map, score) = fit_best(&rows);

        // Never worse than applying nothing.
        let identity_rms = (rows.iter()
            .map(|r| (r.nx - r.want_nx).powi(2) + (r.ny - r.want_ny).powi(2))
            .sum::<f64>() / rows.len() as f64)
            .sqrt();
        assert!(score <= identity_rms + 1e-12, "score {score} vs identity {identity_rms}");

        // Whatever won, it moves points by at most the noise scale, everywhere.
        for gy in -3..=3 {
            for gx in -3..=3 {
                let x = gx as f64 / 3.0;
                let y = gy as f64 / 3.0;

                let (cx, cy) = map.apply(x, y);
                let moved    = ((cx - x).powi(2) + (cy - y).powi(2)).sqrt();
                assert!(moved < 0.06, "map moved ({x}, {y}) by {moved}");
            }
        }
    }

    #[test]
    fn recovers_a_cubic_edge_warp() {
        // Error growing with the cube of eccentricity, flat at the centre: the edge
        // falloff shape measured on the ultrawide.
        let rows = warped_rows(|x, y| (x + 0.05 * x * x * x, y + 0.03 * y * y * y));
        let (map, score) = fit_best(&rows);

        assert_eq!(map.degree, FieldDegree::Cubic);
        assert!(score < 5e-3, "loo score {score}");

        let (wx, wy) = (0.8, -0.7);
        let (nx, ny) = (wx + 0.05 * wx * wx * wx, wy + 0.03 * wy * wy * wy);
        let (cx, cy) = map.apply(nx, ny);
        assert!((cx - wx).abs() < 5e-3, "cx {cx} vs {wx}");
        assert!((cy - wy).abs() < 5e-3, "cy {cy} vs {wy}");
    }

    #[test]
    fn identity_map_is_identity() {
        let (x, y) = FieldMap::identity().apply(0.123, -0.456);
        assert_eq!((x, y), (0.123, -0.456));
    }
}
