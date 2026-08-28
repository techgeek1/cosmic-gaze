//! gaze-clicks: passive gaze labels from the mouse the user is already using.
//!
//! People look at what they click. So every deliberate click on a recognisable control
//! is a labelled gaze sample, free, all day, with no calibration screen and no dot to
//! stare at. This crate collects them: it reads the real mouse read-only, captures the
//! screen at the moment of each press, recognises what was under the pointer, and
//! writes the gaze frames around the press against that element's position.
//!
//! The output is `gaze-provider-et5`'s session format, unchanged, so
//! `gaze-et5-cli dataset export` and the Phase C harness read a day of clicks exactly
//! as they read a five-minute recorded session. The rows carry `session_phase =
//! "click"` and four extra columns describing the element and the screen luminance.
//!
//! # Why the capture is on the press
//!
//! Everything that destroys a click target fires on the *release*: the menu item
//! activates, the link navigates, the popup dismisses. Between press and release the
//! target is still under the pointer. So the press starts the capture, and only when
//! that capture stalls does a slow rolling frame stand in. See [`frames`] for the
//! selection rule.
//!
//! # What it refuses
//!
//! A drag, a click on an output the desk config does not describe, a click with no
//! usable screen frame, a click on nothing recognisable, and a click whose approach
//! carried no gaze. The fourth is the important one: a click on empty space to focus a
//! window says nothing about where the user was looking, and it is by far the most
//! common press on a desktop. See [`collect`] for the order the rules apply in.
//!
//! # Threads
//!
//! - [`mouse`]: the evdev reader. Stamps presses and fires their captures.
//! - [`perceive`]: pointer, capture and recognition. Owns everything Wayland.
//! - [`tracker`]: device frames into a ring.
//! - [`collect`]: the rules and the writing, on the calling thread.

// The workspace style mandates explicit `Foo { x: x }` field syntax everywhere, which
// clippy reads as redundant. Same allow as `gaze-core`.
#![allow(clippy::redundant_field_names)]

pub mod click;
pub mod collect;
pub mod element;
pub mod frames;
pub mod mouse;
pub mod perceive;
pub mod session;
pub mod tracker;

pub use click::{Button, ButtonEvent, MultiCounter, PressKind, classify};
pub use collect::{CollectConfig, Outcome, Tallies};
pub use element::{Crop, crop_around, kind_name, mean_luma, smallest_containing};
pub use frames::{FrameChoice, FrameDedup, GazeRing, RollingCache, gaze_fraction, select_frame};
pub use mouse::{Candidate, MouseReader};
pub use perceive::{Perception, PerceptionConfig, PointerSample};
pub use session::ClickSession;
pub use tracker::TrackerFeed;
