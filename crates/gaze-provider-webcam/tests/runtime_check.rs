//! The runtime path must apply a calibration exactly as the fitter evaluated it.
//!
//! This is a regression test for a real failure. A fitted correction looked healthy in
//! every number the calibration printed, and the live pointer reached only about half the
//! distance to the panel edges. Nothing in the residual table could show it, because the
//! residual table only ever looks at the calibration targets. The two things that catch it
//! are here: replaying a real sweep through the provider's own entry point and comparing
//! with what the fit claimed, and checking that the fitted correction behaves *between*
//! its targets.
//!
//! The fixture is a real capture: nine targets across all three panels, five samples each,
//! taken from a sweep on this desk. It is committed rather than generated so that a change
//! to the capture path cannot quietly change what is being tested.

use gaze_core::DesktopGeometry;
use gaze_provider_webcam::sweep::SweepRecord;
use gaze_provider_webcam::{CameraPose, check, sweep, webcam_profile};

const DESK_TOML: &str = include_str!("../../../config/desk.toml");
const FIXTURE: &str   = include_str!("fixtures/sweep.jsonl");

/// Largest RMS difference between the fit and the replay that is not a bug, degrees.
/// Everything on both sides is `f64` over the same inputs, so this is rounding.
const TOLERANCE_DEG: f64 = 0.01;

fn desk() -> (DesktopGeometry, CameraPose) {
    (
        DesktopGeometry::from_toml(DESK_TOML).unwrap(),
        CameraPose::from_desk_toml(DESK_TOML).unwrap(),
    )
}

fn fixture() -> Vec<SweepRecord> {
    FIXTURE
        .lines()
        .filter(|l| !l.trim().is_empty())
        .map(|l| serde_json::from_str(l).expect("fixture line must parse"))
        .collect()
}

#[test]
fn the_runtime_path_reproduces_the_rms_the_fit_reported() {
    let (geometry, camera) = desk();
    let records            = fixture();

    assert_eq!(records.len(), 45);

    let observations = sweep::from_records(&geometry, &camera, &records);
    assert_eq!(observations.len(), 9);

    let report = sweep::fit(&geometry, &camera, &observations, 2.5);
    assert!(report.chosen.is_some(), "the fixture must produce a usable model");

    let profile = webcam_profile(2.5);
    let replay  = check::replay(&geometry, &camera, &report.calibration, &profile, &records).unwrap();

    // The headline: what the provider does agrees with what the fit measured.
    let gap = replay.disagreement(&report.calibration);

    assert!(
        gap <= TOLERANCE_DEG,
        "runtime path gives {:.4} deg, the fit claimed {:.4} deg (gap {gap:.4})",
        replay.rms_runtime_deg,
        report.calibration.rms_deg,
    );

    // And the two stage-one entry points agree to rounding on every target. These are
    // different code: one calls the polynomial directly, the other goes camera to desk and
    // back around it.
    assert!(
        replay.worst_path_gap_deg < 1.0e-6,
        "the two stage-one paths disagree by {:.6} deg",
        replay.worst_path_gap_deg,
    );

    assert_eq!(replay.targets.len(), 9);
}

#[test]
fn an_uncalibrated_replay_reports_the_raw_error_rather_than_nothing() {
    let (geometry, camera) = desk();
    let records            = fixture();
    let profile            = webcam_profile(2.5);

    // An identity calibration is what a fresh install runs. The replay must still produce
    // a number, and it must be a large one: this stream is badly uncorrected.
    let identity = gaze_provider_webcam::Calibration::identity();
    let replay   = check::replay(&geometry, &camera, &identity, &profile, &records).unwrap();

    assert!(replay.rms_runtime_deg.is_finite());
    assert!(
        replay.rms_runtime_deg > 5.0,
        "an uncorrected webcam stream should be well off, got {:.2} deg",
        replay.rms_runtime_deg,
    );
}

#[test]
fn the_fitted_correction_behaves_between_its_targets() {
    let (geometry, camera) = desk();
    let observations       = sweep::from_records(&geometry, &camera, &fixture());

    let report = sweep::fit(&geometry, &camera, &observations, 2.5);
    let points = sweep::gain_points(&observations);

    let (lo, hi) = report
        .calibration
        .angle
        .gain_bounds(&points)
        .expect("a fitted correction must have a measurable gain");

    // The live symptom was a correction whose gain ran from 0.35 to 3.05 across the desk:
    // right at every target, and at roughly half scale everywhere the user actually looked.
    assert!(lo > 0.0, "the correction folds the field over: gain {lo:.2}");
    assert!(
        (0.25..=2.5).contains(&lo) && (0.25..=2.5).contains(&hi),
        "correction gain {lo:.2} to {hi:.2} is outside the sane band",
    );
}

#[test]
fn every_rejected_candidate_says_why() {
    let (geometry, camera) = desk();
    let observations       = sweep::from_records(&geometry, &camera, &fixture());

    let report = sweep::fit(&geometry, &camera, &observations, 2.5);

    // Every stage-one shape is always considered, with and without the pixel stage. A
    // candidate that vanishes without a reason is one nobody can argue with later.
    assert_eq!(report.candidates.len(), gaze_provider_webcam::angle::candidates().len() * 2);

    for c in &report.candidates {
        if c.rejected.is_none() {
            assert!(c.rms_loo_deg.is_finite(), "an accepted candidate must be scored");
        }
    }
}
