//! Manual test harness for `SyntheticProvider`: grabs a mouse, prints samples as they
//! arrive, and optionally records them to a JSONL file for `ReplayProvider`.

use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::Context;
use clap::Parser;
use gaze_core::DesktopGeometry;
use gaze_provider_synthetic::{GazeProvider, SyntheticProvider, to_jsonl_line};

/// Runs `SyntheticProvider` against a grabbed mouse and prints samples to stdout.
#[derive(Parser)]
struct Args {
    /// Name substring to match a device under /dev/input/event*. Ignored if --path is set.
    #[arg(long, default_value = "Lenovo")]
    device: String,

    /// Exact evdev device path, e.g. /dev/input/event7. Overrides --device.
    #[arg(long)]
    path: Option<PathBuf>,

    /// Desk geometry config.
    #[arg(long, default_value = "config/desk.toml")]
    config: PathBuf,

    /// Print the clean point alongside each noisy sample, with the pixel error.
    #[arg(long)]
    truth: bool,

    /// Append each sample as one JSON object per line to this file.
    #[arg(long)]
    record: Option<PathBuf>,

    /// Stop after this many seconds. Runs until the device stream ends if omitted.
    #[arg(long)]
    seconds: Option<f64>,

    /// Logical pixels of clean-point motion per raw REL_X/REL_Y count.
    #[arg(long, default_value_t = 1.0)]
    gain: f64,

    /// Seeds the noise RNG for reproducible runs.
    #[arg(long, default_value_t = 0)]
    seed: u64,
}

fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .init();

    let args = Args::parse();

    let config_text = std::fs::read_to_string(&args.config)
        .with_context(|| format!("reading {}", args.config.display()))?;

    let geometry = DesktopGeometry::from_toml(&config_text)
        .with_context(|| format!("parsing {}", args.config.display()))?;

    let model = geometry.noise
        .ok_or_else(|| anyhow::anyhow!("{} has no [noise] section", args.config.display()))?;

    let mut builder = SyntheticProvider::create()
        .geometry(geometry)
        .model(model)
        .gain_px_per_count(args.gain)
        .seed(args.seed)
        .device_name(args.device.clone());

    if let Some(path) = &args.path {
        builder = builder.device_path(path.clone());
    }

    let mut provider = builder.start().context("starting synthetic provider")?;

    let mut record_writer = args.record.as_ref()
        .map(|path| -> anyhow::Result<_> {
            let file = File::create(path).with_context(|| format!("creating {}", path.display()))?;

            Ok(BufWriter::new(file))
        })
        .transpose()?;

    let deadline = args.seconds.map(|s| Instant::now() + Duration::from_secs_f64(s));
    let mut count = 0u64;

    loop {
        if deadline.is_some_and(|d| Instant::now() >= d) {
            break;
        }

        let Some(sample) = provider.next() else {
            break;
        };

        count += 1;

        if let Some(writer) = record_writer.as_mut() {
            let line = to_jsonl_line(&sample).context("serializing sample")?;
            writeln!(writer, "{line}").context("writing record file")?;
        }

        if args.truth {
            let truth = provider.truth();

            match sample.point {
                Some(p) => {
                    let err_px = ((p.x - truth.x).powi(2) + (p.y - truth.y).powi(2)).sqrt();

                    println!(
                        "t={:8.3}s  truth=({:8.2}, {:8.2})  noisy=({:8.2}, {:8.2})  err={:6.2}px  sigma={:5.2}deg",
                        sample.t_s, truth.x, truth.y, p.x, p.y, err_px, sample.sigma_deg
                    );
                }
                None => {
                    println!(
                        "t={:8.3}s  truth=({:8.2}, {:8.2})  noisy=INVALID",
                        sample.t_s, truth.x, truth.y
                    );
                }
            }
        }
        else {
            match sample.point {
                Some(p) => println!(
                    "t={:8.3}s  point=({:8.2}, {:8.2})  sigma={:5.2}deg  valid={}",
                    sample.t_s, p.x, p.y, sample.sigma_deg, sample.valid
                ),
                None => println!("t={:8.3}s  point=INVALID  valid={}", sample.t_s, sample.valid),
            }
        }
    }

    if let Some(mut writer) = record_writer {
        writer.flush().context("flushing record file")?;
    }

    provider.stop();
    eprintln!("stopped after {count} samples");

    Ok(())
}
