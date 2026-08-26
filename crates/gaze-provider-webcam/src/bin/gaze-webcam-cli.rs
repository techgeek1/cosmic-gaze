//! Manual test harness for the webcam gaze tier.
//!
//! Three subcommands:
//!
//! - `calibrate` walks a grid of targets across every enabled output, fits the
//!   correction, and writes `config/calibration.toml`.
//! - `run` prints live samples, optionally with a live overlay marker and a JSONL
//!   recording.
//! - `fake-sidecar` streams a synthetic gaze over a Unix socket so both of the above work
//!   with no camera and no sidecar.
//!
//! Anything here that needs a live compositor degrades to a warning rather than an error,
//! so the whole thing is usable over SSH.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Args, Parser, Subcommand};
use gaze_core::{DesktopGeometry, GazeSample, GlobalPx};
use gaze_overlay::{Overlay, OverlayHandle, OverlayState};
use gaze_provider_synthetic::{GazeProvider, to_jsonl_line};
use gaze_provider_webcam::{
    Aim, Calibration, CalibrationSweep, CameraPose, DEFAULT_SIGMA_DEG, DEFAULT_SOCKET, Distortion,
    FakeSidecar, FitReport, Observation, SweepEnd, WebcamProvider, sweep,
};
use gaze_provider_webcam::fake::GainCurve;
use gaze_provider_webcam::sweep::SweepRecord;

/// Socket the fake sidecar binds when no path is given. Deliberately not the real
/// sidecar's path, so starting a fake never shadows a running sidecar by accident.
const FAKE_SOCKET: &str = "/tmp/gaze-ml-fake.sock";

#[derive(Parser)]
#[command(name = "gaze-webcam-cli", about = "webcam gaze provider: calibrate, run, fake")]
struct Cli {
    #[command(subcommand)]
    command : Command,
}

#[derive(Subcommand)]
enum Command {
    /// Walk a grid of targets and fit a calibration.
    Calibrate(CalibrateArgs),

    /// Print live samples from the sidecar.
    Run(RunArgs),

    /// Serve a synthetic gaze stream on a Unix socket.
    FakeSidecar(FakeArgs),

    /// Re-fit a calibration from a saved sweep, with no camera and no new capture.
    Refit(RefitArgs),

    /// Score alternative correction models against a saved sweep.
    Experiment(ExperimentArgs),

    /// Push a saved sweep back through the real provider path and check it agrees with
    /// what the fit claimed.
    Check(CheckArgs),
}

#[derive(Args)]
struct CheckArgs {
    /// Desk geometry and camera pose.
    #[arg(long, default_value = "config/desk.toml")]
    config      : PathBuf,

    /// Raw sweep readings written by `calibrate --raw-out`.
    #[arg(long, default_value = "config/calibration.readings.jsonl")]
    readings    : PathBuf,

    /// The calibration to apply, as the provider would.
    #[arg(long, default_value = "config/calibration.toml")]
    calibration : PathBuf,

    /// Provider sigma, degrees.
    #[arg(long, default_value_t = DEFAULT_SIGMA_DEG)]
    sigma       : f64,

    /// How far the reproduced RMS may differ from the stored one before this is called a
    /// failure, degrees.
    #[arg(long, default_value_t = 0.02)]
    tolerance   : f64,
}

#[derive(Args)]
struct ExperimentArgs {
    /// Desk geometry and camera pose.
    #[arg(long, default_value = "config/desk.toml")]
    config   : PathBuf,

    /// Raw sweep readings written by `calibrate --raw-out`.
    #[arg(long, default_value = "config/calibration.readings.jsonl")]
    readings : PathBuf,

    /// Use a calibration file's stored per-target means instead of a readings log. One
    /// point per target, but it carries `head_rot_mean`, so the head-pose trials can be
    /// run on a sweep whose readings log predates that field.
    #[arg(long)]
    from_calibration : Option<PathBuf>,
}

/// How the fake bends the angles it reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
enum CurveArg {
    /// Constant gain at every eccentricity.
    Linear,
    /// Gain falls off with eccentricity, the way a real appearance model's does.
    Saturating,
}

impl From<CurveArg> for GainCurve {
    fn from(c: CurveArg) -> Self {
        match c {
            CurveArg::Linear     => GainCurve::Linear,
            CurveArg::Saturating => GainCurve::Saturating,
        }
    }
}

/// Options shared by every subcommand.
#[derive(Args, Clone)]
struct CommonArgs {
    /// Desk geometry and camera pose.
    #[arg(long, default_value = "config/desk.toml")]
    config : PathBuf,

    /// Sidecar socket. Defaults to the real sidecar's path, or to the fake's when
    /// `--fake` is set.
    #[arg(long)]
    socket : Option<PathBuf>,
}

#[derive(Args)]
struct CalibrateArgs {
    #[command(flatten)]
    common  : CommonArgs,

    /// Where to write the fitted calibration.
    #[arg(long, default_value = "config/calibration.toml")]
    out     : PathBuf,

    /// Seconds each target stays up.
    #[arg(long, default_value_t = 1.5)]
    dwell   : f64,

    /// Seconds at the end of each dwell whose samples are kept.
    #[arg(long, default_value_t = 1.0)]
    collect : f64,

    /// Targets per axis per output.
    #[arg(long, default_value_t = 3)]
    grid    : usize,

    /// Provider sigma recorded with the calibration, degrees.
    #[arg(long, default_value_t = DEFAULT_SIGMA_DEG)]
    sigma   : f64,

    /// Refuse to write a calibration whose RMS residual exceeds this, degrees.
    #[arg(long, default_value_t = 4.0)]
    max_rms_deg : f64,

    /// Write the calibration even if it fails the `--max-rms-deg` check.
    #[arg(long)]
    force   : bool,

    /// Run a fake sidecar in this process with a known distortion, and report what the
    /// fit recovered against it.
    #[arg(long)]
    fake    : bool,

    /// Per-sample jitter for the fake sidecar, degrees.
    #[arg(long, default_value_t = 0.3)]
    jitter_deg : f64,

    /// Angular gain for the fake sidecar. Below 1 it under-reports how far the eye turned;
    /// on a saturating curve it is the gain straight ahead.
    #[arg(long, default_value_t = 1.0)]
    gain    : f64,

    /// Shape of the fake's angular gain against eccentricity.
    #[arg(long, value_enum, default_value_t = CurveArg::Linear)]
    gain_curve : CurveArg,

    /// Yaw error the fake adds per degree of pitch.
    #[arg(long, default_value_t = 0.0)]
    yaw_shift  : f64,

