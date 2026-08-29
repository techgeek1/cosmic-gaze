//! Picking the element that was clicked, the gates it has to clear, and the two
//! windows measured around the pointer.
//!
//! Recognition is pointer-local but not a crop: `Detector::detect_near` runs the widget
//! tiles of the *full* plan that contain the pointer, so a wide flat list row keeps the
//! surrounding layout the model needs to see it at all, and runs the text model at
//! native resolution over a square window, where line-level boxes come back instead of
//! the paragraph blobs a 3840 px frame shrunk to 1600 produces. [`crate::perceive`]
//! carries the history of the crop that did fail.
//!
//! Three gates sit on top of the detector's own output, and they exist because a
//! collector wants a *click target*, not a snap candidate:
//!
//! - [`WIDGET_MIN_SCORE`], the confidence bar, higher than the snap default.
//! - [`MAX_WIDGET_W_PX`] and [`MAX_WIDGET_H_PX`], the size bar.
//! - the flat check in [`pick`], which refuses a large box whose pixels under the
//!   pointer carry no detail.
//!
//! Boxes come back from the detector in global logical pixels, like everything else in
//! the workspace. Both the collector and the probe go through [`pick`], so the thing the
//! probe draws on screen is the thing a click would have been labelled with.

use gaze_core::{Element, ElementKind, GlobalPx, Rect};
use gaze_detect::{DetectConfig, NearConfig};

/// Half-width of the luminance window taken around the pointer, logical pixels. Wide
/// enough to average over the panel or page the click landed in rather than the widget
/// itself, which is the light the pupil is actually responding to.
pub const LUMA_HALF_PX: f64 = 256.0;

/// The collector's accept bar for a widget box.
///
/// `DetectConfig`'s own default is 0.25, tuned for snap *recall*: a snap engine ranks
/// its candidates and a weak box that turns out to be real is worth having. A collector
/// writes one label per click and a wrong one is training data pointing at the wrong
/// place, so it wants precision instead. On the two reference captures every widget box
/// scoring under 0.5 sat over styled prose (inline-code chips, timestamps, headings)
/// rather than over a control, and every real control scored above it.
pub const WIDGET_MIN_SCORE: f32 = 0.5;

/// Widest a widget box may be to count as a click target, frame pixels.
///
/// A "control" a third of an ultrawide across is a pane, a paragraph or a whole window,
/// and the model does emit those. Dropping them before fusion also stops them swallowing
/// the text lines inside them, which is why the limit lives in the detector rather than
/// here (see `gaze_detect::drop_oversized`).
pub const MAX_WIDGET_W_PX: f64 = 1200.0;

/// Tallest a widget box may be to count as a click target, frame pixels. Buttons, rows,
/// inputs and links are all short; anything taller is a container.
pub const MAX_WIDGET_H_PX: f64 = 240.0;

/// Side of the native-resolution text window read around the pointer, frame pixels.
/// 640 costs about 62 ms and returns line-level boxes, median height around 20 px.
pub const OCR_PX: u32 = 640;

/// Side of the extra widget tile centred on the pointer, frame pixels. The model's own
/// input size, so it sees the pixels there unscaled: the 1024 px plan tiles shrink a
/// 50 px icon button to 31 px and score YouTube's action column 0.23 to 0.42, under the
/// gate, so the label beneath each icon won instead; this tile scores the same buttons
/// 0.79 to 0.95. One more inference, about 28 ms.
pub const NEAR_TILE_PX: u32 = 640;

/// Half-width of the flatness window taken around the pointer, logical pixels.
///
/// Logical rather than buffer pixels so the window means the same thing on the scale-2
/// panel as on the unscaled ones, the same convention [`LUMA_HALF_PX`] uses. Small enough
/// to be about the pixels *under* the pointer rather than about the region around it.
pub const FLAT_HALF_PX: u32 = 24;

/// Luma standard deviation below which the pixels under the pointer carry no detail.
///
/// A window this small over any real control catches an edge, a glyph or a border and
/// lands well above it; a flat panel body, a blank document or a desktop background sits
/// at or near zero.
pub const FLAT_LUMA_SD: f64 = 0.02;

