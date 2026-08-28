//! The webcam's pose on the desk, and the conversion between the camera frame the sidecar
//! reports in and the desk world frame everything else speaks.
//!
//! # The two frames
//!
//! The sidecar works in the OpenCV camera frame: **+X right, +Y down, +Z forward out of
//! the lens**. The desk world frame (see `gaze_core::geometry`) is **+X to the user's
//! right, +Y up, +Z toward the user**. A camera sitting on the monitor and looking
//! straight back at the user has its optical axis along desk +Z, and its image "down"
//! along desk -Y. Right-handedness then forces its image "right" onto desk -X, which is
//! also what you see in an unmirrored webcam image: the hand on your right appears on the
//! left of the frame.
//!
//! So the fixed part of the mapping is `diag(-1, -1, 1)`, a 180 degree rotation about +Z,
//! and the `yaw/pitch/roll` in `desk.toml` are what the camera does *on top of* that
//! nominal user-facing pose. They use the workspace convention
//! (`gaze_core::rotation_ypr`), the same one the panel angles in the same file use, so
//! they are read in the desk frame and not in the camera's. See the note on the pitch
//! sign below.
//!
//! Full camera-to-desk rotation: `R_yaw * R_pitch * R_roll * R_z(180deg)`.
//!
//! # Sign of the pitch, and a discrepancy in `desk.toml`
//!
//! Because the angles are in the desk frame, they mean exactly what they mean for a
//! panel: a negative yaw turns toward -X, and a negative pitch rotates the axis it is
//! applied to *up* (this is why `HDMI-A-1`, whose top is tilted away from the user so its
//! normal points up, carries `pitch_deg = -15`). Applied to the camera, whose nominal
//! axis is the direction it looks in, a negative pitch therefore points the lens up.
//!
//! `config/desk.toml` currently has `[camera] pitch_deg = -20.0` commented "looking down
//! at the face", which under this convention points it 20 degrees above horizontal
//! instead. Both signs cannot be honoured at once: the flip negates X and Y together, so
//! any convention that makes that pitch mean "down" also makes the neighbouring
//! `yaw_deg = -8.0` mean "turned away from the seam", contradicting its own comment. The
//! angles are read the way the panels' are, per the workspace convention; the value in the
//! config wants to be `+20.0` if the comment is the intent. The camera pose is a MEASURE
//! placeholder either way, and the calibration's angular stage absorbs a residual pose
//! error, though not one this large.

use std::f64::consts::PI;

use gaze_core::rotation_ypr;
use glam::{DQuat, DVec3};
use serde::{Deserialize, Serialize};

/// Where the webcam is and which way it points, plus the lens facts the sidecar may want.
/// Parsed from the `[camera]` block of `desk.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct CameraPose {
    /// Device node the sidecar opens. Not used by this crate; carried so one config load
    /// serves both sides.
    #[serde(default = "default_device")]
    pub device      : String,
    /// World position of the camera's optical centre, millimetres.
    pub position_mm : [f64; 3],
    #[serde(default)]
    pub yaw_deg     : f64,
    #[serde(default)]
    pub pitch_deg   : f64,
    #[serde(default)]
    pub roll_deg    : f64,
    /// Horizontal field of view of the lens. Only meaningful to the sidecar, which uses it
    /// when it has no intrinsics file.
    #[serde(default = "default_hfov_deg")]
    pub hfov_deg    : f64,
    #[serde(default = "default_width")]
    pub width       : u32,
    #[serde(default = "default_height")]
    pub height      : u32,
}

/// Wrapper for pulling just the `[camera]` table out of a `desk.toml` document. Every
/// other key in the file is ignored, so the same text can be handed to
/// `DesktopGeometry::from_toml` and to this.
#[derive(Deserialize)]
struct DeskDocument {
    camera : CameraPose,
}

// --- CameraPose ---

impl CameraPose {
    /// Parses the `[camera]` block of a `desk.toml` document.
    pub fn from_desk_toml(text: &str) -> Result<Self, CameraError> {
        let doc: DeskDocument = toml::from_str(text)
            .map_err(|e| CameraError::Parse(e.to_string()))?;

        Ok(doc.camera)
    }

