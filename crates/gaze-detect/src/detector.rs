//! The public detector: two ONNX models, one frame in, `gaze_core::Element`s out.

use std::path::{Path, PathBuf};
use std::time::Instant;

use gaze_core::{Element, GlobalPx, Rect};

use crate::detection::{Detection, drop_oversized, fuse_text, nms};
use crate::error::{DetectError, Result};
use crate::ocr::{OcrWindow, TextConfig, TextModel, ocr_window};
use crate::tile::{plan_tiles, tiles_containing};
use crate::widget::WidgetModel;

/// File name of the exported TargetFinder widget detector inside the models directory.
pub const WIDGET_MODEL : &str = "yolo26n-640.onnx";

/// File name of the PP-OCRv5 text detection model inside the models directory.
pub const TEXT_MODEL : &str = "ch_PP-OCRv5_det.onnx";

/// Everything tunable about a detection pass.
///
/// The defaults come from sweeping the CLI over a 3840x1600 capture of this desk, see
/// `README.md`. In short: 1024 px tiles into the 640 px network recover far more small
/// widgets than 1920 px tiles and cost half what 640 px tiles do, the text model loses
/// almost nothing at 1600 px and is eight times faster there than at native resolution,
/// and eight onnxruntime threads beat the default of one per logical core by 2.8x because
/// these models are too small to fill 32 threads.
#[derive(Clone, Copy, Debug)]
pub struct DetectConfig {
    /// Side of one widget tile in frame pixels. Larger tiles mean fewer inferences and
    /// smaller apparent widgets.
    pub tile_px      : u32,
    /// Fraction of a tile shared with its neighbour, so a widget on a seam is whole in at
    /// least one tile.
    pub tile_overlap : f64,
    /// Minimum widget confidence to keep.
    pub widget_conf  : f32,
    /// Widget boxes wider than this in frame pixels are dropped before NMS and fusion.
    /// Zero means unlimited, which is what the snap engine wants: every box is a snap
    /// candidate there, however wide. A collector that only cares about click targets sets
    /// it, because a "control" the width of a pane is a pane.
    pub max_widget_w : f64,
    /// Widget boxes taller than this in frame pixels are dropped before NMS and fusion.
    /// Zero means unlimited, as for [`DetectConfig::max_widget_w`].
    pub max_widget_h : f64,
    /// IoU above which two widget boxes are the same thing, applied across tiles.
    pub nms_iou      : f64,
    /// Fraction of a text box that must lie inside a widget box for the text to be
    /// dropped as that widget's label.
    pub text_contain : f64,
    /// Longest side fed to the text model; zero for native resolution.
    pub ocr_max_side : u32,
    /// Per-pixel probability at which the text map is binarised.
    pub ocr_map      : f32,
    /// Mean probability a text component must reach.
    pub ocr_box      : f32,
    /// DB unclip ratio for text boxes.
    pub ocr_unclip   : f64,
    /// onnxruntime intra-op threads; zero for its default of one per logical core, which
    /// on a 32 thread 5950X is much slower than a modest fixed count.
    pub threads      : usize,
    /// Whether to run the widget model at all. Off is useful for isolating OCR timings.
    pub widgets      : bool,
    /// Whether to run the text model at all.
    pub ocr          : bool,
}

/// Wall clock cost of one `detect` call, split by stage.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct DetectTimings {
    /// Preprocess, inference and decode for every widget tile.
    pub widget_ms : f64,
    /// Preprocess, inference and DB postprocess for the text model.
    pub ocr_ms    : f64,
    /// Cross-tile NMS and OCR/widget fusion.
    pub fuse_ms   : f64,
    /// Everything above plus the conversion to global coordinates.
    pub total_ms  : f64,
    /// How many widget tiles were run.
    pub tiles     : usize,
    /// Widget boxes surviving NMS.
    pub widgets   : usize,
    /// Text boxes surviving fusion.
    pub texts     : usize,
}

/// Two ONNX models plus the settings they run under.
///
/// `detect` takes `&self` so a capture thread can hand frames to a shared detector; the
/// sessions are internally mutexed, so concurrent calls serialise rather than racing.
pub struct Detector {
    /// The TargetFinder widget model, absent when disabled in the config.
    widget : Option<WidgetModel>,
    /// The PP-OCR text detection model, absent when disabled in the config.
    text   : Option<TextModel>,
    /// Settings this detector was built with.
    config : DetectConfig,
}

