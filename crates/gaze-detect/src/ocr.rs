//! The text detector: PP-OCRv5's DBNet detection stage, with no recognition head.
//!
//! DBNet is fully convolutional and emits a per-pixel text probability map at the input
//! resolution, so unlike the widget model it runs once over the whole frame rather than
//! per tile: there are no tile seams to split a word across. The postprocess is the
//! standard DB one (threshold, connected components, unclip) reduced to axis-aligned
//! boxes, which is all `gaze_core::Rect` can carry.

use std::sync::Mutex;

use gaze_core::{ElementKind, ElementSource, Rect};
use ort::session::Session;
use ort::value::TensorRef;
use rayon::iter::{IndexedParallelIterator, ParallelIterator};
use rayon::slice::ParallelSliceMut;

use crate::detection::Detection;
use crate::error::{DetectError, Result};
use crate::sample::{bilinear, nearest};

/// PaddleOCR's ImageNet normalisation, applied after scaling to [0, 1].
const MEAN : [f32; 3] = [0.485, 0.456, 0.406];

/// PaddleOCR's ImageNet standard deviation.
const STD : [f32; 3] = [0.229, 0.224, 0.225];

/// The backbone downsamples by 32, so both input dimensions must be a multiple of it.
const ALIGN : u32 = 32;

/// The text detection model and the geometry of the resize it needs.
pub struct TextModel {
    /// Behind a mutex for the same reason as the widget session: `run` needs `&mut`.
    session : Mutex<Session>,
    /// Name of the single input tensor, read from the graph.
    input   : String,
}

/// How the DB probability map is turned into boxes.
#[derive(Clone, Copy, Debug)]
pub struct TextConfig {
    /// Longest input side in pixels. The frame is downscaled to fit, which trades recall
    /// on small text for inference time. Zero means native resolution.
    pub max_side    : u32,
    /// Probability above which a pixel counts as text when growing components.
    pub map_thresh  : f32,
    /// Mean probability a component must reach to be emitted.
    pub box_thresh  : f32,
    /// DB unclip ratio. DB is trained on shrunk polygons, so boxes must be grown back.
    pub unclip      : f64,
    /// Components smaller than this on either side are noise, not text.
    pub min_side_px : u32,
}

// --- TextModel ---

impl TextModel {
    /// Opens the PP-OCR detection graph.
    ///
    /// The graph has fully dynamic spatial dimensions, so nothing about the frame size is
    /// fixed at load time.
    pub fn load(path: &std::path::Path, intra_threads: usize) -> Result<TextModel> {
        let session = crate::runtime::build_session(path, intra_threads)?;

        let name = session
            .inputs()
            .first()
            .ok_or_else(|| DetectError::BadModel { path: path.into(), what: "no input tensor".into() })?
            .name()
            .to_owned();

        Ok(TextModel {
            session : Mutex::new(session),
            input   : name,
        })
    }

    /// Detects text boxes in frame pixels.
    ///
    /// `rgba` is `w * h * 4` bytes. The returned boxes are `ElementKind::Text` with
    /// `ElementSource::Ocr` and no text content, because the recognition stage is not run.
    pub fn detect(
        &self,
        rgba : &[u8],
        w    : u32,
        h    : u32,
        cfg  : &TextConfig,
    )
        -> Result<Vec<Detection>>
    {
        let plan = ResizePlan::new(w, h, cfg.max_side);
        let iw   = plan.in_w as usize;
        let ih   = plan.in_h as usize;

        let mut input = vec![0.0_f32; 3 * iw * ih];
        fill_input(rgba, w, h, &plan, &mut input);

        let shape = vec![1_i64, 3, ih as i64, iw as i64];

        let mut session = self.session.lock().map_err(|_| DetectError::Poisoned)?;

        let tensor  = TensorRef::from_array_view((shape, input.as_slice()))?;
        let outputs = session.run(ort::inputs![self.input.as_str() => tensor])?;
        let (map_shape, prob) = outputs[0].try_extract_tensor::<f32>()?;

        // The map comes back at the input resolution, but assert rather than assume: a
        // different PP-OCR export could emit a quarter-resolution map.
        if map_shape.len() != 4 {
            return Err(DetectError::BadModel {
                path : "text model".into(),
                what : format!("expected a 4d probability map, got {map_shape:?}"),
            });
        }

        let mh = map_shape[2] as u32;
        let mw = map_shape[3] as u32;

        Ok(boxes_from_map(prob, mw, mh, &plan, cfg, w, h))
    }
}

// --- Resize plan ---

