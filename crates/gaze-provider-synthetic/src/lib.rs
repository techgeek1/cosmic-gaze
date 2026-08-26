//! gaze-provider-synthetic: a `GazeProvider` that grabs a real evdev mouse and drives it
//! through the desk's noise model to synthesize samples that look like a remote eye
//! tracker's, plus a `ReplayProvider` for replaying a recorded JSONL session. See
//! PLAN.md's "gaze-provider-synthetic" contract for the full spec this implements.

pub mod device;
pub mod provider;
pub mod replay;
pub mod synthetic;

pub use device::{DeviceError, WheelAccumulator};
pub use provider::{Button, ButtonState, GazeProvider, ProviderEvent};
pub use replay::{ReplayError, ReplayProvider, to_jsonl_line};
pub use synthetic::{ProviderError, SyntheticProvider, SyntheticProviderBuilder};
