//! Replaying a saved sweep through the real provider path, to prove that what the system
//! applies at run time is what the fitter evaluated at fit time.
//!
//! These are two different pieces of code reaching the same place by different routes. The
//! fitter works in camera-frame angles and scores with `resolve`; the provider parses a
//! socket line, lifts it into the desk frame, corrects it and intersects. If they ever
//! disagree, every number in `calibration.toml` describes a system other than the one
//! running, the marker lands somewhere the report says it does not, and nothing about the
//! symptom points at the cause. That is worth a permanent check rather than a one-off.
//!
//! The one place the two legitimately differ is the edge clamp: at run time a ray that
//! misses the desk reports where it left, and the fitter deliberately does not clamp (see
//! `crate::calibration::resolve`). Samples that missed are therefore scored on their ray
//! direction here, which is the quantity both sides agree on, and counted separately.

use gaze_core::{DesktopGeometry, GlobalPx, Ray, SigmaProfile};
use glam::DVec3;

use crate::calibration::Calibration;
use crate::camera::{CameraPose, gaze_yaw_pitch_deg};
use crate::protocol::{ProtocolError, SidecarGaze, SidecarMessage};
use crate::provider::sample_from_line;
use crate::sweep::SweepRecord;

/// What one target's samples did when replayed.
#[derive(Clone, Debug)]
pub struct TargetCheck {
    pub output       : String,
    pub target       : GlobalPx,
    /// Mean landing point over the samples that reached a panel.
    pub mean_px      : Option<GlobalPx>,
    /// Distance from that mean to the target.
    pub err_px       : Option<f64>,
    /// The same as a visual angle.
    pub err_deg      : Option<f64>,
    /// Residual when the target's *mean* input is pushed through the runtime path. This is
    /// the one that is comparable with the stored in-sample RMS: the correction is
    /// nonlinear, so the mean of the corrections is not the correction of the mean.
    pub mean_ray_deg : Option<f64>,
    /// Stage one through the fitter's own entry point, `AnglePoly::apply`.
    pub apply_yaw    : Option<f64>,
    /// Stage one through the runtime's, `Calibration::correct_dir_camera`, which makes a
    /// camera to desk to camera round trip on either side of it.
    pub runtime_yaw  : Option<f64>,
    pub clamped      : usize,
    pub samples      : usize,
}

/// The whole replay.
#[derive(Clone, Debug)]
pub struct CheckReport {
    pub targets            : Vec<TargetCheck>,
    /// RMS of `mean_ray_deg`. Compare this with `Calibration::rms_deg`.
    pub rms_runtime_deg    : f64,
    /// RMS of `err_deg`: where the marker actually sits on average.
    pub rms_marker_deg     : f64,
    /// Worst disagreement between the two stage-one entry points, degrees. Anything above
    /// rounding is a bug.
    pub worst_path_gap_deg : f64,
    /// Samples whose corrected ray missed the desk and were clamped at run time.
    pub clamped            : usize,
}

// --- CheckReport ---

impl CheckReport {
    /// How far the replay's RMS is from what the calibration claims.
    pub fn disagreement(&self, calibration: &Calibration) -> f64 {
        (self.rms_runtime_deg - calibration.rms_deg).abs()
    }
}

