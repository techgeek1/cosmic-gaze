//! Manual test harness for `gaze-capture`: lists outputs, writes PNGs, and reports the
//! frame-diff score between successive captures in loop mode.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::Parser;
use gaze_capture::{Capture, CursorTracker, Frame, ToplevelTracker, changed_fraction};
use image::{ColorType, ImageEncoder};
use image::codecs::png::{CompressionType, FilterType, PngEncoder};

/// Command line for `gaze-capture-cli`.
#[derive(Debug, Parser)]
#[command(about = "capture COSMIC outputs to PNG via ext-image-copy-capture-v1")]
struct Args {
    /// Directory the PNGs are written into. Created if missing.
    #[arg(long, default_value = "screenshots")]
    out: PathBuf,

    /// Print the current outputs and exit without capturing.
    #[arg(long)]
    list: bool,

    /// Capture only this connector, for example `DP-1`. Default is every output.
    #[arg(long)]
    output: Option<String>,

    /// Repeat every N seconds instead of capturing once. Fractional values are allowed.
    #[arg(long = "loop", value_name = "SECONDS")]
    loop_s: Option<f64>,

    /// Stop looping after this many seconds. Runs until interrupted when unset.
    #[arg(long, value_name = "SECONDS")]
    duration: Option<f64>,

    /// Use the batch `capture_all` call instead of timing each output separately.
    #[arg(long)]
    all: bool,

    /// Print every window's rectangle in global logical pixels and the window under the
    /// pointer, then exit. With `--duration`, keep printing on every change instead.
    #[arg(long)]
    toplevels: bool,

    /// Poll the pointer position at 10 Hz instead of capturing, and exit after
    /// `--duration` seconds (default 5).
    #[arg(long)]
    cursor: bool,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let mut cap = Capture::connect().context("connecting to the compositor")?;

    if args.list {
        print_outputs(&mut cap);
        return Ok(());
    }

    if args.cursor {
        return track_cursor(args.duration.unwrap_or(5.0));
    }

    if args.toplevels {
        return list_toplevels(args.duration);
    }

    std::fs::create_dir_all(&args.out)
        .with_context(|| format!("creating {}", args.out.display()))?;

    print_outputs(&mut cap);
    println!();

    // Frame index and previous frame per output, so `<output>-<n>.png` keeps counting up
    // across loop iterations and the diff always compares like with like.
    let mut counters: HashMap<String, u32>   = HashMap::new();
    let mut previous: HashMap<String, Frame> = HashMap::new();

    let start = Instant::now();
    let mut iteration = 0u32;

    loop {
        if args.loop_s.is_some() && iteration > 0 {
            println!("--- iteration {iteration} at {:.1} s ---", start.elapsed().as_secs_f64());
        }

        capture_once(&mut cap, &args, &mut counters, &mut previous)?;

        let Some(period_s) = args.loop_s else {
            break;
        };

        iteration += 1;

        if let Some(limit) = args.duration
            && start.elapsed().as_secs_f64() >= limit
        {
            break;
        }

        std::thread::sleep(Duration::from_secs_f64(period_s.max(0.0)));
    }

    Ok(())
}

