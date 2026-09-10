//! Manual test CLI for the ET5 provider: stream inspection, display-area setup,
//! calibration blob diagnostics and upload, the retrain ceremony and its health
//! check, and a live overlay view. Everything here needs the tracker on the bus in
//! runtime mode; the ceremony and the view additionally need a live compositor and a
//! seated user.
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
    CALIBRATION_FORMAT, Et5Calibration, FieldFit, MIN_HEALTH_ROWS, OutputCalibration,
    OutputPose, VIRTUAL_AREA,
};
use gaze_provider_et5::field::FieldMap;
use gaze_provider_et5::device::{ConnectOptions, Device};
use gaze_provider_et5::gaze::combined_ray;
use gaze_provider_et5::provider::Et5Provider;
use gaze_provider_et5::retrain::{self, RetrainConfig, RetrainError, RetrainKey};
use gaze_provider_et5::calibration::{desk_to_sensor, plane_corners};
use gaze_daydream::{Button, Buttons, Controller};
use crossbeam_channel::{Receiver, Sender};
use gaze_provider_et5::ttp::{DisplayArea, DisplayRect};
use gaze_core::GazeProvider;
use gaze_snap::FilterStack;
use signal_hook::consts::SIGINT;
use signal_hook::flag;

/// Conventional on-device blob backup, whose body hash keys the pass history.
const DEFAULT_BLOB_PATH: &str = "config/calibration-et5.bin";

/// Viewing distance the result table's normalised errors are turned into degrees
/// at, millimetres. The eye is not measured here, so this is the desk's nominal
/// distance and the degrees are indicative, not the health check's numbers.
const NOMINAL_VIEW_MM: f64 = 650.0;

/// Half-width of the `info` gauges, degrees: a mark at either end means the eyes are
/// this far off the sensor axis or further.
const GAUGE_SPAN_DEG: f64 = 20.0;

