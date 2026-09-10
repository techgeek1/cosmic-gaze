//! The dev harness: the session loop from flags, with the debug look.
//! The daemon runs the same loop from the stored config; see `gazed`.

use anyhow::Result;
use clap::Parser;
use gaze_proto::cli::Args;
use gaze_proto::live::Live;
use gaze_proto::session;
use tracing_subscriber::EnvFilter;

fn main() -> Result<()> {
    // Info by default; RUST_LOG overrides it, which is how the per-sample debug lines and
    // the detector's own tracing get turned on.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info")))
        .init();

    let args = Args::parse();

    session::run(&args.session()?, &Live::new(args.tuning()?))
}
