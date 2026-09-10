//! Command line surface of the dev harness.
//!
//! The daemon runs the same session loop with no flags at all: the files come from
//! [`gaze_config::Paths`], the numbers from the stored [`Tuning`], and clicking, edge
//! scrolling and the controller are always on. What is left here is what an experiment
//! needs and a desktop does not: which provider, whether anything is injected, a
//! recording, a deadline, the debug look, and `--tune KEY=VALUE` to move any knob for
//! one run without touching the stored tuning.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::{Parser, ValueEnum};
use gaze_config::{Paths, Tuning};
use gaze_core::{DesktopGeometry, NoiseModel, SigmaProfile};

use crate::config::{DaydreamSpec, OverlayMode, SessionConfig, SourceSpec};

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
    /// commit/exit/redetect from its own buttons. The device is `EVIOCGRAB`ed, so none of
    /// it reaches the compositor.
    #[default]
    Synthetic,

    /// Real gaze from the Tobii ET5 over native USB (`gaze-provider-et5`). Controls
    /// come from the controller, and from the Lenovo's buttons (read by a separate
    /// reader that grabs the device, see `--no-grab`, and discards its motion) unless
    /// `--no-buttons`; calibrate first with `gaze-et5-cli calibrate`.
    Et5,

    /// Replay a recorded JSONL session (`--record` from an earlier run). Controls come
    /// from the same button source as `et5` when the device is available, and the run
    /// is otherwise driven entirely by the recording.
    Replay,
}

/// Live gaze prototype: a gaze source to snap to click, with every knob on the command
/// line.
///
/// Clicking is off by default. Nothing reaches the real pointer until `--click` is passed.
#[derive(Debug, Parser)]
#[command(name = "gaze-proto", version)]
pub struct Args {
    /// A checkout to read the desk's files from: `DIR/config` holds the desk file, the
    /// calibration, the residual model, the offset and the flywheel, `DIR/models` the
    /// ONNX models. The daemon reads the XDG locations instead.
    #[arg(long, default_value = ".")]
    pub home: PathBuf,

    /// Name substring matching the mouse read for commits under /dev/input/event*.
    #[arg(long, default_value = "Lenovo")]
    pub device: String,

    /// Read no mouse for commits: the controller is the only commit channel, as in the
    /// daemon. Not for `--provider synthetic`, whose mouse is the gaze.
    #[arg(long, conflicts_with = "device")]
    pub no_buttons: bool,

    /// Where gaze samples come from. See `Provider` for what commits in each mode.
    #[arg(long, value_enum, default_value_t = Provider::Synthetic)]
    pub provider: Provider,

    /// Recorded JSONL session to replay. Required by `--provider replay`.
    #[arg(long)]
    pub replay: Option<PathBuf>,

    /// Do not grab the commit mouse.
    ///
    /// The default grabs it, because the Lenovo is a dedicated spare: without the grab a
    /// commit press also clicks whatever the pointer is over and the exit press also
    /// right-clicks the desktop. Pass this when reading a mouse that is still in use as a
    /// mouse. Has no effect under `--provider synthetic`, which always grabs.
    #[arg(long)]
    pub no_grab: bool,

    /// Keep the ET5 online offset frozen: real clicks are attributed and logged but
    /// do not move it, and nothing is written to the offset file.
    #[arg(long)]
    pub freeze_offset: bool,

    /// Write no flywheel records this run.
    #[arg(long)]
    pub no_flywheel: bool,

    /// Override one tuning knob for this run, `KEY=VALUE`; repeatable. The keys are
    /// the fields of `gaze_config::Tuning`, e.g. `--tune edge_dwell_s=0.4`. The stored
    /// tuning is never read or written by the prototype.
    #[arg(long = "tune", value_name = "KEY=VALUE")]
    pub tune: Vec<String>,

    /// Override the desk noise profile with a flat sigma, in degrees, on every output.
    #[arg(long, conflicts_with = "sigma_profile")]
    pub sigma: Option<f64>,

    /// Use the desk config's own sigma profile. The default; the flag exists so a command
    /// line can say so out loud.
    #[arg(long)]
    pub sigma_profile: bool,

    /// Log what would be clicked, scrolled and warped instead of doing it. The default.
    #[arg(long, conflicts_with = "click")]
    pub dry_run: bool,

    /// Really inject through /dev/uinput: clicks, edge scrolls and the pointer warps
    /// that carry them. Moves the real pointer.
    #[arg(long)]
    pub click: bool,

    /// Draw the noise-free gaze point on the overlay alongside the noisy one.
    #[arg(long)]
    pub show_truth: bool,

    /// Draw the debug overlay (gaze ring, raw candidate box, state caption) instead of
    /// the pointer look. The pointer look shows a dot only near something clickable and
    /// a themed highlight on the favoured control; the debug look shows everything.
    #[arg(long, conflicts_with = "overlay_always")]
    pub overlay_debug: bool,

    /// Show the pointer look for the whole run. The default shows it only while a thumb
    /// rests on the Daydream pad or F14 has latched it on, so reading is never marked
    /// and the mouse is never fought; the look comes up when a commit is being aimed.
    #[arg(long)]
    pub overlay_always: bool,

