//! Manual check of the controller: is it paired, is the bit layout right, do the buttons
//! and touchpad read as they should.
//!
//! ```text
//! gaze-daydream-cli raw   --seconds 5   # every report, bytes and decoded fields
//! gaze-daydream-cli watch --seconds 30  # button edges, touch strokes, a status line per second
//! ```
//!
//! `watch` is the one to hold the controller for: press each button, drag a thumb across
//! the pad, wave it about, and read what came back.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use clap::{Parser, Subcommand};
use gaze_daydream::{Buttons, Controller, Report};
use glam::Vec3;

/// Command line for `gaze-daydream-cli`.
#[derive(Parser)]
#[command(about = "read the Daydream controller over BlueZ")]
struct Args {
    /// The controller's Bluetooth address. Defaults to the first paired device BlueZ
    /// calls "Daydream controller".
    #[arg(long)]
    address: Option<String>,

    #[command(subcommand)]
    command: Command,
}

/// The subcommands.
#[derive(Subcommand)]
enum Command {
    /// Print every report as it arrives, decoded.
    Raw {
        /// Stop after this many seconds instead of waiting for Ctrl-C.
        #[arg(long)]
        seconds: Option<f64>,
    },

    /// Print button presses and releases, touch strokes, and a status line each second.
    Watch {
        /// Stop after this many seconds instead of waiting for Ctrl-C.
        #[arg(long)]
        seconds: Option<f64>,
    },
}

fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env().add_directive("info".parse()?))
        .with_writer(std::io::stderr)
        .init();

    let args = Args::parse();

    let mut controller = Controller::open(args.address.as_deref())
        .context("opening the Daydream controller")?;

    println!("controller {}", controller.address());

    let result = match args.command {
        Command::Raw { seconds }   => raw(&controller, seconds),
        Command::Watch { seconds } => watch(&controller, seconds),
    };

    controller.stop();

    result
}

/// Whether the run's deadline, if any, has passed.
fn expired(start: Instant, seconds: Option<f64>) -> bool {
    seconds.is_some_and(|s| start.elapsed().as_secs_f64() >= s)
}

/// One line per report.
fn raw(controller: &Controller, seconds: Option<f64>) -> Result<()> {
    let start = Instant::now();

    while !expired(start, seconds) {
        for Report { at, packet: p } in controller.reports() {
            let touch = match p.touch {
                Some(t) => format!("({:.2},{:.2})", t.x, t.y),
                None    => "-".to_owned(),
            };

            println!(
                "{:8.3} t={:3} seq={:2} ori=({:+.2},{:+.2},{:+.2}) acc=({:+5.1},{:+5.1},{:+5.1}) gyro=({:+6.2},{:+6.2},{:+6.2}) touch={} btn={:#04x}",
                (at - start).as_secs_f64(),
                p.time, p.seq,
                p.orientation.x, p.orientation.y, p.orientation.z,
                p.accel.x, p.accel.y, p.accel.z,
                p.gyro.x, p.gyro.y, p.gyro.z,
                touch,
                p.buttons.bits(),
            );
        }

        std::thread::sleep(Duration::from_millis(5));
    }

    Ok(())
}

/// Edges and a status line.
fn watch(controller: &Controller, seconds: Option<f64>) -> Result<()> {
    let start = Instant::now();

    let mut buttons  = Buttons::default();
    let mut touching = false;
    let mut last_seq : Option<u8> = None;
    let mut dropped  = 0u64;
    let mut count    = 0u64;
    let mut gyro_max = 0f32;
    // Net rotation about each controller axis since the last status line, in radians:
    // the sign and axis of a deliberate turn, where the peak only says how fast.
    let mut turn     = Vec3::ZERO;
    let mut last_status = Instant::now();
    let mut latest : Option<Report> = None;

    while !expired(start, seconds) {
        for report in controller.reports() {
            let p = report.packet;
            let t = (report.at - start).as_secs_f64();

            count += 1;

            if let Some(prev) = last_seq {
                let gap = (p.seq.wrapping_sub(prev)) & 0x1F;

                if gap > 1 {
                    dropped += u64::from(gap - 1);
                }
            }

            last_seq = Some(p.seq);
            gyro_max = gyro_max.max(p.gyro.length());

            if let Some(prev) = latest {
                let dt = report.at.saturating_duration_since(prev.at).as_secs_f32().min(0.05);

                turn += p.gyro * dt;
            }

            for b in p.buttons.pressed_since(buttons) {
                println!("{t:8.3} press   {b:?}");
            }

            for b in p.buttons.released_since(buttons) {
                println!("{t:8.3} release {b:?}");
            }

            buttons = p.buttons;

            match (touching, p.touch) {
                (false, Some(at)) => println!("{t:8.3} touch   ({:.2}, {:.2})", at.x, at.y),
                (true,  None)     => println!("{t:8.3} lift"),
                _                 => {}
            }

            touching = p.touch.is_some();
            latest   = Some(report);
        }

        if last_status.elapsed() >= Duration::from_secs(1) {
            let hz = count as f64 / last_status.elapsed().as_secs_f64();

            if let Some(r) = latest {
                let p = r.packet;

                println!(
                    "{:8.3} status  {hz:5.1} Hz dropped={dropped} gyro_peak={gyro_max:5.2} rad/s turn=({:+.2},{:+.2},{:+.2}) rad ori=({:+.2},{:+.2},{:+.2}) touch={}",
                    start.elapsed().as_secs_f64(),
                    turn.x, turn.y, turn.z,
                    p.orientation.x, p.orientation.y, p.orientation.z,
                    p.touch.map_or("-".to_owned(), |t| format!("({:.2},{:.2})", t.x, t.y)),
                );
            }

            count       = 0;
            gyro_max    = 0.0;
            turn        = Vec3::ZERO;
            last_status = Instant::now();
        }

        std::thread::sleep(Duration::from_millis(5));
    }

    Ok(())
}
