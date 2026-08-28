//! Decoded 0x500 gaze notifications. One notification is an "xds row" of TLV columns;
//! which columns appear varies by firmware state, so every field here is optional and
//! the decoder keeps whatever prefix of the row it understood.
//!
//! Coordinate spaces, as the device defines them:
//!
//! - tracker space: millimetres, origin at the IR sensor array, +X right, +Y up,
//!   +Z away from the tracker toward the user.
//! - normalised 2D: the gaze ray intersected with the declared display area, [0, 1]^2
//!   with the origin at the top left.
//!
//! The per-eye ray cosmic-gaze runs on is `eye_origin -> gaze_point_3d`, both in
//! tracker space; it is valid regardless of the declared display plane.

use glam::DVec3;
use serde::{Deserialize, Serialize};

use crate::ttp::{DisplayRect, TlvReader};

/// Validity value for a tracked eye.
pub const VALIDITY_OK: u32 = 0;

/// One decoded gaze notification.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Et5Frame {
    /// Device microsecond clock.
    pub timestamp_us        : Option<i64>,
    /// Monotonic frame index.
    pub frame_counter       : Option<u32>,
    /// 0 = valid, 4 = eye not detected.
    pub validity_l          : Option<u32>,
    /// 0 = valid, 4 = eye not detected.
    pub validity_r          : Option<u32>,
    /// Pupil diameter, -1 when the eye is not detected.
    pub pupil_l_mm          : Option<f64>,
    /// Pupil diameter, -1 when the eye is not detected.
    pub pupil_r_mm          : Option<f64>,
    /// Combined binocular 2D gaze on the display area, temporally filtered.
    pub gaze_2d_norm        : Option<[f64; 2]>,
    /// Combined binocular 2D gaze before temporal smoothing.
    pub gaze_2d_unfiltered  : Option<[f64; 2]>,
    /// Left-eye 2D projection on the display area.
    pub gaze_2d_l_norm      : Option<[f64; 2]>,
    /// Right-eye 2D projection on the display area.
    pub gaze_2d_r_norm      : Option<[f64; 2]>,
    /// Calibrated left eye position, tracker space.
    pub eye_origin_l_mm     : Option<[f64; 3]>,
    /// Calibrated right eye position, tracker space.
    pub eye_origin_r_mm     : Option<[f64; 3]>,
    /// Left gaze ray to display-plane intersection, tracker space.
    pub gaze_3d_l_mm        : Option<[f64; 3]>,
    /// Right gaze ray to display-plane intersection, tracker space.
    pub gaze_3d_r_mm        : Option<[f64; 3]>,
    /// Pre-calibration left eye position, tracker space.
    pub eye_origin_raw_l_mm : Option<[f64; 3]>,
    /// Pre-calibration right eye position, tracker space.
    pub eye_origin_raw_r_mm : Option<[f64; 3]>,
}

// --- Et5Frame ---

impl Et5Frame {
    /// True when the left eye is tracked this frame.
    pub fn left_valid(&self) -> bool {
        self.validity_l == Some(VALIDITY_OK)
    }

    /// True when the right eye is tracked this frame.
    pub fn right_valid(&self) -> bool {
        self.validity_r == Some(VALIDITY_OK)
    }

    /// True when at least one eye is tracked.
    pub fn any_valid(&self) -> bool {
        self.left_valid() || self.right_valid()
    }
}

// --- Ray assembly ---

/// The naive combined gaze ray of a frame: midpoint origin, mean direction, or the
/// single tracked eye. Stateless; `EyeCombiner` is what the provider and the sweep
/// actually use, because near the tracking envelope one eye degrades badly before it
/// drops and an unweighted average follows it down.
pub fn combined_ray(frame: &Et5Frame) -> Option<(DVec3, DVec3, bool)> {
    let left  = eye_ray(frame.left_valid(), frame.eye_origin_l_mm, frame.gaze_3d_l_mm);
    let right = eye_ray(frame.right_valid(), frame.eye_origin_r_mm, frame.gaze_3d_r_mm);

    match (left, right) {
        (Some((lo, ld)), Some((ro, rd))) => Some(((lo + ro) * 0.5, (ld + rd).normalize(), true)),
        (Some((lo, ld)), None)           => Some((lo, ld, false)),
        (None, Some((ro, rd)))           => Some((ro, rd, false)),
        (None, None)                     => None,
    }
}

