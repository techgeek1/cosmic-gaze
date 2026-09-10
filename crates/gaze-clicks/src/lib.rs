//! gaze-clicks: the real mouse and the accessibility tree, on threads of their own.
//!
//! What is left of the passive click collector (removed 2026-09-10 with the residual
//! model it fed; git history has it): the two pieces the running session still uses.
//!
//! - [`mouse`]: the evdev reader that watches the mouse the user is actually working
//!   with, read-only, never grabbed, every node that looks like a mouse at once. The
//!   session feeds its presses to the ET5's online offset as gaze labels.
//! - [`tree`]: the accessibility tree on a thread that is watched and replaced when a
//!   blocked D-Bus call wedges it, so a question about what is under the gaze never
//!   stalls the sample loop.
//! - [`click`]: what a press or release looks like on the way out of the reader.

// The workspace style mandates explicit `Foo { x: x }` field syntax everywhere, which
// clippy reads as redundant. Same allow as `gaze-core`.
#![allow(clippy::redundant_field_names)]

pub mod click;
pub mod mouse;
pub mod tree;

pub use click::{Button, ButtonEvent, MultiCounter, PressKind, classify};
pub use mouse::{Candidate, MouseReader};
pub use tree::{Question, TreeReply, TreeRequest, TreeService};
