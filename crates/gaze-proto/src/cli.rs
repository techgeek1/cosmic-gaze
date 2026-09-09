//! Command line surface of the live prototype.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use gaze_core::{NoiseModel, SigmaProfile};
use gaze_provider_webcam::DEFAULT_SOCKET;

use crate::daydream::RefineMode;

/// Off-axis angle the `--sigma` override declares its flat region reaches to. Far beyond
/// anything a desk spans, so `SigmaProfile::sigma_at` never leaves that region and every
/// sample gets exactly the sigma that was asked for, on every output.
const FLAT_OVERRIDE_DEG: f64 = 1.0e6;

/// Which source the gaze samples come from.
///
/// The choice also decides where the commit/exit/redetect controls come from, because only
/// the synthetic provider owns an input device of its own. See the [`Provider`] variants.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, ValueEnum)]
pub enum Provider {
    /// The phase 0 path: grab the Lenovo mouse, integrate its motion as gaze, and take
    /// commit/exit/redetect (and, with `--scroll`, the wheel) from its own buttons. The
    /// device is `EVIOCGRAB`ed, so none of it reaches the compositor.
    #[default]
    Synthetic,

    /// Real gaze from `gaze-provider-webcam`. The controls come from the Lenovo's buttons
    /// read by a separate reader, which grabs the device too (see `--no-grab`) and
    /// discards its motion.
    Webcam,

    /// Real gaze from the Tobii ET5 over native USB (`gaze-provider-et5`). Controls
    /// come from the Lenovo's buttons like `webcam`; calibrate first with
    /// `gaze-et5-cli calibrate`.
    Et5,

    /// Replay a recorded JSONL session (`--record` from an earlier run). Controls come
    /// from the same button source as `webcam` when the device is available, and the run
    /// is otherwise driven entirely by the recording.
    Replay,
}

/// Live gaze prototype: grabbed mouse to synthetic gaze to snap to click.
///
/// Clicking is off by default. Nothing reaches the real pointer until `--click` is passed.
#[derive(Debug, Parser)]
#[command(name = "gaze-proto", version)]
pub struct Args {
    /// Desk geometry and noise config.
    #[arg(long, default_value = "config/desk.toml")]
    pub config: PathBuf,

    /// Name substring matching the mouse to grab under /dev/input/event*.
    #[arg(long, default_value = "Lenovo")]
    pub device: String,

    /// Directory holding the ONNX models.
    #[arg(long, default_value = "models")]
    pub models: PathBuf,

    /// Where gaze samples come from. See `Provider` for what commits in each mode.
    #[arg(long, value_enum, default_value_t = Provider::Synthetic)]
    pub provider: Provider,

    /// Recorded JSONL session to replay. Required by `--provider replay`.
    #[arg(long)]
    pub replay: Option<PathBuf>,

    /// Leave the button device readable by the compositor in `webcam` and `replay` mode.
    ///
    /// The default grabs it, because the Lenovo is a dedicated spare: without the grab a
    /// commit press also clicks whatever the pointer is over and the exit press also
    /// right-clicks the desktop. Pass this when reading a mouse that is still in use as a
    /// mouse, and accept that the scroll tier can then only warp, not re-inject. Has no
    /// effect under `--provider synthetic`, which always grabs.
    #[arg(long)]
    pub no_grab: bool,

    /// Unix socket the webcam gaze sidecar publishes on. Defaults to the sidecar's own
    /// path, so `--provider webcam` needs no flags when the sidecar is running.
    #[arg(long, default_value = DEFAULT_SOCKET)]
    pub webcam_socket: PathBuf,

    /// Overrides the camera node from the desk config's `[camera] device`. The rest of the
    /// camera pose always comes from that config, since it describes the desk rather than
    /// the run.
    #[arg(long)]
    pub camera: Option<PathBuf>,

    /// Calibration file for the webcam or ET5 provider. Omitted, the provider's
    /// conventional file (`config/calibration.toml` / `config/calibration-et5.toml`)
    /// is used when it exists; truly uncalibrated runs are worth several degrees of
    /// error (`gaze-webcam-cli calibrate` / `gaze-et5-cli calibrate` fit one).
    #[arg(long)]
    pub calibration: Option<PathBuf>,

    /// Residual model for the ET5 provider (`gaze-et5-cli fit`). Omitted, the
    /// conventional file (`config/model-et5.json`) is used when it exists.
    #[arg(long)]
    pub model: Option<PathBuf>,

    /// Where the ET5 online offset (the day's bias, learnt from real clicks) persists
    /// across runs. Only meaningful with a residual model.
    #[arg(long, default_value = "config/offset-et5.json")]
    pub offset: PathBuf,

