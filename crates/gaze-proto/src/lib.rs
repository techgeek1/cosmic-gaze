//! gaze-proto: the session loop, wiring every other crate in the workspace into one
//! running gaze pointer. The daemon (`gazed`) runs it as a library through [`session::run`]
//! with a [`SessionConfig`] built from the XDG locations and a [`Live`] it drives; the
//! `gaze-proto` binary runs the same loop from flags, as the dev harness, with the other
//! providers and the debug look.
//!
//! Three concurrent pieces, described in `PLAN.md`'s "Live (the feel)" paragraph:
//!
//! * [`perception`] owns a Wayland capture connection and both ONNX models on its own
//!   thread. It walks the enabled outputs at about 5 Hz, re-runs detection on an output
//!   only when the picture changed or the interval expired, and publishes one merged
//!   element list through an [`ElementStore`].
//! * [`session`] is the gaze loop: samples from the tracker (or a grabbed mouse driven
//!   through the desk's noise model) go through the filter stack, the snap engine, and
//!   out to the overlay; the controller's pad, or the mouse's buttons, commit, exit, and
//!   force a redetect.
//! * [`score`] judges each commit against the provider's noise-free truth point, which is
//!   the number this whole prototype exists to produce. Only the synthetic provider has
//!   one, so with any other provider commits are counted and timed but not graded.
//!
//! Under the loop: [`config`] is what a session is built from and how the tuning maps onto
//! its parts, [`live`] is what crosses between the loop and its owner while it runs,
//! [`source`] hides which provider the samples came from and where the controls are read,
//! [`buttons`] is the evdev reader those controls come from when the provider does not
//! own a device of its own, and [`warp`] remembers where the pointer was last sent.

// The workspace style writes struct fields out in full, aligned, even when the value
// happens to share the field's name. Same allow as every other crate here.
#![allow(clippy::redundant_field_names)]

pub mod buttons;
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

pub use cli::{Args, Provider};
pub use config::{DaydreamSpec, OverlayMode, SessionConfig, SourceSpec};
pub use feedback::ClickFeed;
pub use live::Live;
pub use perception::{ElementStore, Perception, PerceptionConfig};
pub use score::{Outcome, Scoreboard, classify};
pub use source::{Control, GazeSource};
pub use warp::{WarpReason, Warper};
