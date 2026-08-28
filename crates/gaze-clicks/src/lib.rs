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
//! # Why the whole frame is recognised
//!
//! Recognition runs on the entire captured output, not on a crop around the pointer.
//! A crop is much cheaper and it loses the widgets that matter: cut out of the very
//! same pixels, a 512 px crop's pick agreed with the full frame **0 times out of 12**
//! over the twelve largest widgets on a working desktop, coming back with their inner
//! OCR text instead. The losses are wide flat things (list rows, a URL bar) that the
//! widget model can only see with their surrounding layout. [`perceive`] carries the
//! measurement and `examples/recognition_check.rs` reruns it.
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
//! - [`perceive`]: two of them. A capture thread owning everything Wayland, which must
//!   never block for longer than one screen capture, and a detect thread owning the
//!   models, which is allowed to take its 250 to 400 ms per frame.
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
pub use element::{Crop, LUMA_HALF_PX, crop_around, kind_name, mean_luma, smallest_containing};
pub use frames::{FrameChoice, FrameDedup, GazeRing, RollingCache, gaze_fraction, select_frame};
pub use mouse::{Candidate, MouseReader};
pub use perceive::{Perception, PerceptionConfig, PointerSample};
pub use session::ClickSession;
pub use tracker::TrackerFeed;
