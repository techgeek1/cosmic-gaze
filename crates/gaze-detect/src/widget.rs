//! The widget detector: TargetFinder's YOLO26n fine-tune run over a tiled frame.
//!
//! The exported graph is end-to-end (`end2end: True` in the ONNX metadata), so the network
//! itself already emits a fixed 300 deduplicated boxes per tile and no per-tile NMS is
//! needed. NMS across tiles still is, and lives in `detection`.

use std::sync::Mutex;

use gaze_core::{ElementKind, ElementSource, Rect};
use ort::session::Session;
use ort::value::TensorRef;
use rayon::iter::{IndexedParallelIterator, ParallelIterator};
use rayon::slice::ParallelSliceMut;

use crate::detection::Detection;
use crate::error::{DetectError, Result};
use crate::sample::{bilinear, nearest};
use crate::tile::Tile;

/// Letterbox fill colour, matching Ultralytics' own preprocessing so padded tiles look to
/// the network like the ones it was trained on.
const PAD_VALUE : f32 = 114.0 / 255.0;

/// Number of rows in the end-to-end output, one per candidate box.
const MAX_BOXES : usize = 300;

/// Columns of the end-to-end output: `x1, y1, x2, y2, score, class`.
const BOX_STRIDE : usize = 6;

/// TargetFinder's class list, in model index order. Read out of the checkpoint's `names`
/// map at export time; see `scripts/fetch_models.py`.
pub const CLASS_NAMES : [&str; 6] = ["Button", "ToggleButton", "Hyperlink", "Text", "TextInput", "Slider"];

/// The widget model and the buffers its preprocessing needs.
pub struct WidgetModel {
    /// `Session::run` takes `&mut self`, but `Detector::detect` is `&self` per the crate
    /// contract, so the session sits behind a mutex. Contention is not a concern: a frame
    /// is detected on one thread and the model is the slow part anyway.
    session  : Mutex<Session>,
    /// Square side of the network input in pixels, read from the graph.
    input_px : u32,
    /// Name of the single input tensor, read from the graph.
    input    : String,
}

// --- WidgetModel ---

impl WidgetModel {
    /// Opens the exported TargetFinder graph and reads its input geometry.
    ///
    /// `intra_threads` is passed straight to onnxruntime's intra-op thread pool; zero lets
    /// onnxruntime pick.
    pub fn load(path: &std::path::Path, intra_threads: usize) -> Result<WidgetModel> {
        let session = crate::runtime::build_session(path, intra_threads)?;

        // Read the input geometry before the session is moved into the mutex: the export is
        // static shape [1, 3, S, S] and the tiler needs S to size its inputs.
        let (name, side) = {
            let input = session
                .inputs()
                .first()
                .ok_or_else(|| DetectError::BadModel { path: path.into(), what: "no input tensor".into() })?;

            let dims = input
                .dtype()
                .tensor_shape()
                .ok_or_else(|| DetectError::BadModel { path: path.into(), what: "input is not a tensor".into() })?;

            if dims.len() != 4 || dims[2] < 1 || dims[2] != dims[3] {
                return Err(DetectError::BadModel {
                    path : path.into(),
                    what : format!("expected a square [1, 3, S, S] input, got {dims:?}"),
                });
            }

            (input.name().to_owned(), dims[2] as u32)
        };

        Ok(WidgetModel {
            session  : Mutex::new(session),
            input_px : side,
            input    : name,
        })
    }

    /// Square side of the network input in pixels.
    pub fn input_px(&self) -> u32 {
        self.input_px
    }

    /// Runs every tile and returns boxes in frame pixels, before cross-tile NMS.
    ///
    /// `rgba` is `w * h * 4` bytes. Boxes below `conf` are dropped inside the loop so the
    /// NMS pass downstream stays small.
    pub fn detect_tiles(
        &self,
        rgba  : &[u8],
        w     : u32,
        h     : u32,
        tiles : &[Tile],
        conf  : f32,
    )
        -> Result<Vec<Detection>>
    {
        let side  = self.input_px as usize;
        let shape = vec![1_i64, 3, side as i64, side as i64];

        let mut input = vec![0.0_f32; 3 * side * side];
        let mut out   = Vec::new();

        let mut session = self.session.lock().map_err(|_| DetectError::Poisoned)?;

        for tile in tiles {
            fill_input(rgba, w, h, tile, &mut input);

            let tensor  = TensorRef::from_array_view((shape.clone(), input.as_slice()))?;
            let outputs = session.run(ort::inputs![self.input.as_str() => tensor])?;
            let (_, raw) = outputs[0].try_extract_tensor::<f32>()?;

            decode_tile(raw, tile, conf, w, h, &mut out);
        }

        Ok(out)
    }
}

// --- Class mapping ---