    /// Keep the ET5 online offset frozen: real clicks are attributed and logged but
    /// do not move it, and nothing is written to `--offset`.
    #[arg(long)]
    pub freeze_offset: bool,

    /// Where the ET5 flywheel writes every attributed click with its features, one
    /// JSONL file per UTC day (`gaze-et5-cli flywheel` reads them). Only meaningful
    /// with a residual model.
    #[arg(long, default_value = "config/flywheel")]
    pub flywheel: PathBuf,

    /// Write no flywheel records this run.
    #[arg(long)]
    pub no_flywheel: bool,

    /// I-VT saccade velocity threshold, deg/s. Default 30 (tracker) or 80 (webcam).
    #[arg(long)]
    pub filter_velocity_deg_s: Option<f64>,

    /// Velocity estimation window, seconds. Default 0.02 (tracker) or 0.10 (webcam).
    #[arg(long)]
    pub filter_window_s: Option<f64>,

    /// One-euro minimum cutoff, Hz. Lower is smoother and laggier during fixations.
    /// Default 1.0 (tracker) or 0.6 (webcam).
    #[arg(long)]
    pub filter_min_cutoff_hz: Option<f64>,

    /// One-euro speed coefficient. Default 0.3 (tracker) or 0.02 (webcam).
    #[arg(long)]
    pub filter_beta: Option<f64>,

    /// Route the real mouse wheel to the window under the gaze point instead of the one
    /// under the pointer. The scroll tier: no precision needed, highest volume (DESIGN.md
    /// section 3, principle 4).
    #[arg(long)]
    pub scroll: bool,

    /// How far the gaze point must be from the pointer, in degrees of visual angle, before
    /// a wheel event warps the pointer to it. Below this the pointer is already close
    /// enough that warping would only steal the user's own scroll position.
    #[arg(long, default_value_t = 3.0)]
    pub scroll_warp_deg: f64,

    /// Warp the pointer to a fixation that dwells on a different output than the pointer
    /// is on. No click, just the warp, so keyboard-follows-mouse desktops follow gaze.
    #[arg(long)]
    pub focus_follows_gaze: bool,

    /// Seconds a fixation must last before `--focus-follows-gaze` warps to it.
    #[arg(long, default_value_t = 0.4)]
    pub focus_dwell_s: f64,

    /// Scroll the surface under the gaze continuously while the eyes dwell in its lower
    /// or upper band, at a speed that grows with how deep in the band they are. The
    /// surface is the real scrolling region from the accessibility tree, never the
    /// window; where the tree has no answer nothing scrolls. Only while the eyes are
    /// not pointing: a thumb on the Daydream pad or the F14 latch silences it, and with
    /// the thumb up the overlay shows the band the eyes are near as a faint zone. See
    /// `edge_scroll`.
    #[arg(long)]
    pub edge_scroll: bool,

    /// Share of the surface's height forming the lower (read-on) band.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_BAND_FRACTION)]
    pub edge_band: f64,

    /// Share of the surface's height forming the upper (go-back) band.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_TOP_BAND_FRACTION)]
    pub edge_top_band: f64,

    /// Seconds the gaze must stay in the lower band before scrolling starts.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_DWELL_S)]
    pub edge_dwell_s: f64,

    /// Seconds the gaze must stay in the upper band before scrolling starts.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_TOP_DWELL_S)]
    pub edge_top_dwell_s: f64,

    /// Scroll speed at the surface's edge, wheel lines per second.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_MAX_LINES_S)]
    pub edge_max_lines_s: f64,

    /// Seconds for the speed to ramp up from zero when a scroll starts.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_RAMP_S)]
    pub edge_ramp_s: f64,

    /// Exponent on band depth: 1 is linear, 2 slow near the inner edge and fast at the outer.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_EXPONENT)]
    pub edge_exponent: f64,

    /// Seconds the eyes must hold the outer part of the band before the speed grows.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_HOLD_S)]
    pub edge_hold_s: f64,

    /// Speed multiplier gained per second of hold past `--edge-hold-s` (2 doubles it
    /// every second), capped at 40 lines per second overall.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_HOLD_GAIN)]
    pub edge_hold_gain: f64,

    /// Speed multiplier while the tracked eyes are past the edge of the screen itself.
    #[arg(long, default_value_t = crate::edge_scroll::DEFAULT_TURBO)]
    pub edge_turbo: f64,

    /// Override the desk noise profile with a flat sigma, in degrees, on every output.
    /// Also sets the webcam provider's flat sigma, which otherwise defaults to the 2.5
    /// degrees `gaze-provider-webcam` calls the optimistic end of webcam-only gaze.
    #[arg(long, conflicts_with = "sigma_profile")]
    pub sigma: Option<f64>,

