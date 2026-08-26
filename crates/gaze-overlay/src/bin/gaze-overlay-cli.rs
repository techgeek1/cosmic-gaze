//! Manual test for the overlay: animates a gaze marker along a figure-eight across the
//! union of every output, with a highlight box following it, then exits cleanly.
//!
//! There is nothing to assert here, the point is to look at the screen. While it runs the
//! desktop underneath must stay fully usable: clicking, dragging and hovering should all
//! behave as if the overlay were not there. If they do not, the input region is broken.

// Matches the workspace style: explicit `Foo { x: x }` keeps the field columns aligned.
#![allow(clippy::redundant_field_names)]

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::thread;
use std::time::{Duration, Instant};

use clap::Parser;
use tracing_subscriber::EnvFilter;

use gaze_core::{GlobalPx, Rect};
use gaze_overlay::{Overlay, OverlayState, render};

/// How often the animation pushes a new state. The overlay paces its own drawing off
/// frame callbacks, so pushing faster than the display refreshes only coalesces.
const STEP: Duration = Duration::from_millis(16);

/// Seconds for one full lap of the figure-eight.
const PERIOD_S: f64 = 6.0;

/// Size of the highlight box that trails the marker, in global logical pixels.
const BOX_W: f64 = 220.0;

/// Height of that box.
const BOX_H: f64 = 90.0;

/// Fraction of the desktop's half-width and half-height the lissajous sweeps over.
const SWEEP: f64 = 0.8;

#[derive(Parser, Debug)]
#[command(about = "animate a gaze marker across every output to test the overlay")]
struct Args {
    /// How long to run before exiting.
    #[arg(long, default_value_t = 10.0)]
    seconds: f64,

    /// Run the overlay on its own thread through `Overlay::spawn` instead of driving it
    /// from this one. Exercises the path the live prototype will use.
    #[arg(long)]
    threaded: bool,

    /// Print the outputs and their logical rectangles, then exit without drawing.
    #[arg(long)]
    list: bool,

    /// Write one animation frame per output into this directory as `<output>.png`
    /// instead of animating. Nothing is put on screen, so this is the way to check what
    /// the overlay would draw over ssh or in a log.
    #[arg(long, value_name = "DIR")]
    render: Option<PathBuf>,

    /// Which point of the animation `--render` captures, in seconds.
    #[arg(long, default_value_t = 1.0)]
    at: f64,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()))
        .init();

    let args = Args::parse();

    if args.list {
        return list_outputs();
    }

    if let Some(dir) = args.render {
        return render_frames(&dir, args.at);
    }

    if args.threaded {
        run_threaded(args.seconds)
    }
    else {
        run_inline(args.seconds)
    }
}

// --- Modes ---

/// Connects, prints what the overlay found, and exits. Useful for checking the logical
/// rectangles against `cosmic-randr list` without putting anything on screen.
fn list_outputs() -> anyhow::Result<()> {
    let overlay = Overlay::connect()?;

    for map in overlay.outputs() {
        let r = map.logical;

        println!("{} {} {} {} {} scale {}", map.name, r.x, r.y, r.w, r.h, map.scale);
    }

    match overlay.desktop_bounds() {
        Some(b) => println!("union: {} {} {} {}", b.x, b.y, b.w, b.h),
        None    => println!("union: no outputs"),
    }

    Ok(())
}

/// Renders the frame the animation would show at `at` seconds, one PNG per output, and
/// puts nothing on screen. The alpha channel is what the compositor would composite, so a
/// mostly transparent image with a ring and a box in it is the expected result.
fn render_frames(dir: &Path, at: f64) -> anyhow::Result<()> {
    let overlay = Overlay::connect()?;
    let desk    = Desk::of(&overlay)?;

    fs::create_dir_all(dir)?;

    let state = frame_at(&desk, at);

    for map in overlay.outputs() {
        let Some(pixmap) = render(&state, map)
        else {
            eprintln!("{}: degenerate buffer size, skipped", map.name);
            continue;
        };

        let path = dir.join(format!("{}.png", map.name));

        pixmap.save_png(&path)?;
        println!("{} -> {} ({}x{})", map.name, path.display(), pixmap.width(), pixmap.height());
    }

    Ok(())
}

/// Drives the overlay from this thread, stepping the animation between dispatches.
///
/// `run_until` blocks, so the animation runs on a helper thread that pushes state through
/// the overlay's handle and sets the stop flag when the time is up.
fn run_inline(seconds: f64) -> anyhow::Result<()> {
    let mut overlay = Overlay::connect()?;
    let desk        = Desk::of(&overlay)?;

    report(&overlay, desk.bounds);

    let handle = overlay.handle();
    let stop   = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            animate(&desk, seconds, |state| handle.set(state).is_ok());
            handle.stop();
        });

        overlay.run_until(&stop);
    });

    Ok(())
}

