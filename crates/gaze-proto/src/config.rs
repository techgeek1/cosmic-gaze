//! What a session is built from, and how the tuning maps onto the parts that use it.
//!
//! [`SessionConfig`] is everything decided before the loop starts: the desk, the
//! tracker's files, whether the controller is read, and what may reach the real desktop. The
//! daemon builds one from [`gaze_config::Paths`]; the prototype builds one from its
//! flags. [`gaze_config::Tuning`] is everything that may change while the loop runs, and
//! the helpers at the bottom turn it into the filter stack, the snap engine, the pointer
//! style, the scroller's parameters and the controller's mapping, so the loop can
//! rebuild those in one place when a knob moves.

use std::path::PathBuf;

use gaze_config::Tuning;
use gaze_core::DesktopGeometry;
use gaze_overlay::PointerStyle;
use gaze_snap::{FilterStack, SnapEngine};

use crate::daydream::DaydreamConfig;
use crate::edge_scroll::EdgeParams;

/// Everything decided before the loop starts.
#[derive(Clone, Debug)]
pub struct SessionConfig {
    /// The desk: outputs, eye, tracker.
    pub geometry   : DesktopGeometry,
    /// Directory holding the ONNX models.
    pub models_dir : PathBuf,
    /// The tracker's files.
    pub source     : SourceSpec,
    /// Whether and which Daydream controller to read.
    pub daydream   : DaydreamSpec,
    /// Click for real. Off, commits are logged and nothing is injected at all: no
    /// warps, no scrolls, no clicks.
    pub click      : bool,
    /// Which look the overlay draws, and when.
    pub overlay    : OverlayMode,
    /// Exit after this many seconds.
    pub seconds    : Option<f64>,
}

/// What the ET5 provider needs to start.
#[derive(Clone, Debug)]
pub struct SourceSpec {
    /// Calibration file, when one exists.
    pub calibration : Option<PathBuf>,
    /// The on-device calibration blob re-declared on every connect.
    pub device_blob : PathBuf,
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
