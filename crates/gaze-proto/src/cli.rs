//! Command line surface of the dev harness.
//!
//! The daemon runs the same session loop with no flags at all: the files come from
//! [`gaze_config::Paths`], the numbers from the stored [`Tuning`], and clicking, edge
//! scrolling and the controller are always on. What is left here is what an experiment
//! needs and a desktop does not: whether anything is injected, a deadline, the debug
//! look, a frozen offset, and `--tune KEY=VALUE` to move any knob for one run without
//! touching the stored tuning.

use std::path::PathBuf;

use anyhow::{Context, Result};
use clap::Parser;
use gaze_config::{Paths, Tuning};
use gaze_core::DesktopGeometry;

use crate::config::{DaydreamSpec, OverlayMode, SessionConfig, SourceSpec};

/// Live gaze prototype: a gaze source to snap to click, with every knob on the command
/// line.
///
/// Clicking is off by default. Nothing reaches the real pointer until `--click` is passed.
#[derive(Debug, Parser)]
#[command(name = "gaze-proto", version)]
pub struct Args {
    /// A checkout to read the desk's files from: `DIR/config` holds the desk file and
    /// the calibration, `DIR/models` the ONNX models. The daemon reads the
    /// XDG locations instead.
    #[arg(long, default_value = ".")]
    pub home: PathBuf,

    /// Override one tuning knob for this run, `KEY=VALUE`; repeatable. The keys are
    /// the fields of `gaze_config::Tuning`, e.g. `--tune edge_dwell_s=0.4`. The stored
    /// tuning is never read or written by the prototype.
    #[arg(long = "tune", value_name = "KEY=VALUE")]
    pub tune: Vec<String>,

    /// Log what would be clicked, scrolled and warped instead of doing it. The default.
    #[arg(long, conflicts_with = "click")]
    pub dry_run: bool,

    /// Really inject through /dev/uinput: clicks, edge scrolls and the pointer warps
    /// that carry them. Moves the real pointer.
    #[arg(long)]
    pub click: bool,

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

    /// Exit after this many seconds. Runs until the controller's Home button otherwise.
    #[arg(long)]
    pub seconds: Option<f64>,

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

        // Files the provider only reads when they exist, so a missing one is loud
        // where it is opened.
        let existing = |path: PathBuf| path.exists().then_some(path);

        let source = SourceSpec {
            calibration : existing(paths.calibration()),
            device_blob : paths.device_blob(),
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
            daydream   : daydream,
            click      : !self.dry_run && self.click,
            overlay    : overlay,
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

    /// A run starts on the defaults; an explicit `--tune` moves one key and leaves the
    /// rest; a bad assignment is an error, not a silent default.
    #[test]
    fn tuning_layers_defaults_and_overrides() {
        let tracker = Args::parse_from(["gaze-proto"]).tuning().unwrap();

        assert_eq!(tracker, Tuning::default());

        let tuned = Args::parse_from([
            "gaze-proto", "--tune", "filter_beta=0.5", "--tune", "snap_deg=3",
        ])
        .tuning()
        .unwrap();

        assert_eq!(tuned.filter_beta, 0.5);
        assert_eq!(tuned.filter_velocity_deg_s, Tuning::default().filter_velocity_deg_s);
        assert_eq!(tuned.snap_deg, 3.0);

        assert!(Args::parse_from(["gaze-proto", "--tune", "snap=3"]).tuning().is_err());
    }
}