/// Cells on either side of the gauge's centre.
const GAUGE_HALF_CELLS: i64 = 8;

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
    /// stats — the mount-adjustment feedback loop. Each line: frame rate, the share
    /// of frames with either eye and with each eye, the median eye origin in the
    /// sensor frame, and where that origin sits in the sensor's field as azimuth
    /// (positive right) and elevation (positive up) off its axis plus distance, with
    /// a gauge for each angle. Aim the mount so both angles sit near zero and the
    /// distance lands in the tracker's 450 to 950 mm range; the tracked share goes
    /// to 100% as it does. The rate stays at 33 Hz whatever the aim: that is the
    /// ET5's gaze rate (its camera runs at 132 Hz and publishes every fourth frame).
    ///
    /// A replugged tracker is back on its factory-default eye model and streams
    /// nothing at any aim, so this uploads the saved blob the way the provider does
    /// on every connect, under the virtual plane. Pass `--no-blob` to look at
    /// whatever the flash holds.
    Info {
        /// How long to sample; 0 runs until Ctrl-C.
        #[arg(long, default_value_t = 4.0)]
        seconds : f64,

        /// The eye model to upload before sampling.
        #[arg(long, default_value = DEFAULT_BLOB_PATH)]
        blob    : PathBuf,

        /// Do not upload anything; run on the model in the flash.
        #[arg(long)]
        no_blob : bool,
    },

    /// Send one raw opcode and print the response bytes with a best-effort TLV walk.
    /// The probe for opcodes the typed API does not cover (Talon and nottobii name
    /// far more than this crate sends). Payload shortcuts: `--u32 N` sends one bare
    /// u32 field, `--hex ..` sends exact bytes, neither sends an empty payload.
    Op {
        /// Opcode, hex (`0x672`) or decimal.
        #[arg(value_parser = parse_u32_auto)]
        op      : u32,

        /// Send one bare u32 field `[2][4][N]` as the payload.
        #[arg(long, value_parser = parse_u32_auto, conflicts_with = "hex")]
        u32     : Option<u32>,

        /// Send exactly these bytes (hex, spaces allowed) as the payload.
        #[arg(long)]
        hex     : Option<String>,

        /// Watch the gaze stream for this long afterwards and report its rate.
        #[arg(long, default_value_t = 0.0)]
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
    /// `cal_points_apply` after each round, followed by a health check (4x4 by
    /// default) that the client-side correction field is then fitted from. The
    /// session is seeded with the blob it is about to replace, so the rounds are
    /// collected through a working model. Writes the device's model and is meant to
    /// be run once per mount. Keys: Enter forces the current point in, `c` commits it
    /// (the vote checks it), `s` skips it, `q` aborts without committing. With
    /// `--daydream` the controller's pad click commits, App skips and Home aborts,
    /// and the gate no longer accepts on its own: every point is yours to take.
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

        /// Take points from the Daydream controller: pad click commits (checked
        /// against the gate's vote), App skips, Home aborts. Implies `--manual`.
        #[arg(long)]
        daydream : bool,

        /// The controller's Bluetooth address, when more than one is paired.
        #[arg(long, requires = "daydream")]
        daydream_address : Option<String>,

        /// Never accept a point on the gate's vote alone: only a commit (`c` or the
        /// controller) or Enter takes one, and the dwell fallback is off.
        #[arg(long)]
        manual : bool,

        /// Health-check stops per axis. The correction field is fitted from these,
        /// so more is better up to patience: 4 is sixteen seconds.
        #[arg(long, default_value_t = retrain::HEALTH_STEPS)]
        health_grid : usize,

        /// Print the plane, the training area, the points, the seed decision and the
        /// round schedule, then exit. Touches neither the device nor the compositor.
        #[arg(long)]
        dry_run : bool,
    },

    /// Run the health check alone against the committed model: upload the blob,
    /// declare the trained plane, dwell on an n by n grid of stops, replace the
    /// calibration's health table with the result and refit the correction field
    /// from it. Sixteen seconds at the default grid; no retrain. Home or `q` aborts.
    Health {
        /// Where the calibration is read from and written back to.
        #[arg(long, default_value = DEFAULT_CALIBRATION_PATH)]
        calibration : PathBuf,

        /// The eye model to upload before measuring.
        #[arg(long, default_value = DEFAULT_BLOB_PATH)]
        blob : PathBuf,

        /// Stops per axis.
        #[arg(long, default_value_t = retrain::HEALTH_STEPS)]
        grid : usize,

        /// Read the Daydream controller too, for Home to abort.
        #[arg(long)]
        daydream : bool,

        /// The controller's Bluetooth address, when more than one is paired.
        #[arg(long, requires = "daydream")]
        daydream_address : Option<String>,
    },

    /// Refit the direct display's correction field from the health stops already in
    /// the calibration file, and write it back. What `calibrate` does at the end of
    /// a ceremony, for a file written before it did or after editing the desk config.
    /// Needs neither the device nor a compositor.
    FitField {
        /// The calibration to refit in place.
        #[arg(long, default_value = DEFAULT_CALIBRATION_PATH)]
        calibration : PathBuf,

        /// Print the fit and exit without writing.
        #[arg(long)]
        dry_run     : bool,
    },

    /// Draw the live gaze point on every display until interrupted.
    ///
    /// Three mappings: `--calibration` applies a calibration's client-side fits; `--direct`
    /// declares one display's configured plane to the firmware and draws its own 2D
    /// output on that display, no client fit at all (the range check after a remount:
    /// where the eyes can and cannot be followed, whatever the bias); neither is the
    /// raw ray against the configured desk.
    View {
        /// Client-side calibration to apply.
        #[arg(long, conflicts_with = "direct")]
        calibration : Option<PathBuf>,

        /// Connector to declare and draw on directly, from the desk config's pose.
        #[arg(long)]
        direct      : Option<String>,
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
        Command::Info { seconds, blob, no_blob } => info(seconds, &blob, no_blob),
        Command::Op { op, u32, hex, seconds } => raw_op(op, u32, hex.as_deref(), seconds),
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
            daydream,
            daydream_address,
            manual,
            health_grid,
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
            daydream         : daydream,
            daydream_address : daydream_address,
            manual           : manual || daydream,
            health_grid      : health_grid,
            dry_run          : dry_run,
        }),
        Command::Health { calibration, blob, grid, daydream, daydream_address } => {
            health_cmd(&cli.config, &calibration, &blob, grid, daydream, daydream_address.as_deref())
        }
        Command::FitField { calibration, dry_run } => fit_field(&cli.config, &calibration, dry_run),
        Command::View { calibration, direct } => {
            view(&cli.config, calibration.as_deref(), direct.as_deref())
        }
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

