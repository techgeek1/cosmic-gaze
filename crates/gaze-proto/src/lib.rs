//! gaze-proto: the live phase 0 loop, wiring every other crate in the workspace into one
//! running prototype.
//!
//! Three concurrent pieces, described in `PLAN.md`'s "Live (the feel)" paragraph:
//!
//! * [`perception`] owns a Wayland capture connection and both ONNX models on its own
//!   thread. It walks the enabled outputs at about 5 Hz, re-runs detection on an output
//!   only when the picture changed or the interval expired, and publishes one merged
//!   element list through an [`ElementStore`].
//! * [`session`] is the gaze loop: a grabbed mouse driven through the desk's noise model
//!   becomes gaze samples, which go through the filter stack, the snap engine, and out to
//!   the overlay. The grabbed mouse's own buttons commit, exit, and force a redetect.
//! * [`score`] judges each commit against the provider's noise-free truth point, which is
//!   the number this whole prototype exists to produce. Only the synthetic provider has
//!   one, so with any other provider commits are counted and timed but not graded.
//!
//! Three smaller modules sit under the session loop: [`source`] hides which provider the
//! samples came from and where the commit/exit/redetect/wheel controls are read, [`buttons`]
//! is the evdev reader those controls come from when the provider does not own a device of
//! its own, and [`warp`] holds the policy deciding when the pointer is allowed to jump to
//! the gaze point.
//!
//! Nothing here is a library anybody else should link. The crate is split into modules
//! rather than one `main.rs` so the scoring rules can be unit tested without a compositor,
//! a model file, or a mouse.

// The workspace style writes struct fields out in full, aligned, even when the value
// happens to share the field's name. Same allow as every other crate here.
#![allow(clippy::redundant_field_names)]

pub mod buttons;
pub mod cli;
pub mod daydream;
pub mod edge_scroll;
pub mod feedback;
pub mod keys;
pub mod surface;
pub mod perception;
pub mod score;
pub mod session;
pub mod source;
pub mod verify;
pub mod warp;

pub use cli::{Args, Provider};
pub use feedback::ClickFeed;
pub use perception::{ElementStore, Perception, PerceptionConfig};
pub use score::{Outcome, Scoreboard, classify};
pub use source::{Control, GazeSource, WebcamHealth};
pub use warp::{WarpReason, Warper};