/// Origin and unit direction for one eye, when tracked and geometrically sane.
fn eye_ray(valid: bool, origin_mm: Option<[f64; 3]>, target_mm: Option<[f64; 3]>)
    -> Option<(DVec3, DVec3)>
{
    if !valid {
        return None;
    }

    let origin = DVec3::from_array(origin_mm?);
    let target = DVec3::from_array(target_mm?);
    let delta  = target - origin;

    // A degenerate frame can carry a zeroed target; a ray needs real length.
    if delta.length_squared() < 1.0 {
        return None;
    }

    Some((origin, delta.normalize()))
}

/// Reconstructs the firmware's own gaze ray from its filtered 2D output.
///
/// `gaze_2d_norm` is the device's complete pipeline: its per-eye combination, its
/// vergence handling, and its tuned temporal filter, expressed as a normalised point
/// on the declared display area. That point plus the eye origin is a ray, so this
/// recovers the firmware's filtered ray exactly, and it is preferred over re-deriving
/// one from the raw per-eye fields whenever it is present. `area` must be the display
/// area the device currently has declared. Returns origin, unit direction, binocular.
pub fn filtered_ray(frame: &Et5Frame, area: &DisplayRect) -> Option<(DVec3, DVec3, bool)> {
    let [nx, ny] = frame.gaze_2d_norm?;

    // The firmware reports (-1, -1) when the combined gaze is invalid, and clamps to
    // the area bounds otherwise; a clamped point is a wrong direction, so reject the
    // extremes too.
    if !(0.0..1.0).contains(&nx) || !(0.0..1.0).contains(&ny) {
        return None;
    }

    let left  = frame.left_valid().then_some(frame.eye_origin_l_mm).flatten();
    let right = frame.right_valid().then_some(frame.eye_origin_r_mm).flatten();

    let origin = {
        match (left, right) {
            (Some(l), Some(r)) => (DVec3::from_array(l) + DVec3::from_array(r)) * 0.5,
            (Some(l), None)    => DVec3::from_array(l),
            (None, Some(r))    => DVec3::from_array(r),
            (None, None)       => return None,
        }
    };

    // Normalised coordinates run from the area's top-left corner, y downward.
    let point = DVec3::new(
        area.ox_mm + nx * area.w_mm,
        (area.oy_mm + area.h_mm) - ny * area.h_mm,
        area.z_mm,
    );

    let delta = point - origin;

    if delta.length_squared() < 1.0 {
        return None;
    }

    Some((origin, delta.normalize(), left.is_some() && right.is_some()))
}

// --- Eye fusion ---

/// Per-eye jitter floor, degrees. Keeps a perfectly steady eye from getting infinite
/// weight and sets the scale on which jitter differences start to matter.
const JITTER_FLOOR_DEG: f64 = 0.2;

/// EMA rate for the per-eye jitter and offset estimates. Settles in well under a
/// second at 90 Hz.
const FUSION_ALPHA: f64 = 0.05;

/// Inter-eye disagreement, degrees, past which one eye is lying (measured up to 10
/// degrees on a dying eye at the envelope edge) and the frame falls back to the eye
/// that agrees better with the recent combined ray.
const DISAGREE_DEG: f64 = 4.0;

/// The fused output of one frame.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct FusedRay {
    /// Ray origin, tracker space, millimetres.
    pub origin_mm : DVec3,
    /// Unit ray direction, tracker space.
    pub dir       : DVec3,
    /// True when both eyes contributed to this frame.
    pub binocular : bool,
}

/// State for one eye inside the combiner.
#[derive(Clone, Copy, Debug, Default)]
struct EyeState {
    /// EMA of frame-to-frame angular jitter, degrees.
    jitter_deg : f64,
    /// Previous direction, for the jitter measurement.
    prev_dir   : Option<DVec3>,
    /// Learned direction offset from the combined ray, for monocular frames.
    dir_off    : Option<DVec3>,
    /// Learned origin offset from the combined ray, for monocular frames.
    origin_off : Option<DVec3>,
}

