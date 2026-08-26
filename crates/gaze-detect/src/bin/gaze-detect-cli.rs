//! Manual test harness for `gaze-detect`: run the two models over a PNG, dump the
//! elements as JSON, draw them on an overlay image, and time the pipeline.
//!
//! ```text
//! gaze-detect-cli screenshots/DP-1-0.png --origin 2559,0 --scale 1 \
//!     --json out.json --overlay out.png --bench 5
//! ```

// The workspace style is explicit struct field syntax everywhere, which clippy reads as
// redundant. Same allow as `gaze-core`.
#![allow(clippy::redundant_field_names)]

use std::path::PathBuf;

use anyhow::{Context, Result, bail};
use clap::Parser;
use gaze_core::{Element, ElementKind, ElementSource, GlobalPx};
use gaze_detect::{DetectConfig, DetectTimings, Detector};
use image::{Rgba, RgbaImage};

/// Command line arguments.
#[derive(Debug, Parser)]
#[command(name = "gaze-detect-cli", about = "detect UI elements in a screenshot")]
struct Args {
    /// PNG (or any image the `image` crate reads) of one output.
    image : PathBuf,

    /// Directory holding the two `.onnx` files.
    #[arg(long, default_value = "models")]
    models : PathBuf,

    /// Output origin in global logical pixels, as `X,Y`.
    #[arg(long, value_parser = parse_origin, default_value = "0,0")]
    origin : GlobalPx,

    /// Compositor scale factor of the output the image came from.
    #[arg(long, default_value_t = 1.0)]
    scale : f64,

    /// Write the detected elements here as a JSON array.
    #[arg(long)]
    json : Option<PathBuf>,

    /// Write the source image with boxes drawn on it here.
    #[arg(long)]
    overlay : Option<PathBuf>,

    /// Run detection this many extra times and report per-stage milliseconds.
    #[arg(long)]
    bench : Option<u32>,

    /// Side of one widget tile in frame pixels.
    #[arg(long)]
    tile : Option<u32>,

    /// Longest side fed to the text model; 0 for native resolution.
    #[arg(long)]
    ocr_max_side : Option<u32>,

    /// Minimum widget confidence.
    #[arg(long)]
    conf : Option<f32>,

    /// onnxruntime intra-op threads; 0 for its default.
    #[arg(long)]
    threads : Option<usize>,

    /// Skip the widget model.
    #[arg(long)]
    no_widgets : bool,

    /// Skip the text model.
    #[arg(long)]
    no_ocr : bool,
}

// --- Entry point ---

fn main() -> Result<()> {
    let args = Args::parse();

    let image = image::open(&args.image)
        .with_context(|| format!("reading {}", args.image.display()))?
        .to_rgba8();

    let (w, h) = image.dimensions();

    let mut config = DetectConfig::default();

    if let Some(v) = args.tile {
        config.tile_px = v;
    }

    if let Some(v) = args.ocr_max_side {
        config.ocr_max_side = v;
    }

    if let Some(v) = args.conf {
        config.widget_conf = v;
    }

    if let Some(v) = args.threads {
        config.threads = v;
    }

    config.widgets = !args.no_widgets;
    config.ocr     = !args.no_ocr;

    if !config.widgets && !config.ocr {
        bail!("both models disabled, nothing to do");
    }

    let detector = Detector::create()
        .models_dir(&args.models)
        .config(config)
        .build()
        .context("loading models")?;

    let (elements, timings) = detector
        .detect_timed(image.as_raw(), w, h, args.origin, args.scale)
        .context("detecting")?;

    report(&args, w, h, &elements, &timings);

    if let Some(path) = &args.json {
        let text = serde_json::to_string_pretty(&elements)?;

        std::fs::write(path, text).with_context(|| format!("writing {}", path.display()))?;

        println!("json:    {}", path.display());
    }

    if let Some(path) = &args.overlay {
        let mut canvas = image.clone();

        draw_overlay(&mut canvas, &elements, args.origin, args.scale);
        canvas.save(path).with_context(|| format!("writing {}", path.display()))?;

        println!("overlay: {}", path.display());
    }

    if let Some(n) = args.bench {
        bench(&detector, image.as_raw(), w, h, &args, n)?;
    }

    Ok(())
}

// --- Reporting ---

/// Prints the box counts and the single-pass timing.
fn report(args: &Args, w: u32, h: u32, elements: &[Element], t: &DetectTimings) {
    println!("image:   {} ({w}x{h} px, origin {},{} scale {})", args.image.display(), args.origin.x, args.origin.y, args.scale);
    println!("tiles:   {}", t.tiles);
    println!("widgets: {} boxes after nms", t.widgets);
    println!("text:    {} boxes after fusion", t.texts);
    println!("total:   {} elements", elements.len());

    let mut counts = [0_usize; 8];

    for e in elements {
        counts[kind_index(e.kind)] += 1;
    }

    println!(
        "kinds:   button {} icon {} input {} link {} text {} checkbox {} slider {} unknown {}",
        counts[0], counts[1], counts[2], counts[3], counts[4], counts[5], counts[6], counts[7]
    );

    println!(
        "first pass: widget {:.1} ms, ocr {:.1} ms, fuse {:.1} ms, total {:.1} ms (includes model warmup)",
        t.widget_ms, t.ocr_ms, t.fuse_ms, t.total_ms
    );
}

