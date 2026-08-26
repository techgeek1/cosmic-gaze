//! The `GazeProvider` trait and the small vocabulary shared by every provider
//! implementation: mouse button state and the discrete events built on top of it.

use gaze_core::GazeSample;

/// Source of a stream of gaze samples: a live device, a synthetic generator, or a
/// recorded replay. `next` blocks until a sample is available; `try_next` polls without
/// blocking. `providers run at 30 to 133 Hz` (see CLAUDE.md conventions), so `next` is the
/// natural way to drive a dedicated reader thread or loop.
pub trait GazeProvider {
    /// Blocks until the next sample is available. Returns `None` once the provider has
    /// stopped (see `stop`) and no more samples will ever arrive; never blocks forever
    /// past that point.
    fn next(&mut self) -> Option<GazeSample>;

    /// Returns the next sample if one is already available, without blocking. `None`
    /// means either "nothing ready yet" or "stopped" -- a caller that needs to
    /// distinguish the two should track its own stop signal alongside `stop`.
    fn try_next(&mut self) -> Option<GazeSample>;

    /// Stops the provider: releases any grabbed device and unblocks pending and future
    /// `next` calls so they return `None`. Idempotent.
    fn stop(&mut self);
}

/// One button on the grabbed mouse. The live prototype uses `Left` as its commit key
/// (see PLAN.md's gaze-proto contract).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// Up/down state of each button on the grabbed device, as of the most recently processed
/// event. Read with `SyntheticProvider::buttons`.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct ButtonState {
    pub left   : bool,
    pub right  : bool,
    pub middle : bool,
}

/// A discrete event surfaced alongside the continuous gaze sample stream. Drained with
/// `SyntheticProvider::events`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ProviderEvent {
    /// A button transitioned from up to down.
    ButtonPressed(Button),

    /// The scroll wheel turned by this many whole detents since the last event, positive
    /// up (away from the user), matching the kernel's `REL_WHEEL` sign convention. On a
    /// grabbed device the compositor never sees these, so a consumer that wants scrolling
    /// to still work has to re-inject them (that is what `gaze-proto --scroll` does).
    Wheel(i32),
}
