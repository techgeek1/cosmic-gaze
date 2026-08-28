//! Manual test CLI for the ET5 provider: stream inspection, display-area setup,
//! calibration blob diagnostics and upload, the compound calibration sweep, and a
//! live overlay view. Everything here needs the tracker on the bus in runtime mode;
//! the sweep and the view additionally need a live compositor and a seated user.
//!
//! `blob-info` and `blob-watch` are read-only and safe to run unattended.
//! `blob-push` writes the device's eye model and must not be interrupted.

use std::io::Write;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use gaze_core::{DesktopGeometry, Ray};
use gaze_overlay::{Overlay, OverlayState};
use gaze_provider_et5::blob::{
    BlobCheck, BlobReport, CalibrationResult, EyeResult, body, body_sha256_hex,
    decode_trailer, first_difference,
};
use gaze_provider_et5::calibration::{
    CALIBRATION_FORMAT, Et5Calibration, OutputCalibration, OutputPose, VIRTUAL_AREA,
};
use gaze_provider_et5::field::FieldMap;
use gaze_provider_et5::device::{ConnectOptions, Device};
use gaze_provider_et5::dataset;
use gaze_provider_et5::gaze::combined_ray;
use gaze_provider_et5::provider::Et5Provider;
use gaze_provider_et5::record::{self, RecordConfig};
use gaze_provider_et5::retrain::{self, RetrainConfig};
use gaze_provider_et5::sweep::{self, SweepConfig, SweepError};
use gaze_provider_et5::ttp::{DisplayArea, DisplayRect};
use gaze_provider_synthetic::GazeProvider;
use gaze_snap::FilterStack;
use signal_hook::consts::SIGINT;
use signal_hook::flag;

/// Conventional on-device blob backup, whose body hash keys the pass history.
const DEFAULT_BLOB_PATH: &str = "config/calibration-et5.bin";

/// Viewing distance the result table's normalised errors are turned into degrees
/// at, millimetres. The eye is not measured here, so this is the desk's nominal
/// distance and the degrees are indicative, not the health check's numbers.
const NOMINAL_VIEW_MM: f64 = 650.0;

/// Directory archived passes accumulate in for pooled head-gain fits. Under
/// `config/calibration*` so the standard gitignore covers it.
const DEFAULT_HISTORY_DIR: &str = "config/calibration-et5-history";

/// Directory recording sessions are written to and read back from.
const DEFAULT_SESSIONS_DIR: &str = "config/sessions";

/// Conventional client-side calibration, whose trained plane a session runs under.
const DEFAULT_CALIBRATION_PATH: &str = "config/calibration-et5.toml";

/// Display area defaults for `set-display-area`: the physical small panel with the
/// tracker on its top edge (the original bring-up arrangement).
const DEFAULT_AREA: DisplayRect = DisplayRect {
    w_mm  : 237.0,
    h_mm  : 148.0,
    ox_mm : -118.5,
    oy_mm : -148.0,
    z_mm  : 0.0,
};

#[derive(Parser)]
#[command(about = "Tobii ET5 manual test tool for cosmic-gaze")]
struct Cli {
    /// Desk geometry file.
    #[arg(long, default_value = "config/desk.toml")]
    config : PathBuf,

    #[command(subcommand)]
    command : Command,
}

#[derive(Subcommand)]
enum Command {
    /// Connect, print the device display area, and stream per-second tracking
    /// stats (rate, tracked %, median eye origin) — the mount-adjustment feedback
    /// loop: tune the tilt until the rate is high and the origin lands sanely.
    Info {
        /// How long to sample.
        #[arg(long, default_value_t = 4.0)]
        seconds : f64,
    },

    /// Stream decoded frames: JSONL to a file, one summary line per second to stderr.
    Dump {
        /// How long to stream.
        #[arg(long, default_value_t = 10.0)]
        seconds : f64,

        /// Write one JSON object per frame to this file.
        #[arg(long)]
        jsonl   : Option<PathBuf>,
    },

    /// Declare the display plane on the device (defaults: the small panel).
    SetDisplayArea {
        #[arg(long, default_value_t = DEFAULT_AREA.w_mm)]
        w  : f64,
        #[arg(long, default_value_t = DEFAULT_AREA.h_mm)]
        h  : f64,
        #[arg(long, default_value_t = DEFAULT_AREA.ox_mm, allow_hyphen_values = true)]
        ox : f64,
        #[arg(long, default_value_t = DEFAULT_AREA.oy_mm, allow_hyphen_values = true)]
        oy : f64,
        #[arg(long, default_value_t = DEFAULT_AREA.z_mm, allow_hyphen_values = true)]
        z  : f64,
    },

    /// Download the on-device calibration blob to a file.
    CalBackup {
        file : PathBuf,
    },

    /// Retrieve the on-device calibration blob twice, report the identity of its
    /// model body and decode the firmware's per-point calibration result table off
    /// its trailer, optionally against a saved blob. Read-only on the device.
    BlobInfo {
        /// A saved blob to compare the first retrieve against.
        #[arg(long)]
        file        : Option<PathBuf>,

        /// Calibration file whose trained plane the saved blob's table is read
        /// against, so its errors can be quoted in degrees.
        #[arg(long, default_value = DEFAULT_CALIBRATION_PATH)]
        calibration : PathBuf,
    },

    /// Retrieve the blob, stream for a while, retrieve again, and report the diff:
    /// does the firmware mutate its eye model during ordinary use? Read-only.
    BlobWatch {
        /// How long to stream between the two retrieves.
        #[arg(long, default_value_t = 5.0)]
        minutes : f64,
    },

    /// Upload a calibration blob to the device in the connect-time order (blob,
    /// then the plane, then eyes and unpause) and verify it by reading it back.
    /// Writes the device's eye model.
    BlobPush {
        /// The blob to upload.
        file        : PathBuf,

        /// Upload the blob a second time after eye-enable and unpause, as the
        /// Windows driver does.
        #[arg(long)]
        double      : bool,

        /// Calibration file whose trained plane is declared after the upload;
        /// falls back to the oversized virtual plane when it is missing.
        #[arg(long, default_value = "config/calibration-et5.toml")]
        calibration : PathBuf,
    },

    /// Retrain the on-device eye model: six gaze-gated rounds (centre, mid-edges,
    /// corners) on black and then white over a 600x340 mm training area, with
    /// `cal_points_apply` after each round, followed by a 3x3 health check. The
    /// session is seeded with the blob it is about to replace, so the rounds are
    /// collected through a working model. Writes the device's model and is meant to
    /// be run once per mount. Keys: Enter forces the current point in, `s` skips it,
    /// `q` aborts without committing.
    Calibrate {
        /// Where the client-side calibration is written.
        #[arg(long, default_value = DEFAULT_CALIBRATION_PATH)]
        out : PathBuf,

        /// Where the fresh on-device blob backup is written.
        #[arg(long, default_value = DEFAULT_BLOB_PATH)]
        blob : PathBuf,

        /// Blob uploaded into the fresh session after cal_clear, so the acceptance
        /// gate has a working model to read gaze from. Defaults to `--blob` when it
        /// exists and decodes with a result trailer.
        #[arg(long)]
        seed : Option<PathBuf>,

        /// Collect the rounds on a cleared model, with no seed upload at all.
        #[arg(long, conflicts_with = "seed")]
        no_seed : bool,

        /// Connector the tracker is physically mounted on, whose plane is declared.
        #[arg(long, default_value = "DP-1")]
        device_output : String,

        /// Training area as WxH in millimetres, measured along the panel surface.
        #[arg(long, default_value = "600x340")]
        area : String,

        /// Train over the whole panel instead of the training area.
        #[arg(long, conflicts_with = "area")]
        area_full : bool,

        /// Additionally require the median reported gaze to sit within this many
        /// degrees of the target. Off by default: during a retrain the model
        /// reporting the gaze is the one being replaced, so absolute accuracy is not
        /// a thing to gate on. 3 is the value the 2026-08-28 01:24 run used.
        #[arg(long)]
        accept_deg : Option<f64>,

        /// How long a point waits before it says so, seconds. Not a timeout: nothing
        /// is ever skipped without `s`.
        #[arg(long, default_value_t = retrain::POINT_TIMEOUT_S)]
        point_timeout_s : f64,

        /// How long a point waits for the device to report any gaze at all before
        /// falling back to a dwell (both eyes tracked for 1.5 s), seconds.
        #[arg(long, default_value_t = retrain::GAZE_TIMEOUT_S)]
        gaze_timeout_s : f64,

        /// First round that ends in a cal_points_apply. 1 applies after every round,
        /// as Talon does. Raise it to 2 to test the hypothesis that a one-point apply
        /// is what leaves the device reporting no live gaze: rounds before it hold
        /// their points until the first apply instead of losing them.
        #[arg(long, default_value_t = 1)]
        apply_from_round : usize,

        /// Accepted points below which the ceremony commits nothing and keeps the
        /// previous blob and calibration.
        #[arg(long, default_value_t = retrain::MIN_POINTS)]
        min_points : usize,

        /// After each round, ask the device what point it would like next and log
        /// the raw reply. Exploratory; nothing depends on it.
        #[arg(long)]
        suggest : bool,

        /// Print the plane, the training area, the points, the seed decision and the
        /// round schedule, then exit. Touches neither the device nor the compositor.
        #[arg(long)]
        dry_run : bool,
    },