/// Parses `0x..` hex or decimal.
fn parse_u32_auto(s: &str) -> std::result::Result<u32, String> {
    let parsed = {
        match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
            Some(h) => u32::from_str_radix(h, 16),
            None    => s.parse::<u32>(),
        }
    };

    parsed.map_err(|e| format!("{s:?}: {e}"))
}

/// Parses a hex byte string, ignoring whitespace.
fn parse_hex_bytes(s: &str) -> Result<Vec<u8>> {
    let clean: String = s.chars().filter(|c| !c.is_whitespace()).collect();
    if !clean.len().is_multiple_of(2) {
        bail!("odd number of hex digits");
    }

    (0..clean.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&clean[i..i + 2], 16).map_err(Into::into))
        .collect()
}

/// Prints one TLV field per line as far as the payload parses, then the tail as hex.
fn walk_tlv(payload: &[u8]) {
    let mut pos = 0usize;

    while payload.len() - pos >= 5 {
        let t    = payload[pos];
        let size = u32::from_be_bytes([payload[pos + 1], payload[pos + 2], payload[pos + 3], payload[pos + 4]]) as usize;

        if payload.len() - pos - 5 < size {
            break;
        }

        let v = &payload[pos + 5..pos + 5 + size];

        let shown = {
            match (t, size) {
                (2, 4)    => format!("u32 {}", u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
                (3, 4)    => format!("q16 {}", i32::from_be_bytes([v[0], v[1], v[2], v[3]]) as f64 / 65536.0),
                (4, 8)    => format!("q42 {}", gaze_provider_et5::ttp::q42_decode(i64::from_be_bytes(v.try_into().unwrap()))),
                (5, 4)    => format!("tag {:#x}", u32::from_be_bytes([v[0], v[1], v[2], v[3]])),
                (6, 8)    => format!("s64 {}", i64::from_be_bytes(v.try_into().unwrap())),
                (0x17, _) => format!("array {}", hex_string(v)),
                _         => hex_string(v),
            }
        };

        println!("  +{pos:3} type {t:#04x} size {size:3}: {shown}");
        pos += 5 + size;
    }

    if pos < payload.len() {
        println!("  +{pos:3} tail: {}", hex_string(&payload[pos..]));
    }
}

/// Space-separated lowercase hex.
fn hex_string(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect::<Vec<_>>().join(" ")
}

fn raw_op(op: u32, bare_u32: Option<u32>, hex: Option<&str>, seconds: f64) -> Result<()> {
    let payload = {
        match (bare_u32, hex) {
            (Some(v), _)    => {
                let mut p = vec![0x02];
                p.extend_from_slice(&4u32.to_be_bytes());
                p.extend_from_slice(&v.to_be_bytes());
                p
            }
            (None, Some(h)) => parse_hex_bytes(h)?,
            (None, None)    => Vec::new(),
        }
    };

    let mut device = Device::connect().context("connecting to the ET5")?;

    println!("op {op:#x} payload [{}]", hex_string(&payload));

    match device.raw_request(op, &payload) {
        Ok(resp) => {
            println!("response {} bytes: {}", resp.len(), hex_string(&resp));
            if resp.len() >= 2 {
                println!("  status {:#04x} {:#04x}", resp[0], resp[1]);
                walk_tlv(&resp[2..]);
            }
        }
        Err(e)   => println!("no response: {e}"),
    }

    if seconds > 0.0 {
        let rx = device.gaze_stream();
        let mut counters: Vec<(u32, i64)> = Vec::new();
        let deadline = Instant::now() + Duration::from_secs_f64(seconds);
        let mut n = 0u32;

        while Instant::now() < deadline {
            if let Ok(f) = rx.recv_timeout(Duration::from_millis(100)) {
                n += 1;
                if let (Some(c), Some(t)) = (f.frame_counter, f.timestamp_us) {
                    counters.push((c, t));
                }
            }
        }

        println!("stream: {n} frames in {seconds:.1} s = {:.1} Hz", f64::from(n) / seconds);

        if counters.len() >= 2 {
            let (c0, t0) = counters[0];
            let (c1, t1) = counters[counters.len() - 1];
            let ticks    = i64::from(c1.wrapping_sub(c0));
            if ticks > 0 {
                println!(
                    "device counter: {ticks} ticks over {} delivered frames \
                     (every {:.2}th), {:.0} us per tick = {:.1} Hz internal",
                    counters.len() - 1,
                    ticks as f64 / (counters.len() - 1) as f64,
                    (t1 - t0) as f64 / ticks as f64,
                    1e6 * ticks as f64 / (t1 - t0) as f64,
                );
            }
        }
    }

    Ok(())
}

fn info(seconds: f64, blob: &std::path::Path, no_blob: bool) -> Result<()> {
    let upload = {
        if no_blob {
            None
        }
        else {
            match std::fs::read(blob) {
                Ok(bytes) => Some(bytes),
                Err(e)    => {
                    println!("no eye model to upload ({}: {e}); running on the flash", blob.display());

                    None
                }
            }
        }
    };

    let mut device = match upload {
        Some(bytes) => {
            println!("uploading {} from {} (do not interrupt)", BlobReport::of(&bytes), blob.display());

            Device::connect_with(ConnectOptions {
                blob          : Some(bytes),
                area          : Some(DisplayArea::from_rect(VIRTUAL_AREA)),
                double_upload : false,
                check         : BlobCheck::default(),
            })
            .context("connecting to the ET5 with the eye model")?
        }
        None => Device::connect().context("connecting to the ET5")?,
    };

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
            // the usual 33 Hz stream, illuminators on, zero detection at any
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

    match device.frequencies() {
        Ok((camera, gaze)) => println!("device rates: camera {camera} Hz, gaze {gaze} Hz"),
        Err(e)             => println!("device rates unavailable: {e}"),
    }

    // Per-second stats: the rate is the device's constant 33 Hz whether or not a
    // face is locked; the tracked share is the health signal. The origin line is
    // what pins down the mount pitch (see desk.toml); the angles say where the eyes
    // sit in the field.
    let rx       = device.gaze_stream();
    let forever  = seconds <= 0.0;
    let deadline = Instant::now() + Duration::from_secs_f64(seconds.max(0.0));

    while forever || Instant::now() < deadline {
        let second  = Instant::now() + Duration::from_secs(1);
        let mut n       = 0u32;
        let mut valid   = 0u32;
        let mut left_n  = 0u32;
        let mut right_n = 0u32;
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

            left_n  += u32::from(l.is_some());
            right_n += u32::from(r.is_some());

            // One eye is enough to place the head; both is better.
            match (l, r) {
                (Some(l), Some(r)) => origins.push([
                    (l[0] + r[0]) * 0.5,
                    (l[1] + r[1]) * 0.5,
                    (l[2] + r[2]) * 0.5,
                ]),
                (Some(o), None) | (None, Some(o)) => origins.push(o),
                (None, None)                      => {}
            }
        }

        let pct = |k: u32| 100.0 * f64::from(k) / f64::from(n.max(1));

        let placement = {
            if origins.is_empty() {
                "eye origin (sensor mm) -".to_string()
            }
            else {
                let o = [
                    median(origins.iter().map(|o| o[0]).collect()),
                    median(origins.iter().map(|o| o[1]).collect()),
                    median(origins.iter().map(|o| o[2]).collect()),
                ];

                let az_deg   = o[0].atan2(o[2]).to_degrees();
                let el_deg   = o[1].atan2(o[2]).to_degrees();
                let dist_mm  = (o[0] * o[0] + o[1] * o[1] + o[2] * o[2]).sqrt();

                format!(
                    "eye origin (sensor mm) ({:.0}, {:.0}, {:.0})  az {:>+5.1}° {}  el {:>+5.1}° {}  dist {:>4.0} mm",
                    o[0], o[1], o[2],
                    az_deg, gauge(az_deg, GAUGE_SPAN_DEG),
                    el_deg, gauge(el_deg, GAUGE_SPAN_DEG),
                    dist_mm,
                )
            }
        };

        println!(
            "rate {:>3} Hz  tracked {:>3.0}% (L {:>3.0}% R {:>3.0}%)  {placement}",
            n,
            pct(valid),
            pct(left_n),
            pct(right_n),
        );
    }

    Ok(())
}