/// Box height in logical pixels above which the flat check applies.
///
/// Small boxes skip it deliberately. A confident small button can genuinely be flat where
/// the pointer landed, because its body is a couple of dozen pixels from its label and
/// the label is a perfectly good gaze target either way. A *large* flat region under the
/// pointer says the opposite: whatever the model called a control, the user cannot have
/// been aiming at a feature of it, because there is no feature within the window.
pub const FLAT_CHECK_MIN_H_PX: f64 = 60.0;

/// A square of a captured frame, in the frame's buffer pixels, with the global
/// coordinates that place it on the desktop.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Crop {
    /// Left edge in buffer pixels.
    pub x0     : u32,
    /// Top edge in buffer pixels.
    pub y0     : u32,
    /// Width in buffer pixels.
    pub w      : u32,
    /// Height in buffer pixels.
    pub h      : u32,
    /// The crop's top-left corner in global logical pixels.
    pub origin : GlobalPx,
    /// Buffer pixels per logical pixel along x.
    pub scale  : f64,
}

// --- Cropping ---

/// The crop of `half_px` logical pixels either side of `p`, clipped to the frame.
///
/// `logical` is where the frame's output sits in global logical pixels and
/// `width`/`height` are the capture buffer's physical dimensions. `None` when the
/// frame is degenerate or the point lies outside it.
///
/// The two axes have very slightly different logical-to-buffer ratios (cosmic-comp
/// rounds the logical size it reports, so on HDMI-A-1 they differ by 0.07%). The crop
/// is placed with each axis's own ratio and `scale` reports the horizontal one.
pub fn crop_around(
    logical : Rect,
    width   : u32,
    height  : u32,
    p       : GlobalPx,
    half_px : f64,
)
    -> Option<Crop>
{
    if width == 0 || height == 0 || logical.w <= 0.0 || logical.h <= 0.0 {
        return None;
    }

    if !logical.contains(p) {
        return None;
    }

    let sx = f64::from(width)  / logical.w;
    let sy = f64::from(height) / logical.h;

    // Clip in logical space first, so the half-width means the same thing on every
    // output whatever its scale.
    let lx0 = (p.x - half_px).max(logical.x);
    let ly0 = (p.y - half_px).max(logical.y);
    let lx1 = (p.x + half_px).min(logical.x + logical.w);
    let ly1 = (p.y + half_px).min(logical.y + logical.h);

    let x0 = ((lx0 - logical.x) * sx).floor().max(0.0) as u32;
    let y0 = ((ly0 - logical.y) * sy).floor().max(0.0) as u32;
    let x1 = ((lx1 - logical.x) * sx).ceil().min(f64::from(width))  as u32;
    let y1 = ((ly1 - logical.y) * sy).ceil().min(f64::from(height)) as u32;

    if x1 <= x0 || y1 <= y0 {
        return None;
    }

    Some(Crop {
        x0     : x0,
        y0     : y0,
        w      : x1 - x0,
        h      : y1 - y0,
        origin : GlobalPx {
            x : logical.x + f64::from(x0) / sx,
            y : logical.y + f64::from(y0) / sy,
        },
        scale  : sx,
    })
}

/// Copies the crop's pixels out of a full RGBA frame into a tightly packed buffer.
///
/// Returns an empty vector when `rgba` is shorter than the frame the crop was measured
/// against, which can only happen if the two came from different captures.
pub fn extract(rgba: &[u8], width: u32, crop: &Crop) -> Vec<u8> {
    let stride = width as usize * 4;
    let need   = (crop.y0 + crop.h) as usize * stride;

    if rgba.len() < need {
        return Vec::new();
    }

    let mut out = Vec::with_capacity(crop.w as usize * crop.h as usize * 4);

    for y in crop.y0..crop.y0 + crop.h {
        let row = y as usize * stride + crop.x0 as usize * 4;

        out.extend_from_slice(&rgba[row..row + crop.w as usize * 4]);
    }

    out
}

