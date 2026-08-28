//! The calibration model: what a sweep produces, what `config/calibration.toml` stores,
//! and how a raw sidecar ray becomes a corrected point on the desk.
//!
//! # Two stages, in this order
//!
//! 1. **An angle-space polynomial in the camera frame**, applied to the gaze angles the
//!    model reports *before* the ray is built and intersected. See `crate::fit::AnglePoly`.
//! 2. **An optional per-output 2D polynomial** from the observed pixel to the intended
//!    pixel, fitted on whatever stage one leaves behind, and kept only when it earns its
//!    place on held-out targets.
//!
//! # Why stage one is not a single yaw/pitch offset any more
//!
//! It was, and on a real sweep that failed badly. An appearance model's angular gain is
//! not constant: measured on this desk it was about twice the truth straight ahead and
//! fell to roughly correct by 50 degrees off axis, with a yaw error that also depended on
//! where the target sat vertically. One global rotation cannot express a saturating curve,
//! so it split the difference: rays aimed at the middle of the ultrawide came out pointing
//! past its right edge and missed every panel. Those samples were then edge-clamped, and
//! the per-output pixel polynomial was fitted to points sitting on a bezel. The reported
//! RMS looked survivable because the pixel stage had fitted the clamped garbage; the
//! honest angle-space error of that calibration was 10.9 degrees.
//!
//! Fitting in angle space fixes the cause rather than the symptom. An angle exists whether
//! or not the ray happens to hit a screen, so nothing has to be clamped or discarded, and
//! a polynomial there can follow a saturating gain.
//!
//! # Coordinates the polynomial works in
//!
//! Per output, `nx = 2 * (px.x - logical_x) / logical_w - 1`, likewise `ny`, so the visible
//! area is `[-1, 1]^2`. Normalising keeps the quadratic's normal equations conditioned
//! (a raw 3840-pixel abscissa squared is 1.5e7 against a constant term of 1) and makes the
//! stored coefficients comparable between a 27 inch panel and an ultrawide.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use gaze_core::{DesktopGeometry, GlobalPx, OutputGeometry, Ray};
use glam::DVec3;
use serde::{Deserialize, Serialize};

use crate::camera::{CameraPose, gaze_dir_from_yaw_pitch_deg, gaze_yaw_pitch_deg};
use crate::angle::AngleCorrection;
use crate::fit::PolyMap;

/// Bisection steps used by the edge clamp. Sixteen halvings of the arc between the panel
/// centre and the missing ray put the crossing well inside a pixel on any of these panels.
const EDGE_BISECT_STEPS: u32 = 16;

/// Largest angle between a ray and a panel centre for which the edge clamp will report a
/// crossing. Past a right angle the gaze is pointing away from that panel's hemisphere and
/// the "edge it left through" stops meaning anything: the user has turned around, and a
/// point on a bezel is a worse answer than no point at all.
const MAX_CLAMP_DEG: f64 = 90.0;

/// How far inside an output's logical rectangle `snap_to_desk` places a point. The rect is
/// half open (`contains_px` uses `<` on the far edge), so clamping to the exact maximum
/// lands on a pixel no output owns.
const SNAP_INSET_PX: f64 = 0.5;

/// Current calibration file format.
///
/// Version 1 stored a single global yaw/pitch offset as stage one. Version 2 replaced it
/// with a polynomial; version 3 makes stage one a tagged choice between a polynomial and a
/// thin-plate spline. An older file loaded by a newer build would silently run with no
/// angular correction at all, so it is refused instead.
pub const CALIBRATION_FORMAT: u32 = 3;

/// A fitted calibration, as stored in `config/calibration.toml`.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Calibration {
    /// File format version. Bumped when the meaning of a field changes, so an older file
    /// is refused rather than silently misread.
    #[serde(default = "default_format")]
    pub format         : u32,
    /// **The headline number.** Root-mean-square angular residual over targets the fit did
    /// not see, by leave-one-target-out. This is what the calibration will actually do on
    /// a point the user looks at next, which the in-sample figure does not measure: a
    /// ten-coefficient model over thirty targets can drive its in-sample error to almost
    /// nothing and still be useless.
    #[serde(default)]
    pub rms_loo_deg    : f64,
    /// In-sample root-mean-square angular residual. Always the smaller of the two, and the
    /// gap between them is how much the fit is chasing noise.
    pub rms_deg        : f64,
    /// The same residual in global logical pixels. Panel-dependent, so it is the weaker of
    /// the two figures, but it is the one that is easy to picture.
    pub rms_px         : f64,
    /// Provider sigma in force when the sweep ran. Informational: a calibration fitted
    /// against one sigma is still valid at another.
    #[serde(default)]
    pub sigma_deg      : f64,
    /// Wall-clock seconds since the Unix epoch when the fit was made, so a stale
    /// calibration is recognisable without stat-ing the file.
    #[serde(default)]
    pub created_unix_s : f64,
    /// Free text recorded with the fit, empty when there is nothing to say.
    ///
    /// This is where a calibration that was written despite failing its acceptance gate
    /// says so. A file is the only thing a later run sees, and "this was the best of a bad
    /// sweep" is not something to leave in a terminal someone has since closed.
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub note           : String,
    /// Stage one: the angle-space correction.
    #[serde(default)]
    pub angle          : AngleCorrection,
    /// Stage two, one entry per output that got any usable targets.
    #[serde(default)]
    pub outputs        : Vec<OutputCalibration>,
    /// How faithfully the model tracked movement across each output, measured before any
    /// fitting. The first number to look at when a sweep comes back bad.
    #[serde(default)]
    pub gains          : Vec<OutputGain>,
    /// Per-target record of what the sweep saw and what the fit left behind. Kept in the
    /// file because a calibration with one terrible target is a very different thing from
    /// one that is uniformly mediocre, and the RMS alone cannot tell them apart.
    #[serde(default)]
    pub targets        : Vec<TargetResidual>,
}