/// Captures the selected outputs once, writes their PNGs and prints the per-output line.
fn capture_once(
    cap      : &mut Capture,
    args     : &Args,
    counters : &mut HashMap<String, u32>,
    previous : &mut HashMap<String, Frame>,
)
    -> Result<()>
{
    // Per-output capture is the default so each frame gets its own latency figure;
    // `--all` exercises the batch call instead and reports one total.
    let mut results = Vec::new();
    let mut capture_ms = Vec::new();

    if args.all {
        let t0 = Instant::now();
        results = cap.capture_all();
        println!("capture_all: {:.1} ms total", t0.elapsed().as_secs_f64() * 1e3);
    }
    else {
        let names: Vec<String> = {
            match &args.output {
                Some(name) => vec![name.clone()],
                None       => cap.outputs().into_iter().map(|o| o.name).collect(),
            }
        };

        for name in names {
            let t0 = Instant::now();
            let r  = cap.capture_output(&name);
            capture_ms.push(t0.elapsed().as_secs_f64() * 1e3);
            results.push(r);
        }
    }

    if results.is_empty() {
        println!("no outputs available");
        return Ok(());
    }

    for (i, result) in results.into_iter().enumerate() {
        let frame = match result {
            Ok(f)  => f,
            Err(e) => {
                println!("capture failed: {e}");
                continue;
            }
        };

        // Diff against this output's previous frame before it is replaced.
        let diff = previous.get(&frame.output).map(|p| changed_fraction(p, &frame));

        // Append after any files already in the directory so repeated runs accumulate a
        // corpus instead of overwriting `<output>-0.png`.
        let n    = counters.entry(frame.output.clone()).or_insert_with(|| next_index(&args.out, &frame.output));
        let path = args.out.join(format!("{}-{}.png", frame.output, n));
        *n += 1;

        let t0 = Instant::now();
        write_png(&path, &frame).with_context(|| format!("writing {}", path.display()))?;
        let write_ms = t0.elapsed().as_secs_f64() * 1e3;

        let diff_text = {
            match diff {
                Some(d) => format!(", changed {:.3}", d),
                None    => String::new(),
            }
        };

        let capture_text = {
            match capture_ms.get(i) {
                Some(ms) => format!("capture {ms:.1} ms, "),
                None     => String::new(),
            }
        };

        println!(
            "{:<10} {}x{} logical {}x{} at ({},{}) scale {:.2}  {}png {:.1} ms{}  -> {}",
            frame.output,
            frame.width,
            frame.height,
            frame.logical.w as i64,
            frame.logical.h as i64,
            frame.logical.x as i64,
            frame.logical.y as i64,
            frame.scale(),
            capture_text,
            write_ms,
            diff_text,
            path.display(),
        );

        previous.insert(frame.output.clone(), frame);
    }

    Ok(())
}

/// Polls the pointer position at 10 Hz and prints every reading.
///
/// Raw buffer coordinates are printed next to the converted global position so the unit
/// convention cosmic-comp uses can be checked against the known desk layout.
fn track_cursor(duration_s: f64) -> Result<()> {
    let mut tracker = CursorTracker::connect().context("opening cursor sessions")?;

    println!("cursor sessions on: {}", tracker.tracked_outputs().join(", "));

    let start  = Instant::now();
    let period = Duration::from_millis(100);
    let mut samples  = 0u32;
    let mut reported = 0u32;

    while start.elapsed().as_secs_f64() < duration_s {
        // `wait_position` doubles as the pacing sleep: it returns early on a new report
        // and otherwise burns the rest of the 100 ms.
        let moved = tracker.wait_position(period).context("waiting for a pointer position")?;
        let now   = tracker.position().context("reading the pointer position")?;

        samples += 1;

        let Some(report) = tracker.last_report() else {
            println!("{:6.2}s  no position reported yet", start.elapsed().as_secs_f64());
            continue;
        };

        reported += 1;

        println!(
            "{:6.2}s  {:<10} buffer ({:>5},{:>5})  global ({:>8.1},{:>8.1})  cursor {}{}",
            start.elapsed().as_secs_f64(),
            report.output,
            report.buffer_x,
            report.buffer_y,
            report.global.x,
            report.global.y,
            cursor_image(report.hotspot, report.image_px),
            if moved.is_some() { "  new" } else { "" },
        );

        let _ = now;
    }

    println!();
    println!("{reported} of {samples} polls had a position");
    println!("cursor session events seen:");

    for line in tracker.event_log() {
        println!("  {line}");
    }

    Ok(())
}

