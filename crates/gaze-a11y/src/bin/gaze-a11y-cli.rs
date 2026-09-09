//! Manual check of the accessibility path: which applications are on the bus, and what
//! they say is under a point.
//!
//! ```text
//! gaze-a11y-cli apps                 # applications and their windows on the AT-SPI bus
//! gaze-a11y-cli at 4919,698          # the node under a desk point
//! gaze-a11y-cli chain 4919,698       # that node and every ancestor up to the frame
//! gaze-a11y-cli follow --seconds 30  # the node under the pointer, whenever it moves
//! ```
//!
//! `follow` is the one to run against the probe: put the pointer on a YouTube card, an
//! input field or an avatar and read what the tree calls it.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gaze_a11y::{A11y, Answer, Miss};
use gaze_capture::{CursorTracker, Toplevel, ToplevelTracker};
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

    /// Report the node under one or more desk points, each given as `X,Y` in global
    /// logical pixels.
    At {
        #[arg(value_parser = parse_point, num_args = 1..)]
        points: Vec<GlobalPx>,

        /// Ask this window (a title substring) rather than the one on top at the
        /// point, so a window under another can still be probed.
        #[arg(long)]
        window: Option<String>,
    },

    /// Report the node under one or more desk points and every ancestor above it, with
    /// role, name and extents at each level: the shape of the tree around a point.
    Chain {
        #[arg(value_parser = parse_point, num_args = 1..)]
        points: Vec<GlobalPx>,

        /// Ask this window (a title substring) rather than the one on top at the point.
        #[arg(long)]
        window: Option<String>,
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
        Command::At { points, window }    => at(&points, window.as_deref()),
        Command::Chain { points, window } => chain(&points, window.as_deref()),
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

/// Reports the node under each point.
fn at(points: &[GlobalPx], forced: Option<&str>) -> Result<()> {
    let mut windows = ToplevelTracker::connect().context("opening the toplevel list")?;
    let mut a11y    = A11y::connect().context("connecting to the accessibility bus")?;

    windows.pump().context("reading the toplevel list")?;

    for &point in points {
        let Some(window) = pick(&windows, point, forced) else {
            println!("({:.0}, {:.0}): no window", point.x, point.y);

            continue;
        };

        let started = Instant::now();
        let answer  = a11y.ask(point, &window).context("asking the tree")?;

        println!("({:.0}, {:.0}) in {:?} [{}]: {}  ({:.1} ms)",
                 point.x, point.y, truncate(&window.title, 30), window.app_id,
                 describe(&answer), started.elapsed().as_secs_f64() * 1000.0);
    }

    Ok(())
}

/// Reports the ancestor chain above each point, leaf first.
fn chain(points: &[GlobalPx], forced: Option<&str>) -> Result<()> {
    let mut windows = ToplevelTracker::connect().context("opening the toplevel list")?;
    let mut a11y    = A11y::connect().context("connecting to the accessibility bus")?;

    windows.pump().context("reading the toplevel list")?;

    for &point in points {
        let Some(window) = pick(&windows, point, forced) else {
            println!("({:.0}, {:.0}): no window", point.x, point.y);

            continue;
        };

        let started = Instant::now();
        let nodes   = a11y.ancestors(point, &window).context("asking the tree")?;

        println!("({:.0}, {:.0}) in {:?} [{}] window ({:.0},{:.0}) {:.0}x{:.0}  ({:.1} ms)",
                 point.x, point.y, truncate(&window.title, 30), window.app_id,
                 window.rect.x, window.rect.y, window.rect.w, window.rect.h,
                 started.elapsed().as_secs_f64() * 1000.0);

        for (depth, n) in nodes.iter().enumerate() {
            let rect = n.rect.map_or("no extents".to_string(), |r| {
                format!("({:.0},{:.0}) {:.0}x{:.0} bottom {:.0}", r.x, r.y, r.w, r.h, r.y + r.h)
            });

            let span = n.span.map_or(String::new(), |s| {
                format!("  children {:.0}..{:.0}", s.y, s.y + s.h)
            });

            println!("  {depth:2} {:<16} {rect}  {:?}{span}", n.role, truncate(&n.name, 40));
        }
    }

    Ok(())
}

/// The window to ask about `point`: the one on top there, or with `forced` the first
/// whose title contains it (case-insensitively), whether or not it is on top.
fn pick(windows: &ToplevelTracker, point: GlobalPx, forced: Option<&str>) -> Option<Toplevel> {
    let Some(needle) = forced else {
        return windows.at(point);
    };

    let needle = needle.to_lowercase();

    windows
        .toplevels()
        .into_iter()
        .find(|t| t.title.to_lowercase().contains(&needle))
}

/// `s` cut to `n` characters with an ellipsis.
fn truncate(s: &str, n: usize) -> String {
    match s.chars().count() > n {
        true  => format!("{}…", s.chars().take(n - 1).collect::<String>()),
        false => s.to_string(),
    }
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
        let answer  = a11y.ask(p, &window);

        match answer {
            Ok(answer) => println!("({:.0}, {:.0}) [{}] {}  ({:.1} ms)",
                                   p.x, p.y, window.app_id, describe(&answer),
                                   started.elapsed().as_secs_f64() * 1000.0),
            Err(e)  => println!("({:.0}, {:.0}) [{}] error: {e}", p.x, p.y, window.app_id),
        }
    }

    Ok(())
}

/// One line for a hit: the target if there is one, then the leaf it was climbed from.
fn describe(answer: &Answer) -> String {
    let hit = match answer {
        Answer::Hit(hit)                    => hit,
        Answer::Miss(Miss::Unreachable)     => return "no accessible application for the window".to_string(),
        Answer::Miss(Miss::Nothing)         => return "the window answers null at the point".to_string(),
        Answer::Miss(Miss::Outside)         => return "a node answered but its extents exclude the point".to_string(),
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
