//! Checks the collector's recognition path against a reference full-frame detection.
//!
//! This is the measurement that decided the collector recognises whole frames rather
//! than crops. It drives the real [`DetectRequest`] path at the centre of every widget
//! a reference run found, and reports whether the collector's smallest-containing pick
//! is the same kind the reference says is there. With whole-frame recognition it should
//! agree on everything; a systematic disagreement means the collector's path has
//! diverged from what `gaze-detect-cli` sees.
//!
//! # Producing the reference
//!
//! ```sh
//! cargo run --bin gaze-capture-cli -- --out shots
//! cargo run --bin gaze-detect-cli -- shots/DP-2-0.png --json shots/DP-2.json
//! ```
//!
//! **Without `--origin`**: `gaze-detect-cli` writes whatever origin it is given into
//! the boxes themselves, so passing one there and again here would offset every check
//! point by the output's position. The default origin leaves the boxes output-local,
//! and this example adds the output's logical top-left, which `gaze-capture-cli`'s
//! listing prints (`DP-2 ... logical 2560x1440 at (0,160)`).
//!
//! # Running it
//!
//! ```sh
//! cargo run --release --example recognition_check -- shots/DP-2.json DP-2 0 160
//! ```
//!
//! Run it immediately after the reference capture. Nothing is clicked and nothing is
//! written: it only captures and recognises. But every check takes a *fresh* capture,
//! so anything that changed on screen since the reference (a chat that scrolled, a
//! tooltip, a clock) disagrees honestly and is not a fault in the collector. Judge the
//! misses, not just the count: a miss on message text is drift, a miss on a list row
//! or on window chrome is real.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use gaze_clicks::element::smallest_containing;
use gaze_clicks::perceive::{DetectOutcome, DetectRequest, Perception, PerceptionConfig};
use gaze_core::{Element, GlobalPx};

/// How long to wait for one recognition before giving up on a widget.
const REPLY_TIMEOUT: Duration = Duration::from_secs(10);

/// Pause between checks, so the capture thread's rolling pass and the compositor both
/// get a breath. Not required for correctness.
const SETTLE: Duration = Duration::from_millis(150);

/// Widgets to check unless the caller asks for fewer.
const DEFAULT_MAX: usize = 30;

/// One widget from the reference detection.
struct Reference {
    /// The kind `gaze-detect-cli` reported, as its `Debug` spelling (`Button`, `Input`).
    kind : String,
    /// Its box in global logical pixels.
    x    : f64,
    y    : f64,
    w    : f64,
    h    : f64,
}

