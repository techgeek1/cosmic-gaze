//! One face over the three ways a run can get gaze samples and controls.
//!
//! The session loop wants two streams: gaze samples, and the discrete controls that commit,
//! exit, redetect and scroll. Where the second stream comes from depends entirely on the
//! first:
//!
//! * `synthetic` grabs the Lenovo mouse, so its buttons and wheel are already the
//!   provider's own events and nothing else can see them.
//! * `et5` and `replay` have no input device of their own, so the controls come
//!   from a [`ButtonSource`] on that same mouse when one is asked for, which grabs it as
//!   well unless told not to. The daemon asks for none: the controller commits.
//!
//! [`GazeSource`] is an enum rather than a trait object because the shapes differ in more
//! than behaviour: only the synthetic one has a ground-truth point to score against, and
//! only the ET5 one learns from clicks and has a link that can drop.

use std::path::Path;
use std::time::Instant;
use anyhow::{Context, Result};
use gaze_core::{DesktopGeometry, GazeSample, GlobalPx, NoiseModel};
use gaze_provider_synthetic::{
    Button,
    GazeProvider,
    ProviderEvent,
    ReplayProvider,
    SyntheticProvider,
};
use gaze_provider_et5::{ClickFeedback, ClickVia, Et5Calibration, Et5Provider, OffsetParams, OffsetSummary, ResidualModel};
use tracing::{info, warn};

use crate::buttons::ButtonSource;
use crate::config::SourceSpec;

/// A discrete control the session acts on, whatever produced it.
///
/// The mapping is fixed across providers: left commits, right exits, middle redetects, and
/// the wheel scrolls. Only the device the events are read from changes.
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

/// Gaze samples plus controls for one run.
pub enum GazeSource {
    /// The grabbed Lenovo mouse driven through the desk's noise model. Owns its own
    /// buttons and wheel, and knows the noise-free point behind every sample.
    Synthetic(SyntheticProvider),

    /// Real gaze from the ET5 over native USB. Controls come from the mouse when one is
    /// read; the daemon reads none and the controller commits.
    Et5 {
        /// Where the samples come from. Boxed: the provider embeds the USB session
        /// and converter state, and would otherwise dominate the enum's size.
        provider : Box<Et5Provider>,
        /// The mouse's buttons and wheel, when asked for.
        buttons  : Option<ButtonSource>,
    },

    /// A recorded session. Controls come from the same reader as `et5` when the device
    /// is available.
    Replay {
        /// Where the samples come from.
        provider : ReplayProvider,
        /// The Lenovo's buttons and wheel. `None` when no device is present, where the run
        /// is driven by the recording and `--seconds` alone.
        buttons  : Option<ButtonSource>,
    },
}

// --- GazeSource ---

impl GazeSource {
    /// Opens the source `spec` describes, logging what will commit. `buttons` names the
    /// mouse read for commits on the sources that have no controls of their own, and
    /// `grab` whether to grab it; the synthetic source grabs its own device regardless.
    pub fn open(
        spec     : &SourceSpec,
        buttons  : Option<&str>,
        grab     : bool,
        geometry : &DesktopGeometry,
    )
        -> Result<GazeSource>
    {
        match spec {
            SourceSpec::Synthetic { device, gain, seed, model } => {
                open_synthetic(device, *gain, *seed, *model, geometry)
            }

            SourceSpec::Et5 { calibration, model, device_blob, offset, flywheel } => {
                open_et5(
                    calibration.as_deref(),
                    model.as_deref(),
                    device_blob,
                    offset.as_deref(),
                    flywheel.as_deref(),
                    geometry,
                    buttons,
                    grab,
                )
            }

            SourceSpec::Replay { path } => open_replay(path, buttons, grab),
        }
    }

    /// Blocks until the next gaze sample, or `None` once the source has stopped.
    pub fn next_sample(&mut self) -> Option<GazeSample> {
        match self {
            GazeSource::Synthetic(provider)      => provider.next(),
            GazeSource::Et5 { provider, .. }     => provider.next(),
            GazeSource::Replay { provider, .. }  => provider.next(),
        }
    }

    /// Drains the controls queued since the last call, without blocking.
    pub fn controls(&mut self) -> Vec<Control> {
        match self {
            GazeSource::Synthetic(provider) => provider.events().map(control_of).collect(),

            GazeSource::Et5 { buttons, .. }
            | GazeSource::Replay { buttons, .. } => {
                match buttons {
                    Some(buttons) => buttons.events().collect(),
                    None          => Vec::new(),
                }
            }
        }
    }

    /// The noise-free point behind the current sample, when the source knows it. Only the
    /// synthetic provider does, so this is what turns commit scoring on and off.
    pub fn truth(&self) -> Option<GlobalPx> {
        match self {
            GazeSource::Synthetic(provider) => Some(provider.truth()),
            _                               => None,
        }
    }

