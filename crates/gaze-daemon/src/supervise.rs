//! Keeps a session running.
//!
//! The session loop returns when the tracker cannot be opened, when the controller's
//! Home button exits it, or when the overlay thread dies. None of those should end the
//! daemon: the tracker may be unplugged for a minute, the button may have been pressed
//! by accident, the compositor may have restarted. So the daemon runs the session again
//! after a pause, forever, until it is told to stop.

use std::sync::Arc;
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gaze_config::{Paths, Status};
use gaze_core::DesktopGeometry;
use gaze_proto::config::{DaydreamSpec, OverlayMode, SessionConfig, SourceSpec};
use gaze_proto::{Live, session};
use tracing::{error, info, warn};

/// How long to wait before trying again after a session failed to start or ended in
/// an error. Long enough not to spam a log while the tracker is unplugged, short enough
/// that plugging it back in is noticed.
const RETRY_AFTER: Duration = Duration::from_secs(5);

/// How long to wait before running the session again after it ended cleanly (the exit
/// button, the overlay going away).
const RESTART_AFTER: Duration = Duration::from_secs(1);

/// A session that ended sooner than this after starting is failing at startup, and its
/// restart is paced like a failure.
const SHORT_RUN: Duration = Duration::from_secs(10);

/// What the daemon was told on its command line.
#[derive(Clone, Debug)]
pub struct Options {
    /// Where the desk's files are.
    pub paths   : Paths,
    /// Log clicks, warps and scrolls instead of injecting them.
    pub dry_run : bool,
    /// Draw the debug look instead of the pointer look.
    pub debug   : bool,
}

// --- Supervision ---

/// Runs sessions until `live` is stopped.
pub fn run(options: &Options, live: &Arc<Live>) {
    while !live.stopped() {
        let started = Instant::now();

        let outcome = configure(options).and_then(|config| session::run(&config, live));

        // Whatever the session was doing is over; say so on the bus.
        live.set_status(Status::default());

        if live.stopped() {
            break;
        }

        let pause = match outcome {
            Ok(())                                      => RESTART_AFTER,
            Err(_) if started.elapsed() >= SHORT_RUN    => RESTART_AFTER,
            Err(_)                                      => RETRY_AFTER,
        };

        match outcome {
            Ok(())  => info!(retry_s = pause.as_secs(), "session ended, running it again"),
            Err(e)  => warn!(error = format!("{e:#}"), retry_s = pause.as_secs(), "session failed"),
        }

        // Sleep in small steps so a stop request during the pause is honoured promptly.
        let until = Instant::now() + pause;

        while Instant::now() < until && !live.stopped() {
            thread::sleep(Duration::from_millis(100));
        }
    }

    info!("supervisor stopped");
}

/// Builds the session's configuration from the desk's files. Read every time a session
/// starts, so an edited desk file is picked up by the next one.
fn configure(options: &Options) -> Result<SessionConfig> {
    let paths = &options.paths;
    let desk  = paths.desk();

    let text = std::fs::read_to_string(&desk)
        .with_context(|| format!("reading {}", desk.display()))?;

    let geometry = DesktopGeometry::from_toml(&text)
        .with_context(|| format!("parsing {}", desk.display()))?;

    // Optional files are used when present and loud when absent, as the prototype was.
    let calibration = paths.calibration();

    if !calibration.exists() {
        error!(path = %calibration.display(), "no calibration: gaze-et5-cli calibrate fits one");
    }

    Ok(SessionConfig {
        geometry   : geometry,
        models_dir : paths.models_dir.clone(),
        source     : SourceSpec {
            calibration : calibration.exists().then_some(calibration),
            device_blob : paths.device_blob(),
            offset      : Some(paths.offset()),
        },
        daydream   : DaydreamSpec::Auto,
        click      : !options.dry_run,
        overlay    : if options.debug { OverlayMode::Debug } else { OverlayMode::Pointer },
        seconds    : None,
    })
}