    /// Also write every collected sample here, so the fit can be redone without a new
    /// sweep. Defaults to the calibration path with a `.readings.jsonl` suffix.
    #[arg(long)]
    raw_out : Option<PathBuf>,

    /// Skip the on-screen targets. Only sensible with `--fake`, which does not need to
    /// see them.
    #[arg(long)]
    no_overlay : bool,
}

#[derive(Args)]
struct RunArgs {
    #[command(flatten)]
    common      : CommonArgs,

    /// Calibration to apply. `none` runs uncalibrated.
    #[arg(long, default_value = "config/calibration.toml")]
    calibration : String,

    /// JSONL of ground-truth points to score against, or `none`.
    #[arg(long, default_value = "none")]
    truth_file  : String,

    /// Stop after this many seconds. Zero runs until interrupted.
    #[arg(long, default_value_t = 0.0)]
    seconds     : f64,

    /// Record samples as JSONL, replayable by `gaze_provider_synthetic::ReplayProvider`.
    #[arg(long)]
    record      : Option<PathBuf>,

    /// Show a live marker where the system thinks the user is looking.
    #[arg(long)]
    overlay     : bool,

    /// Provider sigma, degrees.
    #[arg(long, default_value_t = DEFAULT_SIGMA_DEG)]
    sigma       : f64,

    /// Run a fake sidecar in this process instead of connecting to the real one.
    #[arg(long)]
    fake        : bool,
}

#[derive(Args)]
struct FakeArgs {
    #[command(flatten)]
    common : CommonArgs,

    /// Frames per second.
    #[arg(long, default_value_t = 30.0)]
    rate   : f64,

    /// Emit an undistorted stream. Useful for checking the transport and the geometry in
    /// isolation from the calibration.
    #[arg(long)]
    clean  : bool,

    /// Per-sample jitter, degrees. Overrides whatever the chosen distortion carries.
    #[arg(long)]
    jitter_deg : Option<f64>,

    /// Angular gain. Below 1 the stream under-reports how far the eye turned.
    #[arg(long, default_value_t = 1.0)]
    gain   : f64,

    /// Shape of the gain against eccentricity.
    #[arg(long, value_enum, default_value_t = CurveArg::Linear)]
    gain_curve : CurveArg,

    /// Yaw error added per degree of pitch.
    #[arg(long, default_value_t = 0.0)]
    yaw_shift : f64,

    /// Stop after this many seconds. Zero runs until interrupted.
    #[arg(long, default_value_t = 0.0)]
    seconds: f64,
}

#[derive(Args)]
struct RefitArgs {
    /// Desk geometry and camera pose.
    #[arg(long, default_value = "config/desk.toml")]
    config      : PathBuf,

    /// Raw sweep readings written by `calibrate --raw-out`.
    #[arg(long)]
    readings    : Option<PathBuf>,

    /// An existing calibration to refit from its stored per-target means. Lower fidelity
    /// than `--readings` (one point per target instead of every sample) but it works on
    /// files written before the readings log existed.
    #[arg(long)]
    from_calibration : Option<PathBuf>,

    /// Where to write the refitted calibration. Omit to score the fit without writing.
    #[arg(long)]
    out         : Option<PathBuf>,

    #[arg(long, default_value_t = DEFAULT_SIGMA_DEG)]
    sigma       : f64,

    #[arg(long, default_value_t = 4.0)]
    max_rms_deg : f64,

    #[arg(long)]
    force       : bool,
}

/// Tolerant view of a calibration file for `refit --from-calibration`.
///
/// Deliberately not `Calibration`: the point is to read files this build would otherwise
/// refuse, including the format 1 files whose stage one was a single rotation. Only the
/// per-target diagnostics are needed, and those have not changed shape.
#[derive(serde::Deserialize)]
struct LegacyCalibration {
    #[serde(default)]
    targets : Vec<LegacyTarget>,
}

#[derive(serde::Deserialize)]
struct LegacyTarget {
    output      : String,
    target_px   : [f64; 2],
    diagnostics : LegacyDiagnostics,
}

#[derive(serde::Deserialize)]
struct LegacyDiagnostics {
    yaw_deg_mean     : f64,
    pitch_deg_mean   : f64,
    target_yaw_deg   : f64,
    target_pitch_deg : f64,
    eye_mm_mean      : [f64; 3],
    #[serde(default)]
    head_rot_mean    : [f64; 3],
}

/// One ground-truth point from a `--truth-file`.
#[derive(serde::Deserialize)]
struct TruthPoint {
    t_s : f64,
    x   : f64,
    y   : f64,
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .with_writer(std::io::stderr)
        .init();

    match Cli::parse().command {
        Command::Calibrate(args)  => calibrate(args),
        Command::Run(args)        => run(args),
        Command::FakeSidecar(args)=> fake_sidecar(args),
        Command::Refit(args)      => refit(args),
        Command::Experiment(args) => experiment(args),
        Command::Check(args)      => check(args),
    }
}