    /// The clock real clicks must be stamped on to be attributed, when this source can
    /// learn from them: the ET5 provider with a residual model loaded. `None` for every
    /// other source, which is also the signal not to read the real mouse for labels.
    pub fn click_clock(&self) -> Option<Instant> {
        match self {
            GazeSource::Et5 { provider, .. } if provider.has_model() => Some(provider.started_at()),
            _                                                         => None,
        }
    }

    /// Hands a real click to the source's online offset. See
    /// `Et5Provider::observe_click`; every other source ignores it.
    pub fn observe_click(&mut self, px: GlobalPx, t_s: f64, via: ClickVia) -> Option<ClickFeedback> {
        match self {
            GazeSource::Et5 { provider, .. } => provider.observe_click(px, t_s, via),
            _                                => None,
        }
    }

    /// Forgets the ET5's online offset. Every other source has none.
    pub fn reset_offset(&mut self) {
        if let GazeSource::Et5 { provider, .. } = self {
            provider.reset_offset();
        }
    }

    /// Whether the samples are flowing: the ET5's link is up, or the source is one
    /// that cannot lose a link.
    pub fn connected(&self) -> bool {
        match self {
            GazeSource::Et5 { provider, .. } => provider.connected(),
            _                                => true,
        }
    }

    /// Whether a calibration was loaded. Only the ET5 reports one.
    pub fn calibrated(&self) -> bool {
        match self {
            GazeSource::Et5 { provider, .. } => provider.calibrated(),
            _                                => false,
        }
    }

    /// Whether a residual model is running.
    pub fn has_model(&self) -> bool {
        match self {
            GazeSource::Et5 { provider, .. } => provider.has_model(),
            _                                => false,
        }
    }

    /// The ET5's online offset in brief, when it has one.
    pub fn offset_summary(&self) -> Option<OffsetSummary> {
        match self {
            GazeSource::Et5 { provider, .. } => provider.offset_summary(),
            _                                => None,
        }
    }

    /// Whether this source has the control device to itself, so its wheel never reaches
    /// the compositor. The synthetic provider always grabs; the others grab their
    /// button device when asked to, so this is a property of the run, not of the variant.
    pub fn grabbed(&self) -> bool {
        match self {
            GazeSource::Synthetic(_) => true,

            // No device means no wheel to own in the first place.
            GazeSource::Et5 { buttons, .. }
            | GazeSource::Replay { buttons, .. } => {
                buttons.as_ref().is_some_and(ButtonSource::grabbed)
            }
        }
    }

    /// What this source is called in logs.
    pub fn label(&self) -> &'static str {
        match self {
            GazeSource::Synthetic(_)  => "synthetic",
            GazeSource::Et5 { .. }    => "et5",
            GazeSource::Replay { .. } => "replay",
        }
    }

    /// Releases the device and stops any reader threads. Idempotent.
    pub fn stop(&mut self) {
        match self {
            GazeSource::Synthetic(provider) => provider.stop(),

            GazeSource::Et5 { provider, buttons } => {
                provider.stop();

                if let Some(buttons) = buttons {
                    buttons.stop();
                }
            }

            GazeSource::Replay { provider, buttons } => {
                provider.stop();

                if let Some(buttons) = buttons {
                    buttons.stop();
                }
            }
        }
    }
}

// --- Opening ---

/// Grabs the mouse and drives it through the desk's noise model. Buttons and wheel come
/// back as provider events, and nothing reaches the compositor.
fn open_synthetic(
    device   : &str,
    gain     : f64,
    seed     : u64,
    model    : NoiseModel,
    geometry : &DesktopGeometry,
)
    -> Result<GazeSource>
{
    let provider = SyntheticProvider::create()
        .geometry(geometry.clone())
        .model(model)
        .device_name(device.to_string())
        .gain_px_per_count(gain)
        .seed(seed)
        .start()
        .with_context(|| format!("grabbing a gaze device matching {device:?}"))?;

    info!(
        device  = device,
        commits = "left button on the grabbed device (right exits, middle redetects)",
        wheel   = "the grabbed device's wheel; the compositor never sees it",
        "provider: synthetic"
    );

    Ok(GazeSource::Synthetic(provider))
}

