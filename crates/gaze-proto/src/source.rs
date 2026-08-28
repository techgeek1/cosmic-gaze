//! One face over the three ways a run can get gaze samples and controls.
//!
//! The session loop wants two streams: gaze samples, and the discrete controls that commit,
//! exit, redetect and scroll. Where the second stream comes from depends entirely on the
//! first:
//!
//! * `synthetic` grabs the Lenovo mouse, so its buttons and wheel are already the
//!   provider's own events and nothing else can see them.
//! * `webcam` and `replay` have no input device of their own, so the controls come from a
//!   [`ButtonSource`] on that same mouse, which grabs it as well unless `--no-grab` says
//!   otherwise.
//!
//! [`GazeSource`] is an enum rather than a trait object because the three shapes differ in
//! more than behaviour: only the synthetic one has a ground-truth point to score against,
//! only the webcam one has a socket whose health is worth reporting, and whether the wheel
//! is owned exclusively (which decides whether the scroll tier re-injects it or only warps
//! ahead of it) varies per run rather than per variant.

use std::path::PathBuf;
use anyhow::{Context, Result};
use gaze_core::{DesktopGeometry, GazeSample, GlobalPx, NoiseModel};
use gaze_provider_synthetic::{
    Button,
    GazeProvider,
    ProviderEvent,
    ReplayProvider,
    SyntheticProvider,
};
use gaze_provider_et5::{Et5Calibration, Et5Provider};
use gaze_provider_webcam::{CameraPose, DEFAULT_SIGMA_DEG, SampleMeta, WebcamProvider};
use tracing::{info, warn};

use crate::buttons::ButtonSource;
use crate::cli::{Args, Provider};

/// Conventional calibration file, used when `--calibration` is not given.
const DEFAULT_CALIBRATION_PATH : &str = "config/calibration.toml";

/// Conventional ET5 calibration file, used when `--calibration` is not given.
const DEFAULT_ET5_CALIBRATION_PATH : &str = "config/calibration-et5.toml";

/// A discrete control the session acts on, whatever produced it.
///
/// The mapping is fixed across providers: left commits, right exits, middle redetects, and
/// the wheel scrolls. Only the device the events are read from changes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Control {
    /// Commit the currently snapped target.
    Commit,
    /// End the session.
    Exit,
    /// Force a detection pass on every output.
    Redetect,
    /// The wheel turned by this many whole detents, positive up.
    Wheel(i32),
}

/// Running health of a webcam session, for the exit summary.
///
/// The provider reports the sidecar's confidence and latency for the *latest* frame only,
/// which says nothing about whether the session as a whole was tracking well. This
/// accumulates them into means. Frames are deduplicated by `seq`, because the session polls
/// once per gaze sample and a stalled sidecar would otherwise have its last good frame
/// counted hundreds of times.
#[derive(Clone, Copy, Debug, Default)]
pub struct WebcamHealth {
    /// Distinct sidecar frames observed.
    pub frames : u64,
    /// Sum of per-frame confidence, for the mean.
    conf_sum   : f64,
    /// Sum of per-frame capture-to-socket latency, milliseconds, for the mean.
    lat_ms_sum : f64,
    /// `seq` of the last frame counted, so a repeat is not counted twice.
    last_seq   : Option<u64>,
}

// --- WebcamHealth ---

impl WebcamHealth {
    /// Folds in the provider's latest frame metadata, ignoring a frame already counted.
    pub fn observe(&mut self, meta: SampleMeta) {
        if self.last_seq == Some(meta.seq) {
            return;
        }

        self.last_seq    = Some(meta.seq);
        self.frames     += 1;
        self.conf_sum   += meta.conf;
        self.lat_ms_sum += meta.lat_ms;
    }

    /// Mean model confidence over the session, or `0.0` before the first frame.
    pub fn conf_mean(&self) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }

        self.conf_sum / self.frames as f64
    }

    /// Mean sidecar capture-to-socket latency in milliseconds, or `0.0` before the first
    /// frame.
    pub fn lat_ms_mean(&self) -> f64 {
        if self.frames == 0 {
            return 0.0;
        }

        self.lat_ms_sum / self.frames as f64
    }
}

/// Gaze samples plus controls for one run.
pub enum GazeSource {
    /// The grabbed Lenovo mouse driven through the desk's noise model. Owns its own
    /// buttons and wheel, and knows the noise-free point behind every sample.
    Synthetic(SyntheticProvider),

