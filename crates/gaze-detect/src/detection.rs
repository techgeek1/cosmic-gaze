//! Frame-local boxes and the two set operations applied to them: non-maximum suppression
//! across tiles, and OCR/widget fusion.
//!
//! Everything here is in frame pixels (the captured buffer's own pixel grid), not global
//! logical pixels. The conversion to `gaze_core::Element` happens once at the end of
//! `Detector::detect`, so the geometry in this file never has to think about output origin
//! or scale.

use gaze_core::{ElementKind, ElementSource, Rect};

/// One detected box before it is placed in the global desktop coordinate space.
#[derive(Clone, Debug, PartialEq)]
pub struct Detection {
    /// Box in frame pixels.
    pub rect   : Rect,
    /// Model confidence in [0, 1].
    pub score  : f32,
    /// Class mapped from the model's own taxonomy.
    pub kind   : ElementKind,
    /// Which model produced the box.
    pub source : ElementSource,
}

// --- Non-maximum suppression ---

/// Greedy class-agnostic NMS: keeps the highest scoring box and drops every later box
/// overlapping it by more than `iou_thresh`.
///
/// Class-agnostic is deliberate. The same widget seen in two overlapping tiles can come
/// back with different classes (a labelled button is a `Button` in one crop and a `Text`
/// in another once its frame is cut off), and for snapping we want one box per target, so
/// the higher-confidence class wins outright rather than both boxes surviving.
///
/// The input is consumed because it gets sorted in place.
pub fn nms(mut dets: Vec<Detection>, iou_thresh: f64) -> Vec<Detection> {
    // Descending confidence, with a deterministic tiebreak so the result does not depend
    // on tile iteration order.
    dets.sort_by(|a, b| {
        b.score
            .partial_cmp(&a.score)
            .unwrap_or(core::cmp::Ordering::Equal)
            .then_with(|| a.rect.x.total_cmp(&b.rect.x))
            .then_with(|| a.rect.y.total_cmp(&b.rect.y))
    });

    let mut kept: Vec<Detection> = Vec::with_capacity(dets.len());

    for d in dets {
        let suppressed = kept.iter().any(|k| k.rect.iou(&d.rect) > iou_thresh);

        if !suppressed {
            kept.push(d);
        }
    }

    kept
}

// --- OCR / widget fusion ---

/// Drops text boxes that are already covered by a widget box, UFO2 style.
///
/// A text run that sits inside a button is the button's label, not a separate target, so
/// snapping to it would produce two competing candidates for one thing on screen. The test
/// is the intersection area over the *text* box area rather than IoU, because a label is
/// much smaller than its button and their IoU is low even when the label is fully
/// enclosed. Text that only clips a widget's edge (a caption abutting a toolbar) survives.
pub fn fuse_text(
    widgets        : &[Detection],
    texts          : Vec<Detection>,
    contain_thresh : f64,
)
    -> Vec<Detection>
{
    texts
        .into_iter()
        .filter(|t| {
            let area = t.rect.w * t.rect.h;

            // A zero-area box can never be a useful target and would divide by zero.
            if area <= 0.0 {
                return false;
            }

            !widgets.iter().any(|w| intersection_area(&w.rect, &t.rect) / area > contain_thresh)
        })
        .collect()
}