/// Real gaze from the ET5 over native USB, with controls from the mouse when one is
/// read.
///
/// The provider claims the tracker's USB interface exclusively, so a sweep or a
/// `gaze-et5-cli view` cannot run at the same time. The calibration matters: without one
/// the tracker runs under an oversized virtual plane and the firmware's trained
/// end-to-end mapping never applies, so its absence is loud.
#[allow(clippy::too_many_arguments)]
fn open_et5(
    calibration : Option<&Path>,
    model       : Option<&Path>,
    device_blob : &Path,
    offset      : Option<&Path>,
    flywheel    : Option<&Path>,
    geometry    : &DesktopGeometry,
    buttons     : Option<&str>,
    grab        : bool,
)
    -> Result<GazeSource>
{
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

    let loaded_model = {
        match model {
            Some(path) => Some(ResidualModel::load(path)
                .with_context(|| format!("loading {}", path.display()))?),
            None       => None,
        }
    };

    if loaded_model.is_none() {
        info!("no residual model: the firmware's ray is used as is (gaze-et5-cli fit fits one)");
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
        .model(loaded_model)
        .offset_path(offset.map(Path::to_path_buf))
        .offset_params(offset_params)
        .flywheel_dir(flywheel.map(Path::to_path_buf))
        .device_blob(device_blob)
        .start()
        .context("starting the ET5 provider (tracker on the bus, nothing else holding it?)")?;

    let buttons = open_buttons(buttons, grab, "an ET5 run")?;

    info!(
        calibration = ?calibration.map(|p| p.display().to_string()),
        model       = ?model.map(|p| p.display().to_string()),
        offset      = ?offset.map(|p| p.display().to_string()),
        frozen      = offset.is_none(),
        flywheel    = ?flywheel.map(|p| p.display().to_string()),
        device      = ?buttons.as_ref().map(|b| b.path().display().to_string()),
        grabbed     = ?buttons.as_ref().map(ButtonSource::grabbed),
        commits     = commit_note(buttons.as_ref()),
        "provider: et5"
    );

    Ok(GazeSource::Et5 {
        provider : Box::new(provider),
        buttons  : buttons,
    })
}

/// Replays a recorded JSONL session, with controls from the mouse when one is read and
/// present.
///
/// A missing device is only a warning here: a replay is the one mode that can run with no
/// hardware at all, driven by the recording and a deadline.
fn open_replay(path: &Path, buttons: Option<&str>, grab: bool) -> Result<GazeSource> {
    let provider = ReplayProvider::from_jsonl(path)
        .with_context(|| format!("reading the replay log {}", path.display()))?;

    let buttons = match buttons.map(|name| ButtonSource::open(None, name, grab)) {
        Some(Ok(buttons)) => Some(buttons),
        None              => None,

        Some(Err(e)) => {
            warn!(
                error = %e,
                "no button device: the replay will run to its end or --seconds, with no commits"
            );

            None
        }
    };

    info!(
        replay  = %path.display(),
        device  = ?buttons.as_ref().map(|b| b.path().display().to_string()),
        grabbed = ?buttons.as_ref().map(ButtonSource::grabbed),
        commits = commit_note(buttons.as_ref()),
        "provider: replay"
    );

    Ok(GazeSource::Replay {
        provider : provider,
        buttons  : buttons,
    })
}

/// Opens the commit mouse when one is named. Failing to open a named one is an error:
/// the run asked for it, and without it a run with no controller cannot commit.
fn open_buttons(name: Option<&str>, grab: bool, what: &str) -> Result<Option<ButtonSource>> {
    let Some(name) = name else {
        return Ok(None);
    };

    let buttons = ButtonSource::open(None, name, grab)
        .with_context(|| format!("opening {name:?} for commits in {what}"))?;

    Ok(Some(buttons))
}

// --- Helpers ---

/// One line for the startup log saying what the commit button does, which differs enough
/// between a grabbed, an un-grabbed and no device to be worth spelling out at every start.
fn commit_note(buttons: Option<&ButtonSource>) -> &'static str {
    match buttons {
        Some(b) if b.grabbed() => "left button on the grabbed device (right exits, middle redetects)",
        Some(_)                => "left button on the un-grabbed device; the compositor sees the press as a click too",
        None                   => "the controller's pad; no mouse is read for commits",
    }
}

/// Translates a grabbed-device provider event into the control it drives.
fn control_of(event: ProviderEvent) -> Control {
    match event {
        ProviderEvent::ButtonPressed(Button::Left)   => Control::Commit,
        ProviderEvent::ButtonPressed(Button::Right)  => Control::Exit,
        ProviderEvent::ButtonPressed(Button::Middle) => Control::Redetect,
        ProviderEvent::Wheel(detents)                => Control::Wheel(detents),
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The grabbed device's buttons must drive the same controls the passive reader drives,
    /// or the same physical button would mean different things in different modes.
    #[test]
    fn grabbed_device_events_map_to_the_same_controls_as_the_passive_reader() {
        assert_eq!(control_of(ProviderEvent::ButtonPressed(Button::Left)),   Control::Commit);
        assert_eq!(control_of(ProviderEvent::ButtonPressed(Button::Right)),  Control::Exit);
        assert_eq!(control_of(ProviderEvent::ButtonPressed(Button::Middle)), Control::Redetect);
        assert_eq!(control_of(ProviderEvent::Wheel(-2)), Control::Wheel(-2));
    }

}