    /// DEPRECATED (removed in Phase D): re-fit the old correction field and head
    /// gain from saved sweep readings. Superseded by `record` plus the Phase C/D
    /// model; nothing in the new calibration flow produces or consumes these fits.
    Refit {
        /// Raw readings from a previous `calibrate` run.
        #[arg(long, default_value = "config/calibration-et5.readings.jsonl")]
        readings : PathBuf,

        /// Where the re-fitted calibration is written.
        #[arg(long, default_value = "config/calibration-et5.toml")]
        out : PathBuf,
    },

    /// DEPRECATED (removed in Phase D): bank head-gain training data as a wandering
    /// dot with posture prompts. Use `record` instead — it writes a session file the
    /// model actually trains from, with the blob hash and the backgrounds attached.
    Collect {
        /// How long to run.
        #[arg(long, default_value_t = 3.0)]
        minutes : f64,

        /// Calibration whose trained plane the session runs under.
        #[arg(long, default_value = "config/calibration-et5.toml")]
        calibration : PathBuf,

        /// On-device blob backup identifying the model the data belongs to.
        #[arg(long, default_value = DEFAULT_BLOB_PATH)]
        blob : PathBuf,

        /// Directory the pass is archived into.
        #[arg(long, default_value = DEFAULT_HISTORY_DIR)]
        history : PathBuf,
    },

    /// Record one training session: a stop grid on black, a prompted wander on white,
    /// then the same grid on white. Nothing is fitted and nothing is uploaded; the
    /// session file is what a model is trained from later. Keys during the session:
    /// Enter advances, `s` skips a stop, `q` ends the current phase early.
    Record {
        /// Total session length, minutes. The grids take what they take; the wander
        /// gets the rest.
        #[arg(long, default_value_t = 5.0)]
        minutes : f64,

        /// Calibration whose trained plane the session runs under.
        #[arg(long, default_value = DEFAULT_CALIBRATION_PATH)]
        calibration : PathBuf,

        /// Connector to record on; defaults to the calibration's trained display.
        #[arg(long)]
        display : Option<String>,

        /// Directory the session file is written into.
        #[arg(long, default_value = DEFAULT_SESSIONS_DIR)]
        out : PathBuf,

        /// Free-text note stored in the session: lighting, time of day, anything odd.
        #[arg(long, default_value = "")]
        note : String,

        /// Half-angle of the gaze cone the stop grid is laid inside, degrees.
        #[arg(long, default_value_t = record::CONE_MAX_DEG)]
        cone_deg : f64,

        /// Stop grid, columns x rows.
        #[arg(long, default_value = "4x3")]
        grid : String,

        /// Record `glasses = true` without asking on the terminal.
        #[arg(long)]
        glasses : bool,

        /// Record `glasses = false` without asking on the terminal.
        #[arg(long, conflicts_with = "glasses")]
        no_glasses : bool,

        /// Print the schedule and exit. Needs neither the device nor a compositor.
        #[arg(long)]
        dry_run : bool,
    },

    /// Session-file utilities.
    Sessions {
        #[command(subcommand)]
        command : SessionsCommand,
    },

    /// Training-row utilities.
    Dataset {
        #[command(subcommand)]
        command : DatasetCommand,
    },

    /// Draw the live gaze point on every display until interrupted.
    View {
        /// Client-side calibration to apply; omit for the raw mapping.
        #[arg(long)]
        calibration : Option<PathBuf>,
    },
}

#[derive(Subcommand)]
enum SessionsCommand {
    /// Convert a pre-`record` readings file into a session file, so the data banked
    /// before sessions existed can serve as session zero. The plane and the blob hash
    /// the readings were taken under have to be supplied: the readings format does not
    /// carry them.
    Import {
        /// The readings file to convert.
        readings : PathBuf,

        /// Directory the session file is written into.
        #[arg(long, default_value = DEFAULT_SESSIONS_DIR)]
        out : PathBuf,

        /// Blob backup of the on-device model the readings were taken under.
        #[arg(long, default_value = DEFAULT_BLOB_PATH)]
        blob : PathBuf,

        /// Calibration supplying the plane the readings were taken under.
        #[arg(long, default_value = DEFAULT_CALIBRATION_PATH)]
        calibration : PathBuf,

        /// Free-text note stored in the synthesised meta line.
        #[arg(long, default_value = "imported from the pre-record readings archive")]
        note : String,
    },
}

#[derive(Subcommand)]
enum DatasetCommand {
    /// Turn session files into model rows and write them as CSV.
    Export {
        /// Session files, or directories of them.
        #[arg(long, num_args = 1.., default_value = DEFAULT_SESSIONS_DIR)]
        sessions : Vec<PathBuf>,

        /// Where the CSV goes.
        #[arg(long)]
        csv : PathBuf,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "info".into()),
        )
        .with_writer(std::io::stderr)
        .init();

    let cli = Cli::parse();

    match cli.command {
        Command::Info { seconds }           => info(seconds),
        Command::Dump { seconds, jsonl }    => dump(&cli.config, seconds, jsonl.as_deref()),
        Command::SetDisplayArea { w, h, ox, oy, z } => {
            set_display_area(DisplayRect { w_mm: w, h_mm: h, ox_mm: ox, oy_mm: oy, z_mm: z })
        }
        Command::CalBackup { file }         => cal_backup(&file),
        Command::BlobInfo { file, calibration } => {
            blob_info(file.as_deref(), &calibration)
        }
        Command::BlobWatch { minutes }      => blob_watch(minutes),
        Command::BlobPush { file, double, calibration } => {
            blob_push(&file, double, &calibration)
        }
        Command::Calibrate {
            out,
            blob,
            seed,
            no_seed,
            device_output,
            area,
            area_full,
            accept_deg,
            point_timeout_s,
            gaze_timeout_s,
            apply_from_round,
            min_points,
            suggest,
            dry_run,
        } => calibrate(CalibrateArgs {
            config           : cli.config.clone(),
            out              : out,
            blob             : blob,
            seed             : seed,
            no_seed          : no_seed,
            device_output    : device_output,
            area             : area,
            area_full        : area_full,
            accept_deg       : accept_deg,
            point_timeout_s  : point_timeout_s,
            gaze_timeout_s   : gaze_timeout_s,
            apply_from_round : apply_from_round,
            min_points       : min_points,
            suggest          : suggest,
            dry_run          : dry_run,
        }),
        Command::Refit { readings, out }    => refit_cmd(&cli.config, &readings, &out),
        Command::Collect { minutes, calibration, blob, history } => {
            collect_cmd(&cli.config, minutes, &calibration, &blob, &history)
        }
        Command::Record {
            minutes,
            calibration,
            display,
            out,
            note,
            cone_deg,
            grid,
            glasses,
            no_glasses,
            dry_run,
        } => record_cmd(&cli.config, minutes, &calibration, display, &out, note,
                        cone_deg, &grid, glasses, no_glasses, dry_run),
        Command::Sessions { command }       => sessions_cmd(&cli.config, command),
        Command::Dataset { command }        => dataset_cmd(&cli.config, command),
        Command::View { calibration }       => view(&cli.config, calibration.as_deref()),
    }
}

