//! The Daydream controller as a commit and refinement channel.
//!
//! DESIGN.md's fine channel: gaze puts the pointer near the target, and a hand-held
//! controller with a touchpad, five buttons and a gyro does the last few pixels and the
//! commit, at a fraction of a mouse's effort. Google's Daydream View controller is that
//! device for a few dollars second hand, talks plain BLE GATT, and its report format is
//! known ([`packet`]).
//!
//! [`Controller`] reads it through BlueZ over the system D-Bus on its own thread; the
//! session drains [`Controller::reports`] once per tick. Pair the controller once with
//! `bluetoothctl` (hold Home until the light blinks, `scan on`, `pair`, `trust`); after that
//! [`Controller::open`] connects it itself, provided it is awake.

pub mod controller;
pub mod packet;

pub use controller::{Controller, DaydreamError, DEVICE_NAME, REPORT_UUID, Report, SERVICE_UUID};
pub use packet::{Button, Buttons, PACKET_LEN, Packet, decode};
