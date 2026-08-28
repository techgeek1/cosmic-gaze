//! Command line for the passive click collector.
//!
//! `run` is the daily driver: leave it running and it fills a session file. `devices`
//! and `probe` are the two things to check before leaving it running, namely that it is
//! reading the right mouse and that the recogniser sees what the user sees.
//!
//! `run` opens the tracker and holds it for the whole session, so `gaze-proto`,
//! `gaze-et5-cli view` and `record` cannot run at the same time. `--no-tracker` runs
//! everything except the device, which is how the click and recognition rules are
//! exercised without hardware.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gaze_clicks::collect::{self, CollectConfig};
use gaze_clicks::element::smallest_containing;
use gaze_clicks::mouse;
use gaze_clicks::perceive::{DetectOutcome, DetectRequest, Perception, PerceptionConfig};
use gaze_core::{Element, GlobalPx};
use signal_hook::consts::SIGINT;
use signal_hook::flag;
use tracing_subscriber::EnvFilter;

/// Passive gaze labels from ordinary mouse clicks.
#[derive(Parser, Debug)]
#[command(name = "gaze-clicks-cli", version)]
struct Cli {
    #[command(subcommand)]
    command : Command,

    /// Log at debug level as well as printing the click and status lines.
    #[arg(long, global = true)]
    verbose : bool,
}

#[derive(Subcommand, Debug)]
enum Command {
    /// Collect clicks until interrupted. Holds the tracker for the whole run.
    Run {
        /// Session file, overriding the generated `config/sessions/<id>.jsonl`.
        #[arg(long)]
        out         : Option<PathBuf>,

        /// Directory sessions are written into.
        #[arg(long, default_value = "config/sessions")]
        out_dir     : PathBuf,

        /// Mouse node to read, overriding the name lookup.
        #[arg(long)]
        mouse       : Option<PathBuf>,

        /// Name substring the mouse is found by.
        #[arg(long, default_value = mouse::DEFAULT_NAME)]
        mouse_name  : String,

        /// Directory holding the ONNX models.
        #[arg(long, default_value = "models")]
        models      : PathBuf,

        /// Desk geometry.
        #[arg(long, default_value = "config/desk.toml")]
        desk        : PathBuf,

        /// Client-side calibration, whose trained plane names the tracker's display.
        #[arg(long, default_value = "config/calibration-et5.toml")]
        calibration : PathBuf,

        /// Host-owned device blob, uploaded to the tracker on every connect.
        #[arg(long, default_value = "config/calibration-et5.bin")]
        blob        : PathBuf,

        /// Run everything except the tracker: clicks, recognition and tallies, with
        /// click records but no gaze frames or stop windows.
        #[arg(long)]
        no_tracker  : bool,

        /// Rolling fallback capture rate, hertz. Only used when a press capture
        /// stalls past 150 ms.
        #[arg(long, default_value_t = 1.0)]
        capture_hz  : f64,

        /// Half-width of the recognition crop, logical pixels.
        #[arg(long, default_value_t = 256.0)]
        crop_px     : f64,
    },

    /// List the evdev nodes a run could read, and mark the one it would pick.
    Devices {
        /// Name substring the default lookup uses.
        #[arg(long, default_value = mouse::DEFAULT_NAME)]
        mouse_name : String,
    },

    /// Print the pointer, its output and the element under it, once a second.
    Probe {
        /// Directory holding the ONNX models.
        #[arg(long, default_value = "models")]
        models  : PathBuf,

        /// How long to run for, seconds.
        #[arg(long, default_value_t = 10.0)]
        seconds : f64,

        /// Half-width of the recognition crop, logical pixels.
        #[arg(long, default_value_t = 256.0)]
        crop_px : f64,
    },
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    // onnxruntime logs a page of graph-transformer chatter per session at info level,
    // which would bury the click lines this tool exists to print. RUST_LOG still wins.
    let default = {
        if cli.verbose {
            "debug,ort=warn"
        }
        else {
            "info,ort=warn"
        }
    };

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(default)))
        .with_target(false)
        .init();

    match cli.command {
        Command::Run {
            out, out_dir, mouse, mouse_name, models, desk, calibration, blob,
            no_tracker, capture_hz, crop_px,
        } => run(CollectConfig {
            out         : out,
            out_dir     : out_dir,
            mouse       : mouse,
            mouse_name  : mouse_name,
            models_dir  : models,
            desk        : desk,
            calibration : calibration,
            blob        : blob,
            no_tracker  : no_tracker,
            capture_hz  : capture_hz,
            crop_px     : crop_px,
        }),

        Command::Devices { mouse_name }           => devices(&mouse_name),
        Command::Probe { models, seconds, crop_px } => probe(models, seconds, crop_px),
    }
}

// --- Commands ---