/// Replays a saved sweep through the real provider path and compares it with the fit.
///
/// The point is not to measure the calibration again, it is to prove that the code the
/// reader thread runs applies the calibration the way the fitter assumed it would. A
/// mismatch here means every number in `calibration.toml` is describing a system other
/// than the one running, which is exactly the failure that looks like "the marker is
/// nowhere near where I am looking" and cannot be diagnosed by staring at the marker.
fn check(args: CheckArgs) -> Result<()> {
    let (geometry, camera) = load_desk(&args.config)?;
    let calibration        = Calibration::load(&args.calibration)
        .with_context(|| format!("cannot load {}", args.calibration.display()))?;

    let records = read_records(&args.readings)?;

    if records.is_empty() {
        anyhow::bail!("no readings in {}", args.readings.display());
    }

    let profile  = gaze_provider_webcam::webcam_profile(args.sigma);
    let verbatim = records.iter().filter(|r| r.raw.is_some()).count();

    println!("{} samples from {}", records.len(), args.readings.display());
    println!("applying {} (held-out {:.2} deg, in-sample {:.2} deg at fit time)",
        args.calibration.display(), calibration.rms_loo_deg, calibration.rms_deg);
    println!(
        "{} of them carry the verbatim sidecar line; the rest are re-encoded from the \n\
         stored camera-frame vectors, which is what the parser would have seen anyway.",
        verbatim,
    );

    let report = gaze_provider_webcam::check::replay(
        &geometry, &camera, &calibration, &profile, &records,
    )?;

    println!("\nStage one, fitter's entry point against the runtime's (camera-frame yaw):");
    println!("{:<10} {:>10} {:>10} {:>10}", "output", "apply", "runtime", "diff");

    for t in &report.targets {
        let Some((a, r)) = t.apply_yaw.zip(t.runtime_yaw) else {
            continue;
        };

        println!("{:<10} {a:>10.2} {r:>10.2} {:>+10.5}", t.output, a - r);
    }

    println!("\nworst disagreement between the two stage-one paths: {:.6} deg",
        report.worst_path_gap_deg);

    println!("\n{:<10} {:>16} {:>16} {:>9} {:>9} {:>9} {:>8} {:>5}",
        "output", "target px", "mean px", "err px", "err deg", "mean-ray", "clamped", "n");

    for t in &report.targets {
        println!("{:<10} {:>7.0},{:>7.0}  {:>16} {:>9} {:>9} {:>9} {:>8} {:>5}",
            t.output,
            t.target.x, t.target.y,
            t.mean_px.map_or("off-desk".to_string(), |p| format!("{:>7.0},{:>7.0}", p.x, p.y)),
            t.err_px.map_or("-".to_string(), |v| format!("{v:.0}")),
            t.err_deg.map_or("-".to_string(), |v| format!("{v:.2}")),
            t.mean_ray_deg.map_or("-".to_string(), |v| format!("{v:.2}")),
            t.clamped,
            t.samples,
        );
    }

    println!("\nRMS through the runtime path, at each target's mean input: {:.3} deg",
        report.rms_runtime_deg);
    println!("RMS of the mean marker position per target               : {:.3} deg",
        report.rms_marker_deg);
    println!("In-sample RMS recorded at fit time                       : {:.3} deg",
        calibration.rms_deg);

    if report.clamped > 0 {
        println!(
            "\n{} samples had their corrected ray miss the desk and were clamped to an edge. \n\
             Those are scored on their ray direction here, which is what the fit scores too.",
            report.clamped,
        );
    }

    let delta = report.disagreement(&calibration);

    if delta <= args.tolerance {
        println!(
            "\nAgrees to {delta:.4} deg. The provider applies the calibration exactly as the \n\
             fitter evaluated it.",
        );

        return Ok(());
    }

    anyhow::bail!(
        "runtime path disagrees with the fit by {delta:.3} deg (tolerance {:.3}). The \n\
         calibration file describes a system other than the one running; fix the runtime \n\
         path before trusting any number in it.",
        args.tolerance,
    )
}

// --- calibrate ---

/// Shows the target grid, collects rays, fits, reports and saves.
fn calibrate(args: CalibrateArgs) -> Result<()> {
    let (geometry, camera) = load_desk(&args.common.config)?;
    let socket             = resolve_socket(&args.common, args.fake);

    // The fake has to exist before the provider tries to connect, and its handle is what
    // points the synthetic eye at each target as it comes up.
    let fake = {
        if args.fake {
            Some(
                FakeSidecar::create()
                    .socket(&socket)
                    .geometry(geometry.clone())
                    .camera(camera.clone())
                    .distortion(fake_distortion(args.jitter_deg, args.gain, args.gain_curve, args.yaw_shift))
                    .rate_hz(30.0)
                    .aim(Aim::Sweep)
                    .seed(1)
                    .start()
                    .context("cannot start the fake sidecar")?,
            )
        }
        else {
            None
        }
    };

    let overlay = {
        if args.no_overlay {
            None
        }
        else {
            connect_overlay()
        }
    };

    let camera_for_sweep = camera.clone();

    // The sweep must see raw rays, so the provider it drives carries no calibration.
    let mut provider = WebcamProvider::create()
        .socket(&socket)
        .geometry(geometry.clone())
        .camera(camera)
        .calibration(None::<PathBuf>)
        .sigma_deg(args.sigma)
        .start()
        .context("cannot start the webcam provider")?;

    let mut builder = CalibrationSweep::create()
        .geometry(geometry.clone())
        .camera(camera_for_sweep.clone())
        .grid(args.grid)
        .dwell_s(args.dwell)
        .collect_s(args.collect);

    if let Some(handle) = overlay.as_ref().map(|(h, _)| h.clone()) {
        builder = builder.overlay(handle);
    }

    if let Some(keys) = sweep::terminal_keys() {
        builder = builder.keys(keys);
    }

    if let Some(gaze) = fake.as_ref().map(|f| f.gaze()) {
        builder = builder.on_target(move |_, target| gaze.look_at(target.px));
    }

    let mut run = builder.build()?;
    let total   = run.targets().len();

    println!("Calibrating the webcam gaze tier against {}.", socket.display());
    println!(
        "{total} targets, {:.1} s each: about {:.0} s in total. A white square with a caption\n\
         will appear on each of your screens in turn; look at the middle of it and hold\n\
         still until it moves.",
        args.dwell,
        total as f64 * args.dwell,
    );
    println!("Press Enter to advance early, `s` then Enter to skip a target, `q` then Enter to quit.");

    if args.fake {
        println!("Running against the fake sidecar, so nothing needs to be looked at.");
    }

    println!();

    // Give the provider a moment to connect so the first target is not collected against
    // an empty socket.
    wait_for_connection(&provider, Duration::from_secs(2));

    let outcome = run.run(&mut provider);

    provider.stop();

    if let Some((handle, join)) = overlay {
        handle.stop();
        let _ = join.join();
    }

    if outcome.observations.is_empty() {
        anyhow::bail!(
            "no usable observations: is the sidecar running on {}? (try --fake)",
            socket.display(),
        );
    }

    // Diagnostics first, before any fitting. When a sweep comes back bad these are the
    // numbers that say why, and a fit that fails must not take them down with it.
    report_diagnostics(&outcome.observations);
    report_gains(&outcome.observations);

    // Write the raw samples before fitting. The fit is the part most likely to change, and
    // a saved sweep means the next change to it costs nothing to evaluate.
    let raw_out = args.raw_out.clone().unwrap_or_else(|| readings_path(&args.out));
    let records = sweep::to_records(&outcome.observations);

    if !records.is_empty() {
        write_records(&raw_out, &records)?;
        println!("\nWrote {} samples to {}", records.len(), raw_out.display());
    }

    let report = sweep::fit(&geometry, &camera_for_sweep, &outcome.observations, args.sigma);

    report_models(&report);
    report_gain_profile(&report.calibration, &outcome.observations);
    report_angle_plot(&report.calibration, &outcome.observations);
    report_columns(&report.calibration, &outcome.observations, "DP-1");
    report_residuals(&report.calibration, &outcome);
    report_headline(&geometry, &report.calibration);

    if outcome.end == SweepEnd::Aborted {
        println!("Sweep was quit early; fitted from the {} targets collected so far.", outcome.observations.len());
    }

    if args.fake {
        report_against_fake(args.jitter_deg, args.gain, args.gain_curve, args.yaw_shift);
    }

    finish(&report.calibration, &args.out, args.max_rms_deg, args.force, Some(&raw_out))
}