// --- DetectConfig ---

impl Default for DetectConfig {
    fn default() -> Self {
        DetectConfig {
            tile_px      : 1024,
            tile_overlap : 0.15,
            widget_conf  : 0.25,
            max_widget_w : 0.0,
            max_widget_h : 0.0,
            nms_iou      : 0.5,
            text_contain : 0.7,
            ocr_max_side : 1600,
            ocr_map      : 0.3,
            ocr_box      : 0.6,
            ocr_unclip   : 1.5,
            threads      : 8,
            widgets      : true,
            ocr          : true,
        }
    }
}

// --- Detector ---

impl Detector {
    /// Loads both models from `models_dir` with the default configuration.
    pub fn load(models_dir: impl AsRef<Path>) -> Result<Detector> {
        Detector::create().models_dir(models_dir).build()
    }

    /// Starts a builder for a detector with non-default settings.
    pub fn create() -> DetectorBuilder {
        DetectorBuilder::new()
    }

    /// Detects UI elements in one output's frame.
    ///
    /// `rgba` is `w * h * 4` bytes of the output's own framebuffer, so `w` and `h` are
    /// physical pixels. `origin` is that output's top-left corner in global logical pixels
    /// and `scale` its compositor scale factor, which is how a 1920x1200 physical HDMI
    /// panel at scale 2 lands as a 960x600 logical rectangle. Returned boxes are global
    /// logical pixels.
    ///
    /// `id`s are the element's index in the returned vector and are only stable within one
    /// call; cross-frame identity belongs to a later stage.
    pub fn detect(
        &self,
        rgba   : &[u8],
        w      : u32,
        h      : u32,
        origin : GlobalPx,
        scale  : f64,
    )
        -> Result<Vec<Element>>
    {
        Ok(self.detect_timed(rgba, w, h, origin, scale)?.0)
    }

    /// Same as `detect`, but also reports where the time went. Used by the CLI's `--bench`.
    pub fn detect_timed(
        &self,
        rgba   : &[u8],
        w      : u32,
        h      : u32,
        origin : GlobalPx,
        scale  : f64,
    )
        -> Result<(Vec<Element>, DetectTimings)>
    {
        let want = w as usize * h as usize * 4;

        if rgba.len() != want {
            return Err(DetectError::FrameSize { got: rgba.len(), want: want, w: w, h: h });
        }

        let mut timings = DetectTimings::default();
        let start       = Instant::now();

        // Widgets first: the tiled pass is the expensive one and its output is what OCR
        // boxes get fused against.
        let mut widgets = Vec::new();

        if let Some(model) = &self.widget {
            let t0    = Instant::now();
            let tiles = plan_tiles(w, h, self.config.tile_px, self.config.tile_overlap, model.input_px());

            widgets           = model.detect_tiles(rgba, w, h, &tiles, self.config.widget_conf)?;
            timings.tiles     = tiles.len();
            timings.widget_ms = ms_since(t0);
        }

        // Text next, over the whole frame in one pass.
        let mut texts = Vec::new();

        if let Some(model) = &self.text {
            let t0 = Instant::now();

            texts          = model.detect(rgba, w, h, &self.text_config())?;
            timings.ocr_ms = ms_since(t0);
        }

        // Drop oversized widgets, collapse tile duplicates, then drop text that is really
        // a widget's own label.
        let t0 = Instant::now();

        let widgets = drop_oversized(widgets, self.config.max_widget_w, self.config.max_widget_h);
        let widgets = nms(widgets, self.config.nms_iou);
        let texts   = fuse_text(&widgets, texts, self.config.text_contain);

        timings.fuse_ms = ms_since(t0);
        timings.widgets = widgets.len();
        timings.texts   = texts.len();

        let elements = to_global(widgets.into_iter().chain(texts), origin, scale);

        timings.total_ms = ms_since(start);

        Ok((elements, timings))
    }