fn main() -> Result<()> {
    let args: Vec<String> = std::env::args().collect();

    if args.len() < 5 {
        bail!("usage: recognition_check <full-frame-json> <output> <origin-x> <origin-y> [max]");
    }

    let output = args[2].clone();
    let ox: f64 = args[3].parse().context("origin-x")?;
    let oy: f64 = args[4].parse().context("origin-y")?;
    let max     = args.get(5).and_then(|s| s.parse().ok()).unwrap_or(DEFAULT_MAX);

    let widgets = load(&args[1], ox, oy)?;

    if widgets.is_empty() {
        bail!("{}: no non-text widgets to check", args[1]);
    }

    println!("{} widgets from the reference, checking up to {max}", widgets.len());

    let t0                   = Instant::now();
    let (press_tx, press_rx) = crossbeam_channel::unbounded();

    let mut perception = Perception::spawn(
        PerceptionConfig {
            models_dir   : PathBuf::from("models"),
            capture_hz   : 1.0,
            luma_half_px : 256.0,
            t0           : t0,
        },
        press_rx,
    )?;

    let mut agreed = 0usize;
    let mut tried  = 0usize;

    for (id, want) in widgets.iter().take(max).enumerate() {
        let id = id as u64;
        let p  = GlobalPx { x: want.x + want.w / 2.0, y: want.y + want.h / 2.0 };

        // Exactly what a press does: fire the capture, then ask for the frame under it
        // to be recognised. The release is claimed a second out, which is past the
        // press-capture deadline and so exercises the same frame choice a click makes.
        press_tx.send(id).context("the capture thread stopped")?;

        let now = t0.elapsed().as_secs_f64();

        perception.requests().send(DetectRequest {
            id         : id,
            capture_id : Some(id),
            output     : output.clone(),
            px         : p,
            t_press    : now,
            t_release  : now + 1.0,
        })
        .context("the capture thread stopped")?;

        let reply = perception.replies().recv_timeout(REPLY_TIMEOUT)
            .context("the recogniser did not answer")?;

        let off_frame = matches!(reply.outcome, DetectOutcome::OffFrame);

        let (got, same) = {
            match reply.outcome {
                DetectOutcome::Found { elements, .. } => report(&elements, p, &want.kind),
                other                                 => (format!("{other:?}"), false),
            }
        };

        // The one mistake this tool invites: a reference produced with `--origin` is
        // already global, and adding the origin again pushes every point off the
        // bottom of the panel.
        if off_frame && tried == 0 {
            println!("     (the very first point is off the output: was the reference \
                      produced with --origin already applied?)");
        }

        tried  += 1;
        agreed += usize::from(same);

        println!(
            "{} {:<8} {:>4.0}x{:<4.0} at ({:.0}, {:.0}) -> {got}",
            if same { "ok  " } else { "MISS" },
            want.kind, want.w, want.h, p.x, p.y,
        );

        std::thread::sleep(SETTLE);
    }

    perception.stop();

    println!("\n{agreed}/{tried} agree with the reference");

    Ok(())
}

/// The reference widgets, in global logical pixels, largest first.
///
/// Text boxes are skipped: they are what a bad recogniser falls back to, so including
/// them in the targets would flatter the result. Largest first because the wide flat
/// widgets are the ones a crop used to lose, and they should be checked first.
fn load(path: &str, ox: f64, oy: f64) -> Result<Vec<Reference>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {path}"))?;

    let value: serde_json::Value = serde_json::from_str(&text)
        .with_context(|| format!("parsing {path}"))?;

    // `gaze-detect-cli --json` writes a bare array; tolerate an object wrapping one.
    let array = value.as_array().cloned()
        .or_else(|| value.get("elements").and_then(|e| e.as_array().cloned()))
        .with_context(|| format!("{path}: expected an array of elements"))?;

    let mut widgets: Vec<Reference> = array.iter()
        .filter(|e| e["kind"] != "Text")
        .filter_map(|e| {
            let bbox = e.get("bbox")?;

            Some(Reference {
                kind : e["kind"].as_str()?.to_string(),
                x    : ox + bbox["x"].as_f64()?,
                y    : oy + bbox["y"].as_f64()?,
                w    : bbox["w"].as_f64()?,
                h    : bbox["h"].as_f64()?,
            })
        })
        .collect();

    widgets.sort_by(|a, b| (b.w * b.h).total_cmp(&(a.w * a.h)));

    Ok(widgets)
}

/// What the collector picked at `p`, and whether it matches the reference kind.
fn report(elements: &[Element], p: GlobalPx, want: &str) -> (String, bool) {
    let containing: Vec<String> = elements.iter()
        .filter(|e| e.bbox.contains(p))
        .map(|e| format!("{:?} {:.0}x{:.0}", e.kind, e.bbox.w, e.bbox.h))
        .collect();

    let Some(pick) = smallest_containing(elements, p) else {
        return (format!("pick none    containing [{}]", containing.join(", ")), false);
    };

    let name = format!("{:?}", pick.kind);
    let same = name == want;

    (
        format!("pick {name:<8} {:.0}x{:.0}, containing [{}]",
                pick.bbox.w, pick.bbox.h, containing.join(", ")),
        same,
    )
}
