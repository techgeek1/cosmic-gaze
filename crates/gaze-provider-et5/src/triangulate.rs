//! Least-squares intersection of gaze-ray bundles: the 3D point a set of rays from
//! different origins most nearly passes through.
//!
//! This is the measurement at the heart of the plane pass: two looks at the same
//! screen dot from two head positions give two rays whose crossing is the dot's
//! physical position, with no assumed display geometry involved. One ray pair at
//! gaze noise is worthless (depth error grows as distance squared over baseline);
//! hundreds of frames across a deliberate sideways head sweep average it down to
//! millimetres. Triangulating three or more non-collinear dots then pins the whole
//! panel: depth *and* orientation, the two things a single-viewpoint angular solve
//! cannot separate.

use glam::DVec3;

/// Outlier trim rounds: solve, drop rays far from the point, solve again.
const TRIM_ROUNDS: usize = 2;

/// A ray is dropped when its distance to the candidate point exceeds this multiple
/// of the median distance (with the absolute floor below).
const TRIM_FACTOR: f64 = 3.0;

/// Distances under this never count as outliers, whatever the median.
const TRIM_FLOOR_MM: f64 = 5.0;

// --- Intersection ---

/// The nearest point to all rays `(origin, unit dir)` and the RMS perpendicular
/// distance, millimetres. `None` when the bundle is degenerate: fewer than two rays,
/// or rays so nearly parallel the point is unconstrained along their direction.
///
/// Blinks and stray mistracks put rays nowhere near the fixation, and the plain LS
/// solve is defenceless against them: its cost is convex but sharply anisotropic
/// (every ray passes near the eye cluster, so the soft axis runs eyes-to-target),
/// and a couple of wild rays drag the unique minimum hundreds of millimetres along
/// it. So the solve seeds from the component-wise median of pairwise closest-point
/// midpoints — a minority of wild rays cannot move a median — trims against that
/// seed, and only then least-squares the survivors, with further trim rounds
/// against the refined estimate.
pub fn intersect_rays(rays: &[(DVec3, DVec3)]) -> Option<(DVec3, f64)> {
    let mut keep = vec![true; rays.len()];

    if let Some(seed) = median_midpoint(rays) {
        let mut dists: Vec<f64> = rays.iter().map(|r| distance(r, seed)).collect();
        dists.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let cutoff = (dists[dists.len() / 2] * TRIM_FACTOR).max(TRIM_FLOOR_MM);

        for (k, r) in keep.iter_mut().zip(rays) {
            *k = distance(r, seed) <= cutoff;
        }

        if keep.iter().filter(|k| **k).count() < 2 {
            keep.iter_mut().for_each(|k| *k = true);
        }
    }

    let mut solution = solve(rays, &keep)?;

    for _ in 0..TRIM_ROUNDS {
        let mut dists: Vec<f64> = rays.iter().zip(&keep)
            .filter(|(_, k)| **k)
            .map(|(r, _)| distance(r, solution.0))
            .collect();

        if dists.len() < 2 {
            break;
        }

        dists.sort_by(|a, b| a.partial_cmp(b).unwrap());

        let cutoff      = (dists[dists.len() / 2] * TRIM_FACTOR).max(TRIM_FLOOR_MM);
        let mut changed = false;

        for (i, r) in rays.iter().enumerate() {
            if keep[i] && distance(r, solution.0) > cutoff {
                keep[i] = false;
                changed = true;
            }
        }

        if !changed {
            break;
        }

        solution = solve(rays, &keep)?;
    }

    Some(solution)
}

/// One least-squares solve over the kept rays.
///
/// Minimising the summed squared perpendicular distance gives the normal equations
/// `sum(I - d d^T) p = sum((I - d d^T) o)`.
// The accumulation indexes rows and columns in lockstep; iterator chains would
// obscure the linear algebra.
#[allow(clippy::needless_range_loop)]
fn solve(rays: &[(DVec3, DVec3)], keep: &[bool]) -> Option<(DVec3, f64)> {
    let mut a = [[0.0; 3]; 3];
    let mut b = [0.0; 3];
    let mut n = 0usize;

    for ((origin, dir), _) in rays.iter().zip(keep).filter(|(_, k)| **k) {
        let d = [dir.x, dir.y, dir.z];
        let o = [origin.x, origin.y, origin.z];

        for i in 0..3 {
            for j in 0..3 {
                let kron = if i == j { 1.0 } else { 0.0 };
                let m    = kron - d[i] * d[j];

                a[i][j] += m;
                b[i]    += m * o[j];
            }
        }

        n += 1;
    }

    if n < 2 {
        return None;
    }

    let p     = solve3(&a, &b)?;
    let point = DVec3::new(p[0], p[1], p[2]);

    let mut sum = 0.0;
    let mut m   = 0usize;

    for (r, _) in rays.iter().zip(keep).filter(|(_, k)| **k) {
        sum += distance(r, point).powi(2);
        m   += 1;
    }

    Some((point, (sum / m as f64).sqrt()))
}

/// The component-wise median of closest-point midpoints over a spread of ray pairs,
/// pairing each ray with the one half the bundle away to maximise baseline. `None`
/// when no pair has a usable baseline (a parallel bundle).
fn median_midpoint(rays: &[(DVec3, DVec3)]) -> Option<DVec3> {
    let n = rays.len();

    if n < 2 {
        return None;
    }

    let step = (n / 64).max(1);

    let mut xs = Vec::new();
    let mut ys = Vec::new();
    let mut zs = Vec::new();

    for i in (0..n).step_by(step) {
        let j = (i + n / 2) % n;

        if i == j {
            continue;
        }

        let Some(m) = pair_midpoint(rays[i], rays[j]) else {
            continue;
        };

        xs.push(m.x);
        ys.push(m.y);
        zs.push(m.z);
    }

    if xs.is_empty() {
        return None;
    }

    Some(DVec3::new(median(&mut xs), median(&mut ys), median(&mut zs)))
}