/// Loads the desk geometry file.
fn load_geometry(path: &std::path::Path) -> Result<DesktopGeometry> {
    let text = std::fs::read_to_string(path)
        .with_context(|| format!("reading {}", path.display()))?;

    DesktopGeometry::from_toml(&text).context("parsing desk geometry")
}

/// The sensor-frame pitch from the desk config, degrees. Lives in `desk.toml` as a
/// top-level `tracker_pitch_deg`; the core geometry parser ignores keys it does not
/// know, so it is read separately here.
///
/// Goes through `toml::from_str` rather than `str::parse`: since toml 0.9 `FromStr for
/// Value` parses a single TOML *value*, not a document, so parsing a config file that way
/// fails at line 1 column 1 and this silently returned 0. That is what left the trained
/// plane in `calibration-et5.toml` declared in the desk frame instead of the sensor
/// frame.
fn load_tracker_pitch(path: &std::path::Path) -> f64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|v| v.get("tracker_pitch_deg").and_then(toml::Value::as_float))
        .unwrap_or(0.0)
}

// --- Commands ---

fn info(seconds: f64) -> Result<()> {
    let mut device = Device::connect().context("connecting to the ET5")?;
    println!("connected and handshaken");

    match device.display_area() {
        Ok(area) => {
            println!(
                "display area (mm): tl=({:.1}, {:.1}, {:.1}) tr=({:.1}, {:.1}, {:.1}) \
                 bl=({:.1}, {:.1}, {:.1})",
                area.tl_mm[0], area.tl_mm[1], area.tl_mm[2],
                area.tr_mm[0], area.tr_mm[1], area.tr_mm[2],
                area.bl_mm[0], area.bl_mm[1], area.bl_mm[2],
            );

            // A freshly booted device carries a degenerate ~4 mm default area, and
            // the firmware reports validity=4 for everything under it (measured:
            // 34 Hz search-mode stream, illuminators on, zero detection at any
            // orientation). The real sessions declare a plane at startup; this
            // diagnostic must too, or it lies about a healthy tracker.
            let w = (area.tr_mm[0] - area.tl_mm[0]).abs();
            let h = (area.tl_mm[1] - area.bl_mm[1]).abs();

            if w < 50.0 || h < 50.0 {
                println!("boot-default (degenerate) area detected; declaring the \
                          virtual plane so tracking can run");
                device.set_display_area(VIRTUAL_AREA)?;
                std::thread::sleep(Duration::from_millis(200));
            }
        }
        Err(e)   => println!("display area unavailable: {e}"),
    }

    // Per-second stats: search mode shows ~30-40 Hz with 0% tracked; a locked face
    // runs 90+ Hz with most frames valid. The origin line is what pins down the
    // mount pitch (see desk.toml).
    let rx       = device.gaze_stream();
    let deadline = Instant::now() + Duration::from_secs_f64(seconds);

    while Instant::now() < deadline {
        let second  = Instant::now() + Duration::from_secs(1);
        let mut n       = 0u32;
        let mut valid   = 0u32;
        let mut origins : Vec<[f64; 3]> = Vec::new();

        while Instant::now() < second {
            let Ok(frame) = rx.recv_timeout(Duration::from_millis(100)) else {
                continue;
            };

            n += 1;

            if frame.any_valid() {
                valid += 1;
            }

            let l = frame.left_valid().then_some(frame.eye_origin_l_mm).flatten();
            let r = frame.right_valid().then_some(frame.eye_origin_r_mm).flatten();

            if let (Some(l), Some(r)) = (l, r) {
                origins.push([
                    (l[0] + r[0]) * 0.5,
                    (l[1] + r[1]) * 0.5,
                    (l[2] + r[2]) * 0.5,
                ]);
            }
        }

        let med = |mut v: Vec<f64>| {
            v.sort_by(|a, b| a.partial_cmp(b).unwrap());
            v[v.len() / 2]
        };

        let origin = {
            if origins.is_empty() {
                "-".to_string()
            }
            else {
                format!(
                    "({:.0}, {:.0}, {:.0})",
                    med(origins.iter().map(|o| o[0]).collect()),
                    med(origins.iter().map(|o| o[1]).collect()),
                    med(origins.iter().map(|o| o[2]).collect()),
                )
            }
        };

        println!(
            "rate {:>3} Hz  tracked {:>3.0}%  eye origin (sensor mm) {origin}",
            n,
            100.0 * valid as f64 / n.max(1) as f64,
        );
    }

    Ok(())
}

fn dump(config: &std::path::Path, seconds: f64, jsonl: Option<&std::path::Path>)
    -> Result<()>
{
    let geometry   = load_geometry(config)?;
    let device = Device::connect().context("connecting to the ET5")?;
    let rx         = device.gaze_stream();

    let mut out: Option<std::io::BufWriter<std::fs::File>> = {
        match jsonl {
            Some(path) => Some(std::io::BufWriter::new(
                std::fs::File::create(path)
                    .with_context(|| format!("creating {}", path.display()))?,
            )),
            None       => None,
        }
    };

    let t0       = Instant::now();
    let deadline = t0 + Duration::from_secs_f64(seconds);

    let mut n           = 0u64;
    let mut valid       = 0u64;
    let mut last_report = t0;

    while Instant::now() < deadline {
        let Ok(frame) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };

        let t_s = t0.elapsed().as_secs_f64();
        n += 1;

        if frame.any_valid() {
            valid += 1;
        }

        // Desk intersection of the combined ray, for the JSONL record.
        let px = combined_ray(&frame)
            .and_then(|(origin, dir, _)| {
                geometry.intersect(&Ray { origin: origin, dir: dir })
            })
            .map(|hit| (hit.output, hit.px));

        if let Some(w) = out.as_mut() {
            let record = serde_json::json!({
                "t_s"    : t_s,
                "frame"  : frame,
                "output" : px.as_ref().map(|(o, _)| o.clone()),
                "px"     : px.as_ref().map(|(_, p)| [p.x, p.y]),
            });

            writeln!(w, "{record}")?;
        }

        if last_report.elapsed() >= Duration::from_secs(1) {
            let gaze = frame.gaze_2d_norm.unwrap_or([f64::NAN, f64::NAN]);

            eprintln!(
                "t={:.1}s n={n} rate={:.1}Hz valid={:.0}% gaze2d=({:.3}, {:.3}) px={:?}",
                t_s,
                n as f64 / t_s,
                100.0 * valid as f64 / n as f64,
                gaze[0], gaze[1],
                px.map(|(_, p)| (p.x.round(), p.y.round())),
            );

            last_report = Instant::now();
        }
    }

    eprintln!("done: {n} frames, {:.0}% with a tracked eye",
              100.0 * valid as f64 / n.max(1) as f64);

    Ok(())
}

