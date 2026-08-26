//! gaze-snap: from a noisy stream of gaze samples to the target the user meant.
//!
//! Three pieces, all pure and deterministic. Nothing here reads a clock, opens a device,
//! or talks to Wayland: timestamps arrive on the samples and geometry arrives through a
//! [`PxScale`].
//!
//! * [`FilterStack`] classifies each sample as fixation or saccade (I-VT, 30 deg/s over a
//!   20 ms window) and smooths only during fixations (one-euro), so a warp lands where
//!   the eye landed.
//! * [`SnapEngine`] resolves a filtered sample against a list of element boxes by fuzzy
//!   hit testing, with hysteresis to keep the highlight steady, and remembers recent
//!   fixation targets so a slow commit channel can be attributed backwards in time.
//! * [`PxScale`] is the one thing they need from the desk: how many logical pixels a
//!   degree of visual angle covers at a point.
//!
//! The design sources are DESIGN.md sections 3 and 7; the contract is in PLAN.md.
//!
//! ```
//! use gaze_core::{Element, ElementKind, ElementSource, GazeSample, GlobalPx, Rect};
//! use gaze_snap::{ConstPxScale, FilterStack, SnapEngine};
//!
//! let mut filter = FilterStack::create()
//!     .scale(Box::new(ConstPxScale::new(60.0)))
//!     .velocity_threshold_deg_s(30.0)
//!     .window_s(0.02)
//!     .one_euro(0.3, 0.3)
//!     .build();
//!
//! let mut snap = SnapEngine::create()
//!     .scale(Box::new(ConstPxScale::new(60.0)))
//!     .radius_deg(2.0)
//!     .hysteresis_margin(0.15)
//!     .ring_window_s(1.5)
//!     .build();
//!
//! let elements = [Element {
//!     id     : 1,
//!     bbox   : Rect { x: 90.0, y: 90.0, w: 40.0, h: 20.0 },
//!     kind   : ElementKind::Button,
//!     source : ElementSource::Detector,
//!     score  : 0.9,
//!     text   : None,
//! }];
//!
//! for i in 0..20 {
//!     let sample = GazeSample {
//!         t_s       : i as f64 * 0.01,
//!         ray       : None,
//!         point     : Some(GlobalPx { x: 112.0, y: 104.0 }),
//!         sigma_deg : 0.7,
//!         valid     : true,
//!     };
//!
//!     let filtered = filter.push(sample);
//!     snap.update(&filtered, &elements);
//! }
//!
//! // A commit 200 ms of channel latency after the fact still lands on the button.
//! assert_eq!(snap.commit(0.19, 0.2).unwrap().element.id, 1);
//! ```

// The house style writes struct fields out in full, aligned, even when the value happens
// to share the field's name. Same as gaze-core.
#![allow(clippy::redundant_field_names)]

pub mod filter;
pub mod scale;
pub mod snap;

pub use filter::{FilterStack, FilterStackBuilder, FixationState, Filtered};
pub use scale::{ConstPxScale, PxScale, FALLBACK_PX_PER_DEG};
pub use snap::{Candidate, ScoreWeights, SnapEngine, SnapEngineBuilder, SnapTarget};
