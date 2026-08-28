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
use gaze_core::{Element, GlobalPx, Rect};
use gaze_overlay::{Overlay, OverlayState};
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

    /// Show what the recogniser sees under the pointer, once a second, until
    /// interrupted: the chosen box is outlined on screen with its kind and text, and a
    /// cross marks where the pointer was read. Nothing containing the pointer outlines
    /// the nearest box instead, labelled NEAREST.
    Probe {
        /// Directory holding the ONNX models.
        #[arg(long, default_value = "models")]
        models  : PathBuf,

        /// Stop after this many seconds instead of waiting for Ctrl-C.
        #[arg(long)]
        seconds : Option<f64>,

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

/// How often the probe looks, wall clock.
const PROBE_PERIOD: Duration = Duration::from_millis(1000);

/// How long the probe leaves the overlay blank before capturing, so the frame it
/// recognises is the desktop and not its own last outline. Two or three refresh
/// intervals on the slowest panel here.
const PROBE_BLANK: Duration = Duration::from_millis(60);

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
fn probe(models: PathBuf, seconds: Option<f64>, crop_px: f64) -> Result<()> {
    let t0 = Instant::now();

    // The overlay's own marks are on the captured screen, and a stroked box is exactly
    // what the widget model calls a button. Every tick therefore blanks the overlay,
    // waits for the compositor to repaint without it, and only then captures.
    let (overlay, overlay_join) = Overlay::spawn().context("spawning the overlay")?;

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

    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let stop     = Arc::new(AtomicBool::new(false));

    flag::register(SIGINT, Arc::clone(&stop)).context("registering the SIGINT handler")?;

    let mut id = 0u64;

    while deadline.is_none_or(|d| Instant::now() < d) && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(PROBE_PERIOD - PROBE_BLANK);

        overlay.set(OverlayState::default()).context("the overlay thread exited")?;
        std::thread::sleep(PROBE_BLANK);

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

        let (line, shown) = {
            match reply.outcome {
                DetectOutcome::Found { elements, crop_luma, choice } => {
                    // The nearest box when nothing contains the pointer: a systematic
                    // offset between what the recogniser reports and what is on screen
                    // shows up here as every box sitting a fixed distance away.
                    let (hit, shown) = {
                        match smallest_containing(&elements, sample.global) {
                            Some(e) => (describe(e), Some((e.bbox, caption("", e)))),
                            None    => {
                                match nearest(&elements, sample.global) {
                                    Some(e) => (
                                        format!("nothing under the pointer; nearest is {}",
                                                describe(e)),
                                        Some((e.bbox, caption("NEAREST ", e))),
                                    ),
                                    None => ("nothing in the crop".to_string(), None),
                                }
                            }
                        }
                    };

                    (format!("{} boxes, luma {crop_luma:.2}, {choice:?} — {hit}",
                             elements.len()), shown)
                }
                DetectOutcome::Stale     => ("no usable frame".to_string(), None),
                DetectOutcome::OffFrame  => {
                    ("the pointer left the captured output".to_string(), None)
                }
                DetectOutcome::Failed(e) => (format!("recognition failed: {e}"), None),
            }
        };

        println!("{} ({:.0}, {:.0}): {}",
                 sample.output, sample.global.x, sample.global.y, line);

        // The cross is where the pointer was read, so a coordinate error in the cursor
        // path shows as the cross sitting away from the real cursor.
        let (highlight, label): (Option<Rect>, Option<String>) = {
            match shown {
                Some((bbox, label)) => (Some(bbox), Some(label)),
                None                => (None, Some("NOTHING".to_string())),
            }
        };

        overlay.set(OverlayState {
            gaze       : None,
            highlight  : highlight,
            truth      : Some(sample.global),
            label      : label,
            background : None,
        })
        .context("the overlay thread exited")?;
    }

    overlay.stop();
    let _ = overlay_join.join();
    perception.stop();

    Ok(())
}

/// The overlay caption for a box: prefix, kind, size and any text, ASCII only and
/// short enough to read at a glance.
fn caption(prefix: &str, e: &Element) -> String {
    let text = e.text.as_deref()
        .map(|t| t.trim().chars().take(24).collect::<String>())
        .filter(|t| !t.is_empty())
        .map(|t| format!(" \"{t}\""))
        .unwrap_or_default();

    format!("{prefix}{:?} {:.0}x{:.0}{text}", e.kind, e.bbox.w, e.bbox.h)
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
fn nearest(elements: &[Element], p: GlobalPx) -> Option<&Element> {
    elements.iter().min_by(|a, b| {
        let d = |e: &Element| {
            let c = e.bbox.center();

            (c.x - p.x).powi(2) + (c.y - p.y).powi(2)
        };

        d(a).total_cmp(&d(b))
    })
}
