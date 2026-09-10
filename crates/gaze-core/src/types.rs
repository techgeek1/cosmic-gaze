//! Plain data types shared across the workspace. Units are explicit in names: `_px` are
//! compositor logical pixels, `_mm` millimetres, `_deg` degrees, `_s` seconds.

use glam::DVec3;
use serde::{Deserialize, Serialize};

/// Position in the compositor's global logical pixel space (the space `wl_output`
/// positions and pointer coordinates live in, after per-output scale is applied).
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct GlobalPx {
    pub x : f64,
    pub y : f64,
}

/// Position in one output's local logical pixel space, origin at its top-left corner.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputPx {
    /// Connector name as reported by `wl_output` (`"DP-1"`, `"HDMI-A-1"`).
    pub output : String,
    pub x      : f64,
    pub y      : f64,
}

/// Axis-aligned rectangle in global logical pixels.
#[derive(Clone, Copy, Debug, PartialEq, Serialize, Deserialize)]
pub struct Rect {
    pub x : f64,
    pub y : f64,
    pub w : f64,
    pub h : f64,
}

/// A ray in the desk world frame: origin at the tracker, +X right, +Y up, +Z toward the
/// user. Millimetres. `dir` is unit length.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Ray {
    pub origin : DVec3,
    pub dir    : DVec3,
}

/// One gaze observation from any provider. A provider that only knows a screen point
/// sets `point` and leaves `ray` as `None`; the geometry layer can reconstruct the ray
/// from the nominal eye position. `sigma_deg` is the
/// provider's own estimate of its 1-sigma angular error for this sample and is what the
/// snap engine uses to size its search radius and to bail out to the coarse tier.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct GazeSample {
    /// Monotonic seconds since provider start.
    pub t_s       : f64,
    pub ray       : Option<Ray>,
    pub point     : Option<GlobalPx>,
    pub sigma_deg : f64,
    /// False when the provider has lost tracking (outside the head box, blink, grabbed
    /// device unplugged). Consumers must not use `ray`/`point` when this is false.
    pub valid     : bool,
}

/// Where an element box came from. Sources are ranked by the snap engine: a11y boxes are
/// authoritative when present, detector boxes fill gaps, OCR boxes are text-only targets.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ElementSource {
    Detector,
    Ocr,
    Accessibility,
}

/// Coarse element class. Detectors that only emit boxes use `Unknown`; classes exist so
/// fuzzy hit testing can rank by type (buttons over decorative icons over text runs).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub enum ElementKind {
    Button,
    Icon,
    Input,
    Link,
    Text,
    Checkbox,
    Slider,
    Unknown,
}

/// A clickable target on screen. `bbox` is global logical pixels so the same element index
/// serves every output. `score` is the source's confidence in [0, 1].
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Element {
    pub id     : u64,
    pub bbox   : Rect,
    pub kind   : ElementKind,
    pub source : ElementSource,
    pub score  : f32,
    /// Recognised text for OCR boxes, label text for a11y boxes, `None` for bare detections.
    pub text   : Option<String>,
}

// --- Rect ---

impl Rect {
    /// Centre point of the rectangle.
    pub fn center(&self) -> GlobalPx {
        GlobalPx { x: self.x + self.w * 0.5, y: self.y + self.h * 0.5 }
    }

    /// True when `p` lies inside or on the edge of the rectangle.
    pub fn contains(&self, p: GlobalPx) -> bool {
        p.x >= self.x && p.x <= self.x + self.w && p.y >= self.y && p.y <= self.y + self.h
    }

    /// Closest point on or inside the rectangle to `p`.
    pub fn clamp(&self, p: GlobalPx) -> GlobalPx {
        GlobalPx {
            x : p.x.clamp(self.x, self.x + self.w),
            y : p.y.clamp(self.y, self.y + self.h),
        }
    }

    /// Intersection-over-union with another rectangle. Zero when disjoint.
    pub fn iou(&self, other: &Rect) -> f64 {
        let ix = (self.x + self.w).min(other.x + other.w) - self.x.max(other.x);
        let iy = (self.y + self.h).min(other.y + other.h) - self.y.max(other.y);

        if ix <= 0.0 || iy <= 0.0 {
            return 0.0;
        }

        let inter = ix * iy;
        let union = self.w * self.h + other.w * other.h - inter;

        inter / union
    }
}
