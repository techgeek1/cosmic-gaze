//! The wire between `gaze-trainer` and `gaze-clicks`.
//!
//! The trainer is a real application with real widgets, and it knows exactly which
//! widget received a press and where that widget's box is. It tells the collector over
//! a Unix socket, one JSON line per press, and the collector uses that answer instead
//! of the accessibility tree or the recogniser for any press it can match by time.
//!
//! Coordinates on the wire are *window-local* logical pixels: the trainer does not know
//! where the compositor put its window. The collector knows the pointer's global
//! position at the press from the compositor, and the trainer reports the pointer's
//! window-local position at the same press, so the difference is the window's origin
//! and the box translates with it. That cancels whatever coordinate space the toolkit
//! uses (client-side shadow included) without either side asking the compositor.
//!
//! These types live here rather than in either crate because both need them and the
//! trainer must not depend on the collector's model stack.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::types::Rect;

/// Socket file name, under `$XDG_RUNTIME_DIR`.
pub const SOCKET_FILE: &str = "gaze-clicks.sock";

/// `source` value the collector writes for a click the trainer labelled.
pub const TRAINER_SOURCE: &str = "trainer";

/// Where the collector listens and the trainer connects.
///
/// `$XDG_RUNTIME_DIR/gaze-clicks.sock`, falling back to `/tmp` when the variable is
/// unset, which under a real session it never is.
pub fn socket_path() -> PathBuf {
    let dir = std::env::var_os("XDG_RUNTIME_DIR")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("/tmp"));

    dir.join(SOCKET_FILE)
}

/// The widget under a press, as the trainer knows it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrainerElement {
    /// `gaze_core::ElementKind` in lowercase, the same vocabulary the recogniser and
    /// the tree use: `button`, `link`, `checkbox`, `input`, `icon`, `text`.
    pub kind : String,
    /// The widget's box, window-local logical pixels.
    pub bbox : Rect,
    /// The widget's label, when it has one.
    pub text : Option<String>,
}

/// What the trainer was showing when the press happened.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TrainerTag {
    /// Index of the generated window within the trainer session, from one. Every
    /// window is a fresh layout, so this is the unit to hold out whole.
    pub task    : u64,
    /// Reserved, always 0. It carried the step index when the trainer ran tasks.
    pub step    : u32,
    /// Whether a labelled control was under the press. A press on padding or the
    /// header bar is still recorded, and this is what lets the export tell the two
    /// apart.
    pub hit     : bool,
    /// The posture the trainer last asked for: `normal`, `back`, `in`, `left`,
    /// `right`, `tall`, `slouch`.
    pub posture : String,
    /// `dark` or `light`.
    pub theme   : String,
}

/// One line on the socket, trainer to collector.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TrainerMessage {
    /// First line of a connection.
    Hello {
        /// The trainer's name and version, for the collector's log.
        app     : String,
        /// Unix time the trainer started, seconds.
        started : f64,
    },
    /// A mouse press inside the trainer's window.
    Press {
        /// Unix time the trainer saw the press, seconds. The collector matches it to
        /// the evdev press it saw itself.
        t_unix_s : f64,
        /// `left` or `right`.
        button   : String,
        /// Where the pointer was at the press, window-local logical pixels.
        px       : [f64; 2],
        /// The widget under the pointer, or `None` when the press landed on nothing
        /// the trainer labels: padding, the header bar, the empty body.
        element  : Option<TrainerElement>,
        /// Mean luminance of the theme's background, [0, 1]: the pupil covariate.
        luma     : f64,
        tag      : TrainerTag,
    },
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_press_round_trips_through_json_with_its_kind_tag() {
        let press = TrainerMessage::Press {
            t_unix_s : 1788000000.25,
            button   : "left".into(),
            px       : [812.0, 431.5],
            element  : Some(TrainerElement {
                kind : "button".into(),
                bbox : Rect { x: 800.0, y: 420.0, w: 96.0, h: 32.0 },
                text : Some("Save".into()),
            }),
            luma     : 0.12,
            tag      : TrainerTag {
                task    : 7,
                step    : 1,
                hit     : true,
                posture : "left".into(),
                theme   : "dark".into(),
            },
        };

        let line = serde_json::to_string(&press).expect("serialises");

        assert!(line.contains("\"kind\":\"press\""));

        let back: TrainerMessage = serde_json::from_str(&line).expect("parses");

        assert_eq!(back, press);
    }

    #[test]
    fn the_socket_lives_in_the_runtime_dir() {
        let path = socket_path();

        assert!(path.ends_with(SOCKET_FILE));
    }
}