/// Midpoint of the closest points of two lines. `None` when they are near parallel.
fn pair_midpoint((o1, d1): (DVec3, DVec3), (o2, d2): (DVec3, DVec3)) -> Option<DVec3> {
    let r     = o1 - o2;
    let a     = d1.dot(d2);
    let denom = 1.0 - a * a;

    if denom < 1e-6 {
        return None;
    }

    let t1 = (a * r.dot(d2) - r.dot(d1)) / denom;
    let t2 = r.dot(d2) + a * t1;

    Some(((o1 + d1 * t1) + (o2 + d2 * t2)) * 0.5)
}

/// Median of a slice; zero when empty.
fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return 0.0;
    }

    values.sort_by(|a, b| a.partial_cmp(b).unwrap());

    values[values.len() / 2]
}

/// Perpendicular distance from a ray to a point.
fn distance((origin, dir): &(DVec3, DVec3), p: DVec3) -> f64 {
    let v = p - *origin;

    (v - *dir * v.dot(*dir)).length()
}

/// Solves a 3x3 linear system by Gaussian elimination with partial pivoting. `None`
/// when the matrix is singular relative to its own scale (for a ray bundle: all rays
/// parallel, leaving the point unconstrained along the common direction).
// The elimination indexes several arrays in lockstep; iterator chains would obscure
// the linear algebra.
#[allow(clippy::needless_range_loop)]
pub(crate) fn solve3(a: &[[f64; 3]; 3], b: &[f64; 3]) -> Option<[f64; 3]> {
    let mut m = *a;
    let mut v = *b;

    let scale = m.iter()
        .flat_map(|row| row.iter())
        .fold(0.0_f64, |acc, x| acc.max(x.abs()));

    if scale <= 0.0 {
        return None;
    }

    for col in 0..3 {
        let mut pivot = col;

        for row in col + 1..3 {
            if m[row][col].abs() > m[pivot][col].abs() {
                pivot = row;
            }
        }

        if m[pivot][col].abs() < 1e-9 * scale {
            return None;
        }

        m.swap(col, pivot);
        v.swap(col, pivot);

        for row in col + 1..3 {
            let f = m[row][col] / m[col][col];

            for k in col..3 {
                m[row][k] -= f * m[col][k];
            }

            v[row] -= f * v[col];
        }
    }

    let mut x = [0.0; 3];

    for col in (0..3).rev() {
        let mut sum = v[col];

        for k in col + 1..3 {
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

    /// Rays through `target` from a sideways sweep of origins.
    fn sweep_rays(target: DVec3, n: usize) -> Vec<(DVec3, DVec3)> {
        (0..n)
            .map(|i| {
                let a      = i as f64 / (n - 1) as f64;
                let origin = DVec3::new(-60.0 + 120.0 * a, 150.0 + 10.0 * (a * 7.0).sin(), 620.0);

                (origin, (target - origin).normalize())
            })
            .collect()
    }

    #[test]
    fn exact_rays_recover_the_point() {
        let target = DVec3::new(35.0, 210.0, -18.0);
        let (p, rms) = intersect_rays(&sweep_rays(target, 40)).expect("solve");

        assert!(p.distance(target) < 1e-6, "point off by {}", p.distance(target));
        assert!(rms < 1e-6);
    }

    #[test]
    fn noisy_rays_average_down() {
        let target   = DVec3::new(-120.0, 180.0, 5.0);
        let mut rays = sweep_rays(target, 200);

        // Half a degree of deterministic angular noise, the device class.
        for (i, (_, d)) in rays.iter_mut().enumerate() {
            let axis = DVec3::new((i as f64).sin(), (i as f64 * 1.7).cos(), 0.4).normalize();
            let quat = glam::DQuat::from_axis_angle(axis, 0.5_f64.to_radians());

            *d = (quat * *d).normalize();
        }

        let (p, _) = intersect_rays(&rays).expect("solve");
        assert!(p.distance(target) < 15.0, "point off by {} mm", p.distance(target));
    }

    #[test]
    fn outliers_are_trimmed() {
        let target   = DVec3::new(0.0, 200.0, 0.0);
        let mut rays = sweep_rays(target, 60);

        // A blink-edge mistrack pointing somewhere else entirely.
        rays.push((DVec3::new(0.0, 150.0, 620.0), DVec3::new(0.6, -0.5, -0.62).normalize()));
        rays.push((DVec3::new(10.0, 150.0, 620.0), DVec3::new(-0.7, 0.1, -0.7).normalize()));

        let (p, rms) = intersect_rays(&rays).expect("solve");
        assert!(p.distance(target) < 1.0, "point off by {} mm", p.distance(target));
        assert!(rms < 1.0, "rms {} mm", rms);
    }

    #[test]
    fn parallel_bundle_is_rejected() {
        let dir  = DVec3::new(0.1, -0.2, -0.97).normalize();
        let rays: Vec<(DVec3, DVec3)> = (0..20)
            .map(|i| (DVec3::new(i as f64, 150.0, 620.0), dir))
            .collect();

        assert!(intersect_rays(&rays).is_none());
    }
}
