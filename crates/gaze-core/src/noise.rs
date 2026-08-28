//! Angular error model for the synthetic provider. Error is specified in degrees and is
//! a function of the angle between the gaze direction and the tracker axis, so the
//! prototype exercises the precision-cone / coarse-tier handoff that a real remote
//! tracker imposes (see DESIGN.md section 11).

use serde::{Deserialize, Serialize};

/// Piecewise sigma profile against off-axis angle.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct SigmaProfile {
    /// Sigma inside the comfortable envelope, degrees. ET5-class is about 0.7.
    pub sigma_deg      : f64,
    /// Off-axis angle up to which `sigma_deg` holds. About 25 for a remote PCCR tracker.
    pub flat_to_deg    : f64,
    /// Off-axis angle at which sigma has grown to `sigma_deg * ramp_factor`. About 35.
    pub ramp_to_deg    : f64,
    pub ramp_factor    : f64,
    /// Off-axis angle beyond which samples are reported invalid. About 40.
    pub invalid_at_deg : f64,
}

/// Full error model: sigma profile plus slow drift and latency.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct NoiseModel {
    pub profile         : SigmaProfile,
    /// Per-sample jitter, degrees (1-sigma, per axis). Real trackers' accuracy figures are
    /// dominated by a per-fixation bias; sample-to-sample precision is far tighter
    /// (0.1 to 0.3 deg). The profile's sigma is the *total* error; the bias sigma is
    /// `sqrt(sigma^2 - jitter^2)`, drawn once per fixation (re-drawn when the clean point
    /// moves more than `bias_redraw_deg`), and the jitter is drawn every sample. Zero makes
    /// every sample independent at the full sigma, which is what the first bench measured
    /// and which starves the I-VT fixation classifier.
    #[serde(default = "default_jitter_deg")]
    pub jitter_deg      : f64,
    /// Clean-point movement, degrees, that counts as a new fixation for bias re-draw.
    #[serde(default = "default_bias_redraw_deg")]
    pub bias_redraw_deg : f64,
    /// Random-walk drift magnitude, degrees per sqrt(second). Zero disables drift.
    #[serde(default)]
    pub drift_deg       : f64,
    /// Fixed delay added to every sample's timestamp, seconds.
    #[serde(default)]
    pub latency_s       : f64,
    /// Sample rate the provider emits at, Hz.
    pub rate_hz         : f64,
}

// --- SigmaProfile ---

impl SigmaProfile {
    /// Sigma in degrees for a gaze direction `off_axis_deg` away from the tracker axis,
    /// or `None` when tracking is lost at that angle.
    pub fn sigma_at(&self, off_axis_deg: f64) -> Option<f64> {
        // The profile is symmetric about the tracker axis, so only the magnitude matters.
        let off = off_axis_deg.abs();

        if off >= self.invalid_at_deg {
            return None;
        }

        if off <= self.flat_to_deg {
            return Some(self.sigma_deg);
        }

        // Slope of the ramp, in sigma-degrees per off-axis degree. A zero-width ramp is a
        // step straight to the ramped sigma rather than a division by zero.
        let span = self.ramp_to_deg - self.flat_to_deg;

        if span <= 0.0 {
            return Some(self.sigma_deg * self.ramp_factor);
        }

        // The same slope continues past `ramp_to_deg` until the profile goes invalid, so
        // sigma keeps growing instead of flattening into a plateau nobody intended.
        let slope = self.sigma_deg * (self.ramp_factor - 1.0) / span;

        Some(self.sigma_deg + slope * (off - self.flat_to_deg))
    }
}

// --- NoiseModel ---

impl NoiseModel {
    /// Per-fixation bias sigma for a total `sigma_deg`: the part of the error the jitter
    /// does not explain. Clamped at zero when the jitter alone exceeds the total.
    pub fn bias_sigma(&self, sigma_deg: f64) -> f64 {
        (sigma_deg * sigma_deg - self.jitter_deg * self.jitter_deg).max(0.0).sqrt()
    }
}