fn set_display_area(rect: DisplayRect) -> Result<()> {
    let mut device = Device::connect().context("connecting to the ET5")?;
    device.set_display_area(rect)?;

    // The set is fire and forget on the wire; the read-back is the confirmation.
    std::thread::sleep(Duration::from_millis(200));
    let area = device.display_area()?;

    println!(
        "display area now: tl=({:.1}, {:.1}, {:.1}) tr=({:.1}, {:.1}, {:.1}) \
         bl=({:.1}, {:.1}, {:.1})",
        area.tl_mm[0], area.tl_mm[1], area.tl_mm[2],
        area.tr_mm[0], area.tr_mm[1], area.tr_mm[2],
        area.bl_mm[0], area.bl_mm[1], area.bl_mm[2],
    );

    Ok(())
}

fn cal_backup(file: &std::path::Path) -> Result<()> {
    let mut device = Device::connect().context("connecting to the ET5")?;
    let blob       = device.cal_retrieve().context("downloading the calibration blob")?;

    std::fs::write(file, &blob).with_context(|| format!("writing {}", file.display()))?;
    println!("saved {} bytes to {}", blob.len(), file.display());

    Ok(())
}

/// Physical size of a declared display area, millimetres across and down. The result
/// table is normalised against exactly this, so it is what turns a normalised error
/// into a distance on the panel.
fn area_size_mm(area: &DisplayArea) -> (f64, f64) {
    let span = |a: [f64; 3], b: [f64; 3]| {
        ((b[0] - a[0]).powi(2) + (b[1] - a[1]).powi(2) + (b[2] - a[2]).powi(2)).sqrt()
    };

    (span(area.tl_mm, area.tr_mm), span(area.tl_mm, area.bl_mm))
}

/// Prints the firmware's own per-point calibration result table.
///
/// `area` is the display area the table's coordinates are normalised against, which
/// is the plane that was declared when the blob was *retrieved*, not the one it was
/// trained under. With it the per-eye errors are also quoted in degrees at
/// [`NOMINAL_VIEW_MM`]; without it the table is normalised units only, which is still
/// enough to see which points the firmware fitted badly.
fn print_result_table(table: &CalibrationResult, area: Option<DisplayArea>) {
    let size = area.as_ref().map(area_size_mm);

    match size {
        Some((w, h)) => println!(
            "  {} points, normalised against the declared {w:.0} x {h:.0} mm plane \
             (errors in degrees at {NOMINAL_VIEW_MM:.0} mm)",
            table.targets.len(),
        ),
        None         => println!(
            "  {} points; no declared plane to read them against, so the errors stay \
             in normalised units",
            table.targets.len(),
        ),
    }

    println!("       target |      left eye     |     right eye     |   err_l   err_r");

    for point in &table.targets {
        // Degrees only when the plane is known; otherwise the normalised distance.
        // The two axes scale differently, so the angle is not the uv error times a
        // constant and has to go through millimetres.
        let quote = |eye: &EyeResult| {
            let du = (eye.position[0] - point.target[0]) as f64;
            let dv = (eye.position[1] - point.target[1]) as f64;

            match size {
                Some((w, h)) => {
                    let mm = ((du * w).powi(2) + (dv * h).powi(2)).sqrt();

                    format!("{:6.2}d", (mm / NOMINAL_VIEW_MM).atan().to_degrees())
                }
                None         => format!("{:7.4}", point.error(eye)),
            }
        };

        println!(
            "  {:.3} {:.3} | {:.3} {:.3} {} | {:.3} {:.3} {} | {} {}",
            point.target[0], point.target[1],
            point.left.position[0] , point.left.position[1] ,
            if point.left.valid  { "ok" } else { "--" },
            point.right.position[0], point.right.position[1],
            if point.right.valid { "ok" } else { "--" },
            quote(&point.left), quote(&point.right),
        );
    }
}

/// Prints a blob's identity and, when it has one, its decoded result table.
fn print_blob(label: &str, bytes: &[u8], area: Option<DisplayArea>) {
    println!("{label}: {}", BlobReport::of(bytes));

    match decode_trailer(bytes) {
        Some((_, table)) => print_result_table(&table, area),
        None             => println!(
            "  no result trailer: this is not a calibrated model of this device \
             (a factory-default blob has none)",
        ),
    }
}

fn blob_info(file: Option<&std::path::Path>, calibration: &std::path::Path) -> Result<()> {
    let mut device = Device::connect().context("connecting to the ET5")?;

    // The table's coordinates are normalised against whatever plane the device holds
    // right now, so the plane has to be read before the numbers mean anything. This
    // connect declares nothing, so it is genuinely whatever was left there.
    let declared = device.display_area().ok();

    // Two back-to-back retrieves with nothing in between: any difference is the
    // firmware's, not ours.
    let first  = device.cal_retrieve().context("first cal_retrieve")?;
    let second = device.cal_retrieve().context("second cal_retrieve")?;

    print_blob("retrieve 1", &first, declared);
    println!("retrieve 2: {}", BlobReport::of(&second));

    match first_difference(&first, &second) {
        None         => println!(
            "identical: cal_retrieve is deterministic on this unit",
        ),
        Some(offset) => println!(
            "DIFFERENT: first difference at offset {offset}; the bodies {}",
            if body(&first) == body(&second) { "still agree" } else { "DISAGREE too" },
        ),
    }

    let Some(path) = file else {
        return Ok(());
    };

    // The saved blob was written straight after a retrain, with the trained plane
    // declared, so its table is normalised against that plane and not the live one.
    let trained = Et5Calibration::load(calibration).ok().and_then(|c| c.device_area);

    if trained.is_none() {
        println!("{} has no trained plane; the saved blob's table stays in \
                  normalised units", calibration.display());
    }

    let saved = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;

    print_blob(&path.display().to_string(), &saved, trained);

    // Identity is the body; the trailer differing across a round trip is expected and
    // says nothing about the model.
    if body_sha256_hex(&saved) == body_sha256_hex(&first) {
        match first_difference(&saved, &first) {
            None         => println!("the file matches retrieve 1 byte for byte, so \
                                      the plane declared now is the one the blob was \
                                      trained under"),
            Some(offset) => println!("the file and retrieve 1 hold the same model \
                                      (identical bodies); their trailers differ from \
                                      offset {offset}, which is the firmware \
                                      re-normalising the result table against the \
                                      plane declared at read time"),
        }
    }
    else {
        let offset = first_difference(&saved, &first).unwrap_or(0);

        println!("DIFFERENT MODELS: the file and retrieve 1 first differ at offset \
                  {offset}, inside the body");
    }

    Ok(())
}

fn blob_watch(minutes: f64) -> Result<()> {
    let mut device = Device::connect().context("connecting to the ET5")?;

    // Everything the table says is in the units of the plane declared right now, and
    // this connect declares nothing, so read it once and use it for both tables.
    let declared = device.display_area().ok();

    let before = device.cal_retrieve().context("cal_retrieve before the watch")?;
    print_blob("before", &before, declared);

    // Streaming is what a session does; if the firmware adapts its model during
    // ordinary use, this is where it shows up.
    let rx       = device.gaze_stream();
    let t0       = Instant::now();
    let deadline = t0 + Duration::from_secs_f64(minutes * 60.0);

    let mut frames      = 0u64;
    let mut valid       = 0u64;
    let mut last_report = t0;

    while Instant::now() < deadline {
        let Ok(frame) = rx.recv_timeout(Duration::from_millis(200)) else {
            continue;
        };

        frames += 1;

        if frame.any_valid() {
            valid += 1;
        }

        if last_report.elapsed() >= Duration::from_secs(10) {
            println!(
                "t={:>5.0}s  frames {frames:>7}  valid {:>3.0}%",
                t0.elapsed().as_secs_f64(),
                100.0 * valid as f64 / frames.max(1) as f64,
            );

            last_report = Instant::now();
        }
    }

    // Retrieving while the gaze stream is live is safe: the device serialises the
    // multi-transfer response rather than interleaving notifications into it
    // (measured by `blob-info`, whose retrieves also run under a live stream and come
    // back with the same body as the saved blob).
    let after = device.cal_retrieve().context("cal_retrieve after the watch")?;
    print_blob("after ", &after, declared);

    // The model is the body. A trailer that moved with the plane untouched would be
    // the firmware rewriting its own result table, which is worth knowing but is not
    // a changed model.
    if body(&before) == body(&after) {
        println!("the model is unchanged after {frames} frames: the firmware does \
                  not mutate its eye model during use");

        if let Some(offset) = first_difference(&before, &after) {
            println!("its result trailer did move, from offset {offset}");
        }

        return Ok(());
    }

    let offset = first_difference(&before, &after).unwrap_or(0);

    println!("CHANGED after {frames} frames: the bodies differ, first difference at \
              offset {offset}");

    Ok(())
}