/// Applies the acceptance gate and writes the file.
fn finish(
    calibration : &Calibration,
    out         : &Path,
    max_rms_deg : f64,
    force       : bool,
    raw_out     : Option<&Path>,
)
    -> Result<()>
{
    let mut calibration = calibration.clone();

    // A file written over its own acceptance gate has to say so in the file. The terminal
    // that explained why will be gone long before the next person opens this.
    if calibration.rms_loo_deg > max_rms_deg && calibration.note.is_empty() {
        calibration.note = format!(
            "WRITTEN OVER THE ACCEPTANCE GATE. Held-out RMS {:.2} deg exceeds the {:.2} deg \
             limit, so expect the pointer to land roughly {:.0} px from where you are \
             looking on a 55 px/deg panel. Kept because it is the best model this sweep \
             supports and is better than nothing; re-sweep with a denser grid to improve it.",
            calibration.rms_loo_deg,
            max_rms_deg,
            calibration.rms_loo_deg * 55.0,
        );
    }

    let calibration = &calibration;

    // The gate is on the held-out figure, not the in-sample one. An overfitted model has a
    // flattering in-sample RMS by construction, and gating on that would let through
    // exactly the calibrations most likely to be useless in practice.
    if calibration.rms_loo_deg > max_rms_deg && !force {
        if let Some(raw) = raw_out {
            println!(
                "\nThe sweep itself is saved at {}, so a better model can be tried against it\n\
                 with `gaze-webcam-cli refit --readings {}`.",
                raw.display(),
                raw.display(),
            );
        }

        println!();
        anyhow::bail!(
            "not writing {}: held-out RMS {:.2} deg exceeds --max-rms-deg {:.2}. The \n\
             diagnostics above say why; fix the cause and sweep again, or pass --force.",
            out.display(),
            calibration.rms_loo_deg,
            max_rms_deg,
        );
    }

    calibration.save(out).with_context(|| format!("cannot write {}", out.display()))?;

    println!("\nWrote {}", out.display());

    if calibration.rms_loo_deg > max_rms_deg {
        println!(
            "(forced: held-out RMS {:.2} deg is above the {:.2} deg limit; the file says so too)",
            calibration.rms_loo_deg, max_rms_deg,
        );
    }

    Ok(())
}

// --- refit ---

/// Re-fits from a saved sweep. No camera, no socket, no overlay.
fn refit(args: RefitArgs) -> Result<()> {
    let (geometry, camera) = load_desk(&args.config)?;

    let observations = {
        match (args.readings.as_ref(), args.from_calibration.as_ref()) {
            (Some(path), _) => {
                let records = read_records(path)?;

                println!("{} samples from {}", records.len(), path.display());

                sweep::from_records(&geometry, &camera, &records)
            }

            (None, Some(path)) => {
                let observations = observations_from_calibration(&geometry, &camera, path)?;

                println!(
                    "{} targets from {} (per-target means only, so the per-sample scatter \n\
                     columns below read zero; use --readings for the full picture)",
                    observations.len(),
                    path.display(),
                );

                observations
            }

            (None, None) => anyhow::bail!("refit needs --readings or --from-calibration"),
        }
    };

    if observations.is_empty() {
        anyhow::bail!("no usable observations in the saved sweep");
    }

    report_diagnostics(&observations);
    report_gains(&observations);

    let report = sweep::fit(&geometry, &camera, &observations, args.sigma);

    report_models(&report);
    report_gain_profile(&report.calibration, &observations);
    report_angle_plot(&report.calibration, &observations);
    report_columns(&report.calibration, &observations, "DP-1");
    report_headline(&geometry, &report.calibration);

    let Some(out) = args.out.as_ref() else {
        println!("\n(no --out given, so nothing was written)");

        return Ok(());
    };

    finish(&report.calibration, out, args.max_rms_deg, args.force, args.readings.as_deref())
}

// --- experiment ---

/// Scores alternative correction models against a saved sweep, and checks how much the
/// answer depends on the camera pose in `desk.toml`.
fn experiment(args: ExperimentArgs) -> Result<()> {
    use gaze_provider_webcam::experiment::{self, Features};

    let (geometry, camera) = load_desk(&args.config)?;

    let observations = {
        match args.from_calibration.as_ref() {
            Some(path) => {
                let obs = observations_from_calibration(&geometry, &camera, path)?;

                println!(
                    "{} targets from {} (one point per target: enough to compare models, \n\
                     not enough to measure scatter)",
                    obs.len(),
                    path.display(),
                );

                obs
            }

            None => {
                let records = read_records(&args.readings)?;
                let obs     = sweep::from_records(&geometry, &camera, &records);

                println!("{} samples over {} targets from {}",
                    records.len(), obs.len(), args.readings.display());

                let has_head = records.iter().all(|r| r.head_rot.is_some());
                let has_raw  = records.iter().all(|r| r.raw.is_some());

                println!("head_rot present: {has_head}    verbatim sidecar line present: {has_raw}");

                if !has_head || !has_raw {
                    println!(
                        "This sweep predates those fields, so head-pose and second-estimator \n\
                         trials cannot be run on it. Sweeps captured from now on carry both.",
                    );
                }

                obs
            }
        }
    };

    let trials = experiment::run(&observations);

    println!("\n{:<12} {:>4} {:>6} {:>7} {:>10} {:>10} {:>8} {:>10}",
        "features", "deg", "terms", "ridge", "in-sample", "held-out", "worst", "wtd held");

    for t in &trials {
        if !t.available {
            continue;
        }

        println!("{:<12} {:>4} {:>6} {:>7.0e} {:>9.2}d {:>9.2}d {:>7.2}d {:>9.2}d",
            t.features.name(), t.degree, t.terms, t.ridge,
            t.rms_deg, t.rms_loo_deg, t.worst_deg, t.weighted_loo_deg);
    }

    let unavailable: Vec<&str> = Features::all()
        .into_iter()
        .filter(|f| trials.iter().any(|t| t.features == *f && !t.available))
        .map(|f| f.name())
        .collect();

    if !unavailable.is_empty() {
        println!("\nnot runnable on this sweep: {}", unavailable.join(", "));
    }

    // The best available trial drives the pose sweep: asking whether the config is wrong is
    // only meaningful once the model is the best on offer.
    let best = trials
        .iter()
        .filter(|t| t.available && t.rms_loo_deg.is_finite())
        .min_by(|a, b| a.rms_loo_deg.partial_cmp(&b.rms_loo_deg).unwrap());

    let Some(best) = best else {
        anyhow::bail!("no trial could be fitted");
    };

    println!("\nbest: {} degree {} ridge {:.0e} at {:.2} deg held-out",
        best.features.name(), best.degree, best.ridge, best.rms_loo_deg);

    println!("\nCamera pose sensitivity (held-out with the best model, pose nudged):");

    let sensitivity = experiment::pose_sensitivity(
        &geometry, &camera, &observations, best.features, best.degree, best.ridge,
    );

    let baseline = sensitivity.first().map(|(_, v)| *v).unwrap_or(f64::NAN);

    for (name, rms) in &sensitivity {
        println!("  {name:<10} {rms:>6.2}d   {:+6.2}", rms - baseline);
    }

    let improved: Vec<_> = sensitivity
        .iter()
        .skip(1)
        .filter(|(_, v)| *v < baseline - 0.15)
        .collect();

    if improved.is_empty() {
        println!(
            "\nNo perturbation helps materially, so the camera pose in desk.toml is not what\n\
             is limiting the fit.",
        );
    }
    else {
        println!("\nThese perturbations improve the fit, so the pose is probably wrong:");

        for (name, rms) in improved {
            println!("  {name} -> {rms:.2} deg");
        }
    }

    Ok(())
}

