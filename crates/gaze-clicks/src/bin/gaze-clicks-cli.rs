//! Manual test CLI for `gaze-clicks`: which mouse nodes a session would read, and the
//! presses they deliver.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gaze_clicks::mouse::{self, MouseReader};
use signal_hook::consts::SIGINT;
use signal_hook::flag;
use tracing_subscriber::EnvFilter;

/// The real mouse, read-only.
#[derive(Parser, Debug)]
#[command(name = "gaze-clicks-cli", version)]
struct Cli {
    #[command(subcommand)]
    command : Command,

    /// Log at debug level as well as printing the press lines.
    #[arg(long, global = true)]
    verbose : bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// List the evdev nodes a session could read, and mark the ones it would.
    Devices {
        /// Name substring the default lookup uses.
        #[arg(long, default_value = mouse::DEFAULT_NAME)]
        mouse_name : String,
    },

    /// Print every press and release the reader sees until interrupted, the way the
    /// session sees them: read-only, every mouse-shaped node at once.
    Presses {
        /// Mouse node to read, overriding the name lookup.
        #[arg(long)]
        mouse      : Option<PathBuf>,

        /// Name substring the mouse is found by.
        #[arg(long, default_value = mouse::DEFAULT_NAME)]
        mouse_name : String,

        /// Stop after this many seconds instead of waiting for Ctrl-C.
        #[arg(long)]
        seconds    : Option<f64>,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    let default = if cli.verbose { "debug" } else { "info" };

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(default)))
        .with_target(false)
        .init();

    match cli.command {
        Command::Devices { mouse_name }                   => devices(&mouse_name),
        Command::Presses { mouse, mouse_name, seconds }   => {
            presses(mouse.as_deref(), &mouse_name, seconds)
        }
    }
}

// --- Commands ---

fn devices(mouse_name: &str) -> Result<()> {
    let all = mouse::candidates();

    if all.is_empty() {
        println!("no readable evdev nodes; the user needs an ACL on the mouse node");

        return Ok(());
    }

    println!("{:<22} {:<8} {:<8} name", "node", "buttons", "keyboard");

    let mut read = 0usize;

    for candidate in &all {
        let mark = {
            if mouse::wanted(candidate, mouse_name) {
                read += 1;

                " <- read"
            }
            else {
                ""
            }
        };

        println!(
            "{:<22} {:<8} {:<8} {}{}",
            candidate.path.display(),
            candidate.buttons,
            candidate.keyboard,
            candidate.name,
            mark,
        );
    }

    // Every marked node is read at once: a grabbed node (a remapper's source) is
    // silent to other readers, so exactly one of them speaks per physical press.
    match read {
        0 => println!("\nnothing to read; pass --mouse PATH"),
        n => println!("\nreading all {n} marked nodes; a rescan picks up ones that \
                       appear later"),
    }

    Ok(())
}

/// Prints presses and releases as the reader delivers them.
fn presses(path: Option<&std::path::Path>, mouse_name: &str, seconds: Option<f64>) -> Result<()> {
    let t0 = Instant::now();

    // The reader fires a capture id per press for a collector that no longer exists;
    // the ids are read and dropped here.
    let (capture_tx, capture_rx) = crossbeam_channel::unbounded();

    let mut reader = MouseReader::open(path, mouse_name, t0, capture_tx)
        .context("opening the mouse")?;

    for (node, name) in reader.nodes() {
        println!("reading {} ({name})", node.display());
    }

    let stop = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&stop)).context("registering SIGINT")?;

    let deadline = seconds.map(|s| t0 + Duration::from_secs_f64(s));

    while !stop.load(Ordering::Relaxed) && deadline.is_none_or(|d| Instant::now() < d) {
        while capture_rx.try_recv().is_ok() {}

        match reader.events().recv_timeout(Duration::from_millis(100)) {
            Ok(event) => println!(
                "{:>8.3}s {:?} {}",
                event.t_s,
                event.button,
                if event.pressed { "press" } else { "release" },
            ),
            Err(crossbeam_channel::RecvTimeoutError::Timeout)      => {}
            Err(crossbeam_channel::RecvTimeoutError::Disconnected) => break,
        }
    }

    reader.stop();

    Ok(())
}
