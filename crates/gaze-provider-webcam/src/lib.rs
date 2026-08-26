//! gaze-provider-webcam: a `GazeProvider` fed by the Python gaze sidecar, plus the
//! calibration that makes an appearance-based gaze model usable across three panels.
//!
//! The sidecar (`sidecar/`) does the vision: it reads the webcam, runs a face and gaze
//! model, and writes one JSON object per frame to a Unix socket, in the OpenCV camera
//! frame. Everything from there on is this crate: camera frame to desk frame, calibration,
//! intersection with the desk, and a `GazeSample` that looks like any other provider's.
//!
//! ```no_run
//! use gaze_core::DesktopGeometry;
//! use gaze_provider_synthetic::GazeProvider;
//! use gaze_provider_webcam::{CameraPose, WebcamProvider};
//!
//! let text     = std::fs::read_to_string("config/desk.toml")?;
//! let geometry = DesktopGeometry::from_toml(&text)?;
//! let camera   = CameraPose::from_desk_toml(&text)?;
//!
//! let mut provider = WebcamProvider::create()
//!     .socket("/run/user/1000/gaze-ml.sock")
//!     .geometry(geometry)
//!     .camera(camera)
//!     .calibration(Some("config/calibration.toml"))
//!     .start()?;
//!
//! while let Some(sample) = provider.next() {
//!     println!("{:?} sigma {}", sample.point, sample.sigma_deg);
//! }
//! # Ok::<(), anyhow::Error>(())
//! ```
//!
//! The webcam may not be there and the sidecar may not be running. Neither is an error:
//! the provider reconnects with backoff, and `crate::fake::FakeSidecar` speaks the same
//! protocol well enough to develop and demonstrate the whole calibration flow with no
//! camera at all.

// The workspace style requires explicit `Foo { x: x }` field syntax everywhere, which
// clippy reads as redundant. The style rule wins inside this crate.
#![allow(clippy::redundant_field_names)]

pub mod angle;
pub mod calibration;
pub mod camera;
pub mod check;
pub mod experiment;
pub mod fake;
pub mod fit;
pub mod protocol;
pub mod provider;
pub mod socket;
pub mod sweep;

pub use calibration::{
    CALIBRATION_FORMAT, Calibration, CalibrationError, OutputCalibration, OutputGain, Resolved,
    TargetDiagnostics, TargetResidual, resolve,
};
pub use camera::{CameraError, CameraPose, gaze_dir_from_yaw_pitch_deg, gaze_yaw_pitch_deg};
pub use check::{CheckReport, TargetCheck};
pub use fake::{Aim, Distortion, FakeError, FakeGaze, FakeSidecar};
pub use fit::{AngleDegree, AnglePoly, AngleRow, PolyDegree, PolyMap};
pub use protocol::{ProtocolError, SidecarGaze, SidecarMessage};
pub use provider::{
    DEFAULT_SIGMA_DEG, DEFAULT_SOCKET, ProviderError, ProviderStats, RawGaze, Reading, SampleMeta,
    WebcamProvider, WebcamProviderBuilder, webcam_profile,
};
pub use sweep::{
    Advance, AngleSample, CalibrationSweep, Candidate, FitReport, Observation, Rejection,
    SweepEnd, SweepError, SweepOutcome, SweepSummary, SweepTarget, TargetHook, default_targets,
    gains, observed_range, terminal_keys,
};