// --- reports ---

/// Prints the per-target angle-space diagnostics and the overall per-sample scatter.
fn report_diagnostics(observations: &[Observation]) {
    println!(
        "\nRaw stream per target (camera frame, before any fitting):\n{:<10} {:>13} {:>13} {:>7} {:>7} {:>6} {:>5} {:>5} {:>7}",
        "output", "yaw mean/sd", "pitch mean/sd", "wantyaw", "wantpit", "spread", "conf", "valid", "missed",
    );

    for o in observations {
        let d = &o.diagnostics;

        println!(
            "{:<10} {:>7.2}/{:<5.2} {:>7.2}/{:<5.2} {:>7.2} {:>7.2} {:>5.2}d {:>5.2} {:>4.0}% {:>7}",
            o.target.output,
            d.yaw_deg_mean, d.yaw_deg_sd,
            d.pitch_deg_mean, d.pitch_deg_sd,
            d.target_yaw_deg, d.target_pitch_deg,
            o.spread_deg,
            d.conf_mean,
            d.valid_fraction * 100.0,
            d.missed,
        );
    }

    let s = sweep::summarise(observations);

    println!(
        "\nper-sample sd: yaw {:.2} deg, pitch {:.2} deg; landing spread {:.2} deg; \n\
         tracked {:.0}% of readings; {} samples missed the desk over {} targets",
        s.yaw_sd_deg, s.pitch_sd_deg, s.spread_deg, s.valid_fraction * 100.0, s.missed, s.targets,
    );

    // The mean eye position is the one thing that must not drift during a sweep.
    if let (Some(first), Some(last)) = (observations.first(), observations.last()) {
        let a = first.diagnostics.eye_mm_mean;
        let b = last.diagnostics.eye_mm_mean;
        let d = ((a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)).sqrt();

        println!("eye moved {d:.0} mm between the first and last target (camera frame)");
    }
}

/// Prints the per-output gain table.
fn report_gains(observations: &[Observation]) {
    let gains = sweep::gains(observations);

    println!("\n{:<10} {:>7} {:>10} {:>10} {:>11} {:>11}",
        "output", "points", "gain px x", "gain px y", "gain deg yaw", "gain deg pit");

    for g in &gains {
        println!("{:<10} {:>7} {:>10} {:>10} {:>11} {:>11}",
            g.name,
            g.points,
            show_gain(g.gain_px_x),
            show_gain(g.gain_px_y),
            show_gain(g.gain_deg_yaw),
            show_gain(g.gain_deg_pitch),
        );
    }

    let worst = gains
        .iter()
        .flat_map(|g| [g.gain_deg_yaw, g.gain_deg_pitch])
        .flatten()
        .fold(f64::INFINITY, f64::min);

    if worst.is_finite() && worst < 0.8 {
        println!(
            "\nAngular gain {worst:.2} is well below 1: the model is under-reporting how far the\n\
             eye turned. A polynomial can only invert that by amplifying the model's noise by\n\
             the same factor, so expect a large RMS however good the fit looks.",
        );
    }
}

/// Prints every model that was tried, why each was refused, and which one won.
fn report_models(report: &FitReport) {
    println!("\n{:<12} {:>10} {:>12} {:>12} {:>8} {:>13}  verdict",
        "angle model", "pixel fit", "in-sample", "held-out", "worst", "gain lo/hi");

    for (i, c) in report.candidates.iter().enumerate() {
        let gain = c
            .gain_bounds
            .map_or("-".to_string(), |(lo, hi)| format!("{lo:.2}/{hi:.2}"));

        let verdict = {
            match &c.rejected {
                None if report.chosen == Some(i) => "kept".to_string(),
                None                             => String::new(),

                Some(gaze_provider_webcam::Rejection::Unfittable) => {
                    "rejected: cannot fit".to_string()
                }

                Some(gaze_provider_webcam::Rejection::AngleGain { min, max }) => {
                    format!("rejected: gain {min:.2} to {max:.2} between targets")
                }

                Some(gaze_provider_webcam::Rejection::PixelFold { min_det, ratio }) => {
                    format!("rejected: pixel map folds ({min_det:.2} min, {ratio:.1}x)")
                }
            }
        };

        let number = |v: f64| {
            if v.is_finite() { format!("{v:.2}d") } else { "-".to_string() }
        };

        println!("{:<12} {:>10} {:>12} {:>12} {:>8} {:>13}  {}",
            c.shape.name(),
            if c.stage_two { "yes" } else { "no" },
            number(c.rms_deg),
            number(c.rms_loo_deg),
            number(c.worst_loo_deg),
            gain,
            verdict,
        );
    }

    match report.chosen.map(|i| &report.candidates[i]) {
        Some(c) => {
            println!(
                "\nChosen on held-out error among the models that behave. The per-output pixel \n\
                 stage {}.",
                if c.stage_two { "earned its place" } else { "did not earn its place" },
            );
        }

        None => {
            println!(
                "\nNo model survived. Every candidate above was either unfittable or \n\
                 misbehaved between the targets, so this calibration corrects nothing and \n\
                 the error reported below is the raw error of the uncorrected stream.",
            );
        }
    }
}