    /// Use the desk config's own sigma profile. The default; the flag exists so a command
    /// line can say so out loud.
    #[arg(long)]
    pub sigma_profile: bool,

    /// Log what would be clicked instead of clicking it. The default.
    #[arg(long, conflicts_with = "click")]
    pub dry_run: bool,

    /// Really inject clicks through /dev/uinput. Moves the real pointer.
    #[arg(long)]
    pub click: bool,

    /// Draw the noise-free gaze point on the overlay alongside the noisy one.
    #[arg(long)]
    pub show_truth: bool,

    /// Draw the debug overlay (gaze ring, raw candidate box, state caption) instead of
    /// the pointer look. The pointer look shows a dot only near something clickable and
    /// a themed highlight on the favoured control; the debug look shows everything.
    #[arg(long)]
    pub overlay_debug: bool,

    /// How close the gaze must be to an element, degrees from its nearest edge, for the
    /// pointer look to show the dot and highlight it. The snap engine still targets out
    /// to `--snap-deg`, so a commit past this distance lands on an unmarked element.
    #[arg(long, default_value_t = 0.8)]
    pub near_deg: f64,

    /// Snap radius: how far the gaze may be from an element's nearest edge, degrees, for
    /// the engine to target it at all. Wider forgives more tracker error and pulls to
    /// more wrong elements.
    #[arg(long, default_value_t = 2.0)]
    pub snap_deg: f64,

    /// Show the pointer look for the whole run. The default shows it only while a thumb
    /// rests on the Daydream pad or F14 has latched it on, so reading is never marked
    /// and the mouse is never fought; the look comes up when a commit is being aimed.
    #[arg(long)]
    pub overlay_always: bool,

    /// How long the pointer dot takes to settle on a new gaze point, seconds. The dot
    /// follows the gaze on a critically damped spring stepped at the display's frame
    /// rate; shorter is more responsive and passes more of the tracker's jitter.
    #[arg(long, default_value_t = 0.2)]
    pub pointer_settle_s: f64,

    /// How long the pointer dot stays up after nothing clickable is near, seconds.
    #[arg(long, default_value_t = 0.3)]
    pub pointer_linger_s: f64,

    /// Do not ask the accessibility tree what the eyes are on. Asked, the tree's answer
    /// outranks the recogniser's kind for the pointer look: a control it names is marked
    /// with its own box, text it names is never marked. Applications off the bus
    /// (Chromium and Electron without accessibility forced on, see gaze-a11y's README)
    /// fall back to the recogniser either way.
    #[arg(long)]
    pub no_a11y: bool,

    /// Let the pointer look mark text elements (OCR words, labels) as well as controls.
    /// Off, a target that is text gets no highlight and no dot, so a page of prose stays
    /// unmarked while it is read; a commit on an OCR word still lands, unmarked.
    #[arg(long)]
    pub highlight_text: bool,

    /// Read the Daydream controller (`gaze-daydream`) alongside the mouse: its pad
    /// commits, Home exits, App holds the voice stack's push-to-talk (forwarded as F13),
    /// the volume keys are a wheel, a thumb resting on the pad shows the pointer look,
    /// and a thumb that travels refines the commit point (see `--refine`). Pair it once with
    /// `bluetoothctl` and wake it with Home before starting.
    #[arg(long)]
    pub daydream: bool,

    /// The controller's Bluetooth address, when more than one is paired.
    #[arg(long, requires = "daydream")]
    pub daydream_address: Option<String>,

    /// What moves the commit point while the thumb rests on the controller's pad.
    #[arg(long, value_enum, default_value_t = RefineMode::Touch, requires = "daydream")]
    pub refine: RefineMode,

    /// Gyro refine gain, logical pixels per radian of wrist turn.
    #[arg(long, default_value_t = crate::daydream::DEFAULT_GYRO_GAIN_PX_PER_RAD)]
    pub refine_gyro_gain: f64,

    /// Touch refine gain, logical pixels per full pad width.
    #[arg(long, default_value_t = crate::daydream::DEFAULT_TOUCH_GAIN_PX)]
    pub refine_touch_gain: f64,

    /// Side of the box, centred on where the refine began, that the refined point stays
    /// inside, in logical pixels.
    #[arg(long, default_value_t = crate::daydream::DEFAULT_RANGE_PX)]
    pub refine_range: f64,

    /// Which gyro axes drive pointer x and y, each `x`, `y` or `z` with an optional
    /// minus, pointer x first. Change it if the wrist and the pointer disagree.
    #[arg(long, default_value = crate::daydream::DEFAULT_AXES)]
    pub refine_axes: String,

    /// Commit channel latency, in seconds. A commit is attributed to the target that was
    /// fixated this long ago, not to whatever is under the gaze now.
    #[arg(long, default_value_t = 0.15)]
    pub commit_latency: f64,