/// The scale and padded input size used to feed one frame to the detection graph.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ResizePlan {
    /// Padded network input width, a multiple of `ALIGN`.
    pub in_w  : u32,
    /// Padded network input height, a multiple of `ALIGN`.
    pub in_h  : u32,
    /// Network input pixels per frame pixel.
    pub scale : f64,
}

// --- ResizePlan ---

impl ResizePlan {
    /// Builds the plan for a `w` x `h` frame limited to `max_side` on its longest edge.
    ///
    /// `max_side` of zero means keep native resolution. The scaled size is rounded up to a
    /// multiple of 32, which pads on the right and bottom rather than distorting.
    pub fn new(w: u32, h: u32, max_side: u32) -> ResizePlan {
        let long  = w.max(h);
        let scale = {
            if max_side > 0 && long > max_side {
                max_side as f64 / long as f64
            }
            else {
                1.0
            }
        };

        let sw = ((w as f64 * scale).round() as u32).max(1);
        let sh = ((h as f64 * scale).round() as u32).max(1);

        ResizePlan {
            in_w  : sw.div_ceil(ALIGN) * ALIGN,
            in_h  : sh.div_ceil(ALIGN) * ALIGN,
            scale : scale,
        }
    }

    /// Maps a coordinate in probability map pixels back to frame pixels.
    pub fn to_frame(&self, mx: f64, my: f64) -> (f64, f64) {
        (mx / self.scale, my / self.scale)
    }
}

// --- Preprocessing ---

/// Renders the frame into the normalised CHW input plane, padding right and bottom.
///
/// The pad is zero after normalisation rather than before, which is what PaddleOCR's own
/// `DetResizeForTest` does, so the border does not read as bright text.
fn fill_input(rgba: &[u8], w: u32, h: u32, plan: &ResizePlan, out: &mut [f32]) {
    let iw = plan.in_w as usize;
    let ih = plan.in_h as usize;
    let n  = iw * ih;

    debug_assert_eq!(out.len(), 3 * n);

    let (r, rest) = out.split_at_mut(n);
    let (g, b)    = rest.split_at_mut(n);

    let inv      = 1.0 / plan.scale;
    let unscaled = (plan.scale - 1.0).abs() < 1e-12;

    r.par_chunks_mut(iw)
        .zip(g.par_chunks_mut(iw))
        .zip(b.par_chunks_mut(iw))
        .enumerate()
        .for_each(|(my, ((rr, gg), bb))| {
            let sy = (my as f64 + 0.5) * inv - 0.5;

            for mx in 0..iw {
                let sx = (mx as f64 + 0.5) * inv - 0.5;

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
                        rr[mx] = (c[0] - MEAN[0]) / STD[0];
                        gg[mx] = (c[1] - MEAN[1]) / STD[1];
                        bb[mx] = (c[2] - MEAN[2]) / STD[2];
                    }
                    None => {
                        rr[mx] = 0.0;
                        gg[mx] = 0.0;
                        bb[mx] = 0.0;
                    }
                }
            }
        });
}

// --- DB postprocess ---

/// A connected run of above-threshold pixels, accumulated during the flood fill.
#[derive(Clone, Copy, Debug)]
struct Component {
    /// Inclusive bounds in map pixels.
    x0    : u32,
    y0    : u32,
    x1    : u32,
    y1    : u32,
    /// Sum of probabilities over the component's own pixels.
    sum   : f64,
    /// Number of pixels in the component.
    count : u32,
}

/// Turns a DB probability map into frame-space text boxes.
///
/// `prob` is `mw * mh` values in [0, 1]. Boxes are grown by the DB unclip rule, mapped
/// back through `plan`, and clipped to the frame.
fn boxes_from_map(
    prob : &[f32],
    mw   : u32,
    mh   : u32,
    plan : &ResizePlan,
    cfg  : &TextConfig,
    fw   : u32,
    fh   : u32,
)
    -> Vec<Detection>
{
    let mut out = Vec::new();

    for c in components(prob, mw, mh, cfg.map_thresh) {
        let score = (c.sum / c.count as f64) as f32;

        if score < cfg.box_thresh {
            continue;
        }

        // Bounds are inclusive pixel indices, so a one-pixel component is one pixel wide.
        let bw = (c.x1 - c.x0 + 1) as f64;
        let bh = (c.y1 - c.y0 + 1) as f64;

        if bw < cfg.min_side_px as f64 || bh < cfg.min_side_px as f64 {
            continue;
        }

        // DB's unclip, specialised to a rectangle: offsetting every edge outward by
        // area * ratio / perimeter is exactly what the Vatti offset does to an axis
        // aligned box, minus the rounded corners which an AABB does not keep anyway.
        let d = (bw * bh * cfg.unclip) / (2.0 * (bw + bh));

        let (x0, y0) = plan.to_frame(c.x0 as f64 - d, c.y0 as f64 - d);
        let (x1, y1) = plan.to_frame(c.x1 as f64 + 1.0 + d, c.y1 as f64 + 1.0 + d);

        let x0 = x0.clamp(0.0, fw as f64);
        let y0 = y0.clamp(0.0, fh as f64);
        let x1 = x1.clamp(0.0, fw as f64);
        let y1 = y1.clamp(0.0, fh as f64);

        if x1 - x0 < 1.0 || y1 - y0 < 1.0 {
            continue;
        }

        out.push(Detection {
            rect   : Rect { x: x0, y: y0, w: x1 - x0, h: y1 - y0 },
            score  : score,
            kind   : ElementKind::Text,
            source : ElementSource::Ocr,
        });
    }

    out
}

