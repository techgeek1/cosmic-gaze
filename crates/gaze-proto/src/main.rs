//! Live phase 0 loop: a grabbed mouse becomes synthetic gaze, the gaze snaps to detected
//! UI boxes, the overlay shows where it landed, and the mouse's own buttons commit, exit,
//! and force a redetect. See `PLAN.md`'s gaze-proto contract.

use anyhow::Result;
use clap::Parser;
use gaze_proto::cli::Args;
use gaze_proto::session;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    // Info by default; RUST_LOG overrides it, which is how the per-sample debug lines and
    // the detector's own tracing get turned on.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    session::run(&Args::parse())
}
