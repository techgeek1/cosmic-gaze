//! Angular scale lookup: how many logical pixels one degree of visual angle covers at a
//! given point of the desktop.
//!
//! Everything in this crate reasons in degrees, but element boxes are pixels, so the
//! conversion happens constantly. It lives behind a trait for two reasons: the real
//! implementation ([`DesktopGeometry`]) is an expensive numeric Jacobian that the snap
//! engine should be free to swap for a cheaper cache, and unit tests want a flat world
//! with a scale they can state in one line.

use gaze_core::{DesktopGeometry, GlobalPx};

/// Pixels per degree used when a scale source cannot answer for a point (a gaze point
/// that lands in the gap between outputs, say). Roughly a 27 inch 1440p panel at 650 mm.
pub const FALLBACK_PX_PER_DEG : f64 = 60.0;

/// Local angular scale of the desktop at a point.
///
/// Implementations must be cheap enough to call a handful of times per gaze sample and
/// must never panic: an unanswerable point returns a plausible fallback rather than an
/// error, because a snap decision made with a slightly wrong scale is better than no
/// decision at all.
pub trait PxScale {
    /// Logical pixels per degree of visual angle at `p`, horizontal and vertical.
    ///
    /// The two axes differ on a rotated or curved panel, and both differ between outputs,
    /// which is why callers must not cache a single number for the whole desktop.
    fn px_per_deg(&self, p: GlobalPx) -> (f64, f64);
}

/// A single scale for the whole desktop. The flat-panel approximation: useful for tests,
/// for single-output setups, and as a stand-in before a desk config is loaded.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ConstPxScale {
    pub x_px_per_deg : f64,
    pub y_px_per_deg : f64,
}

// --- ConstPxScale ---

impl ConstPxScale {
    /// Square pixels: the same scale on both axes.
    pub fn new(px_per_deg: f64) -> Self {
        Self { x_px_per_deg: px_per_deg, y_px_per_deg: px_per_deg }
    }

    /// Different scales per axis.
    pub fn anisotropic(x_px_per_deg: f64, y_px_per_deg: f64) -> Self {
        Self { x_px_per_deg: x_px_per_deg, y_px_per_deg: y_px_per_deg }
    }
}

impl Default for ConstPxScale {
    fn default() -> Self {
        Self::new(FALLBACK_PX_PER_DEG)
    }
}

impl PxScale for ConstPxScale {
    fn px_per_deg(&self, _p: GlobalPx) -> (f64, f64) {
        (self.x_px_per_deg, self.y_px_per_deg)
    }
}

// --- DesktopGeometry ---

impl PxScale for DesktopGeometry {
    /// Defers to the desk model, seen from the nominal eye. Points not covered by an
    /// enabled output fall back to [`FALLBACK_PX_PER_DEG`]; the snap engine only ever
    /// asks about the gaze point and about element boxes, both of which are on screen in
    /// practice.
    fn px_per_deg(&self, p: GlobalPx) -> (f64, f64) {
        let eye = self.eye();

        DesktopGeometry::px_per_deg(self, eye, p)
            .unwrap_or((FALLBACK_PX_PER_DEG, FALLBACK_PX_PER_DEG))
    }
}