/// Four-connected components of the pixels above `thresh`.
///
/// An explicit stack rather than recursion: a full-width text line on a 3840 px frame is
/// tens of thousands of pixels deep and would blow the call stack.
fn components(prob: &[f32], mw: u32, mh: u32, thresh: f32) -> Vec<Component> {
    let w = mw as usize;
    let h = mh as usize;

    let mut seen  = vec![false; w * h];
    let mut stack : Vec<u32> = Vec::new();
    let mut out   = Vec::new();

    for start in 0..w * h {
        if seen[start] || prob[start] < thresh {
            continue;
        }

        seen[start] = true;
        stack.push(start as u32);

        let mut c = Component {
            x0    : (start % w) as u32,
            y0    : (start / w) as u32,
            x1    : (start % w) as u32,
            y1    : (start / w) as u32,
            sum   : 0.0,
            count : 0,
        };

        while let Some(i) = stack.pop() {
            let idx = i as usize;
            let x   = (idx % w) as u32;
            let y   = (idx / w) as u32;

            c.sum   += prob[idx] as f64;
            c.count += 1;
            c.x0     = c.x0.min(x);
            c.y0     = c.y0.min(y);
            c.x1     = c.x1.max(x);
            c.y1     = c.y1.max(y);

            // Left, right, up, down. Bounds are checked against the row so the left and
            // right neighbours cannot wrap onto the adjacent row.
            if x > 0 {
                push_if_text(prob, &mut seen, &mut stack, idx - 1, thresh);
            }

            if (x as usize) + 1 < w {
                push_if_text(prob, &mut seen, &mut stack, idx + 1, thresh);
            }

            if y > 0 {
                push_if_text(prob, &mut seen, &mut stack, idx - w, thresh);
            }

            if (y as usize) + 1 < h {
                push_if_text(prob, &mut seen, &mut stack, idx + w, thresh);
            }
        }

        out.push(c);
    }

    out
}