    /// Detects only what could be under one point, which is all a click needs.
    ///
    /// `at` is the point of interest in global logical pixels; everything else means what
    /// it does on [`Detector::detect_timed`]. Returned boxes are global logical pixels and
    /// are not restricted to ones containing `at`: the caller does the hit test, because
    /// the boxes that merely neighbour the point still diagnose a coordinate offset.
    ///
    /// Two savings, and they are different in kind.
    ///
    /// **Widgets.** The tile plan is exactly the one `detect_timed` builds, and only the
    /// tiles containing the point are run (see [`tiles_containing`] for why that loses
    /// nothing that contains the point). So this is *not* the +/-256 px crop the
    /// `gaze-clicks` README records as failing 0 out of 12. That crop cut new pixels and
    /// fed them to the model as a whole image, which changed the context a wide flat list
    /// row is recognised by. Here the model sees the same 1024 px tile at the same scale
    /// with the same surroundings it would have seen in the full pass; the only difference
    /// is that tiles which cannot hold the point are skipped. One to four tiles instead of
    /// ten on the ultrawide.
    ///
    /// **Text.** The opposite trade. The full pass shrinks the frame to `ocr_max_side` for
    /// time, and on a 3840x1600 panel that merges lines into paragraph blobs. Locally
    /// there is no need: an `ocr_px` square at native resolution costs about 62 ms and
    /// gives line-level boxes, roughly 20 px tall. Text is the thing that benefits from
    /// resolution locally, exactly where the widget model needed context globally.
    ///
    /// A point outside the frame yields no elements and zeroed timings apart from
    /// `total_ms`.
    // The argument list is `detect_timed`'s plus the point and the window it reads.
    // Packing it into a struct would move the same fields somewhere else, not remove them.
    #[allow(clippy::too_many_arguments)]
    pub fn detect_near(
        &self,
        rgba   : &[u8],
        w      : u32,
        h      : u32,
        origin : GlobalPx,
        scale  : f64,
        at     : GlobalPx,
        ocr_px : u32,
    )
        -> Result<(Vec<Element>, DetectTimings)>
    {
        let want = w as usize * h as usize * 4;

        if rgba.len() != want {
            return Err(DetectError::FrameSize { got: rgba.len(), want: want, w: w, h: h });
        }

        let mut timings = DetectTimings::default();
        let start       = Instant::now();

        // The inverse of `to_global`: global = origin + frame / scale, so frame is the
        // offset from the origin scaled back up into the buffer's physical pixels.
        let fx = (at.x - origin.x) * scale;
        let fy = (at.y - origin.y) * scale;

        if fx < 0.0 || fy < 0.0 || fx >= f64::from(w) || fy >= f64::from(h) {
            timings.total_ms = ms_since(start);

            return Ok((Vec::new(), timings));
        }

        // Widgets, from the tiles of the full plan that can hold the point.
        let mut widgets = Vec::new();

        if let Some(model) = &self.widget {
            let t0    = Instant::now();
            let plan  = plan_tiles(w, h, self.config.tile_px, self.config.tile_overlap, model.input_px());
            let tiles = tiles_containing(&plan, fx, fy);

            widgets           = model.detect_tiles(rgba, w, h, &tiles, self.config.widget_conf)?;
            timings.tiles     = tiles.len();
            timings.widget_ms = ms_since(t0);
        }

        // Text, native resolution over a square window centred on the point.
        let mut texts = Vec::new();

        if let Some(model) = &self.text {
            let t0     = Instant::now();
            let window = ocr_window(w, h, fx, fy, ocr_px);
            let pixels = copy_window(rgba, w, &window);

            let cfg = TextConfig {
                max_side : 0,
                ..self.text_config()
            };

            texts = model.detect(&pixels, window.w, window.h, &cfg)?;

            // The model saw the window as its own image, so its boxes are window-local.
            for t in &mut texts {
                t.rect.x += f64::from(window.x);
                t.rect.y += f64::from(window.y);
            }

            timings.ocr_ms = ms_since(t0);
        }

        let t0 = Instant::now();

        let widgets = drop_oversized(widgets, self.config.max_widget_w, self.config.max_widget_h);
        let widgets = nms(widgets, self.config.nms_iou);
        let texts   = fuse_text(&widgets, texts, self.config.text_contain);

        timings.fuse_ms = ms_since(t0);
        timings.widgets = widgets.len();
        timings.texts   = texts.len();

        let elements = to_global(widgets.into_iter().chain(texts), origin, scale);

        timings.total_ms = ms_since(start);

        Ok((elements, timings))
    }

