//! Pixel-only UI element detection: an RGBA frame of one output in, `gaze_core::Element`s
//! in global logical pixels out.
//!
//! Two ONNX models run on the CPU through the system onnxruntime:
//!
//! - **TargetFinder** (arXiv 2607.19907), a YOLO26n fine-tuned on desktop screenshots,
//!   gives widget boxes with six classes mapped into `ElementKind`.
//! - **PP-OCRv5 detection stage**, DBNet with no recognition head, gives text boxes as
//!   `ElementKind::Text` / `ElementSource::Ocr`.
//!
//! The frame is tiled for the widget model because a 3840x1600 ultrawide squashed into a
//! 640 px input loses every small target, and the tiles' boxes are merged with NMS. Text
//! boxes that sit inside a widget box are dropped as that widget's own label, UFO2 style,
//! so one thing on screen produces one candidate.
//!
//! See `README.md` for model provenance, licences, tensor shapes and the fetch commands.

// The workspace style is explicit struct field syntax everywhere, which clippy reads as
// redundant. Same allow as `gaze-core`.
#![allow(clippy::redundant_field_names)]

pub mod detection;
pub mod detector;
pub mod error;
pub mod ocr;
pub mod runtime;
pub mod sample;
pub mod tile;
pub mod widget;

pub use detection::{Detection, drop_oversized, fuse_text, nms};
pub use detector::{DetectConfig, DetectTimings, Detector, DetectorBuilder, NearConfig, TEXT_MODEL, WIDGET_MODEL};
pub use error::{DetectError, Result};
pub use ocr::{OcrWindow, ocr_window};
pub use tile::{Tile, plan_tiles, tile_at, tiles_containing};
pub use widget::{CLASS_NAMES, kind_for_class};
