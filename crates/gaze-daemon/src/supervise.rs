//! Keeps a session running, and runs the quick calibration between two.
//!
//! The session loop returns when the tracker cannot be opened, when the controller's
//! Home button exits it, or when the overlay thread dies. None of those should end the
//! daemon: the tracker may be unplugged for a minute, the button may have been pressed
//! by accident, the compositor may have restarted. So the daemon runs the session again
//! after a pause, forever, until it is told to stop.
//!
//! A calibrate request (the applet's button, over the bus) also ends the session, on
//! purpose: the ceremony needs the tracker to itself. [`calibrate`] then runs the quick
//! plan on the released device and writes the files the next session reads, so the
//! restart that follows is the same restart as any other, on the new model.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gaze_config::{Mode, Paths, Status};
use gaze_core::DesktopGeometry;
use gaze_overlay::Overlay;
use gaze_proto::config::{DaydreamSpec, OverlayMode, SessionConfig, SourceSpec};
use gaze_proto::{Live, session};
use gaze_provider_et5::calibration::load_tracker_pitch;
use gaze_provider_et5::retrain::{self, Background, CalibrationFiles, RetrainConfig, RetrainKey};
use gaze_provider_et5::{Device, Et5Calibration};
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

/// How often the ceremony's abort watch looks at the stop flag.
const ABORT_POLL: Duration = Duration::from_millis(100);

/// The quick plan's target count, which is also the fewest it may commit: a partial
/// top-up is a partial model, and nothing is written below it.
const QUICK_MIN_POINTS: usize = retrain::QUICK_POINTS.len();

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

        // The session let go of the tracker for this. Straight into the next session
        // afterwards, on the new model or, if the ceremony failed, on the old one.
        if live.take_calibrate() {
            match calibrate(options, live) {
                Ok(())  => info!("calibration done, running the session on the new model"),
                Err(e)  => warn!(error = format!("{e:#}"), "calibration failed, running the session on the previous model"),
            }

            live.set_status(Status::default());

            continue;
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
        },
        daydream   : DaydreamSpec::Auto,
        click      : !options.dry_run,
        overlay    : if options.debug { OverlayMode::Debug } else { OverlayMode::Pointer },
        seconds    : None,
    })
}

// --- Calibration ---

