//! gazed: the gaze session as a daemon.
//!
//! Owns the tracker, the overlay and the injector for the life of the desktop session.
//! Reads the desk's files from their XDG locations (`gaze_config::Paths`), the tuning
//! from cosmic-config (watched, applied at the next sample), serves the control
//! interface on the session bus (`service`), and keeps the session running whatever
//! happens to the tracker (`supervise`). Nothing is configurable beyond the tuning and
//! the two flags below; see PLAN-UX.md U2 for what was cut and why.

mod service;
mod supervise;

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use gaze_config::{Paths, Tuning, TuningStore};
use gaze_proto::Live;
use signal_hook::consts::{SIGINT, SIGTERM};
use tracing::{info, warn};
use tracing_subscriber::EnvFilter;

use crate::service::Service;
use crate::supervise::Options;

/// How often the tuning watch flag is polled. A slider drag writes many keys a second;
/// each poll folds whatever landed into one reload.
const TUNING_POLL: Duration = Duration::from_millis(100);

/// The daemon's command line. Everything else is the tuning and the desk's files.
#[derive(Debug, Parser)]
#[command(name = "gazed", version)]
struct Args {
    /// Read the desk's files from a checkout (`DIR/config`, `DIR/models`) instead of
    /// the XDG locations.
    #[arg(long)]
    home : Option<PathBuf>,

    /// Log clicks, warps and scrolls instead of injecting them: nothing reaches the
    /// real pointer.
    #[arg(long)]
    dry_run : bool,

    /// Draw the debug look (gaze ring, raw boxes, caption) instead of the pointer look.
    #[arg(long)]
    overlay_debug : bool,
}

fn main() -> Result<()> {
    // Info by default; RUST_LOG overrides it.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args  = Args::parse();
    let paths = match &args.home {
        Some(dir) => Paths::home(dir),
        None      => Paths::xdg(),
    };

    info!(
        config = %paths.config_dir.display(),
        models = %paths.models_dir.display(),
        state  = %paths.state_dir.display(),
        "gazed starting",
    );

    // --- tuning ---

    // A first run writes the defaults out so there is a file to edit and the applet's
    // sliders have keys to read.
    let store = match TuningStore::open() {
        Ok(store) => Some(store),
        Err(e)    => {
            warn!("tuning config unavailable, running on defaults: {e}");

            None
        }
    };

    let tuning = match &store {
        Some(store) => {
            let tuning = store.load();

            if let Err(e) = store.save(&tuning) {
                warn!("could not write the tuning back: {e}");
            }

            tuning
        }

        None => Tuning::default(),
    };

    let live = Live::new(tuning);

    // --- control interface ---

    // Kept alive for the life of the process: dropping the connection drops the name.
    let _conn = Service::serve(Arc::clone(&live)).context("serving the control interface")?;

    // --- signals ---

    let term = Arc::new(AtomicBool::new(false));

    for signal in [SIGINT, SIGTERM] {
        signal_hook::flag::register(signal, Arc::clone(&term))
            .with_context(|| format!("registering signal {signal}"))?;
    }

    // --- session ---

    let options = Options {
        paths   : paths,
        dry_run : args.dry_run,
        debug   : args.overlay_debug,
    };

    let session = thread::Builder::new()
        .name("gaze-session".to_string())
        .spawn({
            let live = Arc::clone(&live);

            move || supervise::run(&options, &live)
        })
        .context("spawning the session thread")?;

    // The main thread has one job left: turn a tuning write into a reload, and a
    // signal into a stop.
    while !session.is_finished() {
        if term.load(Ordering::Relaxed) {
            info!("signal received, stopping");
            live.stop();

            break;
        }

        // The write-back above trips the watcher too; an unchanged reload is skipped so
        // the session does not rebuild its filters for nothing.
        if let Some(store) = &store
            && store.take_changed()
        {
            let tuning = store.load();

            if tuning != live.tuning() {
                live.set_tuning(tuning);
            }
        }

        thread::sleep(TUNING_POLL);
    }

    let _ = session.join();

    info!("gazed stopped");

    Ok(())
}