    /// Camera position in the desk world frame.
    pub fn position(&self) -> DVec3 {
        DVec3::from_array(self.position_mm)
    }

    /// Camera-to-desk rotation, including the fixed user-facing flip. See the module docs
    /// for the derivation.
    pub fn rotation(&self) -> DQuat {
        rotation_ypr(self.yaw_deg, self.pitch_deg, self.roll_deg) * DQuat::from_rotation_z(PI)
    }

    /// Maps a point given in camera-frame millimetres into the desk world frame.
    pub fn point_to_desk(&self, camera_mm: DVec3) -> DVec3 {
        self.position() + self.rotation() * camera_mm
    }

    /// Maps a direction given in the camera frame into the desk world frame. The result is
    /// renormalised, so a sidecar unit vector that has drifted off unit length by a few
    /// ulps still yields a usable ray.
    pub fn dir_to_desk(&self, camera_dir: DVec3) -> DVec3 {
        let d = self.rotation() * camera_dir;

        // A zero or non-finite direction has no meaningful image; hand back forward rather
        // than a NaN that would poison every downstream intersection.
        if !d.is_finite() || d.length_squared() <= 0.0 {
            return DVec3::Z;
        }

        d.normalize()
    }

    /// Inverse of `point_to_desk`. Used by the fake sidecar to phrase a desk-frame truth
    /// in the terms the real sidecar would report it.
    pub fn point_to_camera(&self, desk_mm: DVec3) -> DVec3 {
        self.rotation().conjugate() * (desk_mm - self.position())
    }

    /// Inverse of `dir_to_desk`.
    pub fn dir_to_camera(&self, desk_dir: DVec3) -> DVec3 {
        let d = self.rotation().conjugate() * desk_dir;

        if !d.is_finite() || d.length_squared() <= 0.0 {
            return DVec3::Z;
        }

        d.normalize()
    }
}

impl Default for CameraPose {
    /// A camera at the desk origin looking straight at the user, which is the pose that
    /// makes the camera-to-desk map the bare flip. Handy in tests.
    fn default() -> Self {
        Self {
            device      : default_device(),
            position_mm : [0.0, 0.0, 0.0],
            yaw_deg     : 0.0,
            pitch_deg   : 0.0,
            roll_deg    : 0.0,
            hfov_deg    : default_hfov_deg(),
            width       : default_width(),
            height      : default_height(),
        }
    }
}

/// Yaw and pitch of a gaze direction in the camera frame, degrees.
///
/// `yaw = atan2(gx, -gz)`, `pitch = asin(-gy)`. The reference is the direction *into* the
/// lens (`-Z`), because that is roughly where a tracked user's gaze goes when they look
/// past the camera at the screens behind it, which keeps both angles small and signed the
/// way a person would describe them: yaw positive to the image right, pitch positive up.
///
/// These are the coordinates to judge a gaze model in. A model that under-reports how far
/// the eye has turned looks like an awkward polynomial in pixel space but like a plain
/// slope below one here.
pub fn gaze_yaw_pitch_deg(gaze: DVec3) -> Option<(f64, f64)> {
    let len = gaze.length();

    if !len.is_finite() || len <= 0.0 {
        return None;
    }

    let g = gaze / len;

    // `asin` is only defined on [-1, 1] and a unit vector's component can sit a bit
    // outside it after arithmetic; clamping is the difference between an angle and a NaN
    // that would poison every mean built on top of it.
    Some((
        g.x.atan2(-g.z).to_degrees(),
        (-g.y).clamp(-1.0, 1.0).asin().to_degrees(),
    ))
}

/// Inverse of `gaze_yaw_pitch_deg`.
pub fn gaze_dir_from_yaw_pitch_deg(yaw_deg: f64, pitch_deg: f64) -> DVec3 {
    let (yaw, pitch) = (yaw_deg.to_radians(), pitch_deg.to_radians());

    DVec3::new(pitch.cos() * yaw.sin(), -pitch.sin(), -pitch.cos() * yaw.cos())
}

fn default_device() -> String {
    "/dev/video0".to_string()
}