/// Maps a TargetFinder class index onto the shared `ElementKind` taxonomy.
///
/// `Slider` has no counterpart in `gaze_core::ElementKind` and becomes `Unknown` rather
/// than being folded into `Button`, so the snap benchmark can tell a genuine button from
/// a control this crate could not name. Adding an `ElementKind::Slider` would let the snap
/// engine rank sliders properly.
pub fn kind_for_class(class: u32) -> ElementKind {
    match class {
        0 => ElementKind::Button,
        1 => ElementKind::Checkbox,
        2 => ElementKind::Link,
        3 => ElementKind::Text,
        4 => ElementKind::Input,
        5 => ElementKind::Slider,
        _ => ElementKind::Unknown,
    }
}

// --- Preprocessing ---

/// Renders one tile of an RGBA frame into a CHW float input plane.
///
/// `out` must be `3 * input * input` long. Pixels outside the frame get the letterbox
/// fill. Sampling is bilinear on the pixel-centre convention, with an exact copy path when
/// the tile is not scaled, which is the common 640 px tile case.
fn fill_input(rgba: &[u8], w: u32, h: u32, tile: &Tile, out: &mut [f32]) {
    let side = tile.input as usize;
    let n    = side * side;

    debug_assert_eq!(out.len(), 3 * n);

    let (r, rest) = out.split_at_mut(n);
    let (g, b)    = rest.split_at_mut(n);

    let inv_scale = 1.0 / tile.scale;
    let unscaled  = (tile.scale - 1.0).abs() < 1e-12;

    r.par_chunks_mut(side)
        .zip(g.par_chunks_mut(side))
        .zip(b.par_chunks_mut(side))
        .enumerate()
        .for_each(|(my, ((rr, gg), bb))| {
            // Frame-space centre of this output row, then back to a sample index.
            let sy = ((my as f64 + 0.5) - tile.pad_y) * inv_scale + tile.y as f64 - 0.5;

            for mx in 0..side {
                let sx = ((mx as f64 + 0.5) - tile.pad_x) * inv_scale + tile.x as f64 - 0.5;

                let px = {
                    if unscaled {
                        nearest(rgba, w, h, sx, sy)
                    }
                    else {
                        bilinear(rgba, w, h, sx, sy)
                    }
                };

                match px {
                    Some(c) => {
                        rr[mx] = c[0];
                        gg[mx] = c[1];
                        bb[mx] = c[2];
                    }
                    None => {
                        rr[mx] = PAD_VALUE;
                        gg[mx] = PAD_VALUE;
                        bb[mx] = PAD_VALUE;
                    }
                }
            }
        });
}

// --- Decoding ---

