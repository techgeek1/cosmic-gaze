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
use gaze_clicks::cursor::classify as classify_cursor;
use gaze_clicks::element::{Pick, is_accepted, pick};
use gaze_clicks::mouse;
use gaze_clicks::perceive::{DetectOutcome, DetectRequest, Perception, PerceptionConfig};
use gaze_clicks::tree::{TREE_TIMEOUT, TreeService};
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

        /// Half-width of the window the screen luminance is averaged over, logical
        /// pixels. Recognition is pointer-local; this is only the pupil covariate.
        #[arg(long, default_value_t = 256.0)]
        luma_px     : f64,

        /// Do not listen for `gaze-trainer`; its clicks then go through recognition
        /// like any other application's.
        #[arg(long)]
        no_trainer  : bool,
    },

    /// List the evdev nodes a run could read, and mark the one it would pick.
    Devices {
        /// Name substring the default lookup uses.
        #[arg(long, default_value = mouse::DEFAULT_NAME)]
        mouse_name : String,
    },

    /// Show what the recogniser sees under the pointer, continuously, until
    /// interrupted: the chosen box is outlined on screen with its kind, size, score,
    /// text and the round trip's latency, and a cross marks where the pointer was read.
    /// Nothing containing the pointer outlines the nearest accepted box instead.
    Probe {
        /// Directory holding the ONNX models.
        #[arg(long, default_value = "models")]
        models  : PathBuf,

        /// Stop after this many seconds instead of waiting for Ctrl-C.
        #[arg(long)]
        seconds : Option<f64>,

        /// How often to look, hertz.
        #[arg(long, default_value_t = 1.0 / PROBE_PERIOD.as_secs_f64())]
        hz      : f64,

        /// Half-width of the window the screen luminance is averaged over, logical
        /// pixels. Recognition is pointer-local; this is only the pupil covariate.
        #[arg(long, default_value_t = 256.0)]
        luma_px : f64,
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
            no_tracker, capture_hz, luma_px, no_trainer,
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
            luma_px     : luma_px,
            trainer     : !no_trainer,
        }),

        Command::Devices { mouse_name }                 => devices(&mouse_name),
        Command::Probe { models, seconds, hz, luma_px } => probe(models, seconds, hz, luma_px),
    }
}

/// How often the probe looks, wall clock, unless `--hz` says otherwise.
///
/// Pointer-local recognition runs in well under this, so four times a second keeps up and
/// the outline follows the pointer closely enough to read as live rather than as a series
/// of stills. The whole-frame version could not have gone faster than about twice this.
const PROBE_PERIOD: Duration = Duration::from_millis(250);

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
        "accepted {} / drag {} / no-element {} / blank {} / no-gaze {} / stale {} \
         (late-capture {}, overrun {}, off-desk {}, error {})",
        t.accepted, t.drag, t.no_element, t.blank, t.no_gaze, t.stale,
        t.late_capture, t.overrun, t.off_desk, t.error,
    );

    Ok(())
}

/// Lists candidate mice.
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