    /// The settings this detector runs with.
    pub fn config(&self) -> &DetectConfig {
        &self.config
    }
}

impl Detector {
    /// The text model's settings, built from the shared config.
    ///
    /// Shared by both entry points so a change to one of the DB thresholds cannot apply to
    /// the full pass and not the local one. `detect_near` overrides `max_side` on top of
    /// this, because locally there is nothing to save by shrinking.
    fn text_config(&self) -> TextConfig {
        TextConfig {
            max_side    : self.config.ocr_max_side,
            map_thresh  : self.config.ocr_map,
            box_thresh  : self.config.ocr_box,
            unclip      : self.config.ocr_unclip,
            min_side_px : 2,
        }
    }
}

// --- DetectorBuilder ---

/// Builds a `Detector` with a chosen models directory and configuration.
pub struct DetectorBuilder {
    /// Where the two `.onnx` files live.
    dir    : PathBuf,
    /// Settings handed to the detector.
    config : DetectConfig,
}

impl DetectorBuilder {
    /// A builder pointing at `models/` relative to the working directory.
    pub fn new() -> DetectorBuilder {
        DetectorBuilder {
            dir    : PathBuf::from("models"),
            config : DetectConfig::default(),
        }
    }

    /// Sets the directory the `.onnx` files are read from.
    pub fn models_dir(mut self, dir: impl AsRef<Path>) -> DetectorBuilder {
        self.dir = dir.as_ref().to_path_buf();

        self
    }

    /// Replaces the whole configuration.
    pub fn config(mut self, config: DetectConfig) -> DetectorBuilder {
        self.config = config;

        self
    }

    /// Opens the sessions.
    pub fn build(self) -> Result<Detector> {
        let widget = {
            if self.config.widgets {
                Some(WidgetModel::load(&self.dir.join(WIDGET_MODEL), self.config.threads)?)
            }
            else {
                None
            }
        };

        let text = {
            if self.config.ocr {
                Some(TextModel::load(&self.dir.join(TEXT_MODEL), self.config.threads)?)
            }
            else {
                None
            }
        };

        Ok(Detector {
            widget : widget,
            text   : text,
            config : self.config,
        })
    }
}

impl Default for DetectorBuilder {
    fn default() -> Self {
        DetectorBuilder::new()
    }
}

// --- Coordinate mapping ---

/// Places frame-pixel detections into the global logical pixel space and numbers them.
///
/// Frame pixels are physical, so dividing by the output scale is what turns a 1920 px wide
/// capture of a scale-2 panel into a 960 px wide logical rectangle.
fn to_global(
    dets   : impl Iterator<Item = Detection>,
    origin : GlobalPx,
    scale  : f64,
)
    -> Vec<Element>
{
    let inv = 1.0 / scale;

    dets.enumerate()
        .map(|(i, d)| Element {
            id     : i as u64,
            bbox   : Rect {
                x : origin.x + d.rect.x * inv,
                y : origin.y + d.rect.y * inv,
                w : d.rect.w * inv,
                h : d.rect.h * inv,
            },
            kind   : d.kind,
            source : d.source,
            score  : d.score,
            text   : None,
        })
        .collect()
}

/// Copies a window out of an RGBA frame into a tightly packed buffer.
///
/// The text model wants a contiguous `w * h * 4` image, so the window's rows are gathered
/// rather than passed as a stride into the frame. `window` is assumed to lie inside the
/// frame, which [`ocr_window`] guarantees.
fn copy_window(rgba: &[u8], w: u32, window: &OcrWindow) -> Vec<u8> {
    let stride = w as usize * 4;
    let bytes  = window.w as usize * 4;

    let mut out = Vec::with_capacity(bytes * window.h as usize);

    for y in window.y..window.y + window.h {
        let row = y as usize * stride + window.x as usize * 4;

        out.extend_from_slice(&rgba[row..row + bytes]);
    }

    out
}

