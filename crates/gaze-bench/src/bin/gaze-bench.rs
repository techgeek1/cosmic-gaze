//! The offline Monte Carlo, end to end: screenshots and a desk config in, a markdown
//! report and (optionally) debug overlays out.
//!
//! ```text
//! gaze-bench --shots screenshots/ --sigma-fixed 0.5,0.7,1.0,1.5 --sigma-profile \
//!     --trials-per-element 20 --out report.md --overlays overlays/
//! ```
//!
//! Detection results are cached beside the screenshots, so the first run pays about
//! 400 ms per ultrawide frame and later runs start instantly. `--redetect` forces a
//! fresh pass.

// The workspace style is explicit struct field syntax everywhere, which clippy reads as
// redundant. Same allow as `gaze-core`.
#![allow(clippy::redundant_field_names)]

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Instant;

use anyhow::{Context, Result, bail};
use clap::Parser;
use gaze_bench::report::ReportInputs;
use gaze_bench::run::{BenchConfig, CandidateSet, FeedMode, SigmaSetting};
use gaze_bench::stats::RivalPolicy;
use gaze_bench::{load_shots, render_report, run_all, write_overlay};
use gaze_core::{DesktopGeometry, NoiseModel, SigmaProfile};
use gaze_snap::ScoreWeights;

/// Command line arguments.
#[derive(Debug, Parser)]
#[command(name = "gaze-bench", about = "offline snap-correct rate on real screenshots")]
struct Args {
    /// Directory of `<output>-<n>.png` screenshots.
    #[arg(long, default_value = "screenshots")]
    shots : PathBuf,

    /// Desk geometry.
    #[arg(long, default_value = "config/desk.toml")]
    desk : PathBuf,

    /// Directory holding the detector's `.onnx` files.
    #[arg(long, default_value = "models")]
    models : PathBuf,

    /// Constant sigmas to sweep, degrees, comma separated. The primary metric.
    #[arg(long, value_delimiter = ',', default_value = "0.5,0.7,1.0,1.5")]
    sigma_fixed : Vec<f64>,

    /// Also sweep the desk's own sigma profile, by off-axis angle.
    #[arg(long)]
    sigma_profile : bool,

    /// RNG seed. The whole run is a deterministic function of it.
    #[arg(long, default_value_t = 0)]
    seed : u64,

    /// Simulated fixations per element in single-sample mode.
    #[arg(long, default_value_t = 20)]
    trials_per_element : u32,

    /// Simulated fixations per element in sequence mode, which costs 24 engine updates
    /// each. Defaults to `--trials-per-element`.
    #[arg(long)]
    seq_trials_per_element : Option<u32>,

    /// Snap radius passed to the engine.
    #[arg(long, default_value_t = 2.0)]
    snap_radius_deg : f64,

    /// Hysteresis margin passed to the engine.
    #[arg(long, default_value_t = 0.15)]
    hysteresis : f64,

    /// Score weights as `kind,area,dist[,center[,center_deg]]`. Omit to use the snap
    /// engine's own defaults, which is what the canonical report must measure; entries
    /// left off the end of a given list keep their default too.
    #[arg(long, value_delimiter = ',')]
    weights : Option<Vec<f64>>,

    /// Drop elements whose shorter side is under this many logical pixels.
    #[arg(long, default_value_t = 0.0)]
    min_size_px : f64,

    /// Drop elements whose longer side is over this many logical pixels. Use 800 to
    /// exclude whole terminal panes boxed as one widget.
    #[arg(long)]
    max_size_px : Option<f64>,

    /// Sigma the breakdown sections are computed at.
    #[arg(long, default_value_t = 0.7)]
    breakdown_sigma : f64,

    /// Cost gap below which a second candidate makes a trial ambiguous, in score units.
    #[arg(long, default_value_t = 0.5)]
    ambiguity_margin : f64,

    /// Draw the full sigma independently every sample instead of splitting it into a
    /// per-fixation bias and per-sample jitter. What the first version of this bench did.
    #[arg(long)]
    jitter_only_legacy : bool,

    /// Which rivals may make a trial ambiguous: `all`, `distinct` (ignore duplicate
    /// detections of the winner), or `separate` (also ignore boxes nested with it).
    #[arg(long, default_value = "distinct")]
    ambiguity_rivals : String,

    /// Clamp a gaze sample that lands off every panel to the edge it left through instead
    /// of scoring it as lost gaze. On by default; `--no-edge-clamp` restores the old
    /// behaviour.
    #[arg(long, default_value_t = true, overrides_with = "no_edge_clamp")]
    edge_clamp : bool,

    /// Score an off-desk gaze sample as lost rather than clamping it to the panel edge.
    #[arg(long)]
    no_edge_clamp : bool,