    /// Real gaze from the sidecar. Controls come from the Lenovo, which is the only commit
    /// channel a webcam run has.
    Webcam {
        /// Where the samples come from.
        provider : WebcamProvider,
        /// The Lenovo's buttons and wheel. Required: gaze cannot commit itself and there
        /// is no keyboard grab, so nothing else here can.
        buttons  : ButtonSource,
        /// Accumulated sidecar health, updated once per sample by
        /// [`poll_health`](GazeSource::poll_health).
        health   : WebcamHealth,
    },

    /// Real gaze from the ET5 over native USB. Controls come from the Lenovo, as in
    /// `webcam`: gaze cannot commit itself.
    Et5 {
        /// Where the samples come from. Boxed: the provider embeds the USB session
        /// and converter state, and would otherwise dominate the enum's size.
        provider : Box<Et5Provider>,
        /// The Lenovo's buttons and wheel. Required, as in `webcam`.
        buttons  : ButtonSource,
    },

    /// A recorded session. Controls come from the same reader as `webcam` when the device
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
    /// Opens whichever source `args.provider` selected, logging what will commit.
    pub fn open(args: &Args, geometry: &DesktopGeometry, model: NoiseModel)
        -> Result<GazeSource>
    {
        match args.provider {
            Provider::Synthetic => open_synthetic(args, geometry, model),
            Provider::Webcam    => open_webcam(args, geometry),
            Provider::Et5       => open_et5(args, geometry),
            Provider::Replay    => open_replay(args),
        }
    }

    /// Blocks until the next gaze sample, or `None` once the source has stopped.
    pub fn next_sample(&mut self) -> Option<GazeSample> {
        match self {
            GazeSource::Synthetic(provider)      => provider.next(),
            GazeSource::Webcam { provider, .. }  => provider.next(),
            GazeSource::Et5 { provider, .. }     => provider.next(),
            GazeSource::Replay { provider, .. }  => provider.next(),
        }
    }