/// Quality-weighted binocular fusion with smooth monocular handoff.
///
/// The measured failure mode this exists for: gaze toward one side degrades the far
/// eye (nose shadow, glint leaving the cornea) long before validity drops, with
/// errors reaching several degrees while the near eye stays sub-degree. So each eye
/// carries a running jitter estimate that sets its blend weight, a frame where the
/// eyes flatly disagree defers to the one closer to the recent combined ray, and a
/// monocular frame reconstructs the combined ray through the offsets learned while
/// both eyes were healthy, so losing an eye does not jump the output.
#[derive(Clone, Debug, Default)]
pub struct EyeCombiner {
    left     : EyeState,
    right    : EyeState,
    /// Recent combined direction, the reference for disagreement resolution.
    prev_out : Option<DVec3>,
}

// --- EyeCombiner ---

impl EyeCombiner {
    /// Creates a combiner with no history.
    pub fn new() -> Self {
        Self::default()
    }

    /// Fuses one frame. `None` when no eye is usable.
    pub fn combine(&mut self, frame: &Et5Frame) -> Option<FusedRay> {
        let left  = eye_ray(frame.left_valid(), frame.eye_origin_l_mm, frame.gaze_3d_l_mm);
        let right = eye_ray(frame.right_valid(), frame.eye_origin_r_mm, frame.gaze_3d_r_mm);

        // Jitter updates happen for every tracked eye, fused or not, so a recovering
        // eye has to demonstrate stability before it earns weight back.
        if let Some((_, d)) = left {
            update_jitter(&mut self.left, d);
        }

        if let Some((_, d)) = right {
            update_jitter(&mut self.right, d);
        }

        let fused = {
            match (left, right) {
                (Some(l), Some(r)) => self.fuse_binocular(l, r),
                (Some(l), None)    => self.monocular(l, self.left),
                (None, Some(r))    => self.monocular(r, self.right),
                (None, None)       => return None,
            }
        };

        self.prev_out = Some(fused.dir);

        Some(fused)
    }
}

impl EyeCombiner {
    /// Blends two tracked eyes by jitter weight, or defers to the better-agreeing
    /// eye when they contradict each other.
    fn fuse_binocular(
        &mut self,
        (lo, ld) : (DVec3, DVec3),
        (ro, rd) : (DVec3, DVec3),
    )
        -> FusedRay
    {
        let disagree = ld.angle_between(rd).to_degrees();

        if disagree > DISAGREE_DEG
            && let Some(prev) = self.prev_out {
                // One eye is lying; trust the one continuing the recent track. The
                // offsets are not updated from a frame like this.
                let keep_left = ld.angle_between(prev) <= rd.angle_between(prev);

                return {
                    if keep_left {
                        self.monocular((lo, ld), self.left)
                    }
                    else {
                        self.monocular((ro, rd), self.right)
                    }
                };
            }

        let wl = eye_weight(&self.left);
        let wr = eye_weight(&self.right);

        let dir    = (ld * wl + rd * wr).normalize();
        let origin = (lo * wl + ro * wr) / (wl + wr);

        // Learn each eye's offset from this healthy combined ray for later
        // monocular frames.
        update_offsets(&mut self.left, ld - dir, lo - origin);
        update_offsets(&mut self.right, rd - dir, ro - origin);

        FusedRay { origin_mm: origin, dir: dir, binocular: true }
    }

    /// Reconstructs the combined ray from one eye through its learned offsets.
    fn monocular(&self, (o, d): (DVec3, DVec3), state: EyeState) -> FusedRay {
        let (origin, dir) = {
            match (state.origin_off, state.dir_off) {
                (Some(oo), Some(dd)) => (o - oo, (d - dd).normalize()),
                _                    => (o, d),
            }
        };

        FusedRay { origin_mm: origin, dir: dir, binocular: false }
    }
}

/// Updates an eye's jitter EMA from its frame-to-frame direction change.
fn update_jitter(state: &mut EyeState, dir: DVec3) {
    if let Some(prev) = state.prev_dir {
        let step = dir.angle_between(prev).to_degrees();

        state.jitter_deg += FUSION_ALPHA * (step - state.jitter_deg);
    }

    state.prev_dir = Some(dir);
}

