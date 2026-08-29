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
//! # Why recognition is local but not a crop
//!
//! Recognition runs around the pointer, but not by cutting a crop and handing it to the
//! models as a whole image. That was the first version, and it lost the widgets that
//! matter: out of the very same pixels, a 512 px crop's pick agreed with the full frame
//! **0 times out of 12** over the twelve largest widgets on a working desktop, coming
//! back with their inner OCR text instead. The losses are wide flat things (list rows, a
//! URL bar) the widget model can only see with their surrounding layout.
//!
//! `Detector::detect_near` keeps the layout and drops only work: the *same* tile plan the
//! whole-frame pass builds, restricted to the one to four tiles that contain the pointer.
//! The text model then runs at native resolution over a 640 px window, which is where the
//! whole-frame pass was losing instead: shrinking a 3840 px panel to 1600 merges lines
//! into paragraph blobs, and locally there is no reason to shrink at all. [`perceive`]
//! carries the measurement and `examples/recognition_check.rs` reruns it.
//!
//! # What it refuses
//!
//! A drag, a click on an output the desk config does not describe, a click with no
//! usable screen frame, a click on nothing recognisable, a click on a large box whose
//! pixels under the pointer are flat, and a click whose approach carried no gaze. The
//! fourth and fifth are the important ones: a click on empty space to focus a window
//! says nothing about where the user was looking, and it is by far the most common press
//! on a desktop. See [`collect`] for the order the rules apply in.
//!
//! # Threads
//!
//! - [`mouse`]: the evdev reader. Stamps presses and fires their captures.
//! - [`perceive`]: two of them. A capture thread owning everything Wayland, which must
//!   never block for longer than one screen capture, and a detect thread owning the
//!   models, which is allowed to be the slow one.
//! - [`tracker`]: device frames into a ring.
//! - [`collect`]: the rules and the writing, on the calling thread.

// The workspace style mandates explicit `Foo { x: x }` field syntax everywhere, which
// clippy reads as redundant. Same allow as `gaze-core`.
#![allow(clippy::redundant_field_names)]

pub mod click;
pub mod cursor;
pub mod collect;
pub mod element;
pub mod frames;
pub mod mouse;
pub mod perceive;
pub mod session;
pub mod tracker;

pub use click::{Button, ButtonEvent, MultiCounter, PressKind, classify};
pub use collect::{CollectConfig, Outcome, Tallies};
pub use element::{
    Crop, FLAT_CHECK_MIN_H_PX, FLAT_HALF_PX, FLAT_LUMA_SD, LUMA_HALF_PX, MAX_WIDGET_H_PX,
    MAX_WIDGET_W_PX, OCR_PX, Pick, WIDGET_MIN_SCORE, collector_config, crop_around, kind_name,
    luma_sd, mean_luma, pick, smallest_containing,
};
pub use frames::{FrameChoice, FrameDedup, GazeRing, RollingCache, gaze_fraction, select_frame};
pub use mouse::{Candidate, MouseReader};
pub use perceive::{Perception, PerceptionConfig, PointerSample};
pub use session::ClickSession;
pub use tracker::TrackerFeed;