fn default_jitter_deg() -> f64 {
    0.2
}

fn default_bias_redraw_deg() -> f64 {
    1.0
}

impl Default for SigmaProfile {
    fn default() -> Self {
        Self {
            sigma_deg      : 0.7,
            flat_to_deg    : 25.0,
            ramp_to_deg    : 35.0,
            ramp_factor    : 3.0,
            invalid_at_deg : 40.0,
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    use crate::geometry::DesktopGeometry;

    /// The frozen 2026-08-25 desk snapshot these assertions were written against;
    /// the live `config/desk.toml` drifts with the physical desk.
    const FIXTURE_TOML: &str = include_str!("../../../config/desk-fixture.toml");

    #[test]
    fn desk_profile_matches_the_config() {
        let g = DesktopGeometry::from_toml(FIXTURE_TOML).unwrap();
        let n = g.noise.expect("desk config carries a noise model");

        assert_eq!(n.rate_hz, 120.0);
        assert_eq!(n.drift_deg, 0.0);
        assert_eq!(n.latency_s, 0.0);
        assert_eq!(n.profile, SigmaProfile::default());
    }

    #[test]
    fn sigma_is_flat_inside_the_envelope() {
        let p = SigmaProfile::default();

        for off in [0.0, 1.0, 12.5, 24.999, 25.0] {
            assert_eq!(p.sigma_at(off), Some(0.7), "off-axis {off}");
        }

        // The profile is symmetric: only the magnitude of the off-axis angle matters.
        assert_eq!(p.sigma_at(-20.0), Some(0.7));
    }

    #[test]
    fn sigma_ramps_linearly_between_the_breakpoints() {
        let p = SigmaProfile::default();

        // 0.7 at 25 degrees to 2.1 at 35, so 0.14 per degree.
        let at = |d: f64| p.sigma_at(d).unwrap();

        assert!((at(30.0) - 1.4).abs() < 1.0e-12, "midpoint = {}", at(30.0));
        assert!((at(35.0) - 2.1).abs() < 1.0e-12, "ramp end = {}", at(35.0));
        assert!((at(27.5) - 1.05).abs() < 1.0e-12);

        // Continuous at the start of the ramp.
        assert!((at(25.0 + 1.0e-9) - 0.7).abs() < 1.0e-6);

        // Monotonically increasing across the ramp.
        let mut prev = at(25.0);

        for step in 1..=100 {
            let s = at(25.0 + step as f64 * 0.1);

            assert!(s >= prev, "sigma dropped at {}", 25.0 + step as f64 * 0.1);
            prev = s;
        }
    }

    #[test]
    fn sigma_keeps_the_ramp_slope_past_the_ramp_breakpoint() {
        let p = SigmaProfile::default();

        // Same 0.14 per degree continues from 35 up to the invalid cutoff.
        assert!((p.sigma_at(37.5).unwrap() - 2.45).abs() < 1.0e-12);
        assert!((p.sigma_at(39.999).unwrap() - 2.79986).abs() < 1.0e-9);
    }

    #[test]
    fn sigma_is_none_at_and_beyond_the_invalid_cutoff() {
        let p = SigmaProfile::default();

        assert_eq!(p.sigma_at(40.0), None);
        assert_eq!(p.sigma_at(40.001), None);
        assert_eq!(p.sigma_at(90.0), None);
        assert_eq!(p.sigma_at(-45.0), None);
    }

    #[test]
    fn sigma_handles_a_zero_width_ramp() {
        let p = SigmaProfile {
            sigma_deg      : 1.0,
            flat_to_deg    : 20.0,
            ramp_to_deg    : 20.0,
            ramp_factor    : 4.0,
            invalid_at_deg : 30.0,
        };

        // No division by zero: the ramp degenerates into a step at the breakpoint.
        assert_eq!(p.sigma_at(20.0), Some(1.0));
        assert_eq!(p.sigma_at(25.0), Some(4.0));
        assert_eq!(p.sigma_at(30.0), None);
    }
}