/// Replays `records` through the provider path under `calibration`.
pub fn replay(
    geometry    : &DesktopGeometry,
    camera      : &CameraPose,
    calibration : &Calibration,
    profile     : &SigmaProfile,
    records     : &[SweepRecord],
)
    -> Result<CheckReport, ProtocolError>
{
    let mut groups: Vec<Group> = Vec::new();

    for r in records {
        // Prefer the line the sidecar actually sent. Where a sweep predates that field, the
        // stored camera-frame vectors are exactly what the parser would have produced from
        // it, so re-encoding them exercises the same path.
        let line = {
            match r.raw.as_deref() {
                Some(raw) => raw.to_string(),
                None      => encode(DVec3::from_array(r.eye_cam_mm), DVec3::from_array(r.gaze_cam))?,
            }
        };

        let Some(reading) =
            sample_from_line(geometry, camera, Some(calibration), profile, &line, 0.0)?
        else {
            continue;
        };

        let key   = (r.target_px[0].to_bits(), r.target_px[1].to_bits());
        let group = {
            match groups.iter_mut().find(|g| g.key == key) {
                Some(g) => g,

                None => {
                    groups.push(Group {
                        key      : key,
                        output   : r.output.clone(),
                        target   : GlobalPx { x: r.target_px[0], y: r.target_px[1] },
                        points   : Vec::new(),
                        eye_sum  : DVec3::ZERO,
                        gaze_sum : DVec3::ZERO,
                        samples  : 0,
                        clamped  : 0,
                    });

                    groups.last_mut().expect("just pushed")
                }
            }
        };

        group.eye_sum  += DVec3::from_array(r.eye_cam_mm);
        group.gaze_sum += DVec3::from_array(r.gaze_cam);
        group.samples  += 1;

        if reading.meta.is_some_and(|m| m.clamped) {
            group.clamped += 1;
        }

        if let Some(p) = reading.sample.point.filter(|_| reading.sample.valid) {
            group.points.push(p);
        }
    }

    let mut targets  = Vec::new();
    let mut gap      = 0.0_f64;
    let mut clamped  = 0_usize;

    for g in &groups {
        let n       = g.samples.max(1) as f64;
        let eye_cam = g.eye_sum / n;
        clamped    += g.clamped;

        let mean_px = {
            if g.points.is_empty() {
                None
            }
            else {
                let count = g.points.len() as f64;

                Some(GlobalPx {
                    x : g.points.iter().map(|p| p.x).sum::<f64>() / count,
                    y : g.points.iter().map(|p| p.y).sum::<f64>() / count,
                })
            }
        };

        let (err_px, err_deg) = {
            match mean_px {
                Some(p) => {
                    let dx = p.x - g.target.x;
                    let dy = p.y - g.target.y;

                    (
                        Some((dx * dx + dy * dy).sqrt()),
                        geometry.angle_between_deg(camera.point_to_desk(eye_cam), p, g.target),
                    )
                }

                None => (None, None),
            }
        };

        // The two stage-one entry points, on the same input.
        let (apply_yaw, runtime_yaw, mean_ray_deg) = {
            if g.gaze_sum.length_squared() > 0.0 {
                let dir     = g.gaze_sum.normalize();
                let applied = gaze_yaw_pitch_deg(dir).map(|(y, p)| calibration.angle.apply(y, p).0);
                let runtime = gaze_yaw_pitch_deg(calibration.correct_dir_camera(dir)).map(|(y, _)| y);

                let line    = encode(eye_cam, dir)?;
                let reading = sample_from_line(geometry, camera, Some(calibration), profile, &line, 0.0)?;

                let residual = reading.and_then(|r| {
                    let ray     = r.sample.ray?;
                    let missed  = r.meta.is_some_and(|m| m.clamped);
                    let point   = r.sample.point.filter(|_| !missed);

                    angle_to_target(geometry, ray, point, g.target)
                });

                (applied, runtime, residual)
            }
            else {
                (None, None, None)
            }
        };

        if let (Some(a), Some(b)) = (apply_yaw, runtime_yaw) {
            gap = gap.max((a - b).abs());
        }

        targets.push(TargetCheck {
            output       : g.output.clone(),
            target       : g.target,
            mean_px      : mean_px,
            err_px       : err_px,
            err_deg      : err_deg,
            mean_ray_deg : mean_ray_deg,
            apply_yaw    : apply_yaw,
            runtime_yaw  : runtime_yaw,
            clamped      : g.clamped,
            samples      : g.samples,
        });
    }

    let runtime: Vec<f64> = targets.iter().filter_map(|t| t.mean_ray_deg).collect();
    let marker: Vec<f64>  = targets.iter().filter_map(|t| t.err_deg).collect();

    Ok(CheckReport {
        targets            : targets,
        rms_runtime_deg    : rms(&runtime),
        rms_marker_deg     : rms(&marker),
        worst_path_gap_deg : gap,
        clamped            : clamped,
    })
}

/// One target's replayed samples, while they are being accumulated.
struct Group {
    key      : (u64, u64),
    output   : String,
    target   : GlobalPx,
    points   : Vec<GlobalPx>,
    eye_sum  : DVec3,
    gaze_sum : DVec3,
    samples  : usize,
    clamped  : usize,
}

/// Builds the sidecar line a pair of camera-frame vectors would have arrived as.
fn encode(eye_cam: DVec3, gaze_cam: DVec3) -> Result<String, ProtocolError> {
    SidecarMessage::from_gaze(&SidecarGaze {
        t      : 0.0,
        seq    : 0,
        eye_mm : eye_cam,
        gaze   : gaze_cam,
        conf   : 1.0,
        lat_ms : 0.0,
    })
    .to_line()
}

/// Angle between where the system ended up believing the gaze went and the target, from the
/// ray's own origin. The same rule the fit uses for a residual.
fn angle_to_target(
    geometry : &DesktopGeometry,
    ray      : Ray,
    point    : Option<GlobalPx>,
    target   : GlobalPx,
)
    -> Option<f64>
{
    let world = geometry.px_to_world(target)?;
    let want  = world - ray.origin;

    if want.length_squared() <= 0.0 {
        return None;
    }

    let believed = {
        match point.and_then(|p| geometry.px_to_world(p)) {
            Some(w) => {
                let v = w - ray.origin;

                if v.length_squared() > 0.0 { v.normalize() } else { ray.dir }
            }

            None => ray.dir,
        }
    };

    Some(believed.angle_between(want.normalize()).to_degrees())
}

/// Root mean square, NaN for an empty set.
fn rms(values: &[f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }

    (values.iter().map(|v| v * v).sum::<f64>() / values.len() as f64).sqrt()
}