/// Median of `v`; `v` must not be empty.
fn median(mut v: Vec<f64>) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// A one-line gauge of `value` over `[-span, span]`: `[.......|#.......]`, the centre
/// bar being zero and the mark clamped to the ends.
fn gauge(value: f64, span: f64) -> String {
    let cells = GAUGE_HALF_CELLS;
    let pos   = ((value / span) * cells as f64).round() as i64;
    let pos   = pos.clamp(-cells, cells);

    let mut out = String::with_capacity((2 * cells + 3) as usize);

    out.push('[');

    for i in -cells..=cells {
        out.push(match (i == pos, i == 0) {
            (true, _)      => '#',
            (false, true)  => '|',
            (false, false) => '.',
        });
    }

    out.push(']');

    out
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
    /// Read the Daydream controller as a key source.
    daydream         : bool,
    /// Its address, when more than one is paired.
    daydream_address : Option<String>,
    /// Accept only on a commit or Enter.
    manual           : bool,
    /// Health-check stops per axis.
    health_grid      : usize,
    /// Print the plan and exit.
    dry_run          : bool,
}

/// The keys a ceremony listens to: stdin always, and the Daydream controller when
/// asked, both into one channel. On the controller the pad click commits, App skips
/// and Home quits; there is no Advance, since Enter is the operator's override and a
/// thumb should not have it.
fn key_sources(daydream: bool, address: Option<&str>) -> Result<Receiver<RetrainKey>> {
    let (tx, rx) = crossbeam_channel::unbounded();

    retrain::terminal_keys_into(tx.clone());

    if daydream {
        let controller = Controller::open(address).context("opening the Daydream controller")?;

        println!("daydream controller {}: pad click commits, App skips, Home aborts",
                 controller.address());

        std::thread::Builder::new()
            .name("daydream-keys".into())
            .spawn(move || controller_keys(controller, tx))
            .context("spawning the controller key thread")?;
    }

    Ok(rx)
}

