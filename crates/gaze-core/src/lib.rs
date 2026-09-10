//! Shared contracts for cosmic-gaze: coordinate types, gaze samples, UI elements, the
//! provider trait, and the desk geometry model that converts between compositor pixels,
//! physical surfaces, and gaze rays. Every other crate speaks these types; nothing here
//! touches Wayland, devices, or models.

// The workspace style requires explicit struct field syntax (`Foo { x: x }`) everywhere,
// which clippy reads as redundant. The style rule wins inside this crate.
#![allow(clippy::redundant_field_names)]

pub mod geometry;
pub mod provider;
pub mod types;

pub use geometry::{DesktopGeometry, OutputGeometry, SurfaceHit, rotation_ypr};
pub use provider::GazeProvider;
pub use types::{Element, ElementKind, ElementSource, GazeSample, GlobalPx, OutputPx, Ray, Rect};