/// Updates an eye's learned offsets from a healthy binocular frame.
fn update_offsets(state: &mut EyeState, dir_off: DVec3, origin_off: DVec3) {
    state.dir_off = Some(match state.dir_off {
        Some(d) => d.lerp(dir_off, FUSION_ALPHA),
        None    => dir_off,
    });

    state.origin_off = Some(match state.origin_off {
        Some(o) => o.lerp(origin_off, FUSION_ALPHA),
        None    => origin_off,
    });
}

/// An eye's blend weight: inverse square of its jitter above the floor.
fn eye_weight(state: &EyeState) -> f64 {
    let j = JITTER_FLOOR_DEG + state.jitter_deg;

    1.0 / (j * j)
}

// --- Decoding ---

/// TLV shape of a gaze column, used to skip fields this decoder does not keep.
enum ColumnKind {
    S64,
    U32,
    Q16,
    Point2,
    Point3,
}

/// Maps a column id to its TLV shape. `None` for ids never observed; hitting one stops
/// the decode because field boundaries past it are unknown.
fn column_kind(col: u32) -> Option<ColumnKind> {
    match col {
        0x01 => Some(ColumnKind::S64),
        0x02 | 0x03 | 0x04 | 0x08 | 0x09 | 0x0a
        | 0x17 | 0x18 | 0x22 | 0x24 | 0x25 | 0x27 => Some(ColumnKind::Point3),
        0x05 | 0x0b | 0x19 | 0x1a | 0x1c | 0x20   => Some(ColumnKind::Point2),
        0x06 | 0x0c | 0x29 | 0x2b                 => Some(ColumnKind::Q16),
        0x07 | 0x0d | 0x0e | 0x11 | 0x14 | 0x15 | 0x16 | 0x1b
        | 0x1d | 0x1e | 0x1f | 0x21 | 0x23 | 0x26 | 0x28
        | 0x2a | 0x2c                             => Some(ColumnKind::U32),
        _                                         => None,
    }
}

