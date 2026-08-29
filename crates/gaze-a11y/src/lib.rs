//! gaze-a11y: what the application says is under a point on the desk.
//!
//! The screen recogniser in `gaze-detect` reads pixels, and pixels cannot say that a
//! thumbnail, a title and a view count are one link, or that a grey rounded rectangle
//! is an input. The application can, through its accessibility tree, and on this desk
//! the tree is already there: an AT-SPI bus runs under the session and Firefox, Chromium
//! and GTK apps are on it. This crate asks it one question, "what is at this point?",
//! and hands back the answer with a rectangle in global logical pixels.
//!
//! # The geometry problem, and cosmic-comp
//!
//! A Wayland client does not know where its window is, so everything an accessibility
//! tree reports is in window coordinates (at-spi2-core#14). That is why no portable
//! tool can do this. cosmic-comp can: `gaze_capture::ToplevelTracker` reads every
//! window's rectangle from `zcosmic_toplevel_info_v1`, and window coordinates plus that
//! origin are desk coordinates. See DESIGN.md §2, "the compositor unlock".
//!
//! # Cost, and what this is not
//!
//! DESIGN.md is emphatic that on-demand AT-SPI tree walks are non-viable, and that
//! stands: this crate never walks a tree. `Component.GetAccessibleAtPoint` is one call,
//! answered in single-digit milliseconds by Firefox, and the handful of property reads
//! that follow it (role, name, extents, a short climb to the nearest actionable
//! ancestor) cost about a millisecond each on the local bus. That is fine for a
//! collector asking once per click, and it is not a candidate set for a snapper. The
//! event-driven mirror the design calls for is separate work.
//!
//! # Coverage
//!
//! Per application, and honest about it: Firefox answers; Chromium and Electron only
//! with their accessibility switched on; COSMIC's own iced applications not yet;
//! terminals never. [`A11y::at`] returns `Ok(None)` for all of those, and the caller
//! falls back to the pixels. That is the division of labour: the tree where there is
//! one, the recogniser where there is not.

// The workspace style mandates explicit `Foo { x: x }` field syntax everywhere, which
// clippy reads as redundant. Same allow as `gaze-core`.
#![allow(clippy::redundant_field_names)]

mod bus;

pub use bus::{A11y, A11yError, Application, CoordMode, Hit, Node, is_actionable};