    /// Drains the controls queued since the last call, without blocking.
    pub fn controls(&mut self) -> Vec<Control> {
        match self {
            GazeSource::Synthetic(provider)     => provider.events().map(control_of).collect(),
            GazeSource::Webcam { buttons, .. }  => buttons.events().collect(),
            GazeSource::Et5 { buttons, .. }     => buttons.events().collect(),

            GazeSource::Replay { buttons, .. } => {
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

    /// Whether this source has the control device to itself.
    ///
    /// It decides what the scroll tier does with a wheel event: on a grabbed device the
    /// compositor never sees the wheel, so the session has to re-inject it; on an
    /// un-grabbed one the compositor is already delivering it and injecting again would
    /// scroll twice, so the session only warps the pointer ahead of it.
    ///
    /// The synthetic provider always grabs. The other two grab their button device unless
    /// `--no-grab`, so this is a property of the run, not of the variant.
    pub fn grabbed(&self) -> bool {
        match self {
            GazeSource::Synthetic(_)           => true,
            GazeSource::Webcam { buttons, .. } => buttons.grabbed(),
            GazeSource::Et5 { buttons, .. }    => buttons.grabbed(),

            // No device means no wheel to own in the first place.
            GazeSource::Replay { buttons, .. } => {
                buttons.as_ref().is_some_and(ButtonSource::grabbed)
            }
        }
    }

    /// What this source is called in logs.
    pub fn label(&self) -> &'static str {
        match self {
            GazeSource::Synthetic(_)     => "synthetic",
            GazeSource::Webcam { .. }    => "webcam",
            GazeSource::Et5 { .. }       => "et5",
            GazeSource::Replay { .. }    => "replay",
        }
    }

    /// Folds the sidecar's latest frame metadata into the running health figures.
    ///
    /// Called once per gaze sample, and a no-op for every provider but the webcam one.
    /// Polling rather than reading a counter inside the provider keeps the means honest
    /// about what this session actually saw, rather than about what arrived on the socket.
    pub fn poll_health(&mut self) {
        if let GazeSource::Webcam { provider, health, .. } = self
            && let Some(meta) = provider.last_meta()
        {
            health.observe(meta);
        }
    }

    /// Logs the webcam session's socket health. Does nothing for the other providers,
    /// which have no socket to be healthy.
    pub fn log_health(&self) {
        let GazeSource::Webcam { provider, health, .. } = self else {
            return;
        };

        let stats = provider.stats();

        info!(
            connected    = stats.connected,
            lines        = stats.lines,
            connects     = stats.connects,
            bad_lines    = stats.bad_lines,
            frames       = health.frames,
            conf_mean    = health.conf_mean(),
            lat_ms_mean  = health.lat_ms_mean(),
            last_output  = ?provider.last_output(),
            "sidecar health"
        );
    }

    /// Releases the device and stops any reader threads. Idempotent.
    pub fn stop(&mut self) {
        match self {
            GazeSource::Synthetic(provider) => provider.stop(),

            GazeSource::Webcam { provider, buttons, .. } => {
                provider.stop();
                buttons.stop();
            }

            GazeSource::Et5 { provider, buttons } => {
                provider.stop();
                buttons.stop();
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

/// Grabs the Lenovo and drives it through the desk's noise model. Buttons and wheel come
/// back as provider events, and nothing reaches the compositor.
fn open_synthetic(args: &Args, geometry: &DesktopGeometry, model: NoiseModel)
    -> Result<GazeSource>
{
    let provider = SyntheticProvider::create()
        .geometry(geometry.clone())
        .model(model)
        .device_name(args.device.clone())
        .gain_px_per_count(args.gain)
        .seed(args.seed)
        .start()
        .with_context(|| format!("grabbing a gaze device matching {:?}", args.device))?;

    info!(
        device  = args.device,
        commits = "left button on the grabbed device (right exits, middle redetects)",
        wheel   = "the grabbed device's wheel; the compositor never sees it",
        "provider: synthetic"
    );

    Ok(GazeSource::Synthetic(provider))
}

/// Real gaze from the webcam sidecar, with controls from the un-grabbed Lenovo.
///
/// The camera pose comes out of the same `desk.toml` the session already parsed, because it
/// describes the desk rather than the run; only the device node is worth overriding per
/// run, and even that is only read by the sidecar. A missing socket is not an error: the
/// provider reconnects with backoff, so starting this before the sidecar is up is fine and
/// the samples begin when it appears.
///
/// The button source is required here, unlike in replay: gaze cannot commit itself and
/// there is no keyboard grab, so those three buttons are the only commit channel a webcam
/// run has. It is grabbed by default, which keeps a commit press from also clicking
/// whatever the pointer is over.
fn open_webcam(args: &Args, geometry: &DesktopGeometry) -> Result<GazeSource> {
    let text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("reading {}", args.config.display()))?;

    let mut camera = CameraPose::from_desk_toml(&text)
        .with_context(|| format!("parsing the [camera] block of {}", args.config.display()))?;

    if let Some(device) = &args.camera {
        camera.device = device.display().to_string();
    }

    // `--sigma` is the desk-wide override, and the webcam profile is flat anyway, so the
    // same flag serves both providers.
    let sigma_deg = args.sigma.unwrap_or(DEFAULT_SIGMA_DEG);

    // An explicit --calibration wins; otherwise the conventional file is used when it
    // exists, so a freshly swept calibration is never silently ignored.
    let default_cal = PathBuf::from(DEFAULT_CALIBRATION_PATH);
    let calibration = args.calibration.clone().or_else(|| default_cal.exists().then_some(default_cal));

    let provider = WebcamProvider::create()
        .socket(&args.webcam_socket)
        .geometry(geometry.clone())
        .camera(camera)
        .calibration(calibration.clone())
        .sigma_deg(sigma_deg)
        .start()
        .with_context(|| format!("starting the webcam provider on {}", args.webcam_socket.display()))?;

    let buttons = ButtonSource::open(None, &args.device, !args.no_grab).with_context(|| {
        format!(
            "opening {:?} for commits: a webcam run has no other commit channel",
            args.device,
        )
    })?;

    info!(
        socket      = %args.webcam_socket.display(),
        camera      = %args.camera.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "from config".to_string()),
        calibration = ?args.calibration.as_ref().map(|p| p.display().to_string()),
        sigma_deg   = sigma_deg,
        device      = %buttons.path().display(),
        grabbed     = buttons.grabbed(),
        commits     = commit_note(buttons.grabbed()),
        "provider: webcam"
    );

    match &calibration {
        Some(path) => info!(calibration = %path.display(), "webcam calibration loaded"),
        None       => warn!("running uncalibrated: expect several degrees of bias (gaze-webcam-cli calibrate fixes it)"),
    }

    Ok(GazeSource::Webcam {
        provider : provider,
        buttons  : buttons,
        health   : WebcamHealth::default(),
    })
}

/// Real gaze from the ET5 over native USB, with controls from the Lenovo.
///
/// The provider claims the tracker's USB interface exclusively, so a sweep or a
/// `gaze-et5-cli view` cannot run at the same time. The calibration matters more
/// here than for the webcam: without one the tracker runs under an oversized
/// virtual plane and the firmware's trained end-to-end mapping never applies, so
/// the conventional file is picked up when present and its absence is loud.
fn open_et5(args: &Args, geometry: &DesktopGeometry) -> Result<GazeSource> {
    let default_cal = PathBuf::from(DEFAULT_ET5_CALIBRATION_PATH);
    let path        = args.calibration.clone()
        .or_else(|| default_cal.exists().then_some(default_cal));

    let calibration = {
        match &path {
            Some(path) => Some(Et5Calibration::load(path)
                .with_context(|| format!("loading {}", path.display()))?),
            None       => None,
        }
    };

    if calibration.is_none() {
        warn!("running uncalibrated: the trained on-device mapping will not apply \
               (gaze-et5-cli calibrate fixes it)");
    }

    let provider = Et5Provider::create()
        .geometry(geometry.clone())
        .calibration(calibration)
        .start()
        .context("starting the ET5 provider (tracker on the bus, nothing else holding it?)")?;

    let buttons = ButtonSource::open(None, &args.device, !args.no_grab).with_context(|| {
        format!(
            "opening {:?} for commits: an ET5 run has no other commit channel",
            args.device,
        )
    })?;

    info!(
        calibration = ?path.as_ref().map(|p| p.display().to_string()),
        device      = %buttons.path().display(),
        grabbed     = buttons.grabbed(),
        commits     = commit_note(buttons.grabbed()),
        "provider: et5"
    );

    Ok(GazeSource::Et5 {
        provider : Box::new(provider),
        buttons  : buttons,
    })
}

/// Replays a recorded JSONL session, with controls from the un-grabbed Lenovo when it is
/// present.
///
/// A missing device is only a warning here: a replay is the one mode that can run with no
/// hardware at all, driven by the recording and `--seconds`.
fn open_replay(args: &Args) -> Result<GazeSource> {
    let path = args.replay.as_ref().context("--provider replay needs --replay <FILE>")?;

    let provider = ReplayProvider::from_jsonl(path)
        .with_context(|| format!("reading the replay log {}", path.display()))?;

    let buttons = match ButtonSource::open(None, &args.device, !args.no_grab) {
        Ok(buttons) => Some(buttons),

        Err(e) => {
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
        commits = commit_note(buttons.as_ref().is_some_and(ButtonSource::grabbed)),
        "provider: replay"
    );

    Ok(GazeSource::Replay {
        provider : provider,
        buttons  : buttons,
    })
}

// --- Helpers ---

/// One line for the startup log saying what the commit button does, which differs enough
/// between a grabbed and an un-grabbed device to be worth spelling out at every start.
fn commit_note(grabbed: bool) -> &'static str {
    if grabbed {
        "left button on the grabbed device (right exits, middle redetects)"
    }
    else {
        "left button on the un-grabbed device; the compositor sees the press as a click too"
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

    fn meta(seq: u64, conf: f64, lat_ms: f64) -> SampleMeta {
        SampleMeta {
            seq         : seq,
            sidecar_t_s : 0.0,
            conf        : conf,
            lat_ms      : lat_ms,
            off_axis_deg: 0.0,
            clamped     : false,
        }
    }

    #[test]
    fn health_means_are_over_distinct_frames() {
        let mut health = WebcamHealth::default();

        health.observe(meta(1, 0.9, 20.0));
        health.observe(meta(2, 0.7, 40.0));

        assert_eq!(health.frames, 2);
        assert!((health.conf_mean() - 0.8).abs() < 1e-12);
        assert!((health.lat_ms_mean() - 30.0).abs() < 1e-12);
    }

    /// The session polls once per gaze sample, so a sidecar that stalls would otherwise
    /// have its last good frame counted over and over and report perfect health.
    #[test]
    fn health_ignores_a_frame_it_has_already_counted() {
        let mut health = WebcamHealth::default();

        health.observe(meta(1, 1.0, 10.0));

        for _ in 0..100 {
            health.observe(meta(1, 1.0, 10.0));
        }

        health.observe(meta(2, 0.0, 30.0));

        assert_eq!(health.frames, 2);
        assert!((health.conf_mean() - 0.5).abs() < 1e-12);
        assert!((health.lat_ms_mean() - 20.0).abs() < 1e-12);
    }

    #[test]
    fn health_means_are_zero_before_the_first_frame() {
        let health = WebcamHealth::default();

        assert_eq!(health.frames, 0);
        assert_eq!(health.conf_mean(), 0.0);
        assert_eq!(health.lat_ms_mean(), 0.0);
    }
}
