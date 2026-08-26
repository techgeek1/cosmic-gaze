//! gaze-capture: per-output screenshots of a running COSMIC session.
//!
//! cosmic-comp exposes no `wlr-screencopy`, so capture goes through the staging
//! `ext_image_copy_capture_v1` protocol with an `ext_output_image_capture_source_manager_v1`
//! source and a `wl_shm` buffer. Output geometry comes from `zxdg_output_manager_v1`,
//! which is the only place logical position and size are reported.
//!
//! ```no_run
//! let mut cap = gaze_capture::Capture::connect()?;
//!
//! for info in cap.outputs() {
//!     println!("{} {:?} scale {}", info.name, info.logical, info.scale);
//! }
//!
//! let frame = cap.capture_output("DP-1")?;
//! assert_eq!(frame.rgba.len(), frame.width as usize * frame.height as usize * 4);
//! # Ok::<(), gaze_capture::CaptureError>(())
//! ```
//!
//! [`CursorTracker`] is the other half: it reads the pointer's position out of
//! `ext_image_copy_capture_cursor_session_v1` in global logical pixels, which is the
//! feedback signal relative uinput injection needs.
//!
//! ```no_run
//! let mut cursor = gaze_capture::CursorTracker::connect()?;
//!
//! if let Some(p) = cursor.position()? {
//!     println!("pointer at {:.0},{:.0}", p.x, p.y);
//! }
//! # Ok::<(), gaze_capture::CaptureError>(())
//! ```

mod capture;
mod cursor;
mod frame;
mod outputs;
mod shm;

pub use capture::{Capture, CaptureError};
pub use cursor::{CursorReport, CursorTracker};
pub use frame::{Frame, OutputInfo, changed_fraction};