    /// Fraction of an output's frame that must change before detection re-runs on it.
    #[arg(long, default_value_t = 0.02)]
    pub redetect_threshold: f32,

    /// Seconds after which an output is re-detected regardless of how little changed. The
    /// frame-diff trigger catches real changes within ~200 ms, so this is only a backstop;
    /// at 2 s it kept the detector pinned (~8 cores) on a static desktop.
    #[arg(long, default_value_t = 15.0)]
    pub redetect_interval: f64,

    /// Append every provider sample to this file as one JSON object per line, for replay.
    #[arg(long)]
    pub record: Option<PathBuf>,

    /// Exit after this many seconds. Runs until the right button otherwise.
    #[arg(long)]
    pub seconds: Option<f64>,

    /// Logical pixels of gaze motion per raw mouse count.
    #[arg(long, default_value_t = 1.0)]
    pub gain: f64,

    /// Seeds the provider's noise RNG, so a recorded session reproduces exactly.
    #[arg(long, default_value_t = 0)]
    pub seed: u64,
}

// --- Args ---

impl Args {
    /// Resolves the noise model to run the provider with.
    ///
    /// `desk` is whatever the config's `[noise]` section held. Rate, drift and latency
    /// always come from there, because they describe the tracker being simulated rather
    /// than the experiment being run; only the sigma profile is overridable, and only by
    /// `--sigma`.
    pub fn noise_model(&self, desk: Option<NoiseModel>) -> Result<NoiseModel> {
        let mut model = desk.with_context(|| {
            format!("{} has no [noise] section", self.config.display())
        })?;

        if let Some(sigma_deg) = self.sigma {
            if sigma_deg <= 0.0 || !sigma_deg.is_finite() {
                anyhow::bail!("--sigma must be a positive number of degrees, got {sigma_deg}");
            }

            model.profile = SigmaProfile {
                sigma_deg      : sigma_deg,
                flat_to_deg    : FLAT_OVERRIDE_DEG,
                ramp_to_deg    : FLAT_OVERRIDE_DEG,
                ramp_factor    : 1.0,
                invalid_at_deg : f64::INFINITY,
            };
        }

        Ok(model)
    }

    /// How long an output may go without a fresh detection.
    pub fn redetect_interval(&self) -> Duration {
        Duration::from_secs_f64(self.redetect_interval.max(0.0))
    }

    /// Whether anything at all may reach the real pointer this run.
    ///
    /// `--dry-run` is the master off switch: it gates warps and scrolls (wheel and edge)
    /// as well as clicks,
    /// so a dry run is safe to leave running while the desk is being used for something
    /// else. Note that the default (neither flag) is *not* a dry run for the scroll tier:
    /// `--scroll` alone scrolls for real, because a scroll is not a click and routing it
    /// to the window under gaze is the whole point of the flag.
    pub fn injects(&self) -> bool {
        !self.dry_run && (self.click || self.scroll || self.focus_follows_gaze || self.edge_scroll)
    }

    /// The edge scroller's tunables from the flags.
    pub fn edge_params(&self) -> crate::edge_scroll::EdgeParams {
        crate::edge_scroll::EdgeParams {
            band_fraction     : self.edge_band,
            top_band_fraction : self.edge_top_band,
            dwell_s           : self.edge_dwell_s,
            top_dwell_s       : self.edge_top_dwell_s,
            max_lines_s       : self.edge_max_lines_s,
            ramp_s            : self.edge_ramp_s,
            exponent          : self.edge_exponent,
            hold_s            : self.edge_hold_s,
            hold_gain         : self.edge_hold_gain,
            turbo             : self.edge_turbo,
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The `--sigma` override is the one piece of CLI parsing with a way to be subtly
    /// wrong: a flat region that stops short leaves the ramp and the invalid cutoff live,
    /// so the far corners of the desk would quietly get a different sigma than was asked
    /// for, or no samples at all.
    #[test]
    fn the_flat_override_holds_its_sigma_everywhere_on_the_desk() {
        let args = Args::parse_from(["gaze-proto", "--sigma", "1.5"]);

        let desk = NoiseModel {
            profile         : SigmaProfile::default(),
            jitter_deg      : 0.2,
            bias_redraw_deg : 1.0,
            drift_deg       : 0.0,
            latency_s       : 0.0,
            rate_hz         : 120.0,
        };

        let model = args.noise_model(Some(desk)).unwrap();

        for off in [0.0, 25.0, 39.0, 60.0, 89.0] {
            assert_eq!(model.profile.sigma_at(off), Some(1.5), "off-axis {off}");
        }

        // Rate still comes from the desk config: the override only touches the profile.
        assert_eq!(model.rate_hz, 120.0);
    }
}