/// Runs the overlay on its own thread and animates from this one.
fn run_threaded(seconds: f64) -> anyhow::Result<()> {
    // The overlay thread owns its connection and nothing exposes the geometry over the
    // handle, so ask a short lived probe connection where the outputs are first. It
    // attaches no buffers, so nothing appears on screen while it lives.
    let desk = {
        let probe = Overlay::connect()?;

        Desk::of(&probe)?
    };

    let (handle, join) = Overlay::spawn()?;
    let b              = desk.bounds;

    println!("desktop union: {} {} {} {}", b.x, b.y, b.w, b.h);

    animate(&desk, seconds, |state| handle.set(state).is_ok());

    handle.stop();
    let _ = join.join();

    Ok(())
}

// --- Animation ---

/// Prints what the overlay is covering, so a run that draws nothing is easy to diagnose.
fn report(overlay: &Overlay, bounds: Rect) {
    let names: Vec<&str> = overlay.outputs().map(|m| m.name.as_str()).collect();

    println!("overlay on {} output(s): {}", names.len(), names.join(", "));
    println!("desktop union: {} {} {} {}", bounds.x, bounds.y, bounds.w, bounds.h);
}

/// Walks a lissajous figure-eight over `bounds` for `seconds`, calling `push` with each
/// new state. Stops early if `push` returns false, which means the overlay has gone away.
fn animate(desk: &Desk, seconds: f64, mut push: impl FnMut(OverlayState) -> bool) {
    let start = Instant::now();

    loop {
        let t = start.elapsed().as_secs_f64();

        if t >= seconds {
            break;
        }

        if !push(frame_at(desk, t)) {
            break;
        }

        thread::sleep(STEP);
    }

    // Leave the screen clean on the way out.
    let _ = push(OverlayState::default());
    thread::sleep(STEP * 4);
}

/// The state the animation shows at time `t`.
fn frame_at(desk: &Desk, t: f64) -> OverlayState {
    let gaze = figure_eight(desk, t);

    // The highlight trails a quarter of a second behind, which is roughly what a snap
    // engine with hysteresis will look like and makes both markers easy to tell apart.
    let trail = figure_eight(desk, t - 0.25);

    OverlayState {
        gaze      : Some(gaze),
        highlight : Some(Rect {
            x : trail.x - BOX_W * 0.5,
            y : trail.y - BOX_H * 0.5,
            w : BOX_W,
            h : BOX_H,
        }),
        truth     : Some(figure_eight(desk, t + 0.25)),
        label     : Some(format!("GAZE {:.0} {:.0}", gaze.x, gaze.y)),
    }
}

/// Position on the figure-eight at time `t`. A 1:2 lissajous over the union of the
/// outputs, which crosses every panel of a layout as wide as this desk and spends time
/// near the seams.
///
/// The union of three panels in an L is not a rectangle, so the raw curve spends part of
/// each lap in dead space where no output exists and the marker would simply vanish. The
/// point is clamped onto the nearest output instead, which is also what the synthetic
/// provider does with its virtual gaze point.
fn figure_eight(desk: &Desk, t: f64) -> GlobalPx {
    let phase  = t / PERIOD_S * std::f64::consts::TAU;
    let bounds = desk.bounds;

    let cx = bounds.x + bounds.w * 0.5;
    let cy = bounds.y + bounds.h * 0.5;

    let p = GlobalPx {
        x : cx + bounds.w * 0.5 * SWEEP * phase.sin(),
        y : cy + bounds.h * 0.5 * SWEEP * (2.0 * phase).sin(),
    };

    desk.clamp(p)
}

// --- Desk ---

/// The output rectangles the animation is allowed to walk over, plus their union.
struct Desk {
    outputs : Vec<Rect>,
    bounds  : Rect,
}

impl Desk {
    /// Reads the layout out of a connected overlay. Fails when the compositor reported no
    /// outputs, since then there is nothing to animate across.
    fn of(overlay: &Overlay) -> anyhow::Result<Desk> {
        let outputs: Vec<Rect> = overlay.outputs().map(|m| m.logical).collect();

        let Some(bounds) = overlay.desktop_bounds()
        else {
            anyhow::bail!("the compositor reported no outputs, so there is nothing to draw on");
        };

        Ok(Desk { outputs: outputs, bounds: bounds })
    }

    /// Moves a point onto the nearest output if it is not already on one.
    fn clamp(&self, p: GlobalPx) -> GlobalPx {
        if self.outputs.iter().any(|r| r.contains(p)) {
            return p;
        }

        let mut best      = p;
        let mut best_dist = f64::INFINITY;

        for r in &self.outputs {
            let c  = r.clamp(p);
            let dx = c.x - p.x;
            let dy = c.y - p.y;
            let d  = dx * dx + dy * dy;

            if d < best_dist {
                best_dist = d;
                best      = c;
            }
        }

        best
    }
}
