//! gaze-bench: the offline Monte Carlo that produces Phase 0's headline number.
//!
//! The chain is: screenshots of the real desktop -> detector boxes -> for every box, a
//! few dozen simulated fixations on it -> the snap engine -> did it come back with the
//! box we aimed at? See PLAN.md's "Offline (the number)" and the `gaze-bench` contract.
//!
//! The error model is deliberately the same one `gaze-provider-synthetic` uses at
//! runtime: lift the landing point to a ray from the nominal eye, rotate it by N(0,
//! sigma) degrees per axis, re-intersect the desk. Anything that misses every panel is a
//! lost sample, not a miss, and is counted separately.
//!
//! Nothing here reads a clock or a device. Given the same seed, the same screenshots and
//! the same detector cache, a run is bit-for-bit reproducible regardless of how many
//! threads rayon decides to use, because every trial draws from an RNG seeded from its
//! own coordinates.

// The workspace style writes struct fields out in full, aligned, even when the value
// happens to share the field's name. Same allow as `gaze-core`.
#![allow(clippy::redundant_field_names)]

pub mod overlay;
pub mod report;
pub mod run;
pub mod shots;
pub mod stats;
pub mod trial;

pub use overlay::write_overlay;
pub use report::{ReportInputs, render_report};
pub use run::{BenchConfig, CandidateSet, FeedMode, OverlayState, RunKey, RunResult, run_all};
pub use shots::{DeskScale, Shot, ShotSet, load_shots};
pub use stats::{ElementStats, FrameResult, SizeBucket, TargetClass, Tally};
pub use trial::{FixationNoise, Outcome, SigmaMode, TrialSetup, landing_point, trial_seed};
