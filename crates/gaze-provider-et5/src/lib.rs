//! gaze-provider-et5: a `GazeProvider` for the Tobii Eye Tracker 5, speaking the ET5's
//! USB protocol natively (no vendor SDK, no daemon).
//!
//! The wire protocol is a from-scratch Rust implementation of the byte formats
//! documented by the tobiifree reverse-engineering effort and confirmed against this
//! unit: TTP frames over bulk USB with TLV payloads, an HMAC-MD5 realm unlock for the
//! calibration ops, and the 0x500 gaze notification stream. The device does the eye
//! tracking; this crate turns its tracker-space output into `gaze_core::GazeSample`s
//! with a desk-frame ray, and owns the calibration story: the on-device eye model,
//! the declared plane, a client-side correction field fitted from the health check,
//! and the day's offset learnt from real clicks.
//!
//! Layering, bottom up:
//!
//! - [`ttp`]: byte-level frames, TLV, reassembly. Pure, no I/O.
//! - [`transport`]: bulk USB via rusb (endpoints, session open, transfer chunking).
//! - [`device`]: a connected tracker: handshake, requests, calibration ops, gaze stream.
//! - [`gaze`]: decoded 0x500 frames (`Et5Frame`).
//! - [`blob`]: identity of the on-device calibration blob (the model body's hash,
//!   diff, check policy) and the firmware's per-point result table off its trailer.
//! - [`retrain`]: the ceremony that writes the on-device eye model, once, and the
//!   health check that follows it.
//! - [`calibration`], [`field`]: the calibration file and the correction field.
//! - [`offset`]: the online offset the real clicks feed.
//! - [`provider`]: the `GazeProvider` implementation on top of it all.
//!
//! The residual model, its trainer and the click flywheel that fed it were removed on
//! 2026-09-10 (git history): the offset alone carried the day.

pub mod blob;
pub mod calibration;
pub mod device;
pub mod field;
pub mod gaze;
pub mod offset;
pub mod provider;
pub mod retrain;
pub mod transport;
pub mod ttp;

pub use blob::{BlobCheck, BlobReport, CalibrationResult};
pub use calibration::{Et5Calibration, HealthStop};
pub use device::{ConnectOptions, Device, DeviceError};
pub use gaze::Et5Frame;
pub use offset::{ClickFeedback, ClickVia, OffsetParams, OnlineOffset};
pub use provider::{Et5Provider, OffsetSummary};
pub use ttp::{DisplayArea, DisplayRect};