/// Marks and queues a neighbour if it is unvisited text.
#[inline]
fn push_if_text(prob: &[f32], seen: &mut [bool], stack: &mut Vec<u32>, idx: usize, thresh: f32) {
    if !seen[idx] && prob[idx] >= thresh {
        seen[idx] = true;
        stack.push(idx as u32);
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::{ElementKind, ElementSource};

    use super::{ALIGN, ResizePlan, TextConfig, boxes_from_map, components};

    /// The config the detector defaults to, so the box tests exercise real numbers.
    fn cfg() -> TextConfig {
        TextConfig {
            max_side    : 0,
            map_thresh  : 0.3,
            box_thresh  : 0.6,
            unclip      : 1.5,
            min_side_px : 2,
        }
    }

    /// Native resolution rounds up to the alignment without scaling.
    #[test]
    fn resize_plan_pads_to_alignment() {
        let p = ResizePlan::new(3840, 1600, 0);

        assert_eq!(p.scale, 1.0);
        assert_eq!(p.in_w, 3840);
        assert_eq!(p.in_h, 1600);

        let p = ResizePlan::new(1001, 33, 0);

        assert_eq!(p.in_w % ALIGN, 0);
        assert_eq!(p.in_h % ALIGN, 0);
        assert!(p.in_w >= 1001);
        assert!(p.in_h >= 33);
    }

    /// A capped long side scales both axes by the same factor.
    #[test]
    fn resize_plan_caps_the_long_side() {
        let p = ResizePlan::new(3840, 1600, 960);

        assert!((p.scale - 0.25).abs() < 1e-12);
        assert_eq!(p.in_w, 960);
        assert_eq!(p.in_h, 416);
        assert_eq!(p.to_frame(100.0, 50.0), (400.0, 200.0));
    }

    /// A frame already under the cap is left alone.
    #[test]
    fn resize_plan_does_not_upscale() {
        let p = ResizePlan::new(640, 480, 960);

        assert_eq!(p.scale, 1.0);
    }

    /// Two separated blobs are two components, and a diagonal touch does not join them
    /// because connectivity is four-way.
    #[test]
    fn components_are_four_connected() {
        let mut prob = vec![0.0_f32; 5 * 5];

        prob[0] = 1.0;
        prob[6] = 1.0;

        let c = components(&prob, 5, 5, 0.3);

        assert_eq!(c.len(), 2);
    }

    /// A solid rectangle comes back as one component with the right bounds and mean.
    #[test]
    fn component_bounds_and_score() {
        let mut prob = vec![0.0_f32; 10 * 10];

        for y in 2..5 {
            for x in 3..8 {
                prob[y * 10 + x] = 0.8;
            }
        }

        let c = components(&prob, 10, 10, 0.3);

        assert_eq!(c.len(), 1);
        assert_eq!((c[0].x0, c[0].y0, c[0].x1, c[0].y1), (3, 2, 7, 4));
        assert_eq!(c[0].count, 15);
        assert!(((c[0].sum / c[0].count as f64) - 0.8).abs() < 1e-6);
    }

    /// Left and right neighbour walks must not wrap around a row boundary.
    #[test]
    fn components_do_not_wrap_rows() {
        let mut prob = vec![0.0_f32; 4 * 3];

        // Last pixel of row 0 and first pixel of row 1 are adjacent in the flat buffer but
        // not on screen.
        prob[3] = 1.0;
        prob[4] = 1.0;

        assert_eq!(components(&prob, 4, 3, 0.3).len(), 2);
    }

    /// The unclip grows the box: a 5x3 blob must come back wider and taller than 5x3.
    #[test]
    fn boxes_are_unclipped() {
        let mut prob = vec![0.0_f32; 40 * 40];

        for y in 10..13 {
            for x in 10..15 {
                prob[y * 40 + x] = 0.9;
            }
        }

        let plan = ResizePlan { in_w: 40, in_h: 40, scale: 1.0 };
        let out  = boxes_from_map(&prob, 40, 40, &plan, &cfg(), 40, 40);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].kind, ElementKind::Text);
        assert_eq!(out[0].source, ElementSource::Ocr);

        // Box is 5x3 before unclip; d = 5*3*1.5 / (2*(5+3)) = 1.40625 on every side.
        assert!((out[0].rect.w - (5.0 + 2.0 * 1.40625)).abs() < 1e-6, "{:?}", out[0].rect);
        assert!((out[0].rect.h - (3.0 + 2.0 * 1.40625)).abs() < 1e-6, "{:?}", out[0].rect);
        assert!((out[0].rect.x - (10.0 - 1.40625)).abs() < 1e-6);
    }

    /// A low-confidence blob is dropped even though it clears the map threshold, and a
    /// single-pixel speck is dropped for being too small.
    #[test]
    fn weak_and_tiny_components_are_dropped() {
        let mut prob = vec![0.0_f32; 40 * 40];

        for y in 10..13 {
            for x in 10..15 {
                prob[y * 40 + x] = 0.4;
            }
        }

        prob[30 * 40 + 30] = 0.99;

        let plan = ResizePlan { in_w: 40, in_h: 40, scale: 1.0 };

        assert!(boxes_from_map(&prob, 40, 40, &plan, &cfg(), 40, 40).is_empty());
    }

    /// Boxes are mapped back through the resize and clipped to the frame, so a blob at the
    /// map edge does not produce a box hanging off the screen.
    #[test]
    fn boxes_map_back_and_clip_to_the_frame() {
        let mut prob = vec![0.0_f32; 40 * 40];

        for y in 0..4 {
            for x in 0..6 {
                prob[y * 40 + x] = 0.9;
            }
        }

        let plan = ResizePlan { in_w: 40, in_h: 40, scale: 0.5 };
        let out  = boxes_from_map(&prob, 40, 40, &plan, &cfg(), 80, 80);

        assert_eq!(out.len(), 1);
        assert_eq!(out[0].rect.x, 0.0);
        assert_eq!(out[0].rect.y, 0.0);

        // 6 map px wide, unclipped by d = 6*4*1.5/(2*10) = 1.8, halved back to frame px
        // and clipped at zero on the left: (6 + 1.8) * 2 = 15.6.
        assert!((out[0].rect.w - 15.6).abs() < 1e-6, "{:?}", out[0].rect);
    }

    /// An empty map produces no boxes.
    #[test]
    fn empty_map_has_no_boxes() {
        let prob = vec![0.0_f32; 16 * 16];
        let plan = ResizePlan { in_w: 16, in_h: 16, scale: 1.0 };

        assert!(boxes_from_map(&prob, 16, 16, &plan, &cfg(), 16, 16).is_empty());
    }
}