/// Mean relative luminance of an RGBA buffer, in [0, 1].
///
/// Rec. 709 weights over the raw sRGB bytes, without linearising: this is a covariate
/// for the pupil, and the pupil responds to the light the panel actually emits, which
/// the display's own transfer function already shapes. The number only has to be
/// monotone in brightness and comparable between clicks.
pub fn mean_luma(rgba: &[u8]) -> f64 {
    if rgba.len() < 4 {
        return f64::NAN;
    }

    let pixels  = rgba.len() / 4;
    let mut sum = 0.0;

    for i in 0..pixels {
        sum += luma_at(rgba, i);
    }

    sum / pixels as f64
}

/// Population standard deviation of the per-pixel luminance of an RGBA buffer, in [0, 1].
///
/// The same luma as [`mean_luma`], so the two are always talking about the same quantity.
/// Population rather than sample because the buffer is the whole window, not a draw from
/// something larger. NaN for an empty buffer, which is what a caller gets when the crop
/// and the frame disagree.
///
/// This is the "is there anything under the pointer" measure. A flat region gives zero
/// whatever its brightness, so it separates a blank panel from a control without caring
/// about the theme.
pub fn luma_sd(rgba: &[u8]) -> f64 {
    if rgba.len() < 4 {
        return f64::NAN;
    }

    let pixels = rgba.len() / 4;
    let mean   = mean_luma(rgba);

    let mut sum = 0.0;

    for i in 0..pixels {
        let d = luma_at(rgba, i) - mean;

        sum += d * d;
    }

    (sum / pixels as f64).sqrt()
}

/// Relative luminance of pixel `i` of an RGBA buffer, in [0, 1].
///
/// Rec. 709 weights over the raw sRGB bytes; see [`mean_luma`] for why they are not
/// linearised first.
#[inline]
fn luma_at(rgba: &[u8], i: usize) -> f64 {
    let p = i * 4;

    (0.2126 * f64::from(rgba[p])
        + 0.7152 * f64::from(rgba[p + 1])
        + 0.0722 * f64::from(rgba[p + 2]))
        / 255.0
}

// --- Elements ---

/// Whether a kind is something a user aims a click at.
///
/// `Unknown` is the one rejection: a box the detector could not classify is as likely
/// to be a panel or a window frame as a target, and a click on nothing is exactly the
/// sample this crate exists to exclude. `Icon` and `Slider` are accepted but reported
/// by [`is_soft`], because an icon's centre is not always where the eye goes and a
/// slider is dragged as often as it is clicked.
pub fn is_accepted(kind: ElementKind) -> bool {
    !matches!(kind, ElementKind::Unknown)
}

/// Whether a kind is accepted but worth treating with suspicion downstream.
pub fn is_soft(kind: ElementKind) -> bool {
    matches!(kind, ElementKind::Icon | ElementKind::Slider)
}

/// The name a kind is written under in the session file and the export.
pub fn kind_name(kind: ElementKind) -> &'static str {
    match kind {
        ElementKind::Button   => "button",
        ElementKind::Icon     => "icon",
        ElementKind::Input    => "input",
        ElementKind::Link     => "link",
        ElementKind::Text     => "text",
        ElementKind::Checkbox => "checkbox",
        ElementKind::Slider   => "slider",
        ElementKind::Unknown  => "unknown",
    }
}

/// The smallest accepted element whose box contains `p`.
///
/// Smallest wins because boxes nest: a line of text sits inside a text area which sits
/// inside a panel, and the innermost one is the thing the user was aiming at. Kinds are
/// filtered before the size comparison rather than after, so an unclassified box
/// wrapping the whole window cannot swallow a click that landed on a real button.
///
/// `elements` is the whole output's list. Feeding it a crop's list instead is what the
/// first version did, and it systematically preferred a widget's inner OCR text to the
/// widget: see [`crate::perceive`] for the measurement.
pub fn smallest_containing(elements: &[Element], p: GlobalPx) -> Option<&Element> {
    elements
        .iter()
        .filter(|e| is_accepted(e.kind) && e.bbox.contains(p))
        .min_by(|a, b| (a.bbox.w * a.bbox.h).total_cmp(&(b.bbox.w * b.bbox.h)))
}

// --- Picking ---

