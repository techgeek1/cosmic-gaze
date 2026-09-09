//! Command line for the trainer.
//!
//! Run `gaze-clicks-cli run` first, on the same desk, then this. The window goes full
//! screen on whichever output it opens on; `--output` names the one whose origin the
//! coverage histogram is computed against, which should be the same one.

use std::path::PathBuf;

use anyhow::Result;
use clap::Parser;
use cosmic::app::Settings;
use gaze_trainer::Config;
use tracing_subscriber::EnvFilter;

/// A working application to navigate, whose clicks are gaze labels.
#[derive(Parser, Debug)]
#[command(name = "gaze-trainer", version)]
struct Cli {
    /// Desk geometry, for the output's origin.
    #[arg(long, default_value = "config/desk.toml")]
    desk          : PathBuf,

    /// The output the window runs full screen on.
    #[arg(long, default_value = "DP-1")]
    output        : String,

    /// Session files the coverage histogram is seeded from.
    #[arg(long, default_value = "config/sessions")]
    sessions      : PathBuf,

    /// Labelled presses between posture prompts.
    #[arg(long, default_value_t = 80)]
    posture_every : u32,

    /// Run without a collector, for looking at the application itself. Presses are
    /// still sent if a collector appears.
    #[arg(long)]
    offline       : bool,

    /// Log at debug level.
    #[arg(long)]
    verbose       : bool,
}

fn main() -> Result<()> {
    let cli = Cli::parse();

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| EnvFilter::new(if cli.verbose { "debug" } else { "info" })))
        .with_target(false)
        .init();

    let config = Config {
        desk          : cli.desk,
        output        : cli.output,
        sessions      : cli.sessions,
        posture_every : cli.posture_every.max(1),
        offline       : cli.offline,
    };

    let settings = Settings::default()
        .client_decorations(true)
        .size(cosmic::iced::Size::new(1920.0, 1080.0));

    cosmic::app::run::<gaze_trainer::App>(settings, config)
        .map_err(|e| anyhow::anyhow!("running the trainer: {e}"))
}
