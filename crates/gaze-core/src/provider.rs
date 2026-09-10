//! The `GazeProvider` trait: a source of gaze samples, whatever the hardware.
//!
//! Lived in `gaze-provider-synthetic` until 2026-09-10, when that crate went with the
//! Phase 0 mouse-driven provider; the ET5 provider and the session loop are the two
//! users left, so the trait sits here with the sample type it speaks.

use crate::types::GazeSample;

/// Source of a stream of gaze samples. `next` blocks until a sample is available;
/// `try_next` polls without blocking. Providers run at 30 to 133 Hz (see CLAUDE.md
/// conventions), so `next` is the natural way to drive a dedicated reader thread or
/// loop.
pub trait GazeProvider {
    /// Blocks until the next sample is available. Returns `None` once the provider has
    /// stopped (see `stop`) and no more samples will ever arrive; never blocks forever
    /// past that point.
    fn next(&mut self) -> Option<GazeSample>;

    /// Returns the next sample if one is already available, without blocking. `None`
    /// means either "nothing ready yet" or "stopped" -- a caller that needs to
    /// distinguish the two should track its own stop signal alongside `stop`.
    fn try_next(&mut self) -> Option<GazeSample>;

    /// Stops the provider: releases the device and unblocks pending and future `next`
    /// calls so they return `None`. Idempotent.
    fn stop(&mut self);
}