/// What was under the pointer, once every gate has been applied.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Pick<'a> {
    /// A box worth labelling a click with.
    Element(&'a Element),
    /// A box contained the pointer, but the pixels there are flat, so the model claimed
    /// a control over what is really empty space. Rejected for the same reason
    /// [`Pick::Nothing`] is: a click on nothing says nothing about where the eye was.
    Blank,
    /// No accepted box contains the pointer.
    Nothing,
}

/// The element a click at `p` should be labelled with, or why there is none.
///
/// `pointer_sd` is [`luma_sd`] over a [`FLAT_HALF_PX`] window around the pointer, taken
/// from the same frame the elements came from. NaN (no window, or a buffer that did not
/// match) never triggers [`Pick::Blank`]: an unmeasurable window is not evidence of
/// flatness.
///
/// The flat check only applies to boxes taller than [`FLAT_CHECK_MIN_H_PX`]. See that
/// constant for why small boxes are exempt.
///
/// Both the collector and the probe call this, so the box the probe outlines on screen is
/// exactly the one a click there would have been written against.
pub fn pick<'a>(elements: &'a [Element], p: GlobalPx, pointer_sd: f64) -> Pick<'a> {
    let Some(element) = smallest_containing(elements, p) else {
        return Pick::Nothing;
    };

    if element.bbox.h > FLAT_CHECK_MIN_H_PX && pointer_sd < FLAT_LUMA_SD {
        return Pick::Blank;
    }

    Pick::Element(element)
}

// --- Detector settings ---

/// The detector settings the collector and the probe both run under.
///
/// One function rather than two literals so the probe cannot drift from the collector and
/// start showing boxes a click would have refused. The three departures from
/// `DetectConfig::default()` are the collector's gates: a higher confidence bar and the
/// two size limits. Everything else, tiling included, stays at the tuned defaults.
pub fn collector_config() -> DetectConfig {
    DetectConfig {
        widget_conf  : WIDGET_MIN_SCORE,
        max_widget_w : MAX_WIDGET_W_PX,
        max_widget_h : MAX_WIDGET_H_PX,
        ..DetectConfig::default()
    }
}