fn default_hfov_deg() -> f64 {
    78.0
}

fn default_width() -> u32 {
    1920
}

fn default_height() -> u32 {
    1080
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum CameraError {
    #[error("camera config parse error: {0}")]
    Parse(String),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The frozen 2026-08-25 desk snapshot these assertions were written against;
    /// the live `config/desk.toml` drifts with the physical desk.
    const FIXTURE_TOML: &str = include_str!("../../../config/desk-fixture.toml");

    /// Angle between two directions, degrees.
    fn angle_deg(a: DVec3, b: DVec3) -> f64 {
        a.angle_between(b).to_degrees()
    }

    #[test]
    fn parses_the_camera_block_of_the_desk_fixture() {
        let c = CameraPose::from_desk_toml(FIXTURE_TOML).unwrap();

        assert_eq!(c.device, "/dev/video0");
        assert_eq!(c.position_mm, [100.0, 80.0, 250.0]);
        assert_eq!(c.yaw_deg, -14.0);
        assert_eq!(c.pitch_deg, -14.0);
        assert_eq!(c.roll_deg, 0.0);
        assert_eq!(c.width, 1920);
        assert_eq!(c.height, 1080);
    }

    #[test]
    fn an_unrotated_camera_faces_the_user_and_flips_x_and_y() {
        let c = CameraPose::default();

        // Straight out of the lens is straight at the user.
        assert!((c.dir_to_desk(DVec3::Z) - DVec3::Z).length() < 1.0e-12);

        // Image right is the user's left, image down is world down.
        assert!((c.dir_to_desk(DVec3::X) + DVec3::X).length() < 1.0e-12);
        assert!((c.dir_to_desk(DVec3::Y) + DVec3::Y).length() < 1.0e-12);
    }

    #[test]
    fn a_pitch_of_minus_twenty_tilts_a_straight_ahead_gaze_up_by_twenty() {
        let c = CameraPose { pitch_deg: -20.0, ..CameraPose::default() };

        // Gaze straight down the optical axis. The pitch is in the desk frame, where a
        // negative rotation about +X takes +Z toward +Y, so the ray comes out 20 degrees
        // *above* horizontal. See the module docs: this is the panels' convention, and it
        // is the opposite of what `desk.toml`'s camera comment expects.
        let d = c.dir_to_desk(DVec3::Z);

        assert!((angle_deg(d, DVec3::Z) - 20.0).abs() < 1.0e-9, "d = {d:?}");
        assert!(d.y > 0.0, "a negative pitch tilts up in this convention: {d:?}");
        assert!(d.x.abs() < 1.0e-12);

        let up = 20.0_f64.to_radians();
        assert!((d - DVec3::new(0.0, up.sin(), up.cos())).length() < 1.0e-12);
    }

    #[test]
    fn a_positive_pitch_points_the_lens_down_at_the_face() {
        let c = CameraPose { pitch_deg: 20.0, ..CameraPose::default() };
        let d = c.dir_to_desk(DVec3::Z);

        assert!((angle_deg(d, DVec3::Z) - 20.0).abs() < 1.0e-9);
        assert!(d.y < 0.0, "a positive pitch must look down: {d:?}");
    }

    #[test]
    fn the_camera_pitch_matches_the_sign_a_panel_uses() {
        // A panel and the camera must agree on what a pitch does, or two poses in the same
        // config file mean different things. Rotate each nominal axis by the same angle
        // and check the vertical components move the same way.
        let panel  = gaze_core::rotation_ypr(0.0, -20.0, 0.0) * DVec3::Z;
        let camera = CameraPose { pitch_deg: -20.0, ..CameraPose::default() }.dir_to_desk(DVec3::Z);

        assert!((panel - camera).length() < 1.0e-12, "panel {panel:?} vs camera {camera:?}");
    }

    #[test]
    fn a_negative_yaw_turns_the_camera_toward_minus_x() {
        let c = CameraPose { yaw_deg: -8.0, ..CameraPose::default() };
        let d = c.dir_to_desk(DVec3::Z);

        assert!((angle_deg(d, DVec3::Z) - 8.0).abs() < 1.0e-9);
        assert!(d.x < 0.0, "negative yaw must turn toward the seam: {d:?}");
        assert!(d.y.abs() < 1.0e-12);
    }

    #[test]
    fn points_land_where_the_pose_says_they_should() {
        let c = CameraPose { position_mm: [220.0, 390.0, -110.0], ..CameraPose::default() };

        // 300 mm straight in front of the lens, on axis.
        let p = c.point_to_desk(DVec3::new(0.0, 0.0, 300.0));
        assert!((p - DVec3::new(220.0, 390.0, 190.0)).length() < 1.0e-9);

        // 100 mm to the image right and 50 mm down: the user's left, and lower.
        let q = c.point_to_desk(DVec3::new(100.0, 50.0, 0.0));
        assert!((q - DVec3::new(120.0, 340.0, -110.0)).length() < 1.0e-9);
    }

    #[test]
    fn camera_and_desk_conversions_round_trip_on_the_real_pose() {
        let c = CameraPose::from_desk_toml(FIXTURE_TOML).unwrap();

        for p in [DVec3::new(10.0, -20.0, 600.0), DVec3::new(-90.0, 45.0, 500.0), DVec3::ZERO] {
            let back = c.point_to_camera(c.point_to_desk(p));
            assert!((back - p).length() < 1.0e-9, "point round trip failed for {p:?}");
        }

        for d in [DVec3::Z, DVec3::new(0.2, 0.1, 0.97).normalize(), DVec3::new(-0.3, -0.2, 0.9).normalize()] {
            let back = c.dir_to_camera(c.dir_to_desk(d));
            assert!((back - d).length() < 1.0e-9, "dir round trip failed for {d:?}");
        }
    }

    #[test]
    fn gaze_angles_round_trip_and_have_the_documented_signs() {
        // Straight into the lens is the zero of both angles.
        let (yaw, pitch) = gaze_yaw_pitch_deg(-DVec3::Z).unwrap();
        assert!(yaw.abs() < 1.0e-12 && pitch.abs() < 1.0e-12);

        // Image right is positive yaw; world up (camera -Y) is positive pitch.
        let (yaw, _) = gaze_yaw_pitch_deg(DVec3::new(1.0, 0.0, -1.0)).unwrap();
        assert!((yaw - 45.0).abs() < 1.0e-9, "yaw = {yaw}");

        let (_, pitch) = gaze_yaw_pitch_deg(DVec3::new(0.0, -1.0, -1.0)).unwrap();
        assert!((pitch - 45.0).abs() < 1.0e-9, "pitch = {pitch}");

        for (y, p) in [(0.0, 0.0), (12.5, -7.25), (-33.0, 18.0), (60.0, 40.0)] {
            let d          = gaze_dir_from_yaw_pitch_deg(y, p);
            let (by, bp)   = gaze_yaw_pitch_deg(d).unwrap();

            assert!((d.length() - 1.0).abs() < 1.0e-12);
            assert!((by - y).abs() < 1.0e-9 && (bp - p).abs() < 1.0e-9, "({y}, {p}) -> ({by}, {bp})");
        }
    }

    #[test]
    fn gaze_angles_reject_a_degenerate_vector_instead_of_returning_nan() {
        assert!(gaze_yaw_pitch_deg(DVec3::ZERO).is_none());
        assert!(gaze_yaw_pitch_deg(DVec3::splat(f64::NAN)).is_none());

        // A component that has drifted outside [-1, 1] must clamp, not produce a NaN.
        let (_, pitch) = gaze_yaw_pitch_deg(DVec3::new(0.0, -1.000_000_1, 0.0)).unwrap();
        assert!(pitch.is_finite() && (pitch - 90.0).abs() < 1.0e-3, "pitch = {pitch}");
    }

    #[test]
    fn a_degenerate_direction_does_not_produce_nans() {
        let c = CameraPose::from_desk_toml(FIXTURE_TOML).unwrap();

        assert!(c.dir_to_desk(DVec3::ZERO).is_finite());
        assert!(c.dir_to_desk(DVec3::splat(f64::NAN)).is_finite());
        assert!(c.dir_to_camera(DVec3::ZERO).is_finite());
    }
}
