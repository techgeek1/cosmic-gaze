//! Manual test for the overlay: animates a gaze marker along a figure-eight across the
//! union of every output, with a highlight box following it, then exits cleanly. With
//! `--pointer` it shows the pointer look instead: a gaze hopping between three fake
//! controls, settling on each, so the dot's fade, ghosting, trail and the highlight's
//! crossfade can all be seen in the desktop's own accent colour.
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
use gaze_overlay::{Motion, Overlay, OverlayState, Pointer, Presenter, Target, Theme, render};

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

/// Seconds the pointer look's gaze rests on each fake control.
const HOP_DWELL_S: f64 = 1.4;

/// Seconds the flight between two controls takes.
const HOP_FLIGHT_S: f64 = 0.3;

/// Seconds after landing before the fixation counts as settled, matching the session.
const HOP_SETTLE_S: f64 = 0.15;

/// Size of a fake control.
const CONTROL_W: f64 = 160.0;

/// Height of a fake control.
const CONTROL_H: f64 = 44.0;

/// How close to a control the gaze has to be for it to count as near.
const NEAR_PX: f64 = 260.0;

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

    /// Show the pointer look (dot, trail, themed highlight) hopping between three fake
    /// controls instead of the debug figure-eight.
    #[arg(long)]
    pointer: bool,

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

    let look = if args.pointer { Look::Pointer } else { Look::Debug };

    if let Some(dir) = args.render {
        return render_frames(&dir, args.at, look);
    }

    if args.threaded {
        run_threaded(args.seconds, look)
    }
    else {
        run_inline(args.seconds, look)
    }
}

/// Which animation to show.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Look {
    /// The figure-eight with the ring, the box, the cross and the caption.
    Debug,
    /// The pointer look hopping between controls.
    Pointer,
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
fn render_frames(dir: &Path, at: f64, look: Look) -> anyhow::Result<()> {
    let overlay = Overlay::connect()?;
    let desk    = Desk::of(&overlay)?;

    fs::create_dir_all(dir)?;

    // The pointer look has state: fades and a trail that depend on the frames before
    // `at`. Replaying the animation into a presenter up to that point gives the frame the
    // screen would have shown.
    let (state, presenter) = match look {
        Look::Debug   => (frame_at(&desk, at), None),
        Look::Pointer => {
            let mut presenter = Presenter::new(Theme::cosmic());
            let mut t         = 0.0;

            while t <= at {
                presenter.observe(pointer_at(&desk, t).as_ref(), t);
                presenter.step(t);

                t += STEP.as_secs_f64();
            }

            (OverlayState::default(), Some(presenter))
        }
    };

    for map in overlay.outputs() {
        let pixmap = match &presenter {
            Some(p) => p.render(&state, map),
            None    => render(&state, map),
        };

        let Some(pixmap) = pixmap else {
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
fn run_inline(seconds: f64, look: Look) -> anyhow::Result<()> {
    let mut overlay = Overlay::connect()?;
    let desk        = Desk::of(&overlay)?;

    report(&overlay, desk.bounds);

    let handle = overlay.handle();
    let stop   = AtomicBool::new(false);

    thread::scope(|scope| {
        scope.spawn(|| {
            animate(&desk, seconds, look, |state| handle.set(state).is_ok());
            handle.stop();
        });

        overlay.run_until(&stop);
    });

    Ok(())
}

/// Runs the overlay on its own thread and animates from this one.
fn run_threaded(seconds: f64, look: Look) -> anyhow::Result<()> {
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

    animate(&desk, seconds, look, |state| handle.set(state).is_ok());

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
fn animate(desk: &Desk, seconds: f64, look: Look, mut push: impl FnMut(OverlayState) -> bool) {
    let start = Instant::now();

    loop {
        let t = start.elapsed().as_secs_f64();

        if t >= seconds {
            break;
        }

        let state = match look {
            Look::Debug   => frame_at(desk, t),
            Look::Pointer => OverlayState { pointer: pointer_at(desk, t), ..OverlayState::default() },
        };

        if !push(state) {
            break;
        }

        thread::sleep(STEP);
    }

    // Leave the screen clean on the way out. The pointer look fades, so give it time.
    let _ = push(OverlayState::default());
    thread::sleep(STEP * 30);
}

/// The pointer look's intent at time `t`: the gaze rests on one of three controls laid
/// across the middle of the desktop, then flies to the next with an ease-out, so each
/// hop shows a saccade with a trail, a landing, and the dot thinning once settled.
fn pointer_at(desk: &Desk, t: f64) -> Option<Pointer> {
    let controls = desk.controls();
    let period   = HOP_DWELL_S + HOP_FLIGHT_S;
    let hop      = (t / period).floor() as usize;
    let phase    = t - hop as f64 * period;
    let from     = controls[hop % controls.len()];
    let to       = controls[(hop + 1) % controls.len()];

    let (gaze, motion) = {
        if phase < HOP_DWELL_S {
            let motion = if phase < HOP_SETTLE_S { Motion::Settling } else { Motion::Settled };

            (from.center(), motion)
        }
        else {
            // Ease out: fast off the mark, slowing into the target, like a saccade.
            let u = (phase - HOP_DWELL_S) / HOP_FLIGHT_S;
            let e = 1.0 - (1.0 - u) * (1.0 - u);
            let a = from.center();
            let b = to.center();

            (GlobalPx { x: a.x + (b.x - a.x) * e, y: a.y + (b.y - a.y) * e }, Motion::Moving)
        }
    };

    // Nearest control, and whether it is close enough to be a target or merely near.
    let (nearest, dist) = controls
        .iter()
        .map(|c| {
            let p = c.clamp(gaze);

            (*c, (p.x - gaze.x).hypot(p.y - gaze.y))
        })
        .min_by(|a, b| a.1.total_cmp(&b.1))?;

    Some(Pointer {
        gaze   : gaze,
        motion : motion,
        near   : dist < NEAR_PX,
        target : (dist < CONTROL_H).then(|| Target {
            id   : controls.iter().position(|c| *c == nearest).unwrap_or(0) as u64,
            rect : nearest,
        }),
    })
}

/// The state the animation shows at time `t`.
fn frame_at(desk: &Desk, t: f64) -> OverlayState {
    let gaze = figure_eight(desk, t);

    // The highlight trails a quarter of a second behind, which is roughly what a snap
    // engine with hysteresis will look like and makes both markers easy to tell apart.
    let trail = figure_eight(desk, t - 0.25);

    OverlayState {
        gaze       : Some(gaze),
        highlight  : Some(Rect {
            x : trail.x - BOX_W * 0.5,
            y : trail.y - BOX_H * 0.5,
            w : BOX_W,
            h : BOX_H,
        }),
        truth      : Some(figure_eight(desk, t + 0.25)),
        label      : Some(format!("GAZE {:.0} {:.0}", gaze.x, gaze.y)),
        background : None,
        pointer    : None,
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

    /// Three fake controls in a row across the middle of the largest output, spaced so
    /// a hop between neighbours is a real saccade and the middle one is near both.
    fn controls(&self) -> [Rect; 3] {
        let widest = self
            .outputs
            .iter()
            .copied()
            .max_by(|a, b| a.w.total_cmp(&b.w))
            .unwrap_or(self.bounds);

        let cy = widest.y + widest.h * 0.5 - CONTROL_H * 0.5;

        [0.25, 0.5, 0.75].map(|f| Rect {
            x : widest.x + widest.w * f - CONTROL_W * 0.5,
            y : cy,
            w : CONTROL_W,
            h : CONTROL_H,
        })
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