fn blob_push(file: &std::path::Path, double: bool, calibration: &std::path::Path)
    -> Result<()>
{
    let blob = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;

    // Mirror the provider: the trained plane when there is one, the oversized
    // virtual plane otherwise. The plane is declared after the upload, which is the
    // whole point of doing this through connect_with.
    let area = {
        match Et5Calibration::load(calibration) {
            Ok(cal) => cal.device_area.unwrap_or_else(|| {
                println!("{} has no trained plane; declaring the virtual plane",
                         calibration.display());

                DisplayArea::from_rect(VIRTUAL_AREA)
            }),
            Err(e)  => {
                println!("no usable calibration ({e}); declaring the virtual plane");

                DisplayArea::from_rect(VIRTUAL_AREA)
            }
        }
    };

    println!("pushing {} from {}", BlobReport::of(&blob), file.display());
    println!("do not interrupt: a process killed mid-upload wedges the tracker \
              until it is physically unplugged and replugged");

    // Hold SIGINT off for the duration. Registering a flag handler replaces the
    // default terminate action, so ctrl-c during the transfer sets the flag instead
    // of leaving the device half-written.
    let interrupted = Arc::new(AtomicBool::new(false));
    flag::register(SIGINT, Arc::clone(&interrupted))
        .context("masking SIGINT for the upload")?;

    let bytes  = blob.len();
    let result = Device::connect_with(ConnectOptions {
        blob          : Some(blob),
        area          : Some(area),
        double_upload : double,
        check         : BlobCheck::default(),
    });

    match result {
        Ok(_)  => println!("applied and verified: the device reports back exactly \
                            the {bytes} bytes that went up"),
        Err(e) => {
            eprintln!("upload failed: {e}");
            eprintln!("if this failed part way through a transfer the tracker is \
                       wedged: unplug it, plug it back in, and push again");

            return Err(e.into());
        }
    }

    if interrupted.load(Ordering::Relaxed) {
        println!("(a ctrl-c arrived during the upload and was held until it \
                  finished)");
    }

    Ok(())
}

/// Everything one `calibrate` invocation needs. A struct rather than fourteen
/// positional arguments, which is where a wrong flag hides.
struct CalibrateArgs {
    /// Desk geometry file.
    config           : PathBuf,
    /// Where the client-side calibration is written.
    out              : PathBuf,
    /// Where the fresh on-device blob backup is written.
    blob             : PathBuf,
    /// Explicit seed blob, if the operator named one.
    seed             : Option<PathBuf>,
    /// Run with no seed at all.
    no_seed          : bool,
    /// Connector the tracker is mounted on.
    device_output    : String,
    /// Training area as `WxH` millimetres.
    area             : String,
    /// Train over the whole panel.
    area_full        : bool,
    /// Optional acceptance radius, degrees.
    accept_deg       : Option<f64>,
    /// Nag interval, seconds.
    point_timeout_s  : f64,
    /// Patience for the first gaze point, seconds.
    gaze_timeout_s   : f64,
    /// First round that applies.
    apply_from_round : usize,
    /// Accepted points below which nothing is written.
    min_points       : usize,
    /// Query the point suggestion after each round.
    suggest          : bool,
    /// Print the plan and exit.
    dry_run          : bool,
}