/// What `detect_near` reads around the pointer, for the collector and the probe alike.
pub fn near_config() -> NearConfig {
    NearConfig {
        ocr_px  : OCR_PX,
        tile_px : NEAR_TILE_PX,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::ElementSource;

    use super::*;

    /// An element of `kind` covering `bbox`.
    fn element(id: u64, kind: ElementKind, bbox: (f64, f64, f64, f64)) -> Element {
        Element {
            id     : id,
            bbox   : Rect { x: bbox.0, y: bbox.1, w: bbox.2, h: bbox.3 },
            kind   : kind,
            source : ElementSource::Detector,
            score  : 0.9,
            text   : None,
        }
    }

    #[test]
    fn a_crop_in_the_middle_of_a_scale_one_output_is_a_plain_square() {
        // DP-1: 3840x1600 at (2559, 0), unscaled.
        let logical = Rect { x: 2559.0, y: 0.0, w: 3840.0, h: 1600.0 };
        let crop    = crop_around(logical, 3840, 1600, GlobalPx { x: 4000.0, y: 800.0 }, 256.0)
            .expect("a crop well inside the panel");

        assert_eq!(crop.w, 512);
        assert_eq!(crop.h, 512);
        assert_eq!(crop.x0, 1185);
        assert_eq!(crop.origin.x, 2559.0 + 1185.0);
        assert_eq!(crop.origin.y, 544.0);
        assert_eq!(crop.scale, 1.0);
    }

    #[test]
    fn a_crop_at_a_corner_is_clamped_to_the_frame() {
        let logical = Rect { x: 0.0, y: 0.0, w: 1000.0, h: 800.0 };
        let crop    = crop_around(logical, 1000, 800, GlobalPx { x: 10.0, y: 5.0 }, 256.0)
            .expect("a crop at the corner");

        assert_eq!((crop.x0, crop.y0), (0, 0));
        assert_eq!((crop.w, crop.h), (266, 261));
        assert_eq!(crop.origin, GlobalPx { x: 0.0, y: 0.0 });
    }

    #[test]
    fn a_scaled_output_crops_in_logical_pixels_and_reports_its_scale() {
        // HDMI-A-1: mode 1920x1200, logical 960x600 at (1506, 1600), scale 2.
        let logical = Rect { x: 1506.0, y: 1600.0, w: 960.0, h: 600.0 };
        let crop    = crop_around(logical, 1920, 1200, GlobalPx { x: 1986.0, y: 1900.0 }, 100.0)
            .expect("a crop inside the scaled panel");

        // 200 logical pixels across is 400 buffer pixels.
        assert_eq!((crop.w, crop.h), (400, 400));
        assert_eq!(crop.scale, 2.0);
        assert_eq!(crop.origin, GlobalPx { x: 1886.0, y: 1800.0 });
    }

    #[test]
    fn a_point_off_the_frame_has_no_crop() {
        let logical = Rect { x: 0.0, y: 0.0, w: 100.0, h: 100.0 };

        assert!(crop_around(logical, 100, 100, GlobalPx { x: 200.0, y: 50.0 }, 32.0).is_none());
        assert!(crop_around(logical, 0, 100, GlobalPx { x: 50.0, y: 50.0 }, 32.0).is_none());
    }

    #[test]
    fn extract_copies_the_square_and_luma_reads_it_back() {
        // A 4x4 frame, black except for a white 2x2 in the bottom right.
        let mut rgba = vec![0u8; 4 * 4 * 4];

        for y in 2..4 {
            for x in 2..4 {
                let p = (y * 4 + x) * 4;
                rgba[p..p + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }

        let crop = Crop {
            x0     : 2,
            y0     : 2,
            w      : 2,
            h      : 2,
            origin : GlobalPx { x: 2.0, y: 2.0 },
            scale  : 1.0,
        };

        let square = extract(&rgba, 4, &crop);

        assert_eq!(square.len(), 2 * 2 * 4);
        assert!((mean_luma(&square) - 1.0).abs() < 1e-9);
        assert!((mean_luma(&rgba) - 0.25).abs() < 1e-9);

        // A buffer that does not match the crop yields nothing rather than garbage.
        assert!(extract(&rgba[..8], 4, &crop).is_empty());
    }

    #[test]
    fn the_smallest_containing_box_wins() {
        let elements = vec![
            element(0, ElementKind::Text  , (  0.0,   0.0, 800.0, 600.0)),
            element(1, ElementKind::Button, (100.0, 100.0, 200.0,  40.0)),
            element(2, ElementKind::Text  , (110.0, 108.0,  60.0,  20.0)),
            element(3, ElementKind::Button, (500.0, 500.0, 100.0,  30.0)),
        ];

        let hit = smallest_containing(&elements, GlobalPx { x: 130.0, y: 115.0 })
            .expect("three boxes contain the point");

        assert_eq!(hit.id, 2);

        // A point inside only the outer box gets the outer box.
        let outer = smallest_containing(&elements, GlobalPx { x: 700.0, y: 50.0 })
            .expect("the panel contains it");

        assert_eq!(outer.id, 0);

        // A point on nothing is a rejection, which is the whole "focus click" case.
        assert!(smallest_containing(&elements, GlobalPx { x: 900.0, y: 900.0 }).is_none());
    }

    #[test]
    fn an_unclassified_box_never_swallows_a_real_target() {
        let elements = vec![
            element(0, ElementKind::Unknown, (100.0, 100.0,  50.0, 20.0)),
            element(1, ElementKind::Link   , (  0.0,   0.0, 400.0, 300.0)),
        ];

        // The Unknown box is smaller and contains the point, and still loses.
        let hit = smallest_containing(&elements, GlobalPx { x: 120.0, y: 110.0 })
            .expect("the link contains it");

        assert_eq!(hit.id, 1);
    }

    #[test]
    fn luma_sd_separates_a_flat_window_from_a_busy_one() {
        // Flat mid grey: no detail at all, whatever the brightness.
        let flat = vec![128u8; 32 * 32 * 4];

        assert!(luma_sd(&flat) < 1e-12, "{}", luma_sd(&flat));
        assert!(luma_sd(&vec![0u8; 32 * 32 * 4]) < 1e-12);

        // A one pixel checkerboard is the busiest a window gets: half at 0, half at 1,
        // so the population sd is exactly 0.5.
        let mut checker = vec![0u8; 32 * 32 * 4];

        for i in 0..32 * 32 {
            if (i / 32 + i % 32) % 2 == 0 {
                checker[i * 4..i * 4 + 4].copy_from_slice(&[255, 255, 255, 255]);
            }
        }

        assert!((luma_sd(&checker) - 0.5).abs() < 1e-9, "{}", luma_sd(&checker));
        assert!(luma_sd(&checker) > FLAT_LUMA_SD);

        // An empty buffer is unmeasurable rather than flat.
        assert!(luma_sd(&[]).is_nan());
        assert!(luma_sd(&[1, 2, 3]).is_nan());
    }

    #[test]
    fn pick_returns_the_element_the_click_lands_on() {
        let elements = vec![
            element(0, ElementKind::Text  , (  0.0,   0.0, 800.0, 600.0)),
            element(1, ElementKind::Button, (100.0, 100.0, 200.0,  40.0)),
        ];

        let hit = pick(&elements, GlobalPx { x: 150.0, y: 110.0 }, 0.3);

        assert_eq!(hit, Pick::Element(&elements[1]));
    }

    #[test]
    fn pick_reports_nothing_when_no_box_contains_the_point() {
        let elements = vec![element(0, ElementKind::Button, (100.0, 100.0, 20.0, 20.0))];

        assert_eq!(pick(&elements, GlobalPx { x: 500.0, y: 500.0 }, 0.3), Pick::Nothing);
        assert_eq!(pick(&[], GlobalPx { x: 0.0, y: 0.0 }, 0.3), Pick::Nothing);
    }

    #[test]
    fn pick_reports_blank_for_a_large_box_over_flat_pixels() {
        // 600 px tall, well past the flat check's floor.
        let elements = vec![element(0, ElementKind::Text, (0.0, 0.0, 800.0, 600.0))];
        let p        = GlobalPx { x: 400.0, y: 300.0 };

        assert_eq!(pick(&elements, p, 0.001), Pick::Blank);

        // Detail under the pointer makes the same box a real target.
        assert_eq!(pick(&elements, p, 0.15), Pick::Element(&elements[0]));

        // An unmeasurable window is not evidence of flatness.
        assert_eq!(pick(&elements, p, f64::NAN), Pick::Element(&elements[0]));
    }

    #[test]
    fn a_small_box_over_flat_pixels_is_still_a_target() {
        // A 30 px tall button under the flat check's floor: its body being flat where the
        // pointer landed says nothing, because its label is a few pixels away.
        let elements = vec![element(0, ElementKind::Button, (100.0, 100.0, 90.0, 30.0))];

        assert_eq!(pick(&elements, GlobalPx { x: 140.0, y: 115.0 }, 0.0), Pick::Element(&elements[0]));
    }

    #[test]
    fn the_collector_config_carries_all_three_gates() {
        let config = collector_config();

        assert_eq!(config.widget_conf , WIDGET_MIN_SCORE);
        assert_eq!(config.max_widget_w, MAX_WIDGET_W_PX);
        assert_eq!(config.max_widget_h, MAX_WIDGET_H_PX);

        // Everything else stays at the tuned detector defaults.
        let default = gaze_detect::DetectConfig::default();

        assert_eq!(config.tile_px     , default.tile_px);
        assert_eq!(config.tile_overlap, default.tile_overlap);
        assert_eq!(config.threads     , default.threads);
    }

    #[test]
    fn kinds_are_accepted_except_unknown_and_two_are_soft() {
        for kind in [ElementKind::Text, ElementKind::Input, ElementKind::Button,
                     ElementKind::Link, ElementKind::Checkbox] {
            assert!(is_accepted(kind), "{kind:?} is a target");
            assert!(!is_soft(kind)   , "{kind:?} is not soft");
        }

        assert!(is_accepted(ElementKind::Icon)   && is_soft(ElementKind::Icon));
        assert!(is_accepted(ElementKind::Slider) && is_soft(ElementKind::Slider));
        assert!(!is_accepted(ElementKind::Unknown));

        assert_eq!(kind_name(ElementKind::Checkbox), "checkbox");
    }
}