/// The cursor image as `WxH@X,Y` (size, hotspot), with `?` for whatever has not been
/// reported yet.
fn cursor_image(hotspot: Option<(i32, i32)>, image_px: Option<(u32, u32)>) -> String {
    let size = image_px.map_or("?".to_string(), |(w, h)| format!("{w}x{h}"));
    let spot = hotspot.map_or("?".to_string(), |(x, y)| format!("{x},{y}"));

    format!("{size}@{spot}")
}

/// Prints every toplevel and the one under the pointer, once or for `duration_s`.
fn list_toplevels(duration_s: Option<f64>) -> Result<()> {
    let mut windows = ToplevelTracker::connect().context("opening the toplevel list")?;
    let mut cursor  = CursorTracker::connect().context("opening cursor sessions")?;
    let start       = Instant::now();

    loop {
        windows.pump().context("reading the toplevel list")?;

        let pointer = cursor.position().context("reading the pointer position")?;

        println!("--- {:6.2}s", start.elapsed().as_secs_f64());

        for t in windows.toplevels() {
            let flags = [
                (t.activated , "activated"),
                (t.minimized , "minimized"),
                (!t.visible  , "off-workspace"),
                (t.fullscreen, "fullscreen"),
            ]
            .iter()
            .filter(|(on, _)| *on)
            .map(|(_, name)| *name)
            .collect::<Vec<_>>()
            .join(",");

            println!(
                "  {:<9} ({:>6.0},{:>6.0}) {:>5.0}x{:<5.0} focus#{:<3} {:<28} {:<40} {flags}",
                t.output, t.rect.x, t.rect.y, t.rect.w, t.rect.h, t.focus_rank,
                truncate(&t.app_id, 28), truncate(&t.title, 40),
            );
        }

        match pointer.and_then(|p| windows.at(p).map(|t| (p, t))) {
            Some((p, t)) => println!(
                "  pointer ({:.0},{:.0}) is on {:?} at ({:.0},{:.0}) in the window",
                p.x, p.y, t.title, p.x - t.rect.x, p.y - t.rect.y,
            ),
            None => println!("  pointer is on no window"),
        }

        match duration_s {
            Some(d) if start.elapsed().as_secs_f64() < d => {
                windows.wait(Duration::from_millis(500)).context("waiting for toplevel events")?;
            }
            _ => return Ok(()),
        }
    }
}

/// `s` cut to `n` characters with an ellipsis.
fn truncate(s: &str, n: usize) -> String {
    match s.chars().count() > n {
        true  => format!("{}…", s.chars().take(n - 1).collect::<String>()),
        false => s.to_string(),
    }
}

/// Prints the current output list.
fn print_outputs(cap: &mut Capture) {
    let outputs = cap.outputs();

    if outputs.is_empty() {
        println!("no outputs reported by the compositor");
        return;
    }

    for info in outputs {
        println!(
            "{:<10} physical {}x{}  logical {}x{} at ({},{})  scale {:.2}",
            info.name,
            info.physical_w,
            info.physical_h,
            info.logical.w as i64,
            info.logical.h as i64,
            info.logical.x as i64,
            info.logical.y as i64,
            info.scale,
        );
    }
}

/// Writes a frame as an RGBA8 PNG.
///
/// Compression is set to fast: these are 3840x1600 screenshots written in a loop, and the
/// default filter search costs more than the disk saving is worth here.
fn write_png(path: &Path, frame: &Frame) -> Result<()> {
    let file    = std::fs::File::create(path)?;
    let writer  = std::io::BufWriter::new(file);
    let encoder = PngEncoder::new_with_quality(writer, CompressionType::Fast, FilterType::Adaptive);

    encoder.write_image(&frame.rgba, frame.width, frame.height, ColorType::Rgba8.into())?;

    Ok(())
}

/// First index `n` for which `<dir>/<output>-<n>.png` does not exist yet.
fn next_index(dir: &std::path::Path, output: &str) -> u32 {
    let mut n = 0;

    while dir.join(format!("{output}-{n}.png")).exists() {
        n += 1;
    }

    n
}
