//! Mapping from the compositor's global logical pixel space into one output's surface
//! and buffer pixel space.
//!
//! Everything the rest of the workspace hands the overlay (gaze points, element boxes) is
//! in global logical pixels. A layer surface, on the other hand, is addressed in
//! surface-local logical pixels with its origin at the output's top-left corner, and the
//! `wl_shm` buffer behind it is addressed in physical pixels, which is the surface size
//! multiplied by the output's integer buffer scale. This module is the only place that
//! knows about that chain, which is why it is also the only part of the crate that can be
//! unit tested without a compositor.

use gaze_core::{GlobalPx, Rect};

/// Where one output sits in the global logical pixel space, plus the integer buffer scale
/// its surfaces must use.
///
/// Built from `wl_output` / `zxdg_output_v1` information. `logical` is in global logical
/// pixels and is exactly the rectangle a layer surface anchored to all four edges covers.
#[derive(Clone, Debug, PartialEq)]
pub struct OutputMapping {
    /// Connector name as reported by `zxdg_output_v1.name` (`"DP-1"`, `"HDMI-A-1"`).
    pub name    : String,
    /// Position and size of the output in global logical pixels.
    pub logical : Rect,
    /// Buffer scale, at least 1. Every output on this desk runs at 1: HDMI-A-1 is
    /// fractionally scaled, so it is rendered at its logical size and the compositor
    /// resamples, rather than paying for a 2x buffer it would only downsample again.
    pub scale   : i32,
}

// --- OutputMapping ---

impl OutputMapping {
    /// Creates a mapping. `scale` below 1 is clamped to 1, since a zero or negative
    /// buffer scale is a protocol error and compositors have been known to report 0 for
    /// an output whose mode is not yet known.
    pub fn new(name: impl Into<String>, logical: Rect, scale: i32) -> Self {
        OutputMapping {
            name    : name.into(),
            logical : logical,
            scale   : scale.max(1),
        }
    }

    /// Converts a global point to surface-local logical pixels. Points outside the output
    /// map to coordinates outside the surface, which is deliberate: the caller decides
    /// whether to draw them (a marker straddling the seam is drawn clipped on both
    /// outputs, not dropped).
    pub fn local(&self, p: GlobalPx) -> (f64, f64) {
        (p.x - self.logical.x, p.y - self.logical.y)
    }

    /// Converts a global point to buffer pixels, ready to hand to tiny-skia.
    pub fn buffer(&self, p: GlobalPx) -> (f32, f32) {
        let (x, y) = self.local(p);
        let s      = f64::from(self.scale);

        ((x * s) as f32, (y * s) as f32)
    }

    /// Converts a global rectangle to buffer pixels as `(x, y, w, h)`.
    pub fn buffer_rect(&self, r: Rect) -> (f32, f32, f32, f32) {
        let (x, y) = self.buffer(GlobalPx { x: r.x, y: r.y });
        let s      = self.scale as f32;

        (x, y, r.w as f32 * s, r.h as f32 * s)
    }

    /// Converts a length in logical pixels to buffer pixels. Stroke widths and marker
    /// radii are authored in logical pixels so they look the same size on every output.
    pub fn buffer_len(&self, logical_px: f64) -> f32 {
        (logical_px * f64::from(self.scale)) as f32
    }

    /// Size of the `wl_shm` buffer this output needs, in physical pixels.
    pub fn buffer_size(&self) -> (u32, u32) {
        let s = f64::from(self.scale);

        ((self.logical.w * s).round() as u32, (self.logical.h * s).round() as u32)
    }

    /// True when the global point falls on this output.
    pub fn contains(&self, p: GlobalPx) -> bool {
        self.logical.contains(p)
    }

