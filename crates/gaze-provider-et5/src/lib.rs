//! gaze-provider-et5: a `GazeProvider` for the Tobii Eye Tracker 5, speaking the ET5's
//! USB protocol natively (no vendor SDK, no daemon).
//!
//! The wire protocol is a from-scratch Rust implementation of the byte formats
//! documented by the tobiifree reverse-engineering effort and confirmed against this
//! unit: TTP frames over bulk USB with TLV payloads, an HMAC-MD5 realm unlock for the
//! calibration ops, and the 0x500 gaze notification stream. The device does the eye
//! tracking; this crate turns its tracker-space output into `gaze_core::GazeSample`s
//! with a desk-frame ray, and owns the calibration story: the on-device eye model,
//! per-display pose solving, and a client-side correction field.
//!
//! Layering, bottom up:
//!
//! - [`ttp`]: byte-level frames, TLV, reassembly. Pure, no I/O.
//! - [`transport`]: bulk USB via rusb (endpoints, session open, transfer chunking).
//! - [`device`]: a connected tracker: handshake, requests, calibration ops, gaze stream.
//! - [`gaze`]: decoded 0x500 frames (`Et5Frame`).
//! - [`blob`]: identity of the on-device calibration blob (hash, diff, check policy).
//! - [`retrain`]: the ceremony that writes the on-device eye model, once.
//! - [`record`], [`dataset`]: recording sessions and the rows a model trains on.
//! - [`triangulate`], [`pose`]: the plane pass geometry (ray-bundle intersection,
//!   pose from points or rays).
//! - [`provider`]: the `GazeProvider` implementation on top of it all.

pub mod blob;
pub mod calibration;
pub mod dataset;
pub mod device;
pub mod field;
pub mod gaze;
pub mod pose;
pub mod provider;
pub mod record;
pub mod retrain;
pub mod sweep;
pub mod transport;
pub mod triangulate;
pub mod ttp;

pub use blob::{BlobCheck, BlobReport};
pub use calibration::{Et5Calibration, HealthStop};
pub use device::{ConnectOptions, Device, DeviceError};
pub use gaze::Et5Frame;
pub use provider::Et5Provider;
pub use ttp::{DisplayArea, DisplayRect};