/// Runs the retrain ceremony, or prints its schedule under `--dry-run`.
fn calibrate(args: CalibrateArgs) -> Result<()> {
    let CalibrateArgs { config, out, blob: blob_path, .. } = &args;

    let geometry = load_geometry(config)?;
    let (w_mm, h_mm) = parse_area(&args.area)?;

    let retrain_config = RetrainConfig {
        display           : args.device_output.clone(),
        tracker_pitch_deg : load_tracker_pitch(config),
        area_w_mm         : w_mm,
        area_h_mm         : h_mm,
        area_full         : args.area_full,
        accept_deg        : args.accept_deg,
        point_timeout_s   : args.point_timeout_s,
        gaze_timeout_s    : args.gaze_timeout_s,
        apply_from_round  : args.apply_from_round,
        min_points        : args.min_points,
        suggest           : args.suggest,
    };

    let plan = retrain::plan(&geometry, &retrain_config)?;

    // Resolved before the device is opened, so a bad --seed costs nothing and the dry
    // run can report the same decision the real run would take.
    let seed = retrain::resolve_seed(args.seed.as_deref(), blob_path, args.no_seed)?;

    print_plan(&retrain_config, &plan, &seed);

    if args.dry_run {
        println!("dry run: no device touched, nothing written");

        return Ok(());
    }

    // The blob is the identity every session file and every host-side fit is keyed
    // to; a retrain orphans all of it. Say so before the device is opened.
    println!("this overwrites the on-device eye model. The current blob backup and \
              calibration are kept as *.prev-<unix>, but every session recorded under \
              the old model is orphaned by the new one.");

    if !prompt_yes_no("retrain now?")? {
        println!("nothing done");

        return Ok(());
    }

    // Blob-less connect: the model is about to be replaced, so there is nothing worth
    // uploading first, and the plane is declared by the ceremony itself.
    let mut device = Device::connect().context("connecting to the ET5")?;

    let (overlay, join) = Overlay::spawn().context("spawning the overlay")?;
    let keys = sweep::terminal_keys();

    println!("eyes on the dot and hold still until it moves. Enter forces the current \
              point in, s skips it, q aborts (nothing is written).");

    let outcome = retrain::run_retrain(&mut device, &overlay, Some(&keys),
                                       &retrain_config, &plan, seed.seed.as_ref());

    let outcome = {
        match outcome {
            Ok(outcome) => outcome,
            Err(e)      => {
                overlay.stop();
                let _ = join.join();

                // The refusal and the reboot are the design working, not a crash: say
                // what is still on disk, because "nothing was written" is the point.
                match e {
                    SweepError::TooFewPoints { .. } => {
                        eprintln!("{e}. {} and {} are untouched; the device holds \
                                   whatever the applied rounds taught it, so re-run \
                                   the ceremony (or push the old blob back with \
                                   `blob-push {}`).",
                                  blob_path.display(), out.display(),
                                  blob_path.display());
                    }
                    SweepError::TrackerLost(_)      => {
                        eprintln!("{e}. The model this ceremony was building is lost \
                                   and the device is back on its factory blob; run \
                                   the ceremony again. Nothing was written, so {} is \
                                   still the previous model and the next connect \
                                   pushes it back to the device.",
                                  blob_path.display());
                    }
                    _                               => {}
                }

                return Err(e.into());
            }
        }
    };

    println!("{} of {} points accepted over {} applied rounds",
             outcome.accepted, outcome.results.len(), outcome.applied);

    for result in &outcome.results {
        let point = &plan.points[result.index];

        println!("  {:<14} uv ({:.3}, {:.3}) {:<7} after {:.1}s ({} hits, gaze \
                  {}/{} frames, eyes ok {}/{})",
                 point.label, point.u, point.v, result.outcome.label(),
                 result.wait_s, result.hits, result.gaze, result.frames, result.eyes,
                 result.frames);
    }

    println!("ceremony summary:");

    for (number, round) in outcome.rounds.iter().enumerate() {
        println!("  round {}/{} {:<9} ({:<5}) {} accepted ({} gate, {} dwell, {} \
                  forced), {} skipped, {}",
                 number + 1, outcome.rounds.len(), round.name, round.background.name(),
                 round.accepted(), round.gate, round.dwell, round.forced,
                 round.skipped,
                 if round.applied { "applied" } else { "not applied" });
    }

    for line in &outcome.suggestions {
        println!("  point suggestion {line}");
    }

    let report = BlobReport::of(&outcome.blob);

    // The health check reads the model that was just committed. A failure here loses
    // numbers, not the retrain, so it never stops the files being written.
    println!("health check: nine stops, a second each, eyes on the dot");

    let health = {
        match retrain::run_health(&mut device, &geometry, &overlay, Some(&keys),
                                  &retrain_config, &plan)
        {
            Ok(health) => health,
            Err(e)     => {
                eprintln!("health check did not finish ({e}); the calibration is \
                           written without it");

                Vec::new()
            }
        }
    };

    overlay.stop();
    let _ = join.join();

    for stop in &health {
        println!("  uv ({:.3}, {:.3}) -> ({:.3}, {:.3})  {:.2} deg  ({} frames)",
                 stop.u, stop.v, stop.gaze_u, stop.gaze_v, stop.error_deg,
                 stop.samples);
    }

    match retrain::health_summary(&health) {
        Some((rms, p50)) => println!("health: rms {rms:.2} deg, p50 {p50:.2} deg over \
                                      {} stops", health.len()),
        None             => println!("health: no stops measured"),
    }

    // Nothing is written until the tracker has been closed, reopened, and asked for
    // its model again. The 2026-08-28 01:24 run re-enumerated as it finished and was
    // holding the factory blob afterwards, so the files it wrote described a model
    // that no longer existed.
    device.close();
    drop(device);

    println!("persistence check: reopening the tracker and reading its model back");

    let persistence = {
        match retrain::verify_persistence(&outcome.blob, outcome.usb_before) {
            Ok(persistence) => persistence,
            Err(e)          => {
                eprintln!("{e}. Nothing was written: {} and {} still describe the \
                           previous model, which the next connect pushes back to the \
                           device. Re-run the ceremony.",
                          blob_path.display(), out.display());

                return Err(e.into());
            }
        }
    };

    println!("usb: bus {}.{} before, bus {}.{} after{}",
             persistence.before.0, persistence.before.1,
             persistence.after.0, persistence.after.1,
             if persistence.re_enumerated() {
                 " — RE-ENUMERATED, the firmware rebooted during the ceremony"
             }
             else {
                 ""
             });
    println!("the tracker still holds the committed model ({} bytes)",
             persistence.retrieved_len);

    // The blob first: it is the only artefact that cannot be recreated without the
    // user sitting down again.
    let lag_s = Et5Calibration::load(out).map(|c| c.lag_s).unwrap_or(retrain::DEFAULT_LAG_S);

    if let Some(kept) = retrain::keep_previous(blob_path)? {
        println!("previous blob kept at {}", kept.display());
    }

    std::fs::write(blob_path, &outcome.blob)
        .with_context(|| format!("writing {}", blob_path.display()))?;
    println!("device blob backed up to {} ({report})", blob_path.display());

    let out_geometry = geometry.outputs.iter()
        .find(|o| o.name == retrain_config.display)
        .context("the retrained display vanished from the desk config")?;

    let calibration = Et5Calibration {
        format             : CALIBRATION_FORMAT,
        created_unix_s     : Et5Calibration::now_unix_s(),
        lag_s              : lag_s,
        device_output      : Some(retrain_config.display.clone()),
        device_area        : Some(plan.area),
        device_blob_sha256 : Some(report.body_sha256.clone()),
        device_result      : outcome.result.clone(),
        outputs            : vec![OutputCalibration {
            name           : retrain_config.display.clone(),
            // The desk pose as configured: the retrain declares the measured plane
            // and fits nothing, so there is no solved pose to prefer over it.
            pose           : OutputPose {
                position_mm : out_geometry.position_mm,
                yaw_deg     : out_geometry.yaw_deg,
                pitch_deg   : out_geometry.pitch_deg,
                roll_deg    : out_geometry.roll_deg,
            },
            // No client-side correction: the firmware model is the whole mapping
            // until the Phase D residual model lands.
            field          : FieldMap::identity(),
            pose_rms_deg   : 0.0,
            field_rms_norm : 0.0,
            targets        : outcome.accepted,
            head_gain      : None,
        }],
        health             : health,
    };

    if let Some(kept) = retrain::keep_previous(out)? {
        println!("previous calibration kept at {}", kept.display());
    }

    calibration.save(out)
        .with_context(|| format!("writing {}", out.display()))?;
    println!("calibration written to {}", out.display());
    println!("client data is keyed to blob {}: record fresh sessions before training \
              anything on it", report.short());

    Ok(())
}

/// Prints the plane, the training area, the seed decision, every point and the round
/// schedule.
fn print_plan(
    config : &RetrainConfig,
    plan   : &retrain::RetrainPlan,
    seed   : &retrain::SeedChoice,
) {
    let (u_lo, u_hi, v_lo, v_hi) = plan.train_uv;

    println!("display {} — plane declared to the device (sensor frame, {}{:.1} deg \
              mount pitch):",
             config.display,
             if config.tracker_pitch_deg >= 0.0 { "+" } else { "" },
             config.tracker_pitch_deg);
    println!("  tl ({:>7.1}, {:>7.1}, {:>7.1}) mm", plan.area.tl_mm[0],
             plan.area.tl_mm[1], plan.area.tl_mm[2]);
    println!("  tr ({:>7.1}, {:>7.1}, {:>7.1}) mm", plan.area.tr_mm[0],
             plan.area.tr_mm[1], plan.area.tr_mm[2]);
    println!("  bl ({:>7.1}, {:>7.1}, {:>7.1}) mm", plan.area.bl_mm[0],
             plan.area.bl_mm[1], plan.area.bl_mm[2]);

    if config.area_full {
        println!("training area: the whole panel (u {u_lo:.3}..{u_hi:.3}, \
                  v {v_lo:.3}..{v_hi:.3})");
    }
    else {
        println!("training area: {:.0}x{:.0} mm, bottom-aligned and centred on the \
                  tracker (u {u_lo:.3}..{u_hi:.3}, v {v_lo:.3}..{v_hi:.3})",
                 config.area_w_mm, config.area_h_mm);
    }

    println!("session order: cal_start, cal_clear, {}, rounds, cal_stop + cal_retrieve",
             if seed.seed.is_some() { "cal_apply(seed)" } else { "no seed" });
    println!("seed: {}", seed.why);

    match plan.tol_uv {
        None               => {
            println!("acceptance: nearest-target vote only ({} of the last {} frames \
                      within {:.0}s), no accuracy radius",
                     retrain::GATE_MIN_HITS, retrain::GATE_WINDOW,
                     retrain::GATE_WINDOW_S);
        }
        Some((tol_u, tol_v)) => {
            println!("acceptance: {} of the last {} frames within {:.0}s, and the \
                      median within {:.1} deg (uv {tol_u:.4} x {tol_v:.4})",
                     retrain::GATE_MIN_HITS, retrain::GATE_WINDOW,
                     retrain::GATE_WINDOW_S, config.accept_deg.unwrap_or_default());
        }
    }

    println!("patience: nag every {:.0}s (nothing is skipped without `s`), dwell \
              fallback after {:.0}s with no gaze at all",
             config.point_timeout_s, config.gaze_timeout_s);
    println!("commit: at least {} accepted points, else nothing is written",
             config.min_points.max(1));

    for (i, point) in plan.points.iter().enumerate() {
        println!("  point {i}: {:<14} uv ({:.3}, {:.3}) px ({:>6.0}, {:>6.0})",
                 point.label, point.u, point.v, point.px.x, point.px.y);
    }

    for (i, round) in plan.rounds.iter().enumerate() {
        let names = round.points.iter()
            .map(|p| plan.points[*p].label)
            .collect::<Vec<_>>()
            .join(", ");

        println!("  round {}/{}: {} on {} — {names}{}",
                 i + 1, plan.rounds.len(), round.name, round.background.name(),
                 if i + 1 >= config.apply_from_round {
                     ", then cal_points_apply"
                 }
                 else {
                     ", points held (--apply-from-round)"
                 });
    }
}