/// Prints the correction's local gain across the range it will be used on.
fn report_gain_profile(calibration: &Calibration, observations: &[Observation]) {
    let profile = calibration.angle.gain_profile(&sweep::gain_points(observations));

    println!("\nCorrection gain across the working range (degrees of correction per degree");
    println!("of reported angle). This is what the model does BETWEEN the targets, which is");
    println!("where the user spends nearly all their time and which no residual can see.");
    println!("Sampled only where the sweep went; the empty corners of the angle box are");
    println!("extrapolation nobody visits.");
    println!("{:>9} {:>9} {:>10} {:>10}", "yaw", "pitch", "yaw gain", "pitch gain");

    // A row per distinct yaw, showing the worst gain seen at that yaw over every pitch.
    let mut yaws: Vec<f64> = profile.iter().map(|(y, _, _, _)| *y).collect();
    yaws.dedup_by(|a, b| (*a - *b).abs() < 1.0e-9);

    for yaw in yaws {
        let at: Vec<&(f64, f64, f64, f64)> = profile
            .iter()
            .filter(|(y, _, _, _)| (y - yaw).abs() < 1.0e-9)
            .collect();

        let Some(worst) = at.iter().min_by(|a, b| a.2.partial_cmp(&b.2).unwrap()) else {
            continue;
        };

        println!("{:>9.1} {:>9.1} {:>10.2} {:>10.2}", worst.0, worst.1, worst.2, worst.3);
    }

    if let Some((lo, hi)) = calibration.angle.gain_bounds(&profile.iter().map(|(y, p, _, _)| (*y, *p)).collect::<Vec<_>>()) {
        println!("\ngain over the visited region: {lo:.2} to {hi:.2}");
    }
}

/// Prints the stage-one residual grouped by target column, for one output.
///
/// The kink in this model's yaw map is a function of yaw alone, so a column of targets at
/// the same horizontal position should all show the same residual if the correction has
/// followed the kink and a spread of residuals if it has not. That is not visible in a
/// table ordered by target index.
fn report_columns(calibration: &Calibration, observations: &[Observation], output: &str) {
    let mut columns: Vec<(f64, Vec<(f64, f64)>)> = Vec::new();

    for o in observations.iter().filter(|o| o.target.output == output) {
        let (wy, cy, _, _) = sweep::angle_residual(calibration, o);

        match columns.iter_mut().find(|(x, _)| (x - o.target.px.x).abs() < 1.0) {
            Some((_, rows)) => rows.push((o.target.px.y, cy - wy)),
            None            => columns.push((o.target.px.x, vec![(o.target.px.y, cy - wy)])),
        }
    }

    if columns.is_empty() {
        return;
    }

    columns.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

    println!("\nStage one yaw residual on {output}, by target column (degrees, + means the");
    println!("correction over-turns). A column that is uniformly off has not followed the");
    println!("kink; scatter within a column is pitch dependence the model has not captured.");
    println!("{:>10} {:>8} {:>8} {:>8}  per target top to bottom", "column px", "mean", "min", "max");

    for (x, mut rows) in columns {
        rows.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap());

        let values: Vec<f64> = rows.iter().map(|(_, e)| *e).collect();
        let mean  = values.iter().sum::<f64>() / values.len() as f64;
        let lo    = values.iter().copied().fold(f64::INFINITY, f64::min);
        let hi    = values.iter().copied().fold(f64::NEG_INFINITY, f64::max);
        let each  = values.iter().map(|e| format!("{e:+.1}")).collect::<Vec<_>>().join(" ");

        println!("{x:>10.0} {mean:>+8.2} {lo:>+8.2} {hi:>+8.2}  {each}");
    }
}

/// Prints wanted against corrected angles per target, after stage one.
fn report_angle_plot(calibration: &Calibration, observations: &[Observation]) {
    println!("\nStage one residual, camera-frame degrees (want -> corrected, and the error):");
    println!("{:<10} {:>8} {:>10} {:>7}   {:>8} {:>10} {:>7}",
        "output", "want yaw", "corrected", "err", "want pit", "corrected", "err");

    for o in observations {
        let (wy, cy, wp, cp) = sweep::angle_residual(calibration, o);

        println!("{:<10} {:>8.2} {:>10.2} {:>+7.2}   {:>8.2} {:>10.2} {:>+7.2}",
            o.target.output, wy, cy, cy - wy, wp, cp, cp - wp);
    }
}

/// Prints the headline numbers and what they mean on each screen.
fn report_headline(geometry: &gaze_core::DesktopGeometry, calibration: &Calibration) {
    println!("\nheld-out RMS : {:.2} deg   <- the number that matters", calibration.rms_loo_deg);
    println!("in-sample RMS: {:.2} deg", calibration.rms_deg);

    if calibration.rms_px.is_finite() && calibration.rms_px > 0.0 {
        println!("in-sample px : {:.0} px (targets that reached a panel)", calibration.rms_px);
    }

    println!("\nHeld-out error on each screen, at its centre:");

    for (name, px) in sweep::px_equivalent(geometry, calibration.rms_loo_deg) {
        println!("  {name:<10} {px:>6.0} px");
    }
}

// --- shared ---

/// Builds the fake's distortion from the CLI flags.
fn fake_distortion(jitter_deg: f64, gain: f64, curve: CurveArg, yaw_shift: f64) -> Distortion {
    Distortion {
        jitter_deg          : jitter_deg,
        gain                : gain,
        curve               : curve.into(),
        yaw_shift_per_pitch : yaw_shift,
        ..Distortion::default()
    }
}

/// Default readings path for a given calibration path.
fn readings_path(out: &Path) -> PathBuf {
    let mut p = out.to_path_buf();

    p.set_extension("readings.jsonl");

    p
}

/// Writes the raw sweep samples as JSONL.
fn write_records(path: &Path, records: &[SweepRecord]) -> Result<()> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        std::fs::create_dir_all(parent)
            .with_context(|| format!("cannot create {}", parent.display()))?;
    }

    let mut w = BufWriter::new(
        File::create(path).with_context(|| format!("cannot create {}", path.display()))?,
    );

    for r in records {
        writeln!(w, "{}", serde_json::to_string(r)?)?;
    }

    w.flush()?;

    Ok(())
}

/// Reads a raw sweep readings log.
fn read_records(path: &Path) -> Result<Vec<SweepRecord>> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    let mut out = Vec::new();

    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        out.push(
            serde_json::from_str::<SweepRecord>(line)
                .with_context(|| format!("{} line {}", path.display(), i + 1))?,
        );
    }

    Ok(out)
}

