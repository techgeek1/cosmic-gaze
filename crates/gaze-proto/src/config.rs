//! What a session is built from, and how the tuning maps onto the parts that use it.
//!
//! [`SessionConfig`] is everything decided before the loop starts: the desk, where the
//! samples come from, which devices are read, and what may reach the real desktop. The
//! daemon builds one from [`gaze_config::Paths`]; the prototype builds one from its
//! flags. [`gaze_config::Tuning`] is everything that may change while the loop runs, and
//! the helpers at the bottom turn it into the filter stack, the snap engine, the pointer
//! style, the scroller's parameters and the controller's mapping, so the loop can
//! rebuild those in one place when a knob moves.

use std::path::PathBuf;

use gaze_config::Tuning;
use gaze_core::{DesktopGeometry, NoiseModel};
use gaze_overlay::PointerStyle;
use gaze_snap::{FilterStack, SnapEngine};

use crate::daydream::DaydreamConfig;
use crate::edge_scroll::EdgeParams;

/// Everything decided before the loop starts.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// The desk: outputs, eye, tracker, noise.
    pub geometry   : DesktopGeometry,
    /// Directory holding the ONNX models.
    pub models_dir : PathBuf,
    /// Where the gaze samples come from.
    pub source     : SourceSpec,
    /// Name substring of a mouse read for commit, exit and redetect on providers that
    /// have no controls of their own. `None` reads no such device, which is the daemon:
    /// the controller commits, and the mouse is the user's.
    pub buttons    : Option<String>,
    /// Grab that mouse, so its presses reach nothing else.
    pub grab       : bool,
    /// Whether and which Daydream controller to read.
    pub daydream   : DaydreamSpec,
    /// Click for real. Off, commits are logged and nothing is injected at all: no
    /// warps, no scrolls, no clicks.
    pub click      : bool,
    /// Which look the overlay draws, and when.
    pub overlay    : OverlayMode,
    /// Draw the provider's noise-free point, when it has one.
    pub show_truth : bool,
    /// Append every sample to this file for replay.
    pub record     : Option<PathBuf>,
    /// Exit after this many seconds.
    pub seconds    : Option<f64>,
}

/// Where the gaze samples come from, with what each source needs to start.
#[derive(Clone, Debug)]
pub enum SourceSpec {
    /// A grabbed mouse driven through the desk's noise model.
    Synthetic {
        /// Name substring of the mouse to grab.
        device : String,
        /// Logical pixels of gaze motion per raw mouse count.
        gain   : f64,
        /// Seeds the noise RNG.
        seed   : u64,
        /// The noise to add.
        model  : NoiseModel,
    },

    /// The webcam sidecar over its socket.
    Webcam {
        /// The sidecar's socket.
        socket      : PathBuf,
        /// Overrides the desk file's camera node.
        camera      : Option<PathBuf>,
        /// Calibration file, when one exists.
        calibration : Option<PathBuf>,
        /// The flat sigma to run under.
        sigma_deg   : f64,
        /// The desk file's text, which holds the camera pose.
        desk_text   : String,
    },

    /// The ET5 over native USB.
    Et5 {
        /// Calibration file, when one exists.
        calibration : Option<PathBuf>,
        /// Residual model, when one exists.
        model       : Option<PathBuf>,
        /// The on-device calibration blob re-declared on every connect.
        device_blob : PathBuf,
        /// Where the online offset persists; `None` freezes it in memory.
        offset      : Option<PathBuf>,
        /// Where the flywheel writes attributed clicks; `None` writes none.
        flywheel    : Option<PathBuf>,
    },

    /// A recorded session.
    Replay {
        /// The JSONL recording.
        path : PathBuf,
    },
}

/// Whether and which Daydream controller to read.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub enum DaydreamSpec {
    /// The first paired controller, if any is paired; retried while it is asleep.
    #[default]
    Auto,
    /// No controller, even if one is paired.
    Off,
    /// The controller at this Bluetooth address.
    Address(String),
}

/// Which look the overlay draws, and when.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum OverlayMode {
    /// The pointer look, armed by a thumb on the pad or the F14 latch.
    #[default]
    Pointer,
    /// The pointer look for the whole run.
    Always,
    /// The debug look: gaze ring, raw candidate box, state caption.
    Debug,
}

// --- Tuning to parts ---

/// The filter stack the tuning describes, scaled to the desk.
pub fn filter_for(geometry: &DesktopGeometry, tuning: &Tuning) -> FilterStack {
    FilterStack::create()
        .scale(Box::new(geometry.clone()))
        .velocity_threshold_deg_s(tuning.filter_velocity_deg_s)
        .window_s(tuning.filter_window_s)
        .one_euro(tuning.filter_min_cutoff_hz, tuning.filter_beta)
        .build()
}

/// The snap engine the tuning describes, scaled to the desk.
pub fn engine_for(geometry: &DesktopGeometry, tuning: &Tuning) -> SnapEngine {
    SnapEngine::create()
        .scale(Box::new(geometry.clone()))
        .radius_deg(tuning.snap_deg)
        .build()
}

/// The pointer look's timing.
pub fn pointer_style(tuning: &Tuning) -> PointerStyle {
    PointerStyle {
        settle_s : tuning.pointer_settle_s,
        linger_s : tuning.pointer_linger_s,
    }
}

/// The edge scroller's parameters.
pub fn edge_params(tuning: &Tuning) -> EdgeParams {
    EdgeParams {
        band_fraction     : tuning.edge_band,
        top_band_fraction : tuning.edge_top_band,
        dwell_s           : tuning.edge_dwell_s,
        top_dwell_s       : tuning.edge_top_dwell_s,
        max_lines_s       : tuning.edge_max_lines_s,
        ramp_s            : tuning.edge_ramp_s,
        exponent          : tuning.edge_exponent,
        hold_s            : tuning.edge_hold_s,
        hold_gain         : tuning.edge_hold_gain,
        turbo             : tuning.edge_turbo,
    }
}

/// The controller's mapping.
pub fn daydream_config(tuning: &Tuning) -> DaydreamConfig {
    DaydreamConfig { touch_gain_px: tuning.refine_touch_gain_px }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Each knob lands on the field it names, so a slider in the applet moves the thing
    /// its label says. Spot checks across the four mappings; the defaults are the
    /// prototype's, so `Default` on both sides must agree too.
    #[test]
    fn knobs_land_on_their_fields() {
        let mut tuning = Tuning::default();

        assert_eq!(edge_params(&tuning), EdgeParams::default());

        tuning.set("edge_top_dwell_s", 0.9);
        tuning.set("edge_turbo", 5.0);
        tuning.set("pointer_linger_s", 0.7);
        tuning.set("refine_touch_gain_px", 400.0);

        let edge = edge_params(&tuning);

        assert_eq!(edge.top_dwell_s, 0.9);
        assert_eq!(edge.turbo, 5.0);
        assert_eq!(edge.dwell_s, EdgeParams::default().dwell_s);
        assert_eq!(pointer_style(&tuning).linger_s, 0.7);
        assert_eq!(pointer_style(&tuning).settle_s, PointerStyle::default().settle_s);
        assert_eq!(daydream_config(&tuning).touch_gain_px, 400.0);
    }
}