/// Names the on-device model: the first 16 hex characters of the SHA-256 of its
/// blob's *body*, so the history key and the calibration file's
/// `device_blob_sha256` are the same identity. The body rather than the whole blob
/// because the result trailer is re-normalised on every retrieve. (Passes archived
/// before 2026-08-28 were keyed by `DefaultHasher`, and those from 2026-08-28 by the
/// whole-blob hash; neither matches. That directory is orphaned either way.)
fn blob_key(path: &std::path::Path) -> Result<String> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("reading {}", path.display()))?;

    Ok(body_sha256_hex(&bytes)[..16].to_string())
}

/// Runs the collect session and archives its pass under the committed model's key.
fn collect_cmd(
    config      : &std::path::Path,
    minutes     : f64,
    calibration : &std::path::Path,
    blob        : &std::path::Path,
    history     : &std::path::Path,
)
    -> Result<()>
{
    let geometry = load_geometry(config)?;

    let cal = Et5Calibration::load(calibration)
        .with_context(|| format!("loading {}", calibration.display()))?;
    let area = cal.device_area
        .context("the calibration has no trained plane; run `calibrate` first")?;
    let out_name = cal.device_output.clone()
        .context("the calibration has no device output; run `calibrate` first")?;

    let key = blob_key(blob)
        .context("hashing the blob backup (run `calibrate` to create it)")?;

    std::fs::create_dir_all(history)
        .with_context(|| format!("creating {}", history.display()))?;

    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let path  = history.join(format!("{key}-{stamp}.jsonl"));

    let mut device = Device::connect().context("connecting to the ET5")?;

    // The trained mapping only applies under the exact plane it was trained on.
    device.set_display_area_corners(area).context("declaring the trained plane")?;

    let (overlay, join) = Overlay::spawn().context("spawning the overlay")?;
    let keys = sweep::terminal_keys();

    println!("collect: eyes on the dot, do what the caption says. Enter advances, \
              s skips a hold, q ends early (data collected so far is kept)");

    let holds = sweep::run_collect(&mut device, &geometry, &out_name, &overlay,
                                   Some(&keys), minutes, &path);

    overlay.stop();
    let _ = join.join();

    let holds = holds?;

    println!("{holds} holds archived to {}", path.display());
    println!("they pool into the next `calibrate` (or `refit`) automatically");

    Ok(())
}

/// Runs one recording session, or prints its schedule under `--dry-run`.
#[allow(clippy::too_many_arguments)]
fn record_cmd(
    config      : &std::path::Path,
    minutes     : f64,
    calibration : &std::path::Path,
    display     : Option<String>,
    out         : &std::path::Path,
    note        : String,
    cone_deg    : f64,
    grid        : &str,
    glasses     : bool,
    no_glasses  : bool,
    dry_run     : bool,
)
    -> Result<()>
{
    let desk     = std::fs::read_to_string(config)
        .with_context(|| format!("reading {}", config.display()))?;
    let geometry = DesktopGeometry::from_toml(&desk).context("parsing desk geometry")?;

    let cal = Et5Calibration::load(calibration)
        .with_context(|| format!("loading {}", calibration.display()))?;
    let area = cal.device_area
        .context("the calibration has no trained plane; run `calibrate` first")?;

    let name = display
        .or_else(|| cal.device_output.clone())
        .context("no display given and the calibration names none")?;

    let (cols, rows) = parse_grid(grid)?;

    // Ask before the key reader takes stdin: both cannot read it at once.
    let glasses = {
        if glasses || no_glasses {
            glasses
        }
        else {
            prompt_yes_no("were you wearing glasses for this session?")?
        }
    };

    let record_config = RecordConfig {
        display           : name,
        minutes           : minutes,
        cone_max_deg      : cone_deg,
        grid_cols         : cols,
        grid_rows         : rows,
        tracker_pitch_deg : load_tracker_pitch(config),
        glasses           : glasses,
        note              : note,
        out_dir           : out.to_path_buf(),
        ..RecordConfig::default()
    };

    let plan = record::plan(&geometry, &record_config)?;

    println!("{} stops in a {:.1} deg cone (u {:.2}..{:.2}, v {:.2}..{:.2}), \
              {:.0}s per grid, {:.0}s of wander, {:.1} min total",
             plan.stops.len(), plan.cone_max_deg, plan.cone_uv.0, plan.cone_uv.1,
             plan.cone_uv.2, plan.cone_uv.3, plan.grid_s, plan.wander_s,
             (2.0 * plan.grid_s + plan.wander_s) / 60.0);

    if dry_run {
        println!("dry run: no device touched, nothing written");

        for (i, (u, v, px)) in plan.stops.iter().enumerate() {
            println!("  stop {:>2}: uv ({u:.3}, {v:.3}) px ({:.0}, {:.0})",
                     i + 1, px.x, px.y);
        }

        return Ok(());
    }

    let mut device = Device::connect().context("connecting to the ET5")?;

    let (overlay, join) = Overlay::spawn().context("spawning the overlay")?;
    let keys = sweep::terminal_keys();

    println!("record: eyes on the dot throughout. The screen goes black, then white; \
              do what the caption says during the wander. Enter advances, s skips a \
              stop, q ends the current phase early.");

    let outcome = record::run_record(&mut device, &geometry, area, &overlay, Some(&keys),
                                     &record_config, &desk);

    overlay.stop();
    let _ = join.join();

    let outcome = outcome?;

    println!("session {} written to {}", outcome.session_id, outcome.path.display());
    println!("{} frames ({} valid), {} stops", outcome.frames, outcome.valid_frames,
             outcome.stops);

    if outcome.blob_start.sha256 == outcome.blob_end.sha256 {
        println!("blob {} unchanged over the session", outcome.blob_start);
    }
    else {
        println!("WARNING: the blob changed mid-session ({} -> {}); these rows describe \
                  two different firmware models and should not be trained on",
                 outcome.blob_start.short(), outcome.blob_end.short());
    }

    Ok(())
}

fn sessions_cmd(config: &std::path::Path, command: SessionsCommand) -> Result<()> {
    match command {
        SessionsCommand::Import { readings, out, blob, calibration, note } => {
            let desk = std::fs::read_to_string(config)
                .with_context(|| format!("reading {}", config.display()))?;

            let cal = Et5Calibration::load(&calibration)
                .with_context(|| format!("loading {}", calibration.display()))?;
            let area = cal.device_area.context(
                "the calibration has no trained plane, so the readings cannot be \
                 placed in tracker space")?;

            let bytes = std::fs::read(&blob)
                .with_context(|| format!("reading {}", blob.display()))?;

            let (path, records) = record::import_readings(
                &readings, &out, &bytes, area, &desk, load_tracker_pitch(config), &note,
            )?;

            println!("{records} records imported to {}", path.display());
            println!("blob {}", BlobReport::of(&bytes));

            Ok(())
        }
    }
}

