//! Shared contracts for cosmic-gaze: coordinate types, gaze samples, UI elements, and the
//! desk geometry model that converts between compositor pixels, physical surfaces, and
//! gaze rays. Every other crate speaks these types; nothing here touches Wayland, devices,
//! or models.

// The workspace style requires explicit struct field syntax (`Foo { x: x }`) everywhere,
// which clippy reads as redundant. The style rule wins inside this crate.
#![allow(clippy::redundant_field_names)]

pub mod geometry;
pub mod noise;
pub mod trainer;
pub mod types;

pub use geometry::{DesktopGeometry, OutputGeometry, SurfaceHit, rotation_ypr};
pub use noise::{NoiseModel, SigmaProfile};
pub use trainer::{TRAINER_SOURCE, TrainerElement, TrainerMessage, TrainerTag, socket_path};
pub use types::{Element, ElementKind, ElementSource, GazeSample, GlobalPx, OutputPx, Ray, Rect};
