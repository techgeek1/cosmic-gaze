//! The session's gaze source: the ET5, and the controls the session acts on.
//!
//! The session loop wants two streams: gaze samples, and the discrete controls that
//! commit, exit, redetect and scroll. The samples come from the ET5 provider; the
//! controls come from the Daydream controller and the overlay latch key, which the
//! session reads itself. Until 2026-09-10 this was an enum over the ET5, a mouse-driven
//! synthetic provider and a replay of a recording, each with its own control device;
//! the ET5 is the only path left, so this is a thin face over its provider that the
//! session and the daemon's status read through.

use std::path::Path;
use std::time::Instant;
use anyhow::{Context, Result};
use gaze_core::{DesktopGeometry, GazeProvider, GazeSample, GlobalPx};
use gaze_provider_et5::{ClickFeedback, Et5Calibration, Et5Provider, OffsetParams, OffsetSummary};
use tracing::{info, warn};

use crate::config::SourceSpec;

/// A discrete control the session acts on, whatever produced it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Control {
    /// Commit the currently snapped target.
    Commit,
    /// Commit with the secondary button: a context menu where the primary would click.
    Context,
    /// End the session.
    Exit,
    /// Force a detection pass on every output.
    Redetect,
    /// The wheel turned by this many whole detents, positive up.
    Wheel(i32),
    /// The voice stack's push-to-talk key went down (`true`) or came back up. Forwarded
    /// as F13 through the injector, nothing more: the voice stack owns what happens
    /// while it is held.
    PushToTalk {
        /// Down when true, up when false.
        down : bool,
    },
    /// The fine channel moved: the controller's thumb or wrist adjusting where the next
    /// commit lands, relative to where gaze put it. See [`Refine`].
    Refine(Refine),
    /// The thumb landed on the controller's pad (`true`) or lifted off it. Arms the
    /// pointer look for as long as it is down; a refine begins only once the thumb has
    /// travelled, so a resting thumb shows where the eyes are without taking over.
    Arm {
        /// Down when true, up when false.
        down : bool,
    },
    /// The overlay latch key (F14) was pressed: toggle whether the pointer look is shown
    /// with no thumb on the pad.
    ToggleOverlay,
}

/// The fine channel's gesture, as the session sees it.
///
/// A refine starts when the thumb lands on the controller's pad, moves the commit point
/// by pixel deltas while it is down, and ends when it lifts. Where the gesture starts from
/// is the session's business (the snap point if there is one, the gaze point otherwise),
/// which is why the deltas are relative.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Refine {
    /// The thumb landed: capture the anchor and put the pointer on it.
    Begin,
    /// Move the commit point by this much, in logical pixels.
    Move {
        /// Rightwards.
        dx_px : f64,
        /// Downwards.
        dy_px : f64,
    },
    /// The thumb lifted. The refined point stands until the next commit or retarget.
    End,
}

/// Gaze samples for one run: the ET5 over native USB.
pub struct GazeSource {
    /// Where the samples come from. Boxed: the provider embeds the USB session and
    /// converter state.
    provider : Box<Et5Provider>,
}

// --- GazeSource ---

impl GazeSource {
    /// Opens the ET5 `spec` describes.
    ///
    /// The provider claims the tracker's USB interface exclusively, so a ceremony or a
    /// `gaze-et5-cli view` cannot run at the same time. The calibration matters: without
    /// one the tracker runs under an oversized virtual plane and the firmware's trained
    /// end-to-end mapping never applies, so its absence is loud.
    pub fn open(spec: &SourceSpec, geometry: &DesktopGeometry) -> Result<GazeSource> {
        let calibration = spec.calibration.as_deref();
        let offset      = spec.offset.as_deref();

        let loaded_calibration = {
            match calibration {
                Some(path) => Some(Et5Calibration::load(path)
                    .with_context(|| format!("loading {}", path.display()))?),
                None       => None,
            }
        };

        if loaded_calibration.is_none() {
            warn!("running uncalibrated: the trained on-device mapping will not apply \
                   (gaze-et5-cli calibrate fixes it)");
        }

        // A frozen offset is a gain of zero with nowhere to write: clicks are still
        // attributed and logged, so a run can watch the leftovers without moving anything.
        let offset_params = match offset {
            Some(_) => OffsetParams::default(),
            None    => OffsetParams { alpha: 0.0, ..OffsetParams::default() },
        };

        let provider = Et5Provider::create()
            .geometry(geometry.clone())
            .calibration(loaded_calibration)
            .offset_path(offset.map(Path::to_path_buf))
            .offset_params(offset_params)
            .device_blob(&spec.device_blob)
            .start()
            .context("starting the ET5 provider (tracker on the bus, nothing else holding it?)")?;

        info!(
            calibration = ?calibration.map(|p| p.display().to_string()),
            offset      = ?offset.map(|p| p.display().to_string()),
            frozen      = offset.is_none(),
            commits     = "the controller's pad; no mouse is read for commits",
            "provider: et5"
        );

        Ok(GazeSource { provider: Box::new(provider) })
    }

    /// Blocks until the next gaze sample, or `None` once the source has stopped.
    pub fn next_sample(&mut self) -> Option<GazeSample> {
        self.provider.next()
    }

    /// The clock real clicks must be stamped on to be attributed, when this source can
    /// learn from them: the provider running its online offset. `None` is also the
    /// signal not to read the real mouse for labels.
    pub fn click_clock(&self) -> Option<Instant> {
        self.provider.offset_summary().map(|_| self.provider.started_at())
    }

    /// Hands a real click to the online offset. See `Et5Provider::observe_click`.
    pub fn observe_click(&mut self, px: GlobalPx, t_s: f64) -> Option<ClickFeedback> {
        self.provider.observe_click(px, t_s)
    }

    /// Forgets the online offset.
    pub fn reset_offset(&mut self) {
        self.provider.reset_offset();
    }

    /// Whether the samples are flowing: the ET5's link is up.
    pub fn connected(&self) -> bool {
        self.provider.connected()
    }

    /// Whether a calibration was loaded.
    pub fn calibrated(&self) -> bool {
        self.provider.calibrated()
    }

    /// The online offset in brief, when it is running.
    pub fn offset_summary(&self) -> Option<OffsetSummary> {
        self.provider.offset_summary()
    }

    /// What this source is called in logs.
    pub fn label(&self) -> &'static str {
        "et5"
    }

    /// Releases the device and stops its reader thread. Idempotent.
    pub fn stop(&mut self) {
        self.provider.stop();
    }
}