/// Stage two for one output.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputCalibration {
    /// Connector name, matched against `OutputGeometry::name`.
    pub name   : String,
    /// How many targets the fit had on this output. Determines the degree.
    pub points : usize,
    /// The fitted map, in the output's normalised coordinates.
    pub map    : PolyMap,
}

/// How well the model's reported gaze tracked real movement across one output, from a
/// straight least-squares slope of observed against intended.
///
/// A gain near 1 means the model moves the right amount and only its offset and shape are
/// wrong, which is what the polynomial is for. A gain well below 1 means it under-reports
/// how far the eye has turned, and no amount of curve fitting will fix that: the fit ends
/// up inverting a 2 or 3 times compression, which amplifies the model's noise by the same
/// factor and extrapolates violently outside the target grid.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OutputGain {
    pub name          : String,
    pub points        : usize,
    /// Slope of observed against target x, in global logical pixels per pixel. `None` when
    /// the targets on this output do not span enough x to measure a slope.
    #[serde(default)]
    pub gain_px_x     : Option<f64>,
    #[serde(default)]
    pub gain_px_y     : Option<f64>,
    /// Slope of the raw sidecar yaw against the yaw the target called for, camera frame,
    /// degrees per degree. This is the honest one: it is measured before the desk geometry
    /// gets involved, so a bad camera pose cannot flatter or spoil it.
    #[serde(default)]
    pub gain_deg_yaw  : Option<f64>,
    #[serde(default)]
    pub gain_deg_pitch: Option<f64>,
}

/// Angle-space diagnostics for one target, all measured on the raw sidecar stream before
/// any correction. Recorded whether or not the fit succeeds, because when a sweep comes
/// back bad these are the numbers that say why.
#[derive(Clone, Copy, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct TargetDiagnostics {
    /// Mean and standard deviation of the raw camera-frame gaze yaw over the collection
    /// window (see `crate::camera::gaze_yaw_pitch_deg`). The sd is per-sample model
    /// jitter, in the units the model actually works in.
    pub yaw_deg_mean     : f64,
    pub yaw_deg_sd       : f64,
    pub pitch_deg_mean   : f64,
    pub pitch_deg_sd     : f64,
    /// Camera-frame yaw and pitch the target called for, given where the sidecar says the
    /// eye is. Against the means above this is the per-target gain and offset.
    pub target_yaw_deg   : f64,
    pub target_pitch_deg : f64,
    /// Mean of the sidecar's `head_rot`, carried through uninterpreted.
    pub head_rot_mean    : [f64; 3],
    /// Mean eye position, camera frame, millimetres. A jumpy one across targets means the
    /// user moved during the sweep, which invalidates the whole thing.
    pub eye_mm_mean      : [f64; 3],
    pub conf_mean        : f64,
    /// Fraction of the readings in the window the sidecar called valid. Well below 1 means
    /// the model was losing the face, not that it was aiming badly.
    pub valid_fraction   : f64,
    /// Valid samples whose raw ray hit no panel at all.
    ///
    /// These are recorded and then left alone. The previous design clamped them to the
    /// nearest bezel and fitted the pixel stage to the result, which is how a calibration
    /// with 45 of 45 samples off the screen still produced confident-looking coefficients.
    /// A high count here means the model is aiming far enough wrong to leave the desk, and
    /// only the angle stage can do anything about that.
    pub missed           : usize,
    /// Readings seen in the window, valid or not.
    pub samples_seen     : usize,
}

/// What one calibration target contributed, and what the fit could not remove.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct TargetResidual {
    /// Output the target was shown on.
    pub output       : String,
    /// Where the target was drawn, global logical pixels.
    pub target_px    : [f64; 2],
    /// Mean uncorrected landing point over the collection window, if the raw rays hit a
    /// panel at all. Never a clamped point.
    #[serde(default)]
    pub observed_px  : Option<[f64; 2]>,
    /// RMS distance of the landing points from their mean, global pixels, over the samples
    /// that landed on a panel at all. `None` when none of them did.
    #[serde(default)]
    pub spread_px    : Option<f64>,
    /// RMS angle between the individual reported gaze directions and their mean. Always
    /// available, and the one that is comparable between panels: 100 pixels is 1.8 degrees
    /// on the ultrawide and 1.4 on the small panel, so a pixel figure alone cannot separate
    /// model jitter from geometry error, which is the whole reason to record a spread.
    #[serde(default)]
    pub spread_deg   : f64,
    /// Distance from the fully corrected landing point to the target, global pixels.
    pub residual_px  : f64,
    /// The same residual as a visual angle from the nominal eye.
    pub residual_deg : f64,
    /// Samples that went into the mean.
    pub samples      : usize,
    /// Angle-space diagnostics for this target, measured before any fitting.
    #[serde(default)]
    pub diagnostics  : TargetDiagnostics,
}