/// Turns the controller's button press edges into ceremony keys until the channel
/// closes. Holds the controller for as long as it runs, so the link stays up.
fn controller_keys(controller: Controller, tx: Sender<RetrainKey>) {
    let mut buttons = Buttons::default();

    loop {
        for report in controller.reports() {
            for button in report.packet.buttons.pressed_since(buttons) {
                let key = match button {
                    Button::Click => Some(RetrainKey::Commit),
                    Button::App   => Some(RetrainKey::Skip),
                    Button::Home  => Some(RetrainKey::Quit),
                    _             => None,
                };

                if let Some(key) = key
                    && tx.send(key).is_err()
                {
                    return;
                }
            }

            buttons = report.packet.buttons;
        }

        std::thread::sleep(Duration::from_millis(8));
    }
}

/// Prints what the health-check field fit decided, in degrees at the desk's nominal
/// distance so it reads next to the health line.
fn report_field_fit(fit: Option<FieldFit>, out: &gaze_core::OutputGeometry) {
    let Some(fit) = fit else {
        println!("correction field: not fitted (fewer than {MIN_HEALTH_ROWS} health stops)");

        return;
    };

    // Normalised units are half the panel per axis, so one unit of rms is a different
    // distance across and down; quote both rather than pretend to a single degree.
    let mm = |rms: f64| format!("{:.0} mm across / {:.0} mm down",
                                rms * out.physical_w_mm * 0.5, rms * out.physical_h_mm * 0.5);

    if fit.score < fit.identity_rms {
        println!("correction field: {:?} from {} health stops; rms {:.3} -> {:.3} held out \
                  (one unit is {})",
                 fit.degree, fit.rows, fit.identity_rms, fit.score, mm(1.0));
    }
    else {
        println!("correction field: identity kept; no degree beat the raw model's rms of \
                  {:.3} over {} health stops (that is {})",
                 fit.identity_rms, fit.rows, mm(fit.identity_rms));
    }
}