    /// True when any part of the global rectangle falls on this output. Used to skip
    /// surfaces that a highlight box does not touch.
    pub fn intersects(&self, r: Rect) -> bool {
        let l = &self.logical;

        r.x < l.x + l.w && r.x + r.w > l.x && r.y < l.y + l.h && r.y + r.h > l.y
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The three outputs of `config/desk.toml`. All three run at buffer scale 1:
    /// HDMI-A-1 is driven at a fractional scale, so its logical size comes straight from
    /// `zxdg_output_v1` and rounding its 1.15 ratio gives 1.
    fn desk() -> (OutputMapping, OutputMapping, OutputMapping) {
        let dp1 = OutputMapping::new(
            "DP-1",
            Rect { x: 2559.0, y: 0.0, w: 3840.0, h: 1600.0 },
            1,
        );
        let dp2 = OutputMapping::new(
            "DP-2",
            Rect { x: 0.0, y: 160.0, w: 2560.0, h: 1440.0 },
            1,
        );
        let hdmi = OutputMapping::new(
            "HDMI-A-1",
            Rect { x: 1506.0, y: 1600.0, w: 1670.0, h: 1043.0 },
            1,
        );

        (dp1, dp2, hdmi)
    }

    /// A genuinely HiDPI output, for the buffer scale arithmetic. Nothing on this desk
    /// is one, but the snap harness has to work on machines that have one.
    fn hidpi() -> OutputMapping {
        OutputMapping::new("HIDPI-1", Rect { x: 100.0, y: 200.0, w: 1000.0, h: 800.0 }, 2)
    }

    #[test]
    fn origin_maps_to_surface_origin() {
        let (dp1, dp2, hdmi) = desk();

        assert_eq!(dp1.local(GlobalPx { x: 2559.0, y: 0.0 })    , (0.0, 0.0));
        assert_eq!(dp2.local(GlobalPx { x: 0.0, y: 160.0 })     , (0.0, 0.0));
        assert_eq!(hdmi.local(GlobalPx { x: 1506.0, y: 1600.0 }), (0.0, 0.0));
    }

    #[test]
    fn fractionally_scaled_output_maps_through_its_logical_rect() {
        let (.., hdmi) = desk();

        // Centre of HDMI-A-1 in global px. Trusting the wl_output scale of 2 instead of
        // the xdg-output logical size would put this at (480, 300) and land every marker
        // in the top left quarter of the panel.
        let centre = GlobalPx { x: 1506.0 + 835.0, y: 1600.0 + 521.5 };

        assert_eq!(hdmi.local(centre) , (835.0, 521.5));
        assert_eq!(hdmi.buffer(centre), (835.0, 521.5));
        assert_eq!(hdmi.buffer_size() , (1670, 1043));
    }

    #[test]
    fn scale_two_output_doubles_into_buffer_space() {
        let map    = hidpi();
        let centre = GlobalPx { x: 600.0, y: 600.0 };

        assert_eq!(map.local(centre)   , (500.0, 400.0));
        assert_eq!(map.buffer(centre)  , (1000.0, 800.0));
        assert_eq!(map.buffer_size()   , (2000, 1600));
        assert_eq!(map.buffer_len(2.0) , 4.0);
    }

    #[test]
    fn scale_one_output_is_the_identity_apart_from_the_offset() {
        let (dp1, ..) = desk();

        assert_eq!(dp1.buffer(GlobalPx { x: 3000.0, y: 400.0 }), (441.0, 400.0));
        assert_eq!(dp1.buffer_size()                          , (3840, 1600));
        assert_eq!(dp1.buffer_len(2.0)                        , 2.0);
    }

    #[test]
    fn points_off_the_output_map_to_negative_or_oversized_coordinates() {
        let (dp1, dp2, _) = desk();

        // A point on DP-2 seen from DP-1's surface is to the left of its origin.
        let (x, y) = dp1.local(GlobalPx { x: 100.0, y: 200.0 });
        assert!(x < 0.0);
        assert_eq!(y, 200.0);

        // And a point on DP-1 seen from DP-2's surface is past its right edge.
        let (x, _) = dp2.local(GlobalPx { x: 4000.0, y: 200.0 });
        assert!(x > dp2.logical.w);
    }

    #[test]
    fn contains_picks_exactly_one_output_for_an_interior_point() {
        let (dp1, dp2, hdmi) = desk();
        let p                = GlobalPx { x: 1700.0, y: 1700.0 };

        assert!(hdmi.contains(p));
        assert!(!dp1.contains(p));
        assert!(!dp2.contains(p));
    }

    #[test]
    fn rect_conversion_keeps_size_in_buffer_pixels() {
        let map = hidpi();
        let r   = Rect { x: 120.0, y: 210.0, w: 100.0, h: 50.0 };

        assert_eq!(map.buffer_rect(r), (40.0, 20.0, 200.0, 100.0));
    }

    #[test]
    fn intersects_is_true_for_a_box_straddling_the_seam() {
        let (dp1, dp2, hdmi) = desk();

        // A box spanning the DP-2 / DP-1 seam at x = 2559 touches both, and neither
        // touches the panel below.
        let straddling = Rect { x: 2500.0, y: 300.0, w: 200.0, h: 100.0 };
        assert!(dp1.intersects(straddling));
        assert!(dp2.intersects(straddling));
        assert!(!hdmi.intersects(straddling));
    }

    #[test]
    fn intersects_is_false_for_a_box_touching_only_the_shared_edge() {
        let (dp1, dp2, _) = desk();

        // DP-2 ends at x = 2560, DP-1 starts at x = 2559, so a zero-overlap box that
        // ends exactly on DP-1's origin belongs to DP-2 alone.
        let flush = Rect { x: 2400.0, y: 300.0, w: 159.0, h: 100.0 };
        assert!(dp2.intersects(flush));
        assert!(!dp1.intersects(flush));
    }

    #[test]
    fn scale_is_clamped_to_at_least_one() {
        let m = OutputMapping::new("X", Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 }, 0);

        assert_eq!(m.scale        , 1);
        assert_eq!(m.buffer_size(), (10, 10));
    }
}