/// Outcome of pushing one ray through the geometry: the corrected ray, where it landed,
/// and how honestly it got there.
#[derive(Clone, Debug, PartialEq)]
pub struct Resolved {
    /// The ray after stage one. Always the honest direction the provider believes in, even
    /// when the point had to be clamped.
    pub ray     : Ray,
    /// Landing point after both stages, or `None` when the ray missed the desk and either
    /// clamping was off or no edge crossing could be found.
    pub point   : Option<GlobalPx>,
    /// Output the intersection landed on, which is the one whose polynomial was applied.
    pub output  : Option<String>,
    /// True when the corrected ray hit nothing. Independent of `clamped`, so a caller that
    /// is not clamping can still count how bad things are.
    pub missed  : bool,
    /// True when the ray missed every panel and `point` is an edge crossing rather than a
    /// real intersection.
    pub clamped : bool,
}

// --- Calibration ---

impl Calibration {
    /// A calibration that changes nothing. What the provider uses when no file was given.
    pub fn identity() -> Self {
        Self {
            format         : CALIBRATION_FORMAT,
            rms_loo_deg    : 0.0,
            rms_deg        : 0.0,
            rms_px         : 0.0,
            sigma_deg      : 0.0,
            created_unix_s : 0.0,
            note           : String::new(),
            angle          : AngleCorrection::identity(),
            outputs        : Vec::new(),
            gains          : Vec::new(),
            targets        : Vec::new(),
        }
    }

    /// Loads a `calibration.toml`.
    pub fn load(path: impl AsRef<Path>) -> Result<Self, CalibrationError> {
        __load(path.as_ref())
    }

    /// Writes a `calibration.toml`, creating parent directories as needed.
    pub fn save(&self, path: impl AsRef<Path>) -> Result<(), CalibrationError> {
        __save(self, path.as_ref())
    }

    /// Seconds since the Unix epoch, for stamping a freshly fitted calibration.
    pub fn now_unix_s() -> f64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs_f64())
            .unwrap_or(0.0)
    }

    /// Stage one applied to a camera-frame gaze direction.
    pub fn correct_dir_camera(&self, gaze_cam: DVec3) -> DVec3 {
        if !self.angle.is_well_formed() {
            return gaze_cam;
        }

        let Some((yaw, pitch)) = gaze_yaw_pitch_deg(gaze_cam) else {
            return gaze_cam;
        };

        let (yaw, pitch) = self.angle.apply(yaw, pitch);

        if !yaw.is_finite() || !pitch.is_finite() {
            return gaze_cam;
        }

        gaze_dir_from_yaw_pitch_deg(yaw, pitch)
    }

    /// The stage-two map for an output, or `None` when the sweep placed no targets there.
    pub fn map_for(&self, output: &str) -> Option<&PolyMap> {
        self.outputs
            .iter()
            .find(|o| o.name == output)
            .map(|o| &o.map)
            .filter(|m| m.is_well_formed())
    }

    /// Applies stage one to a desk-frame ray, by way of the camera frame the correction is
    /// defined in. The round trip through the camera pose is exact.
    pub fn correct_ray(&self, camera: &CameraPose, ray: &Ray) -> Ray {
        let corrected = self.correct_dir_camera(camera.dir_to_camera(ray.dir));

        Ray { origin: ray.origin, dir: camera.dir_to_desk(corrected) }
    }

    /// Applies stage two: the polynomial for the output `p` landed on. Points on an output
    /// with no fitted map come back unchanged.
    pub fn correct_point(&self, output: &OutputGeometry, p: GlobalPx) -> GlobalPx {
        let Some(map) = self.map_for(&output.name) else {
            return p;
        };

        let (nx, ny) = normalise(output, p);
        let (cx, cy) = map.apply(nx, ny);

        denormalise(output, cx, cy)
    }
}