/// Measures the committed model on a fresh health grid and refits the field from it.
fn health_cmd(
    config      : &std::path::Path,
    calibration : &std::path::Path,
    blob        : &std::path::Path,
    grid        : usize,
    daydream    : bool,
    address     : Option<&str>,
)
    -> Result<()>
{
    let geometry = load_geometry(config)?;
    let mut cal  = Et5Calibration::load(calibration)
        .with_context(|| format!("loading {}", calibration.display()))?;

    let name = cal.device_output.clone().context("the calibration has no direct display")?;
    let area = cal.device_area.context("the calibration has no trained plane")?;
    let out  = geometry.outputs.iter()
        .find(|o| o.name == name)
        .with_context(|| format!("no output named {name} in the desk config"))?;

    let bytes  = std::fs::read(blob).with_context(|| format!("reading {}", blob.display()))?;
    let report = BlobReport::of(&bytes);

    if cal.device_blob_sha256.as_deref() != Some(report.body_sha256.as_str()) {
        bail!("{} is not the model {} was calibrated against ({} vs {:?})",
              blob.display(), calibration.display(), report.short(), cal.device_blob_sha256);
    }

    let retrain_config = RetrainConfig {
        display           : name.clone(),
        tracker_pitch_deg : load_tracker_pitch(config),
        health_steps      : grid.max(2),
        ..RetrainConfig::default()
    };

    let plan = retrain::plan(&geometry, &retrain_config)
        .map_err(|e| anyhow::anyhow!("planning the health grid: {e}"))?;

    // The grid is laid out over the plan's training rectangle, so the file's plane
    // and the plan's must agree or the stops would be measured against one plane
    // and reported against another.
    if plan.area != area {
        bail!("the desk config now yields a different plane than {} was trained on; \
               re-run calibrate rather than measuring across the two",
              calibration.display());
    }

    println!("uploading {report} and declaring the trained plane (do not interrupt)");

    let mut device = Device::connect_with(ConnectOptions {
        blob          : Some(bytes),
        area          : Some(area),
        double_upload : false,
        check         : BlobCheck::default(),
    })
    .context("connecting to the ET5 with the eye model")?;

    let (overlay, join) = Overlay::spawn().context("spawning the overlay")?;
    let keys = key_sources(daydream, address)?;

    println!("health check: {} stops, a second each, eyes on the dot",
             retrain_config.health_steps * retrain_config.health_steps);

    let health = retrain::run_health(&mut device, &geometry, &overlay, Some(&keys),
                                     &retrain_config, &plan);

    overlay.stop();
    let _ = join.join();

    let health = health.map_err(|e| anyhow::anyhow!("health check did not finish: {e}"))?;

    if let Some((rms, median)) = retrain::health_summary(&health) {
        println!("health: {} stops, {rms:.2} deg rms, {median:.2} deg median", health.len());
    }

    cal.health = health;

    report_field_fit(cal.fit_field_from_health(), out);

    if let Some(kept) = retrain::keep_previous(calibration)? {
        println!("previous calibration kept at {}", kept.display());
    }

    cal.save(calibration).with_context(|| format!("writing {}", calibration.display()))?;
    println!("health and field written to {}", calibration.display());

    Ok(())
}