/// Collects until SIGINT, then writes the end line.
fn run(config: CollectConfig) -> Result<()> {
    let stop = Arc::new(AtomicBool::new(false));

    // A collector left running all day is stopped with Ctrl-C, and a session that
    // loses its end line loses its "did the firmware mutate its own model" check.
    flag::register(SIGINT, Arc::clone(&stop))
        .context("registering the SIGINT handler")?;

    if config.no_tracker {
        println!("--no-tracker: clicks and recognition only, no gaze frames");
    }

    println!("collecting; Ctrl-C to stop");

    let outcome = collect::run(&config, stop)?;
    let t       = outcome.tallies;

    println!(
        "\n{} clicks written to {} ({} session file{})",
        outcome.clicks,
        outcome.path.display(),
        outcome.files,
        if outcome.files == 1 { "" } else { "s" },
    );
    println!(
        "accepted {} / drag {} / no-element {} / no-gaze {} / stale {} \
         (late-capture {}, off-desk {}, error {})",
        t.accepted, t.drag, t.no_element, t.no_gaze, t.stale,
        t.late_capture, t.off_desk, t.error,
    );

    Ok(())
}

/// Lists candidate mice.
fn devices(mouse_name: &str) -> Result<()> {
    let all    = mouse::candidates();
    let chosen = mouse::choose(mouse_name).ok();

    if all.is_empty() {
        println!("no readable evdev nodes; the user needs an ACL on the mouse node");

        return Ok(());
    }

    println!("{:<22} {:<8} {:<8} name", "node", "buttons", "keyboard");

    for candidate in &all {
        let mark = {
            if chosen.as_deref() == Some(candidate.path.as_path()) {
                " <- default"
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

    match chosen {
        Some(path) => println!("\ndefault: {}", path.display()),
        None       => println!("\nno default; pass --mouse PATH"),
    }

    Ok(())
}

/// Prints what the recogniser sees under the pointer, once a second.
fn probe(models: PathBuf, seconds: f64, crop_px: f64) -> Result<()> {
    let t0 = Instant::now();

    // The probe fires its own captures down the channel the mouse reader would use,
    // so it exercises exactly the path a click takes.
    let (press_tx, press_rx) = crossbeam_channel::unbounded();

    let mut perception = Perception::spawn(
        PerceptionConfig {
            models_dir   : models,
            capture_hz   : 1.0,
            crop_half_px : crop_px,
            t0           : t0,
        },
        press_rx,
    )?;

    let deadline = Instant::now() + Duration::from_secs_f64(seconds);
    let stop     = Arc::new(AtomicBool::new(false));

    flag::register(SIGINT, Arc::clone(&stop)).context("registering the SIGINT handler")?;

    let mut id = 0u64;

    while Instant::now() < deadline && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(Duration::from_secs(1));

        let Some(sample) = perception.pointer_now() else {
            println!("pointer: not reported on any output yet");

            continue;
        };

        press_tx.send(id).context("the perception thread stopped")?;

        let now = t0.elapsed().as_secs_f64();

        perception.requests().send(DetectRequest {
            id         : id,
            capture_id : Some(id),
            output     : sample.output.clone(),
            px         : sample.global,
            // A probe has no release; claiming one just past the press deadline keeps
            // the same rule that a click uses.
            t_press    : now,
            t_release  : now + 1.0,
        })
        .context("the perception thread stopped")?;

        id += 1;

        let reply = perception.replies().recv_timeout(Duration::from_secs(5))
            .context("the recogniser did not answer")?;

        let line = {
            match reply.outcome {
                DetectOutcome::Found { elements, crop_luma, choice } => {
                    let hit = {
                        match smallest_containing(&elements, sample.global) {
                            Some(e) => describe(e),
                            // The nearest box instead: a systematic offset between
                            // what the recogniser reports and what is on screen shows
                            // up here as every box sitting a fixed distance away.
                            None    => nearest(&elements, sample.global),
                        }
                    };

                    format!("{} boxes, luma {crop_luma:.2}, {choice:?} — {hit}",
                            elements.len())
                }
                DetectOutcome::Stale     => "no usable frame".to_string(),
                DetectOutcome::OffFrame  => "the pointer left the captured output".to_string(),
                DetectOutcome::Failed(e) => format!("recognition failed: {e}"),
            }
        };

        println!("{} ({:.0}, {:.0}): {}",
                 sample.output, sample.global.x, sample.global.y, line);
    }

    perception.stop();

    Ok(())
}

// --- Reporting ---

/// One element as the probe prints it.
fn describe(e: &Element) -> String {
    format!(
        "{:?} at ({:.0}, {:.0}) {:.0}x{:.0} score {:.2}{}",
        e.kind, e.bbox.x, e.bbox.y, e.bbox.w, e.bbox.h, e.score,
        e.text.as_deref().map(|t| format!(" \"{}\"", t.trim())).unwrap_or_default(),
    )
}

/// The box whose centre is closest to `p`, for when nothing contains it.
fn nearest(elements: &[Element], p: GlobalPx) -> String {
    let closest = elements.iter().min_by(|a, b| {
        let d = |e: &Element| {
            let c = e.bbox.center();

            (c.x - p.x).powi(2) + (c.y - p.y).powi(2)
        };

        d(a).total_cmp(&d(b))
    });

    match closest {
        Some(e) => format!("nothing under the pointer; nearest is {}", describe(e)),
        None    => "nothing in the crop".to_string(),
    }
}