/// Pushes one raw ray all the way through: stage one, intersection, then stage two.
///
/// `clamp` decides what happens when the corrected ray misses every panel. At run time it
/// is on, and the ray reports where it left the desk rather than vanishing. During
/// **calibration it must be off**: a clamped point sits on a bezel, and fitting a pixel
/// map to bezel points is how a calibration comes back with a plausible RMS and useless
/// coefficients. `Resolved::missed` is set either way, so a caller that is not clamping can
/// still count how often it happened.
///
/// `calibration` may be `None`, in which case this is a plain intersect and the result is
/// the *uncorrected* landing point.
pub fn resolve(
    geometry    : &DesktopGeometry,
    camera      : &CameraPose,
    calibration : Option<&Calibration>,
    ray         : &Ray,
    clamp       : bool,
)
    -> Resolved
{
    let corrected = calibration.map_or(*ray, |c| c.correct_ray(camera, ray));

    let (hit_px, output_name, missed, clamped) = {
        match geometry.intersect(&corrected) {
            Some(hit) => (Some(hit.px), Some(hit.output), false, false),

            None if clamp => {
                let edge = edge_point(geometry, &corrected);
                let name = edge.and_then(|p| geometry.output_at(p)).map(|o| o.name.clone());

                (edge, name, true, edge.is_some())
            }

            None => (None, None, true, false),
        }
    };

    let point = {
        match (hit_px, output_name.as_deref(), calibration) {
            (Some(p), Some(name), Some(cal)) => {
                let corrected_px = geometry
                    .outputs
                    .iter()
                    .find(|o| o.name == name)
                    .map_or(p, |o| cal.correct_point(o, p));

                // The polynomial can legitimately push a point over a seam, but it must
                // not push it off the desk: a snap engine given an off-desk point has
                // nothing to search.
                Some(snap_to_desk(geometry, corrected_px))
            }

            (p, _, _) => p,
        }
    };

    Resolved {
        ray     : corrected,
        point   : point,
        output  : output_name,
        missed  : missed,
        clamped : clamped,
    }
}

/// Where a ray that misses every panel crosses the edge of the desk, if it can be found.
///
/// The ray is swung back toward the centre of the panel it points closest to; the
/// crossing is bisected along that arc. The result is a point on a panel edge in the
/// direction the gaze went, which is all the clamp claims to be. `None` when there are no
/// enabled outputs or when even the panel centres are unreachable from the ray's origin
/// (the user has turned right around).
pub fn edge_point(geometry: &DesktopGeometry, ray: &Ray) -> Option<GlobalPx> {
    // Order the panels by how far the ray is from their centres, so the clamp lands on
    // the panel the gaze was closest to leaving through.
    let mut candidates: Vec<(f64, DVec3)> = geometry
        .outputs
        .iter()
        .filter(|o| o.enabled)
        .map(|o| {
            let centre = o.uv_to_world(0.5, 0.5) - ray.origin;

            (centre.angle_between(ray.dir), centre)
        })
        .filter(|(angle, _)| angle.is_finite())
        .collect();

    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    for (angle, centre) in candidates {
        if angle.to_degrees() > MAX_CLAMP_DEG {
            break;
        }

        let anchor = Ray { origin: ray.origin, dir: centre.normalize() };

        // The anchor must hit for the bisection to have a bracket. A panel whose centre is
        // occluded by another panel from this origin does not, so try the next one.
        if geometry.intersect(&anchor).is_none() {
            continue;
        }

        let mut lo   = 0.0_f64;
        let mut hi   = 1.0_f64;
        let mut best = None;

        for _ in 0..EDGE_BISECT_STEPS {
            let mid   = 0.5 * (lo + hi);
            let probe = Ray { origin: ray.origin, dir: slerp_dir(anchor.dir, ray.dir, mid) };

            match geometry.intersect(&probe) {
                Some(hit) => {
                    best = Some(hit.px);
                    lo   = mid;
                }

                None => hi = mid,
            }
        }

        if best.is_some() {
            return best;
        }
    }

    None
}

/// Normalised `[-1, 1]` coordinates for a global pixel on `output`.
pub fn normalise(output: &OutputGeometry, p: GlobalPx) -> (f64, f64) {
    let (u, v) = output.px_to_uv(p);

    (2.0 * u - 1.0, 2.0 * v - 1.0)
}

/// Inverse of `normalise`.
pub fn denormalise(output: &OutputGeometry, nx: f64, ny: f64) -> GlobalPx {
    output.uv_to_px(0.5 * (nx + 1.0), 0.5 * (ny + 1.0))
}

/// Moves a point onto the nearest enabled output's logical rectangle when it has fallen
/// into a gap between panels or off the edge of the desk. A point already on an output is
/// returned untouched.
pub fn snap_to_desk(geometry: &DesktopGeometry, p: GlobalPx) -> GlobalPx {
    if geometry.output_at(p).is_some() {
        return p;
    }

    let mut best: Option<(f64, GlobalPx)> = None;

    for out in geometry.outputs.iter().filter(|o| o.enabled) {
        // Inset so the clamped point is inside the half-open rectangle rather than on the
        // far edge, which belongs to nobody.
        let rect    = gaze_core::Rect {
            x : out.logical_x,
            y : out.logical_y,
            w : (out.logical_w - SNAP_INSET_PX).max(0.0),
            h : (out.logical_h - SNAP_INSET_PX).max(0.0),
        };
        let near    = rect.clamp(p);
        let dist_sq = (near.x - p.x).powi(2) + (near.y - p.y).powi(2);

        if best.as_ref().is_none_or(|(d, _)| dist_sq < *d) {
            best = Some((dist_sq, near));
        }
    }

    best.map_or(p, |(_, near)| near)
}

/// Interpolates between two unit directions along their great circle. Normalised linear
/// interpolation is enough: the bisection only needs the path to be continuous and
/// monotone in `t`, not to be at constant angular speed.
fn slerp_dir(a: DVec3, b: DVec3, t: f64) -> DVec3 {
    let mixed = a.lerp(b, t);

    if mixed.length_squared() <= 1.0e-18 {
        return a;
    }

    mixed.normalize()
}