/// Refits the field in an existing calibration file from its health stops.
fn fit_field(config: &std::path::Path, calibration: &std::path::Path, dry_run: bool) -> Result<()> {
    let geometry = load_geometry(config)?;
    let mut cal  = Et5Calibration::load(calibration)
        .with_context(|| format!("loading {}", calibration.display()))?;

    let name = cal.device_output.clone().context("the calibration has no direct display")?;
    let out  = geometry.outputs.iter()
        .find(|o| o.name == name)
        .with_context(|| format!("no output named {name} in the desk config"))?;

    println!("{} health stops for {name}", cal.health.len());

    let before = cal.output(&name).map(|e| e.field.clone());

    report_field_fit(cal.fit_field_from_health(), out);

    let after = cal.output(&name).map(|e| e.field.clone());

    if dry_run {
        println!("dry run: {} not written", calibration.display());
    }
    else if before == after {
        println!("field unchanged; {} left as it was", calibration.display());
    }
    else {
        cal.save(calibration)
            .with_context(|| format!("writing {}", calibration.display()))?;
        println!("field written to {}", calibration.display());
    }

    Ok(())
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
        manual            : args.manual,
        health_steps      : args.health_grid.max(2),
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
    let keys = key_sources(args.daydream, args.daydream_address.as_deref())?;

    match args.manual {
        true  => println!("eyes on the dot, settle, then click (or c) to take it. Enter \
                           forces the current point in, s skips it, q aborts (nothing \
                           is written)."),
        false => println!("eyes on the dot and hold still until it moves. Enter forces the \
                           current point in, s skips it, q aborts (nothing is written)."),
    }

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
                    RetrainError::TooFewPoints { .. } => {
                        eprintln!("{e}. {} and {} are untouched; the device holds \
                                   whatever the applied rounds taught it, so re-run \
                                   the ceremony (or push the old blob back with \
                                   `blob-push {}`).",
                                  blob_path.display(), out.display(),
                                  blob_path.display());
                    }
                    RetrainError::TrackerLost(_)      => {
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

    match outcome.kept {
        Some(kept) if kept < outcome.accepted => {
            println!("WARNING: the device kept only {kept} of them; the oldest {} were \
                      evicted (store cap {} points / {} bytes, this blob {} bytes)",
                     outcome.accepted - kept, retrain::DEVICE_POINT_CAP,
                     retrain::DEVICE_BLOB_CAP_BYTES, outcome.blob.len());
        }
        Some(kept) => println!("the device kept all {kept}"),
        None       => println!("WARNING: no result trailer on the committed blob"),
    }

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
        println!("  round {}/{} {:<9} ({:<5}) {} accepted ({} gate, {} clicked, {} dwell, \
                  {} forced), {} skipped, {}",
                 number + 1, outcome.rounds.len(), round.name, round.background.name(),
                 round.accepted(), round.gate, round.clicked, round.dwell, round.forced,
                 round.skipped,
                 if round.applied { "applied" } else { "not applied" });
    }

    for line in &outcome.suggestions {
        println!("  point suggestion {line}");
    }

    let report = BlobReport::of(&outcome.blob);

    // The health check reads the model that was just committed. A failure here loses
    // numbers, not the retrain, so it never stops the files being written.
    println!("health check: {} stops, a second each, eyes on the dot",
             retrain_config.health_steps * retrain_config.health_steps);

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
            // Replaced below by the field fitted from the health check, when it
            // beats leaving the firmware's mapping alone.
            field          : FieldMap::identity(),
            pose_rms_deg   : 0.0,
            field_rms_norm : 0.0,
            targets        : outcome.accepted,
            head_gain      : None,
        }],
        health             : health,
    };

    let mut calibration = calibration;

    report_field_fit(calibration.fit_field_from_health(), out_geometry);

    if let Some(kept) = retrain::keep_previous(out)? {
        println!("previous calibration kept at {}", kept.display());
    }

    calibration.save(out)
        .with_context(|| format!("writing {}", out.display()))?;
    println!("calibration written to {}", out.display());
    println!("the online offset is keyed to blob {} and starts over under it", report.short());

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
        None if config.manual => {
            println!("acceptance: manual — a pad click or `c` takes the point when the \
                      vote names it in at least half of the last {} frames ({:.2}s), \
                      Enter forces it; nothing is accepted on the vote alone",
                     retrain::GATE_WINDOW, retrain::GATE_WINDOW_S);
        }
        None               => {
            println!("acceptance: nearest-target vote only ({} of the last {} frames, \
                      {:.2}s of {:.2}s), no accuracy radius",
                     retrain::GATE_MIN_HITS, retrain::GATE_WINDOW,
                     retrain::GATE_HOLD_S, retrain::GATE_WINDOW_S);
        }
        Some((tol_u, tol_v)) => {
            println!("acceptance: {} of the last {} frames ({:.2}s of {:.2}s), and the \
                      median within {:.1} deg (uv {tol_u:.4} x {tol_v:.4})",
                     retrain::GATE_MIN_HITS, retrain::GATE_WINDOW,
                     retrain::GATE_HOLD_S, retrain::GATE_WINDOW_S,
                     config.accept_deg.unwrap_or_default());
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

/// Asks a yes/no question on the terminal. Anything but `y` is no.
fn prompt_yes_no(question: &str) -> Result<bool> {
    print!("{question} [y/N] ");
    std::io::stdout().flush().ok();

    let mut line = String::new();
    std::io::stdin().read_line(&mut line).context("reading the answer")?;

    Ok(line.trim().eq_ignore_ascii_case("y"))
}

fn view(config: &std::path::Path, calibration: Option<&std::path::Path>, direct: Option<&str>)
    -> Result<()>
{
    let geometry = load_geometry(config)?;

    let calibration = {
        match (calibration, direct) {
            (Some(path), _) => Some(Et5Calibration::load(path)
                .with_context(|| format!("loading {}", path.display()))?),
            (None, Some(name)) => Some(direct_calibration(&geometry, name, load_tracker_pitch(config))?),
            (None, None)       => None,
        }
    };

    // The raw per-eye stream is unfiltered (unlike the device's own smoothed 2D
    // output); run it through the standard filter stack so the marker is comparable
    // to what the snapping pipeline will actually consume.
    let mut scale_geometry = geometry.clone();

    if let Some(cal) = &calibration {
        cal.apply_poses(&mut scale_geometry);
    }

    // Raw per-frame jitter (~0.3 deg between 33 Hz frames, ~10 deg/s) plus the odd
    // recovery frame kept tripping the default 30 deg/s fixation gate, so the
    // smoother would flap on and off. A higher gate, a wider velocity window, and a
    // heavier one-euro keep the marker steady; the snapping pipeline tunes its own
    // stack separately.
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
            label      : None,
            background : None,
            pointer    : None,
            mark       : None,
        };

        let _ = overlay.set(state);
    }

    overlay.stop();
    let _ = join.join();

    Ok(())
}