/// Runs the quick ceremony on the released tracker: the current blob seeds the
/// session, five targets on the dimmed desktop top the model up, the health grid
/// measures the result and fits the correction field, the device is reopened to
/// prove it kept the model, and the blob and calibration are written where the next
/// session reads them. `Mode` reads `calibrating` throughout. A stop request aborts
/// it (nothing is written).
///
/// The display is the calibration file's, or the desk's detected output when there is
/// no calibration yet, so a first calibration from the applet is possible.
fn calibrate(options: &Options, live: &Arc<Live>) -> Result<()> {
    let paths = &options.paths;
    let desk  = paths.desk();

    let text = std::fs::read_to_string(&desk)
        .with_context(|| format!("reading {}", desk.display()))?;

    let geometry = DesktopGeometry::from_toml(&text)
        .with_context(|| format!("parsing {}", desk.display()))?;

    let calibration = paths.calibration();
    let blob        = paths.device_blob();

    let display = Et5Calibration::load(&calibration)
        .ok()
        .and_then(|c| c.device_output)
        .or_else(|| {
            geometry.outputs.iter()
                .find(|o| o.enabled && o.detect)
                .map(|o| o.name.clone())
        })
        .context("no display to calibrate: no calibration file and no detected output")?;

    let config = RetrainConfig {
        display           : display,
        tracker_pitch_deg : load_tracker_pitch(&desk),
        min_points        : QUICK_MIN_POINTS,
        health_background : Background::Dim,
        ..RetrainConfig::default()
    };

    let plan = retrain::plan_quick(&geometry, &config).context("planning the ceremony")?;
    let seed = retrain::resolve_seed(None, &blob, false).context("resolving the seed")?;

    info!(
        display = config.display,
        points  = plan.rounds.iter().map(|r| r.points.len()).sum::<usize>(),
        seed    = seed.why,
        "quick calibration starting"
    );

    live.set_status(Status {
        tracker    : true,
        calibrated : seed.seed.is_some(),
        mode       : Mode::Calibrating,
        ..Status::default()
    });

    // Blob-less connect: the ceremony seeds and declares the plane itself.
    let mut device = Device::connect().context("connecting to the ET5")?;

    let (overlay, join) = Overlay::spawn().context("spawning the overlay")?;

    // A stop from the applet or a signal aborts the ceremony through its own key
    // channel, the way `q` does at the terminal.
    let (keys_tx, keys_rx) = crossbeam_channel::unbounded();
    let finished           = Arc::new(AtomicBool::new(false));

    let watch = {
        let live     = Arc::clone(live);
        let finished = Arc::clone(&finished);

        thread::Builder::new()
            .name("calibrate-abort".to_string())
            .spawn(move || {
                while !finished.load(Ordering::Relaxed) {
                    if live.stopped() {
                        let _ = keys_tx.send(RetrainKey::Quit);

                        break;
                    }

                    thread::sleep(ABORT_POLL);
                }
            })
            .context("spawning the abort watch")?
    };

    let outcome = retrain::run_retrain(
        &mut device, &overlay, Some(&keys_rx), &config, &plan, seed.seed.as_ref(),
    );

    let outcome = {
        match outcome {
            Ok(outcome) => outcome,
            Err(e)      => {
                finished.store(true, Ordering::Relaxed);
                let _ = watch.join();

                overlay.stop();
                let _ = join.join();

                return Err(e).context("the ceremony");
            }
        }
    };

    info!(
        accepted = outcome.accepted,
        of       = outcome.results.len(),
        kept     = outcome.kept,
        bytes    = outcome.blob.len(),
        "ceremony committed"
    );

    // The health check reads the committed model back against a grid of known
    // targets: the numbers that say whether the top-up helped, and the rows the
    // correction field is fitted from. A failure here loses those, not the retrain,
    // so it never stops the files being written.
    let health = {
        match retrain::run_health(&mut device, &geometry, &overlay, Some(&keys_rx), &config, &plan) {
            Ok(health) => health,
            Err(e)     => {
                warn!("health check did not finish ({e}); the calibration is written without it");

                Vec::new()
            }
        }
    };

    finished.store(true, Ordering::Relaxed);
    let _ = watch.join();

    overlay.stop();
    let _ = join.join();

    match retrain::health_summary(&health) {
        Some((rms, p50)) => info!(stops = health.len(), rms_deg = format_args!("{rms:.2}"),
                                  p50_deg = format_args!("{p50:.2}"), "health check"),
        None             => warn!("health check: no stops measured"),
    }

    // Nothing is written until the tracker has been closed, reopened, and asked for
    // its model again: a firmware reboot mid-ceremony leaves the factory blob behind.
    device.close();
    drop(device);

    let persistence = retrain::verify_persistence(&outcome.blob, outcome.usb_before)
        .context("the persistence check")?;

    if persistence.re_enumerated() {
        warn!("the tracker re-enumerated during the ceremony; it kept the model anyway");
    }

    let written = retrain::write_calibration(
        CalibrationFiles { blob: &blob, calibration: &calibration },
        &geometry, &config, &plan, &outcome, health,
    ).context("writing the calibration")?;

    match &written.field {
        Some(fit) => info!(degree = ?fit.degree, rows = fit.rows,
                           identity_rms = format_args!("{:.3}", fit.identity_rms),
                           fitted_rms   = format_args!("{:.3}", fit.score),
                           "correction field"),
        None      => info!("correction field: not fitted, too few health stops"),
    }

    info!(
        blob        = %written.report.short(),
        blob_kept   = ?written.blob_kept,
        calibration = %calibration.display(),
        "calibration written"
    );

    Ok(())
}