/// Prints and draws what the recogniser sees under the pointer, continuously.
///
/// Everything here goes through the collector's own path: `Perception` builds its
/// detector from `element::collector_config`, so the score and size gates are the ones a
/// click gets, and the label comes from `element::pick`, so the flat check is too. What
/// the probe outlines is what a click at that instant would have been written against.
///
/// The reported latency is capture-start to overlay-draw, the whole round trip including
/// the queue, not just the detector's own milliseconds.
fn probe(models: PathBuf, seconds: Option<f64>, hz: f64, luma_px: f64) -> Result<()> {
    let t0     = Instant::now();
    let period = Duration::from_secs_f64(1.0 / hz.max(0.05)).max(PROBE_BLANK);

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
            luma_half_px : luma_px,
            t0           : t0,
        },
        press_rx,
    )?;

    let mut tree = TreeService::spawn();
    let deadline = seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let stop     = Arc::new(AtomicBool::new(false));

    flag::register(SIGINT, Arc::clone(&stop)).context("registering the SIGINT handler")?;

    let mut id = 0u64;

    while deadline.is_none_or(|d| Instant::now() < d) && !stop.load(Ordering::Relaxed) {
        std::thread::sleep(period - PROBE_BLANK);

        overlay.set(OverlayState::default()).context("the overlay thread exited")?;
        std::thread::sleep(PROBE_BLANK);

        let Some(sample) = perception.pointer_now() else {
            println!("pointer: not reported on any output yet");

            continue;
        };

        // The latency the caption reports starts here, with the capture request, and
        // ends when the overlay is asked to draw the answer.
        let started = Instant::now();

        press_tx.send(id).context("the perception thread stopped")?;
        tree.ask(id, sample.global);

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

        let reply = perception.replies().recv_timeout(Duration::from_secs(5))
            .context("the recogniser did not answer")?;

        let shape = sample.cursor.map(classify_cursor);
        let asked = tree.take(id, TREE_TIMEOUT);

        id += 1;
        let hit   = asked.as_ref().and_then(|reply| reply.hit.as_ref());
        let tree_note = match &asked {
            Some(reply) => match &reply.hit {
                Some(h) => format!(", tree {:.0} ms: leaf {} {:?}{}", reply.ms, h.leaf.role, h.leaf.name,
                                   h.target.as_ref().map(|t| format!(" → {} {:?}", t.role, t.name)).unwrap_or_default()),
                None    => format!(", tree {:.0} ms: no answer", reply.ms),
            },
            None => ", tree: no reply".to_string(),
        };

        // `body` is the caption without its latency, `note` the extra detail only
        // stdout gets, and `highlight` the box drawn on screen.
        let (body, note, highlight): (String, String, Option<Rect>) = {
            match reply.outcome {
                DetectOutcome::Found { elements, crop_luma, pointer_sd, detect_ms, choice } => {
                    let seen = format!(
                        "{} boxes, luma {crop_luma:.2}, sd {pointer_sd:.3}, \
                         detect {detect_ms:.0} ms, {choice:?}",
                        elements.len(),
                    );

                    match pick(&elements, sample.global, pointer_sd, shape, hit) {
                        Pick::Tree(t)    => {
                            // The overlay font is ASCII; a Japanese label would draw
                            // as a row of question marks, so it is left off.
                            let name = t.name.as_deref()
                                .map(|n| n.chars().filter(|c| c.is_ascii_graphic() || *c == ' ')
                                          .take(24).collect::<String>())
                                .map(|n| n.trim().to_string())
                                .filter(|n| n.len() >= 2)
                                .map(|n| format!(" \"{n}\""))
                                .unwrap_or_default();

                            (
                                format!("TREE {} {:.0}x{:.0}{name}", t.kind.to_uppercase(),
                                        t.bbox.w, t.bbox.h),
                                format!("{seen}{tree_note} — {} at ({:.0}, {:.0}) {:.0}x{:.0}",
                                        t.role, t.bbox.x, t.bbox.y, t.bbox.w, t.bbox.h),
                                Some(t.bbox),
                            )
                        }

                        Pick::Element(e) => (caption(e), format!("{seen}{tree_note} — {}", describe(e)),
                                             Some(e.bbox)),

                        Pick::Caret(b)   => (
                            "CARET".to_string(),
                            format!("{seen} — nothing under the pointer, accepted on the \
                                     I-beam's word"),
                            Some(b),
                        ),

                        Pick::Blank      => (
                            "BLANK".to_string(),
                            format!("{seen} — a box contains the pointer but the pixels \
                                     there are flat"),
                            None,
                        ),

                        // The nearest box when nothing contains the pointer: a systematic
                        // offset between what the recogniser reports and what is on
                        // screen shows up here as every box sitting a fixed distance
                        // away. The caption still says NOTHING, because that is the
                        // verdict; the outline is the diagnosis.
                        Pick::Nothing    => {
                            match nearest_accepted(&elements, sample.global) {
                                Some(e) => (
                                    "NOTHING".to_string(),
                                    format!("{seen} — nothing under the pointer; nearest \
                                             is {}", describe(e)),
                                    Some(e.bbox),
                                ),
                                None    => ("NOTHING".to_string(),
                                            format!("{seen} — nothing on the frame"), None),
                            }
                        }
                    }
                }

                DetectOutcome::Stale     => {
                    ("STALE".to_string(), "no usable frame".to_string(), None)
                }
                DetectOutcome::OffFrame  => {
                    ("OFF-FRAME".to_string(),
                     "the pointer left the captured output".to_string(), None)
                }
                DetectOutcome::Overrun   => {
                    ("OVERRUN".to_string(),
                     "the detector is still busy with earlier frames".to_string(), None)
                }
                DetectOutcome::Failed(e) => {
                    ("FAILED".to_string(), format!("recognition failed: {e}"), None)
                }
            }
        };

        let latency_ms = started.elapsed().as_secs_f64() * 1000.0;

        // The pointer's shape goes on screen only when the application is claiming
        // something the caption may not: a hand or an I-beam.
        let claim  = shape.filter(|s| s.says_something_is_there())
                          .map(|s| format!(" ({})", s.name().to_uppercase()))
                          .unwrap_or_default();
        let label  = format!("{body}{claim} {latency_ms:.0} ms");
        let cursor = match (sample.cursor, shape) {
            (Some(c), Some(s)) => format!("{} {s}", c.describe()),
            _                  => "?".to_string(),
        };

        println!("{} ({:.0}, {:.0}) cursor {cursor}: {label} [{note}]",
                 sample.output, sample.global.x, sample.global.y);

        // The cross is where the pointer was read, so a coordinate error in the cursor
        // path shows as the cross sitting away from the real cursor.
        overlay.set(OverlayState {
            gaze       : None,
            highlight  : highlight,
            truth      : Some(sample.global),
            label      : Some(label),
            background : None,
        })
        .context("the overlay thread exited")?;
    }

    overlay.stop();
    let _ = overlay_join.join();
    perception.stop();

    Ok(())
}

/// The overlay caption for a box: kind, size, score and any text, ASCII only and short
/// enough to read at a glance. The caller appends the latency.
fn caption(e: &Element) -> String {
    let text = e.text.as_deref()
        .map(|t| t.trim().chars().take(24).collect::<String>())
        .filter(|t| !t.is_empty())
        .map(|t| format!(" \"{t}\""))
        .unwrap_or_default();

    format!("{:?} {:.0}x{:.0} s={:.2}{text}", e.kind, e.bbox.w, e.bbox.h, e.score)
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

/// The accepted box whose centre is closest to `p`, for when nothing contains it.
///
/// Filtered by [`is_accepted`] so the outline is a box a click could have been labelled
/// with, rather than an unclassified panel that would never have won anyway.
fn nearest_accepted(elements: &[Element], p: GlobalPx) -> Option<&Element> {
    elements.iter().filter(|e| is_accepted(e.kind)).min_by(|a, b| {
        let d = |e: &Element| {
            let c = e.bbox.center();

            (c.x - p.x).powi(2) + (c.y - p.y).powi(2)
        };

        d(a).total_cmp(&d(b))
    })
}