/// Decodes a 0x500 notification payload. `None` only when the payload is not an xds
/// row at all; a row that ends early or hits an unknown column yields the fields
/// decoded up to that point.
pub fn decode(payload: &[u8]) -> Option<Et5Frame> {
    if payload.len() < 2 {
        return None;
    }

    let mut r = TlvReader::new(payload);
    r.pos = 2;

    let n_cols = r.read_xds_row().ok()?;
    let mut frame = Et5Frame::default();

    for _ in 0..n_cols {
        if r.remaining() == 0 {
            break;
        }

        let Ok(col) = r.read_xds_column() else {
            break;
        };

        // Each arm reads exactly one field; a short read mid-field ends the row with
        // whatever was decoded so far.
        let ok = {
            match col {
                0x01 => r.read_s64().map(|v| frame.timestamp_us = Some(v)).is_ok(),
                0x14 => r.read_u32().map(|v| frame.frame_counter = Some(v)).is_ok(),
                0x07 => r.read_u32().map(|v| frame.validity_l = Some(v)).is_ok(),
                0x0d => r.read_u32().map(|v| frame.validity_r = Some(v)).is_ok(),
                0x06 => r.read_q16().map(|v| frame.pupil_l_mm = Some(v)).is_ok(),
                0x0c => r.read_q16().map(|v| frame.pupil_r_mm = Some(v)).is_ok(),
                0x1c => r.read_point2().map(|v| frame.gaze_2d_norm = Some(v)).is_ok(),
                0x20 => r.read_point2().map(|v| frame.gaze_2d_unfiltered = Some(v)).is_ok(),
                0x05 => r.read_point2().map(|v| frame.gaze_2d_l_norm = Some(v)).is_ok(),
                0x0b => r.read_point2().map(|v| frame.gaze_2d_r_norm = Some(v)).is_ok(),
                0x02 => r.read_point3().map(|v| frame.eye_origin_l_mm = Some(v)).is_ok(),
                0x08 => r.read_point3().map(|v| frame.eye_origin_r_mm = Some(v)).is_ok(),
                0x04 => r.read_point3().map(|v| frame.gaze_3d_l_mm = Some(v)).is_ok(),
                0x0a => r.read_point3().map(|v| frame.gaze_3d_r_mm = Some(v)).is_ok(),
                0x17 => r.read_point3().map(|v| frame.eye_origin_raw_l_mm = Some(v)).is_ok(),
                0x18 => r.read_point3().map(|v| frame.eye_origin_raw_r_mm = Some(v)).is_ok(),
                other => {
                    // Skip columns the frame does not keep, by their known shape.
                    let Some(kind) = column_kind(other) else {
                        return Some(frame);
                    };

                    match kind {
                        ColumnKind::S64    => r.read_s64().is_ok(),
                        ColumnKind::U32    => r.read_u32().is_ok(),
                        ColumnKind::Q16    => r.read_q16().is_ok(),
                        ColumnKind::Point2 => r.read_point2().is_ok(),
                        ColumnKind::Point3 => r.read_point3().is_ok(),
                    }
                }
            }
        };

        if !ok {
            break;
        }
    }

    Some(frame)
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ttp::q42_encode;

    /// Appends `[type][size BE][..]` headers and bodies for the test payload.
    fn put(out: &mut Vec<u8>, t: u8, body: &[u8]) {
        out.push(t);
        out.extend_from_slice(&(body.len() as u32).to_be_bytes());
        out.extend_from_slice(body);
    }

    fn put_tag(out: &mut Vec<u8>, tag: u32) {
        put(out, 5, &tag.to_be_bytes());
    }

    fn put_u32_col(out: &mut Vec<u8>, col: u32) {
        put_tag(out, 0x020bb9);
        put(out, 2, &col.to_be_bytes());
    }

    fn put_q42(out: &mut Vec<u8>, v: f64) {
        put(out, 4, &q42_encode(v).to_be_bytes());
    }

    fn put_point3(out: &mut Vec<u8>, v: [f64; 3]) {
        put_tag(out, 0x31f41);
        put_q42(out, v[0]);
        put_q42(out, v[1]);
        put_q42(out, v[2]);
    }

    #[test]
    fn decodes_a_synthesised_row() {
        let mut p = vec![0x00, 0x00];
        // xds row with 4 columns: count packed into the prolog tag's high bits.
        put_tag(&mut p, (4 << 16) | 0x0bb8);

        // timestamp.
        put_u32_col(&mut p, 0x01);
        put(&mut p, 6, &123456789i64.to_be_bytes());
        // validity_L = 0.
        put_u32_col(&mut p, 0x07);
        put(&mut p, 2, &0u32.to_be_bytes());
        // eye_origin_L.
        put_u32_col(&mut p, 0x02);
        put_point3(&mut p, [-31.5, 12.0, 512.25]);
        // gaze_point_3d_L.
        put_u32_col(&mut p, 0x04);
        put_point3(&mut p, [10.0, 150.0, -20.0]);

        let f = decode(&p).expect("row decodes");
        assert_eq!(f.timestamp_us, Some(123456789));
        assert_eq!(f.validity_l, Some(0));
        assert!(f.left_valid());
        assert!(!f.right_valid());

        let eye = f.eye_origin_l_mm.expect("eye origin");
        assert!((eye[0] - -31.5).abs() < 1e-9);
        assert!((eye[2] - 512.25).abs() < 1e-9);
        assert!(f.gaze_3d_l_mm.is_some());
    }

    #[test]
    fn unknown_column_ends_the_row_gracefully() {
        let mut p = vec![0x00, 0x00];
        put_tag(&mut p, (2 << 16) | 0x0bb8);
        put_u32_col(&mut p, 0x07);
        put(&mut p, 2, &0u32.to_be_bytes());
        // A column id outside the known table; decode keeps what it has.
        put_u32_col(&mut p, 0xff);
        put(&mut p, 2, &7u32.to_be_bytes());

        let f = decode(&p).expect("prefix decodes");
        assert_eq!(f.validity_l, Some(0));
    }

    #[test]
    fn non_row_payload_is_rejected() {
        assert!(decode(&[0x00]).is_none());
        assert!(decode(&[0x00, 0x00, 0xde, 0xad]).is_none());
    }

    #[test]
    fn filtered_ray_reconstructs_the_plane_point() {
        let area = DisplayRect { w_mm: 2400.0, h_mm: 1400.0, ox_mm: -1200.0, oy_mm: -700.0, z_mm: 0.0 };

        // Centre of the area, slightly above middle: nx 0.5 -> x 0, ny 0.25 -> y 350.
        let mut f = Et5Frame {
            validity_l      : Some(0),
            validity_r      : Some(0),
            eye_origin_l_mm : Some([-32.0, 100.0, 600.0]),
            eye_origin_r_mm : Some([32.0, 100.0, 600.0]),
            gaze_2d_norm    : Some([0.5, 0.25]),
            ..Default::default()
        };

        let (origin, dir, bino) = filtered_ray(&f, &area).expect("ray");
        assert!(bino);
        assert!((origin.x - 0.0).abs() < 1e-9);
        // The ray from (0, 100, 600) through (0, 350, 0) heads up and toward the plane.
        let t = -origin.z / dir.z;
        let hit = origin + dir * t;
        assert!((hit.x - 0.0).abs() < 1e-6);
        assert!((hit.y - 350.0).abs() < 1e-6);

        // Invalid and clamped 2D are rejected.
        f.gaze_2d_norm = Some([-1.0, -1.0]);
        assert!(filtered_ray(&f, &area).is_none());
        f.gaze_2d_norm = Some([1.0, 0.5]);
        assert!(filtered_ray(&f, &area).is_none());
    }

    /// A frame with both eyes at the given directions from fixed origins.
    fn frame(left: Option<[f64; 3]>, right: Option<[f64; 3]>) -> Et5Frame {
        let mut f = Et5Frame::default();

        if let Some(d) = left {
            f.validity_l      = Some(0);
            f.eye_origin_l_mm = Some([-32.0, 0.0, 600.0]);
            f.gaze_3d_l_mm    = Some([-32.0 + d[0], d[1], 600.0 - d[2]]);
        }
        else {
            f.validity_l = Some(4);
        }

        if let Some(d) = right {
            f.validity_r      = Some(0);
            f.eye_origin_r_mm = Some([32.0, 0.0, 600.0]);
            f.gaze_3d_r_mm    = Some([32.0 + d[0], d[1], 600.0 - d[2]]);
        }
        else {
            f.validity_r = Some(4);
        }

        f
    }

    #[test]
    fn combiner_blends_stable_eyes() {
        let mut c = EyeCombiner::new();
        let mut last = None;

        for _ in 0..50 {
            last = c.combine(&frame(Some([10.0, 0.0, 600.0]), Some([-10.0, 0.0, 600.0])));
        }

        let fused = last.expect("fused");
        assert!(fused.binocular);
        // Symmetric convergence blends to straight ahead.
        assert!(fused.dir.x.abs() < 0.02, "dir {:?}", fused.dir);
        assert!(fused.origin_mm.x.abs() < 2.0);
    }

    #[test]
    fn combiner_defers_to_the_agreeing_eye() {
        let mut c = EyeCombiner::new();

        for _ in 0..30 {
            c.combine(&frame(Some([0.0, 0.0, 600.0]), Some([0.0, 0.0, 600.0])));
        }

        // The right eye swings off by ~9 degrees; the fused ray must stay with the
        // left rather than splitting the difference.
        let fused = c.combine(&frame(Some([0.0, 0.0, 600.0]), Some([95.0, 0.0, 600.0])))
            .expect("fused");
        let off_deg = fused.dir.angle_between(glam::DVec3::new(0.0, 0.0, -1.0)).to_degrees();
        assert!(off_deg < 1.0, "fused ray dragged {off_deg} deg off");
    }

    #[test]
    fn combiner_hands_off_to_monocular_smoothly() {
        let mut c = EyeCombiner::new();
        let mut before = None;

        for _ in 0..100 {
            before = c.combine(&frame(Some([10.0, 5.0, 600.0]), Some([-10.0, 5.0, 600.0])));
        }

        let before = before.expect("binocular");
        let after  = c.combine(&frame(Some([10.0, 5.0, 600.0]), None)).expect("monocular");

        assert!(!after.binocular);
        let jump = before.dir.angle_between(after.dir).to_degrees();
        assert!(jump < 0.2, "monocular handoff jumped {jump} deg");
        assert!((after.origin_mm.x - before.origin_mm.x).abs() < 2.0);
    }
}