/// Rebuilds observations from a calibration file's stored per-target means.
///
/// One pseudo-sample per target, which is what the file preserves. Enough to compare
/// fitting models on a sweep that predates the readings log; not enough to reproduce
/// per-sample scatter.
fn observations_from_calibration(
    geometry : &gaze_core::DesktopGeometry,
    camera   : &CameraPose,
    path     : &Path,
)
    -> Result<Vec<Observation>>
{
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    let doc: LegacyCalibration = toml::from_str(&text)
        .with_context(|| format!("cannot parse {}", path.display()))?;

    let mut out = Vec::new();

    for t in &doc.targets {
        let target = gaze_provider_webcam::SweepTarget {
            output : t.output.clone(),
            px     : GlobalPx { x: t.target_px[0], y: t.target_px[1] },
        };

        let d      = &t.diagnostics;
        let sample = gaze_provider_webcam::AngleSample {
            eye_cam_mm     : glam::DVec3::from_array(d.eye_mm_mean),
            gaze_cam       : gaze_provider_webcam::gaze_dir_from_yaw_pitch_deg(
                d.yaw_deg_mean,
                d.pitch_deg_mean,
            ),
            yaw_deg        : d.yaw_deg_mean,
            pitch_deg      : d.pitch_deg_mean,
            want_yaw_deg   : d.target_yaw_deg,
            want_pitch_deg : d.target_pitch_deg,
            missed         : false,
            head_rot       : Some(d.head_rot_mean),
            raw            : None,
        };

        if let Some(o) = sweep::rebuild(geometry, camera, target, vec![sample]) {
            out.push(o);
        }
    }

    Ok(out)
}

/// Formats an optional gain, or a dash when there was not enough spread to measure one.
fn show_gain(gain: Option<f64>) -> String {
    match gain {
        Some(g) => format!("{g:.3}"),
        None    => "-".to_string(),
    }
}

/// Prints the per-target table and the headline RMS.
fn report_residuals(calibration: &Calibration, outcome: &sweep::SweepOutcome) {
    println!("\n{:<10} {:>16} {:>16} {:>9} {:>10} {:>9} {:>9} {:>5}",
        "output", "target px", "observed px", "spread px", "spread deg", "resid px", "resid deg", "n");

    for t in &calibration.targets {
        let observed = {
            match t.observed_px {
                Some(p) => format!("{:>7.0},{:>7.0}", p[0], p[1]),
                None    => format!("{:>15}", "off-desk"),
            }
        };

        println!(
            "{:<10} {:>7.0},{:>7.0}  {} {:>9} {:>10.2} {:>9} {:>9.2} {:>5}",
            t.output,
            t.target_px[0], t.target_px[1],
            observed,
            t.spread_px.map_or("-".to_string(), |v| format!("{v:.1}")),
            t.spread_deg,
            if t.residual_px.is_finite() { format!("{:.1}", t.residual_px) } else { "-".to_string() },
            t.residual_deg,
            t.samples,
        );
    }

    for o in &calibration.outputs {
        println!("  pixel stage {:<10} {:?} from {} points", o.name, o.map.degree, o.points);
    }

    if !outcome.missed.is_empty() {
        println!("{} targets produced nothing (skipped or no valid samples).", outcome.missed.len());
    }
}

/// After a `--fake` calibration, prints the distortion that was actually applied so the
/// recovered numbers can be checked against it.
fn report_against_fake(jitter_deg: f64, gain: f64, curve: CurveArg, yaw_shift: f64) {
    let d = fake_distortion(jitter_deg, gain, curve, yaw_shift);

    println!("\nFake sidecar ground truth:");
    println!("  applied angular bias : yaw {:+.3} deg, pitch {:+.3} deg", d.yaw_deg, d.pitch_deg);
    println!("  applied pixel warp   : x {:?}", d.warp_x);
    println!("                         y {:?}", d.warp_y);
    println!("  per-sample jitter    : {:.2} deg", d.jitter_deg);
    println!("  angular gain         : {:.2} ({:?})", d.gain, d.curve);
    println!("  yaw shift per pitch  : {:.2}", d.yaw_shift_per_pitch);
}

// --- run ---

/// Streams samples, printing one line each.
fn run(args: RunArgs) -> Result<()> {
    let (geometry, camera) = load_desk(&args.common.config)?;
    let socket             = resolve_socket(&args.common, args.fake);

    let fake = {
        if args.fake {
            Some(
                FakeSidecar::create()
                    .socket(&socket)
                    .geometry(geometry.clone())
                    .camera(camera.clone())
                    .distortion(Distortion::default())
                    .rate_hz(30.0)
                    .start()
                    .context("cannot start the fake sidecar")?,
            )
        }
        else {
            None
        }
    };

    let calibration_path = {
        if args.calibration.eq_ignore_ascii_case("none") {
            None
        }
        else {
            Some(PathBuf::from(&args.calibration))
        }
    };

    // A missing calibration is a warning, not a failure: running uncalibrated is exactly
    // what you do before the first sweep.
    let calibration = {
        match calibration_path.as_ref().filter(|p| p.exists()) {
            Some(p) => {
                println!("using calibration {}", p.display());

                Some(Calibration::load(p).with_context(|| format!("cannot load {}", p.display()))?)
            }

            None => {
                if let Some(p) = calibration_path.as_ref() {
                    eprintln!("no calibration at {}, running uncalibrated", p.display());
                }

                None
            }
        }
    };

    let truth = load_truth(&args.truth_file)?;

    let mut builder = WebcamProvider::create()
        .socket(&socket)
        .geometry(geometry.clone())
        .camera(camera)
        .sigma_deg(args.sigma);

    if let Some(cal) = calibration {
        builder = builder.calibration_model(cal);
    }

    let mut provider = builder.start().context("cannot start the webcam provider")?;

    let overlay = {
        if args.overlay {
            connect_overlay()
        }
        else {
            None
        }
    };

    let mut recorder = {
        match args.record.as_ref() {
            Some(p) => Some(BufWriter::new(
                File::create(p).with_context(|| format!("cannot create {}", p.display()))?,
            )),
            None => None,
        }
    };

    println!("reading {} (ctrl-c to stop)", socket.display());

    let start    = Instant::now();
    let deadline = (args.seconds > 0.0).then(|| start + Duration::from_secs_f64(args.seconds));

    while deadline.is_none_or(|d| Instant::now() < d) {
        let Some(sample) = provider.next() else {
            break;
        };

        print_sample(&geometry, &provider, &sample, truth.as_deref());

        if let Some(w) = recorder.as_mut()
            && let Ok(line) = to_jsonl_line(&sample)
        {
            let _ = writeln!(w, "{line}");
        }

        if let Some((handle, _)) = overlay.as_ref() {
            let _ = handle.set(OverlayState {
                gaze  : sample.point.filter(|_| sample.valid),
                label : sample.valid.then(|| format!("sigma {:.1} deg", sample.sigma_deg)),
                ..OverlayState::default()
            });
        }
    }

    provider.stop();

    if let Some((handle, join)) = overlay {
        handle.stop();
        let _ = join.join();
    }

    if let Some(mut w) = recorder {
        let _ = w.flush();
    }

    let stats = provider.stats();
    println!(
        "\n{} lines, {} connections, {} malformed",
        stats.lines, stats.connects, stats.bad_lines,
    );

    drop(fake);

    Ok(())
}