/// A calibration that is nothing but a plane: `name`'s configured pose, pitched into
/// the sensor frame, as the direct-mode display, with no fits at all. Under it the
/// provider declares that plane and maps the firmware's 2D output straight onto the
/// display's logical rect, and disables every other output. The eye model in the
/// flash is whatever it is; the point's bias is not the question this answers.
fn direct_calibration(geometry: &DesktopGeometry, name: &str, tracker_pitch_deg: f64)
    -> Result<Et5Calibration>
{
    let out = geometry.outputs.iter()
        .find(|o| o.name == name)
        .ok_or_else(|| anyhow::anyhow!("no output named {name} in the desk config"))?;

    let desk = plane_corners(out);
    let area = DisplayArea {
        tl_mm : desk_to_sensor(desk.tl_mm, tracker_pitch_deg),
        tr_mm : desk_to_sensor(desk.tr_mm, tracker_pitch_deg),
        bl_mm : desk_to_sensor(desk.bl_mm, tracker_pitch_deg),
    };

    println!(
        "direct on {name}: declaring tl=({:.0}, {:.0}, {:.0}) tr=({:.0}, {:.0}, {:.0}) \
         bl=({:.0}, {:.0}, {:.0}) sensor mm",
        area.tl_mm[0], area.tl_mm[1], area.tl_mm[2],
        area.tr_mm[0], area.tr_mm[1], area.tr_mm[2],
        area.bl_mm[0], area.bl_mm[1], area.bl_mm[2],
    );

    Ok(Et5Calibration {
        format             : CALIBRATION_FORMAT,
        created_unix_s     : Et5Calibration::now_unix_s(),
        lag_s              : 0.0,
        device_output      : Some(name.to_string()),
        device_area        : Some(area),
        device_blob_sha256 : None,
        device_result      : None,
        outputs            : Vec::new(),
        health             : Vec::new(),
    })
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