/// Non-generic body of `Calibration::load`.
fn __load(path: &Path) -> Result<Calibration, CalibrationError> {
    let text = fs::read_to_string(path)
        .map_err(|source| CalibrationError::Io { path: path.to_path_buf(), source })?;

    // Check the version before deserialising the rest. A file from an older format will
    // usually also fail to parse, and "TOML parse error at line 122" is a much worse thing
    // to hand someone than "this file is format 1, re-run calibrate".
    #[derive(Deserialize)]
    struct Stamp {
        #[serde(default = "default_format")]
        format : u32,
    }

    let stamp: Stamp = toml::from_str(&text)
        .map_err(|e| CalibrationError::Parse { path: path.to_path_buf(), reason: e.to_string() })?;

    if stamp.format != CALIBRATION_FORMAT {
        return Err(CalibrationError::Version {
            path  : path.to_path_buf(),
            found : stamp.format,
            want  : CALIBRATION_FORMAT,
        });
    }

    let cal: Calibration = toml::from_str(&text)
        .map_err(|e| CalibrationError::Parse { path: path.to_path_buf(), reason: e.to_string() })?;

    Ok(cal)
}

fn default_format() -> u32 {
    1
}

/// Non-generic body of `Calibration::save`.
fn __save(cal: &Calibration, path: &Path) -> Result<(), CalibrationError> {
    if let Some(parent) = path.parent().filter(|p| !p.as_os_str().is_empty()) {
        fs::create_dir_all(parent)
            .map_err(|source| CalibrationError::Io { path: parent.to_path_buf(), source })?;
    }

    let body = toml::to_string_pretty(cal)
        .map_err(|e| CalibrationError::Encode(e.to_string()))?;

    let text = format!(
        "# gaze-provider-webcam calibration. Generated by `gaze-webcam-cli calibrate`;\n\
         # re-run that rather than editing by hand. Coefficients are in each output's\n\
         # normalised [-1, 1] coordinates (see crates/gaze-provider-webcam/src/calibration.rs).\n\
         {body}"
    );

    fs::write(path, text)
        .map_err(|source| CalibrationError::Io { path: path.to_path_buf(), source })
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum CalibrationError {
    #[error("cannot read or write calibration at {}: {source}", path.display())]
    Io { path: PathBuf, #[source] source: std::io::Error },

    #[error("calibration file {} is malformed: {reason}", path.display())]
    Parse { path: PathBuf, reason: String },

    #[error(
        "calibration file {} is format {found}, this build writes {want}; re-run \
         `gaze-webcam-cli calibrate`",
        path.display(),
    )]
    Version { path: PathBuf, found: u32, want: u32 },

    #[error("cannot encode calibration: {0}")]
    Encode(String),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::Ray;

    use super::*;
    use crate::angle::AngleCorrection;
    use crate::fit::{AngleDegree, AnglePoly, PolyDegree};

    /// The frozen 2026-08-25 desk snapshot these assertions were written against;
    /// the live `config/desk.toml` drifts with the physical desk.
    const FIXTURE_TOML: &str = include_str!("../../../config/desk-fixture.toml");

    fn desk() -> DesktopGeometry {
        DesktopGeometry::from_toml(FIXTURE_TOML).unwrap()
    }

    fn cam() -> CameraPose {
        CameraPose::from_desk_toml(FIXTURE_TOML).unwrap()
    }

    #[test]
    fn live_desk_config_parses_a_camera_pose() {
        let live = include_str!("../../../config/desk.toml");

        CameraPose::from_desk_toml(live).expect("config/desk.toml camera block must parse");
    }

    /// A calibration whose only content is a per-output shift of half the panel, so the
    /// stage-two path is obvious in the numbers.
    fn shifting_calibration(output: &str) -> Calibration {
        Calibration {
            outputs : vec![OutputCalibration {
                name   : output.to_string(),
                points : 9,
                map    : PolyMap {
                    degree   : PolyDegree::Affine,
                    x_coeffs : vec![0.5, 1.0, 0.0],
                    y_coeffs : vec![0.0, 0.0, 1.0],
                },
            }],
            ..Calibration::identity()
        }
    }

    /// A stage one that adds a constant to both angles, in the camera frame.
    fn offsetting_calibration(yaw_deg: f64, pitch_deg: f64) -> Calibration {
        let scale = crate::fit::ANGLE_SCALE_DEG;

        Calibration {
            angle : AngleCorrection::Poly(AnglePoly {
                degree          : AngleDegree::Quadratic,
                yaw_coeffs      : vec![yaw_deg / scale, 1.0, 0.0, 0.0, 0.0, 0.0],
                pitch_coeffs    : vec![pitch_deg / scale, 0.0, 1.0, 0.0, 0.0, 0.0],
                input_scale_deg : scale,
            }),
            ..Calibration::identity()
        }
    }

    #[test]
    fn normalising_a_pixel_round_trips() {
        let g   = desk();
        let out = &g.outputs[0];

        for p in [
            GlobalPx { x: 2559.0, y: 0.0 },
            GlobalPx { x: 6398.0, y: 1599.0 },
            GlobalPx { x: 4479.0, y: 800.0 },
        ] {
            let (nx, ny) = normalise(out, p);
            let back     = denormalise(out, nx, ny);

            assert!((back.x - p.x).abs() < 1.0e-9 && (back.y - p.y).abs() < 1.0e-9);
        }

        let (nx, ny) = normalise(out, GlobalPx { x: 2559.0 + 1920.0, y: 800.0 });
        assert!(nx.abs() < 1.0e-12 && ny.abs() < 1.0e-12);
    }

    #[test]
    fn an_identity_calibration_leaves_a_ray_and_a_point_alone() {
        let g   = desk();
        let c   = cam();
        let cal = Calibration::identity();
        let ray = g.px_to_ray(GlobalPx { x: 4479.0, y: 800.0 }).unwrap();

        // The identity angle polynomial goes out through the camera frame and back, so
        // this also proves that round trip is lossless.
        assert!(cal.correct_ray(&c, &ray).dir.angle_between(ray.dir).to_degrees() < 1.0e-9);

        let plain    = resolve(&g, &c, None, &ray, true);
        let with_cal = resolve(&g, &c, Some(&cal), &ray, true);

        let a = plain.point.unwrap();
        let b = with_cal.point.unwrap();

        assert!((a.x - b.x).abs() < 0.01 && (a.y - b.y).abs() < 0.01);
        assert!(!plain.clamped && !plain.missed);
    }

    #[test]
    fn stage_two_moves_the_point_by_the_fitted_amount() {
        let g   = desk();
        let c   = cam();
        let cal = shifting_calibration("DP-1");
        let p   = GlobalPx { x: 4479.0, y: 800.0 };
        let ray = g.px_to_ray(p).unwrap();

        let out = resolve(&g, &c, Some(&cal), &ray, true).point.unwrap();

        // +0.5 in normalised x is a quarter of the panel's 3840 logical pixels.
        assert!((out.x - (p.x + 960.0)).abs() < 0.05, "landed at {out:?}");
        assert!((out.y - p.y).abs() < 0.05);
    }

    #[test]
    fn stage_two_is_skipped_for_an_output_with_no_fitted_map() {
        let g   = desk();
        let c   = cam();
        let cal = shifting_calibration("DP-1");
        let p   = GlobalPx { x: 1200.0, y: 700.0 };
        let ray = g.px_to_ray(p).unwrap();

        let out = resolve(&g, &c, Some(&cal), &ray, true).point.unwrap();

        assert!((out.x - p.x).abs() < 0.05 && (out.y - p.y).abs() < 0.05);
    }

    #[test]
    fn stage_one_rescues_a_ray_that_would_have_missed_the_desk() {
        let g = desk();
        let c = cam();

        // Aim a few degrees past the top of the tall panel so the raw ray misses.
        let on_panel = g.px_to_ray(GlobalPx { x: 4479.0, y: 20.0 }).unwrap();
        let missing  = DesktopGeometry::perturb_ray(&on_panel, 0.0, 4.0);

        assert!(g.intersect(&missing).is_none(), "the test ray must actually miss");

        let uncorrected = resolve(&g, &c, None, &missing, true);
        assert!(uncorrected.missed, "the raw ray misses");
        assert!(uncorrected.clamped, "with clamping on it must still report a point");
        assert!(uncorrected.point.is_some());

        // Stage one works in the camera frame, so the correction that undoes a desk-frame
        // pitch is not exactly a pitch there. Find it by asking what the camera frame does.
        let want = crate::camera::gaze_yaw_pitch_deg(c.dir_to_camera(on_panel.dir)).unwrap();
        let got  = crate::camera::gaze_yaw_pitch_deg(c.dir_to_camera(missing.dir)).unwrap();

        let cal       = offsetting_calibration(want.0 - got.0, want.1 - got.1);
        let corrected = resolve(&g, &c, Some(&cal), &missing, true);

        assert!(!corrected.missed, "stage one should put it back on the panel");
        assert!(!corrected.clamped);

        let point = corrected.point.unwrap();
        assert!((point.y - 20.0).abs() < 1.0, "landed at {point:?}");
    }

    #[test]
    fn a_miss_is_reported_and_not_clamped_when_clamping_is_off() {
        let g = desk();
        let c = cam();

        let on_panel = g.px_to_ray(GlobalPx { x: 4479.0, y: 20.0 }).unwrap();
        let missing  = DesktopGeometry::perturb_ray(&on_panel, 0.0, 4.0);

        // This is the mode the calibration sweep runs in. A clamped point sits on a bezel,
        // and fitting a pixel map to bezel points is what made the previous design fail.
        let out = resolve(&g, &c, None, &missing, false);

        assert!(out.missed, "the miss must still be reported");
        assert!(!out.clamped, "nothing may be clamped with clamping off");
        assert_eq!(out.point, None);
        assert_eq!(out.output, None);
    }

    #[test]
    fn the_edge_clamp_lands_on_a_panel_in_the_direction_the_gaze_went() {
        let g = desk();

        let on_panel = g.px_to_ray(GlobalPx { x: 2600.0, y: 30.0 }).unwrap();
        let missing  = DesktopGeometry::perturb_ray(&on_panel, 0.0, 6.0);

        assert!(g.intersect(&missing).is_none());

        let clamped = edge_point(&g, &missing).expect("the clamp must find an edge");
        let output  = g.output_at(clamped).expect("clamped off the desk");

        assert!(
            clamped.y - output.logical_y < 40.0,
            "clamped at {clamped:?} on {}, not near its top edge",
            output.name,
        );
    }

    #[test]
    fn a_ray_pointing_away_from_the_desk_has_no_edge_and_no_point() {
        let g = desk();
        let c = cam();

        let ray = Ray { origin: g.eye(), dir: DVec3::new(0.0, 0.0, 1.0) };

        assert!(edge_point(&g, &ray).is_none());
        assert!(resolve(&g, &c, None, &ray, true).point.is_none());
    }

    #[test]
    fn a_point_in_the_gap_between_panels_is_snapped_onto_the_nearest_one() {
        let g = desk();

        let gap = GlobalPx { x: 3300.0, y: 2000.0 };
        assert!(g.output_at(gap).is_none(), "the test point must start off-desk");

        let snapped = snap_to_desk(&g, gap);
        assert!(g.output_at(snapped).is_some(), "snapped to {snapped:?}");

        let on = GlobalPx { x: 4479.0, y: 800.0 };
        assert_eq!(snap_to_desk(&g, on), on);
    }

    #[test]
    fn a_calibration_round_trips_through_toml() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("nested").join("calibration.toml");

        let cal = Calibration {
            format         : CALIBRATION_FORMAT,
            rms_loo_deg    : 0.62,
            rms_deg        : 0.41,
            rms_px         : 24.6,
            sigma_deg      : 2.5,
            created_unix_s : 1_800_000_000.0,
            note           : String::new(),
            angle          : AngleCorrection::Poly(AnglePoly {
                degree          : AngleDegree::Cubic,
                yaw_coeffs      : vec![0.01, 1.02, 0.0, 0.003, 0.0, -0.004, 0.0, 0.0, 0.0, 0.001],
                pitch_coeffs    : vec![-0.02, 0.0, 0.98, 0.0, 0.002, 0.001, 0.0, 0.0, 0.0, 0.0],
                input_scale_deg : 45.0,
            }),
            outputs        : vec![OutputCalibration {
                name   : "DP-1".to_string(),
                points : 9,
                map    : PolyMap {
                    degree   : PolyDegree::Quadratic,
                    x_coeffs : vec![0.01, 1.02, 0.0, 0.003, 0.0, -0.004],
                    y_coeffs : vec![-0.02, 0.0, 0.98, 0.0, 0.002, 0.001],
                },
            }],
            gains          : vec![OutputGain {
                name           : "DP-1".to_string(),
                points         : 9,
                gain_px_x      : Some(0.98),
                gain_px_y      : Some(1.01),
                gain_deg_yaw   : Some(0.97),
                gain_deg_pitch : None,
            }],
            targets        : vec![TargetResidual {
                output       : "DP-1".to_string(),
                target_px    : [3000.0, 400.0],
                observed_px  : Some([3120.0, 366.0]),
                spread_px    : Some(18.2),
                spread_deg   : 0.33,
                residual_px  : 9.4,
                residual_deg : 0.17,
                samples      : 30,
                diagnostics  : TargetDiagnostics {
                    yaw_deg_mean   : -4.5,
                    yaw_deg_sd     : 0.42,
                    pitch_deg_mean : 2.1,
                    pitch_deg_sd   : 0.38,
                    valid_fraction : 1.0,
                    samples_seen   : 30,
                    ..TargetDiagnostics::default()
                },
            }],
        };

        cal.save(&path).unwrap();

        let back = Calibration::load(&path).unwrap();
        assert_eq!(back, cal);

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.starts_with("# gaze-provider-webcam calibration"));
        assert!(text.contains("degree = \"cubic\""));
    }

    #[test]
    fn a_spline_calibration_round_trips_through_toml() {
        use crate::angle::TpsWarp;

        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("calibration.toml");

        // The tagged enum has to survive TOML in both directions, or a spline calibration
        // writes fine and comes back as something else.
        let cal = Calibration {
            rms_loo_deg : 2.41,
            rms_deg     : 1.55,
            angle       : AngleCorrection::Tps(TpsWarp {
                centres         : vec![[-0.5, 0.1], [0.0, 0.2], [0.5, -0.1], [0.2, 0.4]],
                yaw_weights     : vec![0.01, -0.02, 0.03, -0.01],
                yaw_affine      : [0.02, 0.9, 0.05],
                pitch_weights   : vec![-0.01, 0.02, 0.0, 0.01],
                pitch_affine    : [-0.01, 0.0, 1.05],
                input_scale_deg : 45.0,
                lambda          : 0.1,
                range_deg       : [-30.0, 25.0, -12.0, 20.0],
            }),
            ..Calibration::identity()
        };

        cal.save(&path).unwrap();

        let back = Calibration::load(&path).unwrap();
        assert_eq!(back, cal);

        let text = fs::read_to_string(&path).unwrap();
        assert!(text.contains("kind = \"tps\""), "the variant tag must be in the file");

        // And it must still correct the same way after the round trip.
        for (yaw, pitch) in [(0.0, 0.0), (-20.0, 5.0), (15.0, -8.0)] {
            let a = cal.angle.apply(yaw, pitch);
            let b = back.angle.apply(yaw, pitch);

            assert!((a.0 - b.0).abs() < 1.0e-12 && (a.1 - b.1).abs() < 1.0e-12);
        }
    }

    #[test]
    fn a_malformed_spline_is_ignored_rather_than_applied() {
        use crate::angle::TpsWarp;

        // Weights that do not match the centres: hand-edited, or a truncated write.
        let cal = Calibration {
            angle : AngleCorrection::Tps(TpsWarp {
                centres         : vec![[0.0, 0.0], [0.1, 0.1], [0.2, 0.2], [0.3, 0.3]],
                yaw_weights     : vec![0.1],
                yaw_affine      : [0.0, 1.0, 0.0],
                pitch_weights   : vec![0.1],
                pitch_affine    : [0.0, 0.0, 1.0],
                input_scale_deg : 45.0,
                lambda          : 0.1,
                range_deg       : [-30.0, 30.0, -20.0, 20.0],
            }),
            ..Calibration::identity()
        };

        assert!(!cal.angle.is_well_formed());

        let dir = crate::camera::gaze_dir_from_yaw_pitch_deg(12.0, -5.0);
        assert_eq!(cal.correct_dir_camera(dir), dir);
    }

    #[test]
    fn a_target_that_never_reached_a_panel_records_no_observed_point() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("calibration.toml");

        let cal = Calibration {
            targets : vec![TargetResidual {
                output       : "DP-1".to_string(),
                target_px    : [3000.0, 400.0],
                observed_px  : None,
                spread_px    : None,
                spread_deg   : 2.4,
                residual_px  : f64::NAN,
                residual_deg : 5.1,
                samples      : 45,
                diagnostics  : TargetDiagnostics { missed: 45, ..TargetDiagnostics::default() },
            }],
            ..Calibration::identity()
        };

        cal.save(&path).unwrap();

        let back = Calibration::load(&path).unwrap();
        assert_eq!(back.targets[0].observed_px, None);
        assert_eq!(back.targets[0].spread_px, None);
        assert_eq!(back.targets[0].diagnostics.missed, 45);
    }

    #[test]
    fn a_missing_or_malformed_calibration_file_is_an_error_not_a_panic() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("calibration.toml");

        assert!(matches!(Calibration::load(&path), Err(CalibrationError::Io { .. })));

        fs::write(&path, "this is not toml = = =").unwrap();
        assert!(matches!(Calibration::load(&path), Err(CalibrationError::Parse { .. })));
    }

    #[test]
    fn a_file_from_a_previous_format_is_refused_rather_than_silently_ignored() {
        let dir  = tempfile::tempdir().unwrap();
        let path = dir.path().join("old.toml");

        // Format 1 kept stage one under `[angular]` as a single rotation. Serde would skip
        // the unknown table and leave the angle polynomial at identity, which looks like a
        // working calibration and is not one.
        fs::write(&path, "rms_deg = 0.4\nrms_px = 20.0\n\n[angular]\nyaw_deg = 1.0\npitch_deg = -0.5\n").unwrap();

        let err = Calibration::load(&path).unwrap_err();
        assert!(matches!(err, CalibrationError::Version { found: 1, want: 3, .. }), "{err}");
    }

    #[test]
    fn a_coefficient_vector_of_the_wrong_length_is_ignored_rather_than_applied() {
        let g = desk();
        let c = cam();

        let cal = Calibration {
            outputs : vec![OutputCalibration {
                name   : "DP-1".to_string(),
                points : 9,
                map    : PolyMap {
                    degree   : PolyDegree::Quadratic,
                    x_coeffs : vec![0.5, 1.0, 0.0],
                    y_coeffs : vec![0.0, 0.0, 1.0],
                },
            }],
            ..Calibration::identity()
        };

        assert!(cal.map_for("DP-1").is_none());

        let p     = GlobalPx { x: 4479.0, y: 800.0 };
        let ray   = g.px_to_ray(p).unwrap();
        let point = resolve(&g, &c, Some(&cal), &ray, true).point.unwrap();

        assert!((point.x - p.x).abs() < 0.05 && (point.y - p.y).abs() < 0.05);
    }

    #[test]
    fn a_malformed_angle_polynomial_is_ignored_rather_than_applied() {
        let c = cam();

        let cal = Calibration {
            angle : AngleCorrection::Poly(AnglePoly {
                degree          : AngleDegree::Cubic,
                yaw_coeffs      : vec![0.0, 1.0, 0.0],
                pitch_coeffs    : vec![0.0, 0.0, 1.0],
                input_scale_deg : 45.0,
            }),
            ..Calibration::identity()
        };

        assert!(!cal.angle.is_well_formed());

        let dir = crate::camera::gaze_dir_from_yaw_pitch_deg(12.0, -5.0);
        assert_eq!(cal.correct_dir_camera(dir), dir);

        let ray = Ray { origin: DVec3::ZERO, dir: c.dir_to_desk(dir) };
        assert!(cal.correct_ray(&c, &ray).dir.angle_between(ray.dir).to_degrees() < 1.0e-9);
    }
}