fn dataset_cmd(config: &std::path::Path, command: DatasetCommand) -> Result<()> {
    match command {
        DatasetCommand::Export { sessions, csv } => {
            let geometry = load_geometry(config)?;
            let rows     = dataset::load_rows(&sessions, &geometry)?;

            dataset::write_csv(&rows, &csv)?;

            println!("{} rows written to {}", rows.len(), csv.display());
            report_rows(&rows);

            Ok(())
        }
    }
}

/// Prints what a set of exported rows contains and how far the firmware alone misses.
/// This is the Phase C baseline: any model has to beat it on held-out sessions.
fn report_rows(rows: &[dataset::Row]) {
    let mut sessions: Vec<&str> = rows.iter().map(|r| r.session_id.as_str()).collect();
    sessions.sort_unstable();
    sessions.dedup();

    println!("{} session(s), {} stop frames, {} hold frames, {} means, {} glide rows",
             sessions.len(),
             rows.iter().filter(|r| r.phase == "stop" && !r.is_mean).count(),
             rows.iter().filter(|r| r.phase == "hold" && !r.is_mean).count(),
             rows.iter().filter(|r| r.is_mean).count(),
             rows.iter().filter(|r| r.phase == "glide").count());

    let summarise = |label: &str, selected: Vec<&dataset::Row>| {
        let mut errors: Vec<f64> = selected.iter()
            .map(|r| r.residual_deg())
            .filter(|d| d.is_finite())
            .collect();

        if errors.is_empty() {
            return;
        }

        let rms = (errors.iter().map(|d| d * d).sum::<f64>() / errors.len() as f64).sqrt();

        errors.sort_by(f64::total_cmp);

        let at = |q: f64| errors[((errors.len() - 1) as f64 * q) as usize];

        println!("  {label:<12} n {:>6}  rms {:.2} deg  p50 {:.2}  p90 {:.2}",
                 errors.len(), rms, at(0.5), at(0.9));
    };

    println!("firmware-only residual (in sample, no split):");
    summarise("all"        , rows.iter().collect());
    summarise("stops"      , rows.iter().filter(|r| r.phase == "stop" && !r.is_mean).collect());
    summarise("holds"      , rows.iter().filter(|r| r.phase == "hold" && !r.is_mean).collect());
    summarise("means"      , rows.iter().filter(|r| r.is_mean).collect());
    summarise("glides"     , rows.iter().filter(|r| r.phase == "glide").collect());
}

/// Asks a yes/no question on the terminal. Anything but `y` is no.
fn prompt_yes_no(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("reading the answer")?;

    Ok(line.trim().eq_ignore_ascii_case("y"))
}

fn refit_cmd(config: &std::path::Path, readings: &std::path::Path, out: &std::path::Path)
    -> Result<()>
{
    let geometry = load_geometry(config)?;

    // Refitting cannot know which plane the readings were collected under; assume
    // the ray path (virtual plane) unless told otherwise. Direct-mode readings
    // should be refitted with the same trained plane the file already records.
    let previous = Et5Calibration::load(out).ok();
    let refit_config = SweepConfig {
        direct      : previous.as_ref().is_some_and(|p| p.device_area.is_some()),
        history_dir : Some(PathBuf::from(DEFAULT_HISTORY_DIR)),
        history_key : blob_key(std::path::Path::new(DEFAULT_BLOB_PATH)).ok(),
        ..SweepConfig::default()
    };

    let mut outcome = sweep::refit(&geometry, readings, &refit_config)?;

    if let Some(previous) = previous {
        outcome.calibration.device_output      = previous.device_output;
        outcome.calibration.device_area        = previous.device_area;
        outcome.calibration.device_blob_sha256 = previous.device_blob_sha256;
    }

    for s in &outcome.summaries {
        println!(
            "{:>10}: {} targets, pose rms {:.2} deg (moved {:.0} mm), lag {:.0} ms, \
             {} glide rows ({} saccade frames cut), field cv {:.4}",
            s.name, s.targets, s.pose_rms_deg, s.pose_shift_mm, s.lag_s * 1000.0,
            s.glide_rows, s.saccade_frames, s.field_rms_norm,
        );
    }

    outcome.calibration.save(out)
        .with_context(|| format!("writing {}", out.display()))?;
    println!("re-fitted calibration written to {}", out.display());

    Ok(())
}

fn view(config: &std::path::Path, calibration: Option<&std::path::Path>) -> Result<()> {
    let geometry = load_geometry(config)?;

    let calibration = {
        match calibration {
            Some(path) => Some(Et5Calibration::load(path)
                .with_context(|| format!("loading {}", path.display()))?),
            None       => None,
        }
    };

    // The raw per-eye stream is unfiltered (unlike the device's own smoothed 2D
    // output); run it through the standard filter stack so the marker is comparable
    // to what the snapping pipeline will actually consume.
    let mut scale_geometry = geometry.clone();

    if let Some(cal) = &calibration {
        cal.apply_poses(&mut scale_geometry);
    }

    // Raw per-frame noise (~0.3 deg at 90 Hz) reads as ~27 deg/s of velocity, right
    // at the default 30 deg/s fixation gate, so the smoother would flap on and off.
    // A higher gate, a wider velocity window, and a heavier one-euro keep the marker
    // steady; the snapping pipeline tunes its own stack separately.
    let mut filters = FilterStack::create()
        .scale(Box::new(scale_geometry))
        .velocity_threshold_deg_s(60.0)
        .window_s(0.05)
        .one_euro(0.4, 0.2)
        .build();

    let mut provider = Et5Provider::create()
        .geometry(geometry)
        .calibration(calibration)
        .start()
        .context("starting the ET5 provider")?;

    let (overlay, join) = Overlay::spawn().context("spawning the overlay")?;

    println!("drawing live gaze; ctrl-c to stop");

    // One status line per second: enough to tell apart a pinned edge (wide sigma),
    // a dropout hold (ramping sigma, no ray), and a plain invalid.
    let mut last_status = Instant::now();

    while let Some(sample) = provider.next() {
        if last_status.elapsed() >= Duration::from_secs(1) {
            eprintln!(
                "valid={} sigma={:.1} ray={} point={:?}",
                sample.valid,
                sample.sigma_deg,
                if sample.ray.is_some() { "y" } else { "-" },
                sample.point.map(|p| (p.x.round(), p.y.round())),
            );

            last_status = Instant::now();
        }

        let filtered = filters.push(sample);

        let state = OverlayState {
            gaze       : filtered.sample.point.filter(|_| filtered.sample.valid),
            highlight  : None,
            truth      : None,
            label      : None,
            background : None,
        };

        let _ = overlay.set(state);
    }

    overlay.stop();
    let _ = join.join();

    Ok(())
}

// --- Helpers ---

/// Parses a `WxH` training-area spec in millimetres.
fn parse_area(area: &str) -> Result<(f64, f64)> {
    let Some((w, h)) = area.split_once('x') else {
        bail!("area must look like 600x340 (millimetres), got {area}");
    };

    let w_mm: f64 = w.trim().parse().context("area width")?;
    let h_mm: f64 = h.trim().parse().context("area height")?;

    if w_mm <= 0.0 || h_mm <= 0.0 || !w_mm.is_finite() || !h_mm.is_finite() {
        bail!("area must be positive, got {area}");
    }

    Ok((w_mm, h_mm))
}

/// Parses a `COLSxROWS` grid spec.
fn parse_grid(grid: &str) -> Result<(usize, usize)> {
    let Some((c, r)) = grid.split_once('x') else {
        bail!("grid must look like 4x3, got {grid}");
    };

    let cols: usize = c.parse().context("grid columns")?;
    let rows: usize = r.parse().context("grid rows")?;

    if cols < 2 || rows < 2 {
        bail!("grid needs at least 2x2 stops");
    }

    Ok((cols, rows))
}
