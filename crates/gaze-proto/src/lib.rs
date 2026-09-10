//! gaze-proto: the session loop, wiring every other crate in the workspace into one
//! running gaze pointer. The daemon (`gazed`) runs it as a library through [`session::run`]
//! with a [`SessionConfig`] built from the XDG locations and a [`Live`] it drives; the
//! `gaze-proto` binary runs the same loop from flags, as the dev harness, with the debug
//! look and a frozen offset on offer.
//!
//! Two concurrent pieces, described in `PLAN.md`'s "Live (the feel)" paragraph:
//!
//! * [`perception`] owns a Wayland capture connection and both ONNX models on its own
//!   thread. It walks the enabled outputs at about 5 Hz, re-runs detection on an output
//!   only when the picture changed or the interval expired, and publishes one merged
//!   element list through an [`ElementStore`].
//! * [`session`] is the gaze loop: samples from the tracker go through the filter
//!   stack, the snap engine, and out to the overlay; the controller's pad commits,
//!   Home exits, and the real mouse's presses feed the tracker's online offset.
//!
//! Under the loop: [`config`] is what a session is built from and how the tuning maps onto
//! its parts, [`live`] is what crosses between the loop and its owner while it runs,
//! [`source`] is the tracker as the session sees it, [`score`] counts and times the
//! commits, and [`warp`] remembers where the pointer was last sent.

// The workspace style writes struct fields out in full, aligned, even when the value
// happens to share the field's name. Same allow as every other crate here.
#![allow(clippy::redundant_field_names)]

pub mod cli;
pub mod config;
pub mod daydream;
pub mod edge_scroll;
pub mod feedback;
pub mod keys;
pub mod live;
pub mod surface;
pub mod perception;
pub mod score;
pub mod session;
pub mod source;
pub mod verify;
pub mod warp;

pub use cli::Args;
pub use config::{DaydreamSpec, OverlayMode, SessionConfig, SourceSpec};
pub use feedback::ClickFeed;
pub use live::Live;
pub use perception::{ElementStore, Perception, PerceptionConfig};
pub use score::Scoreboard;
pub use source::{Control, GazeSource};
pub use warp::{WarpReason, Warper};