/// Turns one tile's raw `[1, 300, 6]` output into frame-space detections.
///
/// Rows are ordered by descending confidence and the tail is zero-padded, so the loop can
/// stop at the first row under `conf`.
fn decode_tile(
    raw   : &[f32],
    tile  : &Tile,
    conf  : f32,
    w     : u32,
    h     : u32,
    out   : &mut Vec<Detection>,
)
{
    let rows = (raw.len() / BOX_STRIDE).min(MAX_BOXES);

    for i in 0..rows {
        let row   = &raw[i * BOX_STRIDE..i * BOX_STRIDE + BOX_STRIDE];
        let score = row[4];

        if score < conf {
            break;
        }

        let (x0, y0) = tile.to_frame(row[0] as f64, row[1] as f64);
        let (x1, y1) = tile.to_frame(row[2] as f64, row[3] as f64);

        // A box hanging over the tile edge is clipped to the frame, not to the tile: the
        // overlap means the neighbouring tile has the rest of it and NMS will pick whichever
        // copy is more confident.
        let x0 = x0.clamp(0.0, w as f64);
        let y0 = y0.clamp(0.0, h as f64);
        let x1 = x1.clamp(0.0, w as f64);
        let y1 = y1.clamp(0.0, h as f64);

        if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
            continue;
        }

        out.push(Detection {
            rect   : Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 },
            score  : score,
            kind   : kind_for_class(row[5] as u32),
            source : ElementSource::Detector,
        });
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::{ElementKind, ElementSource};

    use super::{PAD_VALUE, decode_tile, fill_input, kind_for_class};
    use crate::tile::Tile;

    /// A 4x4 RGBA frame where each pixel's red channel encodes its index.
    fn ramp_frame() -> (Vec<u8>, u32, u32) {
        let mut rgba = Vec::new();

        for y in 0..4_u32 {
            for x in 0..4_u32 {
                rgba.extend_from_slice(&[(y * 4 + x) as u8 * 16, 0, 255, 255]);
            }
        }

        (rgba, 4, 4)
    }

    /// An unscaled, unpadded tile must copy the frame through untouched.
    #[test]
    fn unscaled_tile_copies_pixels_exactly() {
        let (rgba, w, h) = ramp_frame();

        let tile = Tile { x: 0, y: 0, w: 4, h: 4, scale: 1.0, pad_x: 0.0, pad_y: 0.0, input: 4 };
        let mut out = vec![0.0_f32; 3 * 16];

        fill_input(&rgba, w, h, &tile, &mut out);

        for i in 0..16 {
            assert!((out[i] - (i as f32 * 16.0) / 255.0).abs() < 1e-6, "red plane index {i}");
            assert_eq!(out[16 + i], 0.0);
            assert!((out[32 + i] - 1.0).abs() < 1e-6);
        }
    }

    /// An offset tile reads from the right part of the frame.
    #[test]
    fn offset_tile_reads_the_right_region() {
        let (rgba, w, h) = ramp_frame();

        let tile = Tile { x: 2, y: 2, w: 2, h: 2, scale: 1.0, pad_x: 0.0, pad_y: 0.0, input: 2 };
        let mut out = vec![0.0_f32; 3 * 4];

        fill_input(&rgba, w, h, &tile, &mut out);

        // Frame indices (2,2), (3,2), (2,3), (3,3) are 10, 11, 14, 15.
        for (slot, idx) in [(0, 10), (1, 11), (2, 14), (3, 15)] {
            assert!((out[slot] - (idx as f32 * 16.0) / 255.0).abs() < 1e-6, "slot {slot}");
        }
    }

    /// Padding outside the frame gets the letterbox fill, not black, and not a wrapped read.
    #[test]
    fn letterbox_region_is_filled() {
        let (rgba, w, h) = ramp_frame();

        let tile = Tile { x: 0, y: 0, w: 4, h: 4, scale: 1.0, pad_x: 0.0, pad_y: 2.0, input: 8 };
        let mut out = vec![0.0_f32; 3 * 64];

        fill_input(&rgba, w, h, &tile, &mut out);

        // Row 0 is above the frame and row 7 below it.
        for x in 0..8 {
            assert_eq!(out[x], PAD_VALUE);
            assert_eq!(out[7 * 8 + x], PAD_VALUE);
        }
    }

    /// Decoding maps a tile-local box into frame space and tags it as a detector box.
    #[test]
    fn decode_maps_boxes_into_frame_space() {
        let tile = Tile { x: 544, y: 0, w: 640, h: 640, scale: 1.0, pad_x: 0.0, pad_y: 0.0, input: 640 };

        let mut raw = vec![0.0_f32; 6 * 3];
        raw[0..6].copy_from_slice(&[10.0, 20.0, 60.0, 40.0, 0.9, 0.0]);
        raw[6..12].copy_from_slice(&[10.0, 20.0, 60.0, 40.0, 0.4, 2.0]);

        let mut out = Vec::new();
        decode_tile(&raw, &tile, 0.5, 3840, 1600, &mut out);

        assert_eq!(out.len(), 1, "the 0.4 row is below threshold and stops the scan");
        assert_eq!(out[0].rect.x, 554.0);
        assert_eq!(out[0].rect.w, 50.0);
        assert_eq!(out[0].kind, ElementKind::Button);
        assert_eq!(out[0].source, ElementSource::Detector);
    }

    /// Boxes that clip the frame edge are clamped, and sub-pixel leftovers are dropped.
    #[test]
    fn decode_clamps_to_the_frame_and_drops_slivers() {
        let tile = Tile { x: 3200, y: 0, w: 640, h: 640, scale: 1.0, pad_x: 0.0, pad_y: 0.0, input: 640 };

        let mut raw = vec![0.0_f32; 6 * 2];
        raw[0..6].copy_from_slice(&[600.0, 10.0, 700.0, 30.0, 0.9, 0.0]);
        raw[6..12].copy_from_slice(&[639.9, 10.0, 700.0, 30.0, 0.8, 0.0]);

        let mut out = Vec::new();
        decode_tile(&raw, &tile, 0.5, 3840, 1600, &mut out);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rect.x, 3800.0);
        assert_eq!(out[0].rect.w, 40.0);
    }

    /// The class mapping is part of the crate's contract with the snap engine.
    #[test]
    fn class_mapping_is_stable() {
        assert_eq!(kind_for_class(0), ElementKind::Button);
        assert_eq!(kind_for_class(1), ElementKind::Checkbox);
        assert_eq!(kind_for_class(2), ElementKind::Link);
        assert_eq!(kind_for_class(3), ElementKind::Text);
        assert_eq!(kind_for_class(4), ElementKind::Input);
        assert_eq!(kind_for_class(5), ElementKind::Slider);
        assert_eq!(kind_for_class(99), ElementKind::Unknown);
    }
}
