//! A transparent, click-through debug overlay for the COSMIC desktop.
//!
//! The overlay puts one `zwlr_layer_shell_v1` surface on the overlay layer of every
//! output, anchored to all four edges with an exclusive zone of -1, and draws a gaze
//! marker, a highlighted candidate box and a ground-truth marker on top of whatever is
//! already on screen. Every surface has an empty input region, so pointer events pass
//! straight through and the desktop underneath stays fully usable while the overlay is
//! up.
//!
//! Everything the caller passes in is in global logical pixels, the same space
//! `gaze_core::Rect` and the snap engine use. The mapping onto individual outputs is
//! handled here, including outputs at a scale other than 1 and boxes that straddle the
//! seam between two panels.
//!
//! # Using it from another thread
//!
//! The Wayland connection is not `Send`, so the usual pattern is to give the overlay its
//! own thread and talk to it over a channel:
//!
//! ```no_run
//! use gaze_core::GlobalPx;
//! use gaze_overlay::{Overlay, OverlayState};
//!
//! let (handle, join) = Overlay::spawn()?;
//!
//! handle.set(OverlayState {
//!     gaze : Some(GlobalPx { x: 3000.0, y: 800.0 }),
//!     ..OverlayState::default()
//! })?;
//!
//! handle.stop();
//! let _ = join.join();
//! # Ok::<(), gaze_overlay::OverlayError>(())
//! ```
//!
//! Driving it from the current thread with [`Overlay::connect`], [`Overlay::set`] and
//! [`Overlay::run_until`] also works, and is what the CLI does.
//!
//! # Rendering
//!
//! Frames are `wl_shm` `Argb8888` buffers drawn with tiny-skia, two per surface so a
//! frame is never modified while the compositor is reading it. Only the union of the
//! previous and the new marker positions is cleared, redrawn and damaged, so a moving
//! ring costs a few thousand pixels rather than the whole ultrawide surface.
//!
//! There is no text shaping: labels use a built in 5x7 bitmap font covering printable
//! ASCII, which is enough for the debug captions the phase 0 harness prints.

// The workspace style requires explicit `Foo { x: x }` field initialisation so the
// column alignment survives; clippy would rather have the shorthand.
#![allow(clippy::redundant_field_names)]

mod draw;
mod error;
mod font;
mod mapping;
mod state;
mod wayland;

pub use draw::render;
pub use error::OverlayError;
pub use mapping::OutputMapping;
pub use state::OverlayState;
pub use tiny_skia::Pixmap;
pub use wayland::{Overlay, OverlayHandle};
