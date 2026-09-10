//! `cosmic-ext-applet-gaze`: the panel applet for the gaze daemon. See `PLAN-UX.md` U3.

// The workspace style writes struct fields out in full, aligned, even when the value
// happens to share the field's name.
#![allow(clippy::redundant_field_names)]

mod app;
mod daemon;
mod knobs;

use tracing_subscriber::EnvFilter;

fn main() -> cosmic::iced::Result {
    // Warnings by default: the panel launches this, and its log is nobody's terminal.
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("warn")))
        .init();

    cosmic::applet::run::<app::App>(())
}