/// Area of the overlap between two rectangles, zero when they are disjoint.
fn intersection_area(a: &Rect, b: &Rect) -> f64 {
    let ix = (a.x + a.w).min(b.x + b.w) - a.x.max(b.x);
    let iy = (a.y + a.h).min(b.y + b.h) - a.y.max(b.y);

    if ix <= 0.0 || iy <= 0.0 {
        return 0.0;
    }

    ix * iy
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::{ElementKind, ElementSource, Rect};

    use super::{Detection, fuse_text, intersection_area, nms};

    /// Builds a widget detection from a rect and a score.
    fn widget(x: f64, y: f64, w: f64, h: f64, score: f32) -> Detection {
        Detection {
            rect   : Rect { x: x, y: y, w: w, h: h },
            score  : score,
            kind   : ElementKind::Button,
            source : ElementSource::Detector,
        }
    }

    /// Builds an OCR text detection from a rect.
    fn text(x: f64, y: f64, w: f64, h: f64) -> Detection {
        Detection {
            rect   : Rect { x: x, y: y, w: w, h: h },
            score  : 0.9,
            kind   : ElementKind::Text,
            source : ElementSource::Ocr,
        }
    }

    /// The tile-seam case: one widget straddles the overlap and is found twice with a
    /// couple of pixels of disagreement. NMS must leave exactly one, the better scoring.
    #[test]
    fn nms_collapses_duplicate_boxes_from_overlapping_tiles() {
        let dets = vec![
            widget(100.0, 100.0, 80.0, 24.0, 0.72),
            widget(102.0, 101.0, 78.0, 23.0, 0.91),
        ];

        let kept = nms(dets, 0.5);

        assert_eq!(kept.len(), 1);
        assert_eq!(kept[0].score, 0.91);
        assert_eq!(kept[0].rect.x, 102.0);
    }

    /// Adjacent toolbar buttons overlap slightly but are distinct targets and must all
    /// survive. This is the failure mode of an over-aggressive IoU threshold.
    #[test]
    fn nms_keeps_adjacent_toolbar_buttons() {
        let dets = vec![
            widget(0.0, 0.0, 32.0, 32.0, 0.9),
            widget(30.0, 0.0, 32.0, 32.0, 0.9),
            widget(60.0, 0.0, 32.0, 32.0, 0.9),
        ];

        assert_eq!(nms(dets, 0.5).len(), 3);
    }

    /// A box fully inside a larger one is suppressed only when the IoU actually clears the
    /// threshold. A small icon inside a big panel has low IoU and must survive, otherwise
    /// nesting is destroyed.
    #[test]
    fn nms_keeps_a_small_box_nested_in_a_large_one() {
        let dets = vec![
            widget(0.0, 0.0, 400.0, 400.0, 0.95),
            widget(10.0, 10.0, 20.0, 20.0, 0.80),
        ];

        assert_eq!(nms(dets, 0.5).len(), 2);
    }

    /// Empty input, single input, and total-overlap input are the boundary cases.
    #[test]
    fn nms_edge_cases() {
        assert!(nms(Vec::new(), 0.5).is_empty());
        assert_eq!(nms(vec![widget(0.0, 0.0, 10.0, 10.0, 0.5)], 0.5).len(), 1);

        let identical = vec![
            widget(0.0, 0.0, 10.0, 10.0, 0.5),
            widget(0.0, 0.0, 10.0, 10.0, 0.6),
            widget(0.0, 0.0, 10.0, 10.0, 0.7),
        ];

        assert_eq!(nms(identical, 0.5).len(), 1);
    }

    /// NMS output order and content must not depend on the order boxes arrive in.
    #[test]
    fn nms_is_order_independent() {
        let a = vec![
            widget(0.0, 0.0, 32.0, 32.0, 0.9),
            widget(31.0, 0.0, 32.0, 32.0, 0.7),
            widget(1.0, 1.0, 32.0, 32.0, 0.8),
        ];

        let mut b = a.clone();
        b.reverse();

        assert_eq!(nms(a, 0.5), nms(b, 0.5));
    }

    /// A button label is dropped, so the button is the only candidate for that pixel.
    #[test]
    fn fusion_drops_a_label_inside_its_button() {
        let widgets = vec![widget(100.0, 100.0, 120.0, 32.0, 0.9)];
        let texts   = vec![text(112.0, 108.0, 60.0, 16.0)];

        assert!(fuse_text(&widgets, texts, 0.7).is_empty());
    }

    /// A standalone paragraph nowhere near a widget stays a target of its own.
    #[test]
    fn fusion_keeps_free_standing_text() {
        let widgets = vec![widget(100.0, 100.0, 120.0, 32.0, 0.9)];
        let texts   = vec![text(400.0, 400.0, 200.0, 18.0)];

        assert_eq!(fuse_text(&widgets, texts, 0.7).len(), 1);
    }

    /// Text that only clips a widget edge is a separate thing on screen and must survive.
    /// Half the text box inside the widget is 0.5, under the 0.7 threshold.
    #[test]
    fn fusion_keeps_text_that_only_clips_a_widget() {
        let widgets = vec![widget(100.0, 100.0, 100.0, 40.0, 0.9)];
        let texts   = vec![text(150.0, 110.0, 100.0, 20.0)];

        assert_eq!(fuse_text(&widgets, texts, 0.7).len(), 1);
    }

    /// Exactly at the threshold the text is kept, because the test is strictly greater.
    /// 0.7 of the text box inside the widget must not be enough to drop it.
    #[test]
    fn fusion_threshold_is_exclusive() {
        let widgets = vec![widget(0.0, 0.0, 70.0, 10.0, 0.9)];
        let texts   = vec![text(0.0, 0.0, 100.0, 10.0)];

        assert_eq!(fuse_text(&widgets, texts, 0.7).len(), 1);

        let widgets = vec![widget(0.0, 0.0, 71.0, 10.0, 0.9)];
        let texts   = vec![text(0.0, 0.0, 100.0, 10.0)];

        assert!(fuse_text(&widgets, texts, 0.7).is_empty());
    }

    /// Coverage that only adds up across several widgets does not drop the text: a caption
    /// spanning two adjacent buttons is its own run and each button covers less than the
    /// threshold. This documents that the test is per widget, not cumulative.
    #[test]
    fn fusion_does_not_accumulate_coverage_across_widgets() {
        let widgets = vec![
            widget(0.0, 0.0, 50.0, 10.0, 0.9),
            widget(50.0, 0.0, 50.0, 10.0, 0.9),
        ];

        let texts = vec![text(0.0, 0.0, 100.0, 10.0)];

        assert_eq!(fuse_text(&widgets, texts, 0.7).len(), 1);
    }

    /// Degenerate text boxes are never usable targets.
    #[test]
    fn fusion_drops_zero_area_text() {
        assert!(fuse_text(&[], vec![text(0.0, 0.0, 0.0, 10.0)], 0.7).is_empty());
    }

    /// Disjoint rectangles have zero overlap in both axis orders.
    #[test]
    fn intersection_area_of_disjoint_rects_is_zero() {
        let a = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
        let b = Rect { x: 20.0, y: 0.0, w: 10.0, h: 10.0 };
        let c = Rect { x: 0.0, y: 20.0, w: 10.0, h: 10.0 };

        assert_eq!(intersection_area(&a, &b), 0.0);
        assert_eq!(intersection_area(&a, &c), 0.0);
        assert_eq!(intersection_area(&a, &a), 100.0);
    }
}