/// Milliseconds elapsed since `t`.
fn ms_since(t: Instant) -> f64 {
    t.elapsed().as_secs_f64() * 1000.0
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::{ElementKind, ElementSource, GlobalPx, Rect};

    use super::{copy_window, to_global};
    use crate::detection::Detection;
    use crate::ocr::OcrWindow;

    /// Builds a frame-space detection.
    fn det(x: f64, y: f64, w: f64, h: f64) -> Detection {
        Detection {
            rect   : Rect { x: x, y: y, w: w, h: h },
            score  : 0.5,
            kind   : ElementKind::Button,
            source : ElementSource::Detector,
        }
    }

    /// The scale-1 case: frame pixels are logical pixels, only the origin shifts. DP-1 sits
    /// at (2559, 0) on this desk.
    #[test]
    fn scale_one_only_translates() {
        let out = to_global(vec![det(10.0, 20.0, 30.0, 40.0)].into_iter(), GlobalPx { x: 2559.0, y: 0.0 }, 1.0);

        assert_eq!(out[0].bbox, Rect { x: 2569.0, y: 20.0, w: 30.0, h: 40.0 });
    }

    /// The HDMI panel: 1920x1200 physical at scale 2 is a 960x600 logical rectangle at
    /// (1506, 1600), so a box filling the frame must fill exactly that rectangle.
    #[test]
    fn scale_two_halves_the_frame() {
        let out = to_global(
            vec![det(0.0, 0.0, 1920.0, 1200.0)].into_iter(),
            GlobalPx { x: 1506.0, y: 1600.0 },
            2.0,
        );

        assert_eq!(out[0].bbox, Rect { x: 1506.0, y: 1600.0, w: 960.0, h: 600.0 });
    }

    /// Ids are the index in the returned vector, dense from zero.
    #[test]
    fn ids_are_dense_indices() {
        let dets = vec![det(0.0, 0.0, 1.0, 1.0), det(5.0, 5.0, 1.0, 1.0), det(9.0, 9.0, 1.0, 1.0)];
        let out  = to_global(dets.into_iter(), GlobalPx { x: 0.0, y: 0.0 }, 1.0);

        assert_eq!(out.iter().map(|e| e.id).collect::<Vec<_>>(), vec![0, 1, 2]);
    }

    /// The frame-pixel conversion `detect_near` does must be the exact inverse of
    /// `to_global`, or every local pass looks in the wrong place. Checked here by round
    /// tripping a box corner rather than by duplicating the arithmetic.
    #[test]
    fn frame_to_global_round_trips_through_both_scales() {
        for (origin, scale) in [
            (GlobalPx { x: 2559.0, y: 0.0 }   , 1.0),
            (GlobalPx { x: 1506.0, y: 1600.0 }, 2.0),
        ] {
            let out = to_global(vec![det(640.0, 480.0, 1.0, 1.0)].into_iter(), origin, scale);
            let at  = GlobalPx { x: out[0].bbox.x, y: out[0].bbox.y };

            assert_eq!(((at.x - origin.x) * scale, (at.y - origin.y) * scale), (640.0, 480.0));
        }
    }

    /// The window copy takes exactly the requested rectangle, packed.
    #[test]
    fn a_window_is_copied_out_row_by_row() {
        // A 4x3 frame whose red channel is `y * 4 + x`, so a copy is identifiable.
        let mut rgba = vec![0_u8; 4 * 3 * 4];

        for y in 0..3_u32 {
            for x in 0..4_u32 {
                rgba[(y * 4 + x) as usize * 4] = (y * 4 + x) as u8;
            }
        }

        let out = copy_window(&rgba, 4, &OcrWindow { x: 1, y: 1, w: 2, h: 2 });

        assert_eq!(out.len(), 2 * 2 * 4);
        assert_eq!([out[0], out[4], out[8], out[12]], [5, 6, 9, 10]);
    }

    /// Kind, source and score pass through untouched, and text stays empty because the
    /// recognition stage is not run.
    #[test]
    fn metadata_passes_through() {
        let out = to_global(vec![det(0.0, 0.0, 1.0, 1.0)].into_iter(), GlobalPx { x: 0.0, y: 0.0 }, 1.0);

        assert_eq!(out[0].kind, ElementKind::Button);
        assert_eq!(out[0].source, ElementSource::Detector);
        assert_eq!(out[0].score, 0.5);
        assert_eq!(out[0].text, None);
    }
}
