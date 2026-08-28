//! Cropping a frame around the pointer and picking the element that was clicked.
//!
//! Detection over a whole ultrawide frame costs about 400 ms, which is far too much to
//! pay on every click. A click only ever lands on something under the pointer, so the
//! recogniser is handed a square of a few hundred logical pixels around it and told
//! where that square sits in global coordinates. The boxes come back in the same global
//! logical pixel space as everything else in the workspace.

use gaze_core::{Element, ElementKind, GlobalPx, Rect};

/// Half-width of the crop taken around the pointer, logical pixels. Wide enough that a
/// full-width toolbar button or a line of text is whole inside it, small enough that
/// the detector runs in tens of milliseconds.
pub const CROP_HALF_PX: f64 = 256.0;

/// A square of a captured frame, in the frame's buffer pixels, with the global
/// coordinates the detector needs to place its boxes.
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
    /// Buffer pixels per logical pixel, what `Detector::detect` is handed as `scale`.
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
/// is placed with each axis's own ratio and the detector is handed the horizontal one,
/// which over a 512 px square is worth under half a logical pixel of vertical scale
/// error.
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

    let pixels = rgba.len() / 4;
    let mut sum = 0.0;

    for i in 0..pixels {
        let p = i * 4;

        sum += 0.2126 * f64::from(rgba[p])
             + 0.7152 * f64::from(rgba[p + 1])
             + 0.0722 * f64::from(rgba[p + 2]);
    }

    sum / (pixels as f64 * 255.0)
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
pub fn smallest_containing(elements: &[Element], p: GlobalPx) -> Option<&Element> {
    elements
        .iter()
        .filter(|e| is_accepted(e.kind) && e.bbox.contains(p))
        .min_by(|a, b| (a.bbox.w * a.bbox.h).total_cmp(&(b.bbox.w * b.bbox.h)))
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
