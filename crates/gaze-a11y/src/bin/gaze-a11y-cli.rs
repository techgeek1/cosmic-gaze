//! Manual check of the accessibility path: which applications are on the bus, and what
//! they say is under a point.
//!
//! ```text
//! gaze-a11y-cli apps                 # applications and their windows on the AT-SPI bus
//! gaze-a11y-cli at 4919,698          # the node under a desk point
//! gaze-a11y-cli follow --seconds 30  # the node under the pointer, whenever it moves
//! ```
//!
//! `follow` is the one to run against the probe: put the pointer on a YouTube card, an
//! input field or an avatar and read what the tree calls it.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gaze_a11y::{A11y, Hit};
use gaze_capture::{CursorTracker, ToplevelTracker};
use gaze_core::GlobalPx;

/// Command line for `gaze-a11y-cli`.
#[derive(Parser)]
#[command(about = "what the accessibility tree says is under a point on the desk")]
struct Args {
    #[command(subcommand)]
    command: Command,
}

/// The subcommands.
#[derive(Subcommand)]
enum Command {
    /// List the applications on the AT-SPI bus and their windows.
    Apps,

    /// Report the node under a desk point, given as `X,Y` in global logical pixels.
    At {
        #[arg(value_parser = parse_point)]
        point: GlobalPx,
    },

    /// Report the node under the pointer whenever it moves.
    Follow {
        /// Stop after this many seconds instead of waiting for Ctrl-C.
        #[arg(long)]
        seconds: Option<f64>,

        /// Polls per second.
        #[arg(long, default_value_t = 5.0)]
        hz: f64,
    },
}

fn main() -> Result<()> {
    let args = Args::parse();

    match args.command {
        Command::Apps               => apps(),
        Command::At { point }       => at(point),
        Command::Follow { seconds, hz } => follow(seconds, hz),
    }
}

/// Lists applications and their frames.
fn apps() -> Result<()> {
    let a11y = A11y::connect().context("connecting to the accessibility bus")?;

    for app in a11y.applications().context("listing applications")? {
        println!("{:<8} {}", app.bus, app.name);

        for (i, window) in app.windows.iter().enumerate() {
            println!("  [{i}] {:?}", window);
        }
    }

    Ok(())
}

/// Reports the node under one point.
fn at(point: GlobalPx) -> Result<()> {
    let mut windows = ToplevelTracker::connect().context("opening the toplevel list")?;
    let mut a11y    = A11y::connect().context("connecting to the accessibility bus")?;

    windows.pump().context("reading the toplevel list")?;

    let Some(window) = windows.at(point) else {
        println!("({:.0}, {:.0}): no window", point.x, point.y);

        return Ok(());
    };

    let started = Instant::now();
    let hit     = a11y.at(point, &window).context("asking the tree")?;

    println!("({:.0}, {:.0}) in {:?} [{}]: {}  ({:.1} ms)",
             point.x, point.y, window.title, window.app_id, describe(hit.as_ref()),
             started.elapsed().as_secs_f64() * 1000.0);

    Ok(())
}

/// Reports the node under the pointer on every move.
fn follow(seconds: Option<f64>, hz: f64) -> Result<()> {
    let mut windows = ToplevelTracker::connect().context("opening the toplevel list")?;
    let mut cursor  = CursorTracker::connect().context("opening cursor sessions")?;
    let mut a11y    = A11y::connect().context("connecting to the accessibility bus")?;
    let start       = Instant::now();
    let period      = Duration::from_secs_f64(1.0 / hz.max(0.1));
    let mut last: Option<GlobalPx> = None;

    println!("following the pointer; move it over things");

    while seconds.is_none_or(|s| start.elapsed().as_secs_f64() < s) {
        std::thread::sleep(period);

        windows.pump().context("reading the toplevel list")?;

        let Some(p) = cursor.position().context("reading the pointer position")? else {
            continue;
        };

        if last.is_some_and(|q| (q.x - p.x).abs() < 1.0 && (q.y - p.y).abs() < 1.0) {
            continue;
        }

        last = Some(p);

        let Some(window) = windows.at(p) else {
            println!("({:.0}, {:.0}): no window", p.x, p.y);

            continue;
        };

        let started = Instant::now();
        let hit     = a11y.at(p, &window);

        match hit {
            Ok(hit) => println!("({:.0}, {:.0}) [{}] {}  ({:.1} ms)",
                                p.x, p.y, window.app_id, describe(hit.as_ref()),
                                started.elapsed().as_secs_f64() * 1000.0),
            Err(e)  => println!("({:.0}, {:.0}) [{}] error: {e}", p.x, p.y, window.app_id),
        }
    }

    Ok(())
}

/// One line for a hit: the target if there is one, then the leaf it was climbed from.
fn describe(hit: Option<&Hit>) -> String {
    let Some(hit) = hit else {
        return "no answer from the tree".to_string();
    };

    let node = |n: &gaze_a11y::Node| {
        let rect = n.rect.map_or("no extents".to_string(), |r| {
            format!("({:.0},{:.0}) {:.0}x{:.0}", r.x, r.y, r.w, r.h)
        });

        format!("{} {:?} {rect}", n.role, n.name)
    };

    match &hit.target {
        Some(t) if t.path != hit.leaf.path => {
            format!("{} ← leaf {} [{:?}, {} up]", node(t), node(&hit.leaf), hit.coord, hit.climbed)
        }
        Some(t) => format!("{} [{:?}]", node(t), hit.coord),
        None    => format!("no actionable ancestor; leaf {} [{:?}, {} up]",
                           node(&hit.leaf), hit.coord, hit.climbed),
    }
}

/// `X,Y` in global logical pixels.
fn parse_point(s: &str) -> std::result::Result<GlobalPx, String> {
    let (x, y) = s.split_once(',').ok_or_else(|| format!("expected X,Y, got {s:?}"))?;

    Ok(GlobalPx {
        x : x.trim().parse().map_err(|e| format!("{x:?}: {e}"))?,
        y : y.trim().parse().map_err(|e| format!("{y:?}: {e}"))?,
    })
}