    /// Read no Daydream controller even if one is paired. The default reads the first
    /// paired one (`gaze-daydream`): its pad commits, Home exits, App holds the voice
    /// stack's push-to-talk (forwarded as F13), the volume keys are a wheel, a thumb
    /// resting on the pad shows the pointer look, and a thumb that travels refines the
    /// commit point. Pair it once with `bluetoothctl`; a sleeping controller is retried
    /// until Home wakes it.
    #[arg(long, conflicts_with = "daydream_address")]
    pub no_daydream: bool,

    /// The controller's Bluetooth address, when more than one is paired.
    #[arg(long)]
    pub daydream_address: Option<String>,

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
    /// The files this run reads.
    pub fn paths(&self) -> Paths {
        Paths::home(&self.home)
    }

    /// Everything the session needs before it starts, read from the desk file and the
    /// flags. Fails when the desk file is missing or malformed, or a required flag is.
    pub fn session(&self) -> Result<SessionConfig> {
        let paths = self.paths();
        let desk  = paths.desk();

        let text = std::fs::read_to_string(&desk)
            .with_context(|| format!("reading {}", desk.display()))?;

        let geometry = DesktopGeometry::from_toml(&text)
            .with_context(|| format!("parsing {}", desk.display()))?;

        // Files the provider only reads when they exist, so a freshly fitted model is
        // never silently ignored and a missing one is loud where it is opened.
        let existing = |path: PathBuf| path.exists().then_some(path);

        let source = match self.provider {
            Provider::Synthetic => SourceSpec::Synthetic {
                device : self.device.clone(),
                gain   : self.gain,
                seed   : self.seed,
                model  : self.noise_model(geometry.noise)?,
            },

            Provider::Et5 => SourceSpec::Et5 {
                calibration : existing(paths.calibration()),
                model       : existing(paths.model()),
                device_blob : paths.device_blob(),
                offset      : (!self.freeze_offset).then(|| paths.offset()),
                flywheel    : (!self.no_flywheel).then(|| paths.flywheel()),
            },

            Provider::Replay => SourceSpec::Replay {
                path : self.replay.clone().context("--provider replay needs --replay <FILE>")?,
            },
        };

        let daydream = match (&self.daydream_address, self.no_daydream) {
            (Some(address), _) => DaydreamSpec::Address(address.clone()),
            (None, true)       => DaydreamSpec::Off,
            (None, false)      => DaydreamSpec::Auto,
        };

        let overlay = match (self.overlay_debug, self.overlay_always) {
            (true, _)      => OverlayMode::Debug,
            (false, true)  => OverlayMode::Always,
            (false, false) => OverlayMode::Pointer,
        };

        Ok(SessionConfig {
            geometry   : geometry,
            models_dir : paths.models_dir,
            source     : source,
            buttons    : (!self.no_buttons).then(|| self.device.clone()),
            grab       : !self.no_grab,
            daydream   : daydream,
            click      : !self.dry_run && self.click,
            overlay    : overlay,
            show_truth : self.show_truth,
            record     : self.record.clone(),
            seconds    : self.seconds,
        })
    }

    /// The tuning this run starts under: the defaults, then every `--tune` in order.
    /// Fails on a bad key or value.
    pub fn tuning(&self) -> Result<Tuning> {
        let mut tuning = Tuning::default();

        for assignment in &self.tune {
            tuning.apply(assignment).map_err(|e| anyhow::anyhow!("--tune: {e}"))?;
        }

        Ok(tuning)
    }

    /// Resolves the noise model to run the synthetic provider with.
    ///
    /// `desk` is whatever the config's `[noise]` section held. Rate, drift and latency
    /// always come from there, because they describe the tracker being simulated rather
    /// than the experiment being run; only the sigma profile is overridable, and only by
    /// `--sigma`.
    pub fn noise_model(&self, desk: Option<NoiseModel>) -> Result<NoiseModel> {
        let mut model = desk.with_context(|| {
            format!("{} has no [noise] section", self.paths().desk().display())
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

    /// Whether anything at all may reach the real pointer this run.
    ///
    /// `--dry-run` is the master off switch, and so is the default: without `--click`
    /// there is no injector, so no warp, scroll or click can happen even by accident.
    /// With it, all three are live, because the warps and the edge scrolls are what
    /// carry the clicks to where they land.
    pub fn injects(&self) -> bool {
        !self.dry_run && self.click
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

    /// A run starts on the defaults; an explicit `--tune` moves one key and leaves the
    /// rest; a bad assignment is an error, not a silent default.
    #[test]
    fn tuning_layers_defaults_and_overrides() {
        let tracker = Args::parse_from(["gaze-proto", "--provider", "et5"]).tuning().unwrap();

        assert_eq!(tracker, Tuning::default());

        let tuned = Args::parse_from([
            "gaze-proto", "--provider", "et5", "--tune", "filter_beta=0.5", "--tune", "snap_deg=3",
        ])
        .tuning()
        .unwrap();

        assert_eq!(tuned.filter_beta, 0.5);
        assert_eq!(tuned.filter_velocity_deg_s, Tuning::default().filter_velocity_deg_s);
        assert_eq!(tuned.snap_deg, 3.0);

        assert!(Args::parse_from(["gaze-proto", "--tune", "snap=3"]).tuning().is_err());
    }
}