/// Formats one sample.
fn print_sample(
    geometry : &DesktopGeometry,
    provider : &WebcamProvider,
    sample   : &GazeSample,
    truth    : Option<&[TruthPoint]>,
)
{
    let Some(point) = sample.point.filter(|_| sample.valid) else {
        println!("t={:7.3}  INVALID", sample.t_s);

        return;
    };

    let meta   = provider.last_meta().unwrap_or_default();
    let output = provider.last_output().unwrap_or_else(|| "-".to_string());

    let error = {
        match truth.and_then(|t| truth_at(t, sample.t_s)) {
            Some(t) => {
                let dx  = point.x - t.x;
                let dy  = point.y - t.y;
                let px  = (dx * dx + dy * dy).sqrt();
                let deg = geometry.angle_between_deg(geometry.eye(), point, t).unwrap_or(f64::NAN);

                format!("  err={px:6.1} px / {deg:4.2} deg")
            }

            None => String::new(),
        }
    };

    println!(
        "t={:7.3} seq={:<6} ({:8.1},{:8.1}) {:<9} sigma={:4.2} off={:5.1} deg conf={:4.2} lat={:5.1} ms{}{}",
        sample.t_s,
        meta.seq,
        point.x,
        point.y,
        output,
        sample.sigma_deg,
        meta.off_axis_deg,
        meta.conf,
        meta.lat_ms,
        if meta.clamped { " CLAMPED" } else { "" },
        error,
    );
}

// --- fake-sidecar ---

/// Serves the synthetic stream until interrupted.
fn fake_sidecar(args: FakeArgs) -> Result<()> {
    let (geometry, camera) = load_desk(&args.common.config)?;
    let socket             = args.common.socket.clone().unwrap_or_else(|| PathBuf::from(FAKE_SOCKET));

    let base = {
        if args.clean {
            Distortion::none()
        }
        else {
            Distortion::default()
        }
    };

    let distortion = Distortion {
        jitter_deg          : args.jitter_deg.unwrap_or(base.jitter_deg),
        gain                : args.gain,
        curve               : args.gain_curve.into(),
        yaw_shift_per_pitch : args.yaw_shift,
        ..base
    };

    let fake = FakeSidecar::create()
        .socket(&socket)
        .geometry(geometry)
        .camera(camera)
        .distortion(distortion)
        .rate_hz(args.rate)
        .start()
        .context("cannot start the fake sidecar")?;

    println!(
        "fake sidecar on {} at {:.0} Hz{}, jitter {:.2} deg, gain {:.2}",
        fake.path().display(),
        args.rate,
        if args.clean { ", undistorted" } else { ", distorted" },
        distortion.jitter_deg,
        distortion.gain,
    );
    println!("ctrl-c to stop");

    let start    = Instant::now();
    let deadline = (args.seconds > 0.0).then(|| start + Duration::from_secs_f64(args.seconds));

    while deadline.is_none_or(|d| Instant::now() < d) {
        thread::sleep(Duration::from_millis(200));
    }

    Ok(())
}

// --- shared ---

/// Loads the desk geometry and the camera pose from one `desk.toml`.
fn load_desk(path: &Path) -> Result<(DesktopGeometry, CameraPose)> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("cannot read {}", path.display()))?;

    let geometry = DesktopGeometry::from_toml(&text)
        .with_context(|| format!("cannot parse {}", path.display()))?;

    let camera = CameraPose::from_desk_toml(&text)
        .with_context(|| format!("no usable [camera] block in {}", path.display()))?;

    Ok((geometry, camera))
}

/// Picks the socket path: whatever was asked for, or the fake's default under `--fake`,
/// or the real sidecar's.
fn resolve_socket(common: &CommonArgs, fake: bool) -> PathBuf {
    common.socket.clone().unwrap_or_else(|| {
        PathBuf::from(if fake { FAKE_SOCKET } else { DEFAULT_SOCKET })
    })
}

/// Brings up the overlay, or warns and carries on without one. There is no compositor
/// over SSH and no reason for that to stop a calibration against the fake.
fn connect_overlay() -> Option<(OverlayHandle, thread::JoinHandle<()>)> {
    match Overlay::spawn() {
        Ok(pair) => Some(pair),

        Err(e) => {
            eprintln!("no overlay ({e}); continuing without on-screen targets");

            None
        }
    }
}

/// Waits up to `timeout` for the provider to connect, so the first target is not shown to
/// a socket that is not up yet.
fn wait_for_connection(provider: &WebcamProvider, timeout: Duration) {
    let deadline = Instant::now() + timeout;

    while Instant::now() < deadline {
        if provider.connected() {
            return;
        }

        thread::sleep(Duration::from_millis(20));
    }

    eprintln!("warning: the sidecar has not connected yet; targets may collect nothing");
}

/// Loads a ground-truth JSONL, or `None` for the literal `none`.
fn load_truth(spec: &str) -> Result<Option<Vec<TruthPoint>>> {
    if spec.eq_ignore_ascii_case("none") {
        return Ok(None);
    }

    let text = std::fs::read_to_string(spec)
        .with_context(|| format!("cannot read truth file {spec}"))?;

    let mut points = Vec::new();

    for (i, line) in text.lines().enumerate() {
        if line.trim().is_empty() {
            continue;
        }

        points.push(
            serde_json::from_str::<TruthPoint>(line)
                .with_context(|| format!("{spec} line {}", i + 1))?,
        );
    }

    Ok(Some(points))
}

/// Truth point nearest in time to `t_s`, or `None` when the file is empty or the nearest
/// entry is too far away to mean anything.
fn truth_at(truth: &[TruthPoint], t_s: f64) -> Option<GlobalPx> {
    let nearest = truth
        .iter()
        .min_by(|a, b| (a.t_s - t_s).abs().partial_cmp(&(b.t_s - t_s).abs()).unwrap())?;

    ((nearest.t_s - t_s).abs() < 0.1).then_some(GlobalPx { x: nearest.x, y: nearest.y })
}