/// Runs `n` more passes and prints the mean and best per stage.
fn bench(detector: &Detector, rgba: &[u8], w: u32, h: u32, args: &Args, n: u32) -> Result<()> {
    if n == 0 {
        return Ok(());
    }

    let mut widget = Vec::with_capacity(n as usize);
    let mut ocr    = Vec::with_capacity(n as usize);
    let mut total  = Vec::with_capacity(n as usize);

    for _ in 0..n {
        let (_, t) = detector.detect_timed(rgba, w, h, args.origin, args.scale)?;

        widget.push(t.widget_ms);
        ocr.push(t.ocr_ms);
        total.push(t.total_ms);
    }

    println!("\nbench over {n} iterations ({w}x{h}):");
    print_stat("widget model", &widget);
    print_stat("ocr model   ", &ocr);
    print_stat("total       ", &total);

    Ok(())
}

/// Prints mean, best and worst of one stage's samples.
fn print_stat(label: &str, samples: &[f64]) {
    let mean = samples.iter().sum::<f64>() / samples.len() as f64;
    let best = samples.iter().copied().fold(f64::INFINITY, f64::min);
    let worst = samples.iter().copied().fold(f64::NEG_INFINITY, f64::max);

    println!("  {label}  mean {mean:7.1} ms   best {best:7.1} ms   worst {worst:7.1} ms");
}

// --- Overlay ---

/// Draws every element's box on the image, coloured by kind.
///
/// Elements are in global logical pixels, so they are mapped back into the frame's own
/// physical pixels to line up with what the models saw.
fn draw_overlay(canvas: &mut RgbaImage, elements: &[Element], origin: GlobalPx, scale: f64) {
    let (w, h) = canvas.dimensions();

    for e in elements {
        let colour = colour_for(e.kind, e.source);

        let x0 = ((e.bbox.x - origin.x) * scale).round() as i64;
        let y0 = ((e.bbox.y - origin.y) * scale).round() as i64;
        let x1 = x0 + (e.bbox.w * scale).round() as i64;
        let y1 = y0 + (e.bbox.h * scale).round() as i64;

        // Two pixel border so thin boxes stay visible on a busy desktop.
        for t in 0..2 {
            stroke_rect(canvas, (x0 - t, y0 - t, x1 + t, y1 + t), colour, (w, h));
        }
    }
}

/// Draws a one pixel rectangle outline, clipping to the canvas.
///
/// `rect` is `(x0, y0, x1, y1)` in canvas pixels and may lie partly outside it. `bounds` is
/// the canvas size, passed in rather than re-read per pixel.
fn stroke_rect(
    canvas : &mut RgbaImage,
    rect   : (i64, i64, i64, i64),
    colour : Rgba<u8>,
    bounds : (u32, u32),
)
{
    let (x0, y0, x1, y1) = rect;

    for x in x0..=x1 {
        put(canvas, x, y0, colour, bounds);
        put(canvas, x, y1, colour, bounds);
    }

    for y in y0..=y1 {
        put(canvas, x0, y, colour, bounds);
        put(canvas, x1, y, colour, bounds);
    }
}

/// Writes one pixel if it lands on the canvas.
#[inline]
fn put(canvas: &mut RgbaImage, x: i64, y: i64, colour: Rgba<u8>, bounds: (u32, u32)) {
    if x < 0 || y < 0 || x >= bounds.0 as i64 || y >= bounds.1 as i64 {
        return;
    }

    canvas.put_pixel(x as u32, y as u32, colour);
}

/// Colour per element kind. OCR text is dimmer than detector boxes so the two sources are
/// distinguishable at a glance when reviewing recall by eye.
fn colour_for(kind: ElementKind, source: ElementSource) -> Rgba<u8> {
    if source == ElementSource::Ocr {
        return Rgba([255, 200, 0, 255]);
    }

    match kind {
        ElementKind::Button   => Rgba([0, 220, 60, 255]),
        ElementKind::Checkbox => Rgba([0, 190, 255, 255]),
        ElementKind::Link     => Rgba([180, 80, 255, 255]),
        ElementKind::Input    => Rgba([255, 60, 60, 255]),
        ElementKind::Text     => Rgba([255, 255, 255, 255]),
        ElementKind::Icon     => Rgba([255, 140, 0, 255]),
        ElementKind::Slider   => Rgba([255, 220, 0, 255]),
        ElementKind::Unknown  => Rgba([130, 130, 130, 255]),
    }
}

/// Stable index per kind, for the summary histogram.
fn kind_index(kind: ElementKind) -> usize {
    match kind {
        ElementKind::Button   => 0,
        ElementKind::Icon     => 1,
        ElementKind::Input    => 2,
        ElementKind::Link     => 3,
        ElementKind::Text     => 4,
        ElementKind::Checkbox => 5,
        ElementKind::Slider   => 6,
        ElementKind::Unknown  => 7,
    }
}

// --- Argument parsing ---

/// Parses an `X,Y` origin in global logical pixels.
fn parse_origin(s: &str) -> Result<GlobalPx, String> {
    let (x, y) = s.split_once(',').ok_or_else(|| format!("expected X,Y, got {s:?}"))?;

    Ok(GlobalPx {
        x : x.trim().parse().map_err(|_| format!("bad x in {s:?}"))?,
        y : y.trim().parse().map_err(|_| format!("bad y in {s:?}"))?,
    })
}