    /// Write the markdown report here as well as to stdout.
    #[arg(long)]
    out : Option<PathBuf>,

    /// Write one debug overlay per screenshot into this directory.
    #[arg(long)]
    overlays : Option<PathBuf>,

    /// Ignore cached detections and run the models again.
    #[arg(long)]
    redetect : bool,
}

// --- Entry point ---

fn main() -> Result<()> {
    let args = Args::parse();

    if args.sigma_fixed.is_empty() && !args.sigma_profile {
        bail!("nothing to sweep: pass --sigma-fixed and/or --sigma-profile");
    }

    if args.weights.as_ref().is_some_and(|w| w.is_empty() || w.len() > 5) {
        bail!("--weights takes one to five numbers: kind,area,dist[,center[,center_deg]]");
    }

    let weights = match &args.weights {
        Some(list) => ScoreWeights::from_list(list),
        None       => ScoreWeights::default(),
    };

    let rivals = RivalPolicy::parse(&args.ambiguity_rivals)
        .context("--ambiguity-rivals must be all, distinct or separate")?;

    let desk_text = std::fs::read_to_string(&args.desk)
        .with_context(|| format!("reading {}", args.desk.display()))?;

    let geometry = DesktopGeometry::from_toml(&desk_text)
        .with_context(|| format!("parsing {}", args.desk.display()))?;

    // The desk's noise model supplies the jitter and the bias split. A desk config
    // without a `[noise]` section still benches: the defaults are the ET5-class ones.
    let model = geometry.noise.unwrap_or(NoiseModel {
        profile         : SigmaProfile::default(),
        jitter_deg      : 0.2,
        bias_redraw_deg : 1.0,
        drift_deg       : 0.0,
        latency_s       : 0.0,
        rate_hz         : 120.0,
    });

    let profile = {
        if args.sigma_profile {
            geometry.noise
                .context("--sigma-profile needs a [noise] section in the desk config")?;

            Some(model.profile)
        }
        else {
            None
        }
    };

    // Detection first, so its cost is visible separately from the Monte Carlo's.
    let max_size_px = args.max_size_px.unwrap_or(f64::INFINITY);
    let detect_at   = Instant::now();

    let shots = load_shots(
        &args.shots,
        &geometry,
        &args.models,
        args.redetect,
        args.min_size_px,
        max_size_px,
    )?;

    if shots.detected {
        eprintln!(
            "detected {} screenshots in {:.1} s (cached beside the PNGs)",
            shots.shots.len(),
            detect_at.elapsed().as_secs_f64(),
        );
    }

    let config = BenchConfig {
        seed       : args.seed,
        trials     : args.trials_per_element,
        seq_trials : args.seq_trials_per_element.unwrap_or(args.trials_per_element),
        radius_deg : args.snap_radius_deg,
        hysteresis : args.hysteresis,
        weights    : weights,
        sigmas     : args.sigma_fixed.clone(),
        profile    : profile,
        model      : model,
        legacy     : args.jitter_only_legacy,
        margin     : args.ambiguity_margin,
        rivals     : rivals,
        clamp      : args.edge_clamp && !args.no_edge_clamp,
    };

    let geometry = Arc::new(geometry);
    let bench_at = Instant::now();
    let runs     = run_all(&shots.shots, &geometry, &config);
    let elapsed  = bench_at.elapsed().as_secs_f64();

    eprintln!("monte carlo: {} runs in {elapsed:.1} s", runs.len());

    let report = render_report(&ReportInputs {
        shots           : &shots.shots,
        geometry        : &geometry,
        runs            : &runs,
        config          : &config,
        breakdown_sigma : args.breakdown_sigma,
        min_size_px     : args.min_size_px,
        max_size_px     : max_size_px,
        elapsed_s       : elapsed,
    });

    print!("{report}");

    if let Some(path) = &args.out {
        std::fs::write(path, &report)
            .with_context(|| format!("writing {}", path.display()))?;
    }

    if let Some(dir) = &args.overlays {
        // The overlays describe the full candidate set: they are drawn over the whole
        // screenshot, so a widgets-only run's ids would not line up with what is visible.
        let overlay_run = runs.iter().find(|r| {
            r.key.mode == FeedMode::Single
                && r.key.candidates == CandidateSet::All
                && matches!(r.key.sigma, SigmaSetting::Fixed(s)
                    if (s - args.breakdown_sigma).abs() < 1.0e-9)
        });

        let Some(run) = overlay_run else {
            bail!("--overlays needs a single-sample run at sigma {}", args.breakdown_sigma);
        };

        for frame in &run.frames {
            write_overlay(&shots.shots[frame.shot], frame, dir)?;
        }

        eprintln!("wrote {} overlays to {}", run.frames.len(), dir.display());
    }

    Ok(())
}
