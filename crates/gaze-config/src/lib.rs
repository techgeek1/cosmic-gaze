//! What the daemon, the applet and the prototype agree on: the tuning knobs and where
//! they are stored, where the desk's files live, and the daemon's control interface.
//!
//! Nothing in here touches a tracker, a compositor or a model. The applet links this
//! crate and nothing else from the workspace, so it must stay that light: a struct of
//! numbers, a table describing them, three directories, and a D-Bus proxy.
//!
//! * [`Tuning`] is every number the feel still depends on, one cosmic-config key per
//!   field under [`CONFIG_ID`]; [`KNOBS`] describes the numeric ones so a UI can draw
//!   them without naming any. [`TuningStore`] reads the config and watches it.
//! * [`Paths`] is where the desk file, the calibration, the ONNX models and the offset
//!   live, XDG by default and a checkout's layout on request.
//! * [`bus`] is the daemon's D-Bus name, path, proxy and the [`Status`] its properties
//!   describe.

// The workspace style writes struct fields out in full, aligned, even when the value
// happens to share the field's name.
#![allow(clippy::redundant_field_names)]

pub mod bus;
pub mod paths;
pub mod tuning;

pub use bus::{BUS_NAME, BUS_PATH, GazeProxy, GazeProxyBlocking, Mode, Status};
pub use paths::Paths;
pub use tuning::{CONFIG_ID, CONFIG_VERSION, KNOBS, Knob, Tuning, TuningStore};
