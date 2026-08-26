//! A stand-in for the Python sidecar: a Unix socket server that streams the same line
//! protocol, driven by a synthetic eye that looks wherever it is told.
//!
//! This exists because the webcam is not always plugged in and the sidecar is being
//! written in parallel, but it earns its keep beyond that: it is the only way to run the
//! calibration end to end against a *known* distortion and check what the fit recovers.
//! The fake applies a deliberate error to every ray it emits, and a good calibration is
//! one that takes the residual back to nearly nothing.
//!
//! # The distortion it applies, and why in that order
//!
//! Per sample it takes the point the synthetic eye is looking at, warps it in the
//! output's normalised coordinates (a quadratic, standing in for a model whose error
//! grows toward the edges of its training distribution), lifts the warped point to a ray,
//! and then rotates the ray by a fixed yaw and pitch (standing in for a mis-measured
//! camera pose). Warp first, rotate second.
//!
//! `crate::calibration` undoes them in the opposite order, which is the only order that
//! composes: the angular stage runs before intersection, the polynomial after it.

use std::io::Write;
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use gaze_core::{DesktopGeometry, GlobalPx, Ray};
use glam::DVec3;
use rand::SeedableRng;
use rand::rngs::StdRng;
use rand_distr::{Distribution, Normal};

use crate::calibration::{denormalise, normalise, snap_to_desk};
use crate::camera::{CameraPose, gaze_dir_from_yaw_pitch_deg, gaze_yaw_pitch_deg};
use crate::protocol::{SidecarGaze, SidecarMessage};

/// How long the synthetic eye takes to travel to a newly commanded target, seconds. Real
/// saccades are faster than this; the point is only that the samples right after a jump
/// are not yet on the target, so a collection window that ignores them is doing something.
const SACCADE_S: f64 = 0.2;

/// How often the accept loop wakes to look for a client or a stop request.
const ACCEPT_POLL: Duration = Duration::from_millis(20);

/// Where the synthetic eye is looking.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Aim {
    /// Trace a Lissajous figure across the whole desk. The standalone demo mode.
    Sweep,
    /// Hold a fixed point, after a short saccade to reach it.
    At(GlobalPx),
    /// Report invalid frames, as the sidecar does when it cannot find a face.
    Lost,
}

/// Shape of the fake's angular gain.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum GainCurve {
    /// Reported angle is the true angle times a constant. A single global rotation cannot
    /// fix even this, but a linear angle polynomial can, exactly.
    Linear,
    /// Reported angle saturates: `A * atan(k * theta / A)`. The gain is `k` straight ahead
    /// and falls toward 1 and below as the eye turns further.
    ///
    /// This is the shape a real appearance model showed on this desk, and the one that
    /// broke the previous calibration design: no rotation and no linear gain can follow
    /// it, so a fit that assumes either sends rays for the middle of a panel past its edge.
    Saturating,
}

/// The error the fake bakes into every ray. Defaults to a distortion big enough to be
/// obviously wrong and small enough to be plausible for a webcam model.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Distortion {
    /// Ray rotation about world +Y, degrees. Stands in for a camera yaw that was measured
    /// wrong.
    pub yaw_deg    : f64,
    /// Ray rotation in elevation, degrees.
    pub pitch_deg  : f64,
    /// Quadratic warp in the output's normalised coordinates, producing x. Term order
    /// matches `PolyDegree::basis`: `[1, x, y, x^2, xy, y^2]`.
    pub warp_x     : [f64; 6],
    /// The same for y.
    pub warp_y     : [f64; 6],
    /// Per-sample angular jitter, degrees, 1-sigma per axis. Makes the mean over a
    /// collection window a real average rather than a copy of one sample.
    pub jitter_deg : f64,
    /// Multiplier applied to the reported camera-frame gaze yaw and pitch. Under `Linear`
    /// it is the gain everywhere; under `Saturating` it is the gain straight ahead.
    pub gain       : f64,
    /// Shape of the gain against eccentricity.
    pub curve      : GainCurve,
    /// Angle at which the saturating curve has bent appreciably, degrees. Larger is
    /// gentler.
    pub curve_scale_deg : f64,
    /// Yaw error added per degree of pitch, degrees. Stands in for the vertical-dependent
    /// yaw shift a real model showed, which is what makes the correction genuinely two
    /// dimensional rather than two independent one-axis curves.
    pub yaw_shift_per_pitch : f64,
}

/// A running fake sidecar. Dropping it stops the thread and removes the socket file.
pub struct FakeSidecar {
    path      : PathBuf,
    aim       : Arc<Mutex<Aim>>,
    stop_flag : Arc<AtomicBool>,
    thread    : Option<JoinHandle<()>>,
}

/// Cheap clonable handle for pointing the synthetic eye somewhere. The calibration sweep
/// holds one so the fake looks at whatever target is currently on screen.
#[derive(Clone, Debug)]
pub struct FakeGaze {
    aim : Arc<Mutex<Aim>>,
}

/// Builds a `FakeSidecar`.
pub struct FakeSidecarBuilder {
    socket     : PathBuf,
    geometry   : Option<DesktopGeometry>,
    camera     : Option<CameraPose>,
    distortion : Distortion,
    rate_hz    : f64,
    aim        : Aim,
    seed       : u64,
}

// --- FakeSidecar ---

impl FakeSidecar {
    /// Entry point for the builder.
    pub fn create() -> FakeSidecarBuilder {
        FakeSidecarBuilder::new()
    }

    /// Handle for redirecting the synthetic eye.
    pub fn gaze(&self) -> FakeGaze {
        FakeGaze { aim: Arc::clone(&self.aim) }
    }

    /// The socket the fake is listening on.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Stops the server thread and unlinks the socket. Idempotent.
    pub fn stop(&mut self) {
        self.stop_flag.store(true, Ordering::Relaxed);

        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }

        let _ = std::fs::remove_file(&self.path);
    }
}

impl Drop for FakeSidecar {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- FakeGaze ---

impl FakeGaze {
    /// Points the synthetic eye at a target.
    pub fn look_at(&self, p: GlobalPx) {
        self.set(Aim::At(p));
    }

    /// Puts the synthetic eye back on its sweep.
    pub fn sweep(&self) {
        self.set(Aim::Sweep);
    }

    /// Makes the fake report lost tracking, as a face leaving the frame would.
    pub fn lose_tracking(&self) {
        self.set(Aim::Lost);
    }

    /// Replaces the aim outright.
    pub fn set(&self, aim: Aim) {
        if let Ok(mut a) = self.aim.lock() {
            *a = aim;
        }
    }
}

// --- FakeSidecarBuilder ---

impl FakeSidecarBuilder {
    fn new() -> Self {
        Self {
            socket     : PathBuf::from("/tmp/gaze-ml-fake.sock"),
            geometry   : None,
            camera     : None,
            distortion : Distortion::default(),
            rate_hz    : 30.0,
            aim        : Aim::Sweep,
            seed       : 0,
        }
    }

    /// Socket path to bind. A stale file at that path is removed first.
    pub fn socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = path.into();
        self
    }

    /// Desk geometry the synthetic eye looks at. Required.
    pub fn geometry(mut self, geometry: DesktopGeometry) -> Self {
        self.geometry = Some(geometry);
        self
    }

    /// Camera pose used to phrase the desk-frame truth in camera-frame terms. Required.
    pub fn camera(mut self, camera: CameraPose) -> Self {
        self.camera = Some(camera);
        self
    }

    /// The error to bake in. Defaults to `Distortion::default()`.
    pub fn distortion(mut self, distortion: Distortion) -> Self {
        self.distortion = distortion;
        self
    }

    /// Frames per second. Defaults to 30, matching the sidecar.
    pub fn rate_hz(mut self, rate_hz: f64) -> Self {
        self.rate_hz = rate_hz;
        self
    }

    /// Where the synthetic eye starts out looking. Defaults to `Aim::Sweep`.
    pub fn aim(mut self, aim: Aim) -> Self {
        self.aim = aim;
        self
    }

    /// Seeds the jitter RNG for reproducible runs.
    pub fn seed(mut self, seed: u64) -> Self {
        self.seed = seed;
        self
    }

    /// Binds the socket and starts serving.
    pub fn start(self) -> Result<FakeSidecar, FakeError> {
        let geometry = self.geometry.ok_or(FakeError::MissingGeometry)?;
        let camera   = self.camera.ok_or(FakeError::MissingCamera)?;

        if let Some(parent) = self.socket.parent().filter(|p| !p.as_os_str().is_empty()) {
            std::fs::create_dir_all(parent)
                .map_err(|source| FakeError::Bind { path: parent.to_path_buf(), source })?;
        }

        // A socket file left behind by a crashed run would make bind fail with EADDRINUSE
        // even though nobody is listening.
        let _ = std::fs::remove_file(&self.socket);

        let listener = UnixListener::bind(&self.socket)
            .map_err(|source| FakeError::Bind { path: self.socket.clone(), source })?;

        listener.set_nonblocking(true)
            .map_err(|source| FakeError::Bind { path: self.socket.clone(), source })?;

        let aim       = Arc::new(Mutex::new(self.aim));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let thread = thread::Builder::new()
            .name("gaze-fake-sidecar".to_string())
            .spawn({
                let aim       = Arc::clone(&aim);
                let stop_flag = Arc::clone(&stop_flag);

                let ctx = ServerContext {
                    geometry   : geometry,
                    camera     : camera,
                    distortion : self.distortion,
                    rate_hz    : self.rate_hz.max(1.0),
                    seed       : self.seed,
                };

                move || run_server(listener, ctx, &aim, &stop_flag)
            })
            .map_err(FakeError::Spawn)?;

        Ok(FakeSidecar {
            path      : self.socket,
            aim       : aim,
            stop_flag : stop_flag,
            thread    : Some(thread),
        })
    }
}

// --- Distortion ---

impl Distortion {
    /// A distortion that changes nothing, for a fake used purely as a transport.
    pub fn none() -> Self {
        Self {
            yaw_deg    : 0.0,
            pitch_deg  : 0.0,
            warp_x     : [0.0, 1.0, 0.0, 0.0, 0.0, 0.0],
            warp_y     : [0.0, 0.0, 1.0, 0.0, 0.0, 0.0],
            jitter_deg : 0.0,
            gain       : 1.0,
            curve      : GainCurve::Linear,
            curve_scale_deg     : 45.0,
            yaw_shift_per_pitch : 0.0,
        }
    }

    /// Applies the angular gain to a camera-frame gaze direction. A gain of 1 on a linear
    /// curve with no shift is the identity, and the round trip through yaw/pitch is exact.
    pub fn apply_gain(&self, camera_dir: DVec3) -> DVec3 {
        if self.gain == 1.0 && self.curve == GainCurve::Linear && self.yaw_shift_per_pitch == 0.0 {
            return camera_dir;
        }

        let Some((yaw, pitch)) = gaze_yaw_pitch_deg(camera_dir) else {
            return camera_dir;
        };

        let (mut out_yaw, out_pitch) = {
            match self.curve {
                GainCurve::Linear     => (yaw * self.gain, pitch * self.gain),
                GainCurve::Saturating => (self.saturate(yaw), self.saturate(pitch)),
            }
        };

        // The cross term is applied to the *reported* yaw against the true pitch, which is
        // what a model whose horizontal estimate leans on vertical head cues looks like.
        out_yaw += self.yaw_shift_per_pitch * pitch;

        gaze_dir_from_yaw_pitch_deg(out_yaw, out_pitch)
    }

    /// `A * atan(k * theta / A)`, in degrees. Gain `k` at zero, falling with eccentricity.
    fn saturate(&self, angle_deg: f64) -> f64 {
        let a = self.curve_scale_deg.to_radians();

        if a <= 0.0 || !a.is_finite() {
            return angle_deg * self.gain;
        }

        (a * (self.gain * angle_deg.to_radians() / a).atan()).to_degrees()
    }

    /// Applies the pixel warp to a point on `geometry`, leaving points that are not on any
    /// output alone.
    pub fn warp_px(&self, geometry: &DesktopGeometry, p: GlobalPx) -> GlobalPx {
        let Some(out) = geometry.output_at(p) else {
            return p;
        };

        let (nx, ny) = normalise(out, p);
        let basis    = [1.0, nx, ny, nx * nx, nx * ny, ny * ny];

        let wx: f64 = self.warp_x.iter().zip(basis.iter()).map(|(c, b)| c * b).sum();
        let wy: f64 = self.warp_y.iter().zip(basis.iter()).map(|(c, b)| c * b).sum();

        denormalise(out, wx, wy)
    }

    /// Applies the angular part to a ray.
    pub fn bias_ray(&self, ray: &Ray) -> Ray {
        DesktopGeometry::perturb_ray(ray, self.yaw_deg, self.pitch_deg)
    }
}

impl Default for Distortion {
    /// About 1.5 degrees of pose error plus a few per cent of scale and curvature, which
    /// is the shape of error an uncalibrated appearance model produces.
    fn default() -> Self {
        Self {
            yaw_deg    : 1.5,
            pitch_deg  : -1.0,
            warp_x     : [0.02, 1.04, 0.03, -0.05, 0.02, 0.01],
            warp_y     : [-0.03, 0.02, 0.96, 0.01, -0.03, 0.04],
            jitter_deg : 0.3,
            gain       : 1.0,
            curve      : GainCurve::Linear,
            curve_scale_deg     : 45.0,
            yaw_shift_per_pitch : 0.0,
        }
    }
}

/// Everything the server thread needs.
struct ServerContext {
    geometry   : DesktopGeometry,
    camera     : CameraPose,
    distortion : Distortion,
    rate_hz    : f64,
    seed       : u64,
}

/// Accepts one client at a time and streams to it until it goes away.
fn run_server(
    listener  : UnixListener,
    ctx       : ServerContext,
    aim       : &Mutex<Aim>,
    stop_flag : &AtomicBool,
)
{
    let start = Instant::now();

    let mut rng   = StdRng::seed_from_u64(ctx.seed);
    let mut seq   = 0_u64;
    let mut eye   = EyeState::new(&ctx.geometry);

    while !stop_flag.load(Ordering::Relaxed) {
        match listener.accept() {
            Ok((stream, _)) => {
                tracing::info!("fake sidecar: client connected");
                stream_to(&stream, &ctx, aim, stop_flag, start, &mut seq, &mut eye, &mut rng);
                tracing::info!("fake sidecar: client gone");
            }

            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(ACCEPT_POLL);
            }

            Err(e) => {
                tracing::error!("fake sidecar accept failed: {e}");
                break;
            }
        }
    }
}

/// Writes frames to one client until the write fails or the caller stops.
#[allow(clippy::too_many_arguments)]
fn stream_to(
    mut stream : &UnixStream,
    ctx        : &ServerContext,
    aim        : &Mutex<Aim>,
    stop_flag  : &AtomicBool,
    start      : Instant,
    seq        : &mut u64,
    eye        : &mut EyeState,
    rng        : &mut StdRng,
)
{
    let period = Duration::from_secs_f64(1.0 / ctx.rate_hz);

    while !stop_flag.load(Ordering::Relaxed) {
        let t   = start.elapsed().as_secs_f64();
        let now = {
            match aim.lock() {
                Ok(a)  => *a,
                Err(_) => Aim::Lost,
            }
        };

        let message = {
            match eye.point_at(&ctx.geometry, now, t) {
                Some(point) => frame(ctx, point, t, *seq, rng),
                None        => SidecarMessage::invalid(t, *seq),
            }
        };

        *seq += 1;

        let Ok(line) = message.to_line() else {
            continue;
        };

        // A client that has gone away shows up as a broken pipe here; that is the signal
        // to go back to accepting.
        if writeln!(stream, "{line}").is_err() || stream.flush().is_err() {
            return;
        }

        thread::sleep(period);
    }
}

/// Builds one valid frame for a point the synthetic eye is looking at.
fn frame(ctx: &ServerContext, point: GlobalPx, t: f64, seq: u64, rng: &mut StdRng)
    -> SidecarMessage
{
    // Warp the pixel, lift it to a ray from the nominal eye, bias the ray, jitter it.
    let Some(output) = ctx.geometry.output_at(point) else {
        return SidecarMessage::invalid(t, seq);
    };

    let warped = ctx.distortion.warp_px(&ctx.geometry, point);
    let eye_mm = ctx.geometry.eye();

    // Lift the warped pixel on the panel the *true* point is on, extrapolating its
    // surface. A warp that pushes the point just off the panel edge is a real thing for a
    // gaze model to do, and dropping back to the unwarped point there would quietly
    // remove the distortion exactly where it is largest.
    let world = output.px_to_world(warped);

    let clean  = Ray { origin: eye_mm, dir: (world - eye_mm).normalize() };
    let biased = ctx.distortion.bias_ray(&clean);

    let jittered = {
        if ctx.distortion.jitter_deg > 0.0 {
            let n = Normal::new(0.0, ctx.distortion.jitter_deg).expect("jitter sigma must be finite");

            DesktopGeometry::perturb_ray(&biased, n.sample(rng), n.sample(rng))
        }
        else {
            biased
        }
    };

    // The gain is applied last, in the camera frame, because that is where a gaze model
    // lives: it is a property of the model's output, not of the desk.
    let reported = ctx.distortion.apply_gain(ctx.camera.dir_to_camera(jittered.dir));

    SidecarMessage::from_gaze(&SidecarGaze {
        t      : t,
        seq    : seq,
        eye_mm : ctx.camera.point_to_camera(eye_mm),
        gaze   : reported,
        conf   : 0.9,
        lat_ms : 40.0,
    })
}

/// Where the synthetic eye is, including the short glide to a newly commanded target.
struct EyeState {
    /// Point the eye was at when the current aim was commanded.
    from      : GlobalPx,
    /// The aim currently being served, so a change can be detected.
    aim       : Aim,
    /// Time the current aim was commanded.
    changed_s : f64,
}

// --- EyeState ---

impl EyeState {
    /// Starts at the centre of the desk's first enabled output.
    fn new(geometry: &DesktopGeometry) -> Self {
        let start = geometry
            .outputs
            .iter()
            .find(|o| o.enabled)
            .map(|o| GlobalPx {
                x : o.logical_x + o.logical_w * 0.5,
                y : o.logical_y + o.logical_h * 0.5,
            })
            .unwrap_or(GlobalPx { x: 0.0, y: 0.0 });

        Self { from: start, aim: Aim::Sweep, changed_s: 0.0 }
    }

    /// The point to look at now, or `None` when tracking is lost.
    fn point_at(&mut self, geometry: &DesktopGeometry, aim: Aim, t: f64) -> Option<GlobalPx> {
        if aim != self.aim {
            // Glide from wherever the eye currently is, not from the previous target: a
            // second command mid-saccade should not teleport it back.
            self.from      = self.current(geometry, t).unwrap_or(self.from);
            self.aim       = aim;
            self.changed_s = t;
        }

        self.current(geometry, t)
    }

    /// Where the eye is right now, part way through the saccade if one is in progress.
    fn current(&self, geometry: &DesktopGeometry, t: f64) -> Option<GlobalPx> {
        let settled = self.settled(geometry, t)?;
        let elapsed = t - self.changed_s;

        if elapsed >= SACCADE_S {
            return Some(settled);
        }

        let k = (elapsed / SACCADE_S).clamp(0.0, 1.0);

        let between = GlobalPx {
            x : self.from.x + (settled.x - self.from.x) * k,
            y : self.from.y + (settled.y - self.from.y) * k,
        };

        // A straight line between two panels crosses the gap between them. Snapping keeps
        // the synthetic eye on a surface for the whole flight, so lost tracking stays
        // something only `Aim::Lost` produces.
        Some(snap_to_desk(geometry, between))
    }

    /// Where the current aim points once the saccade is over.
    fn settled(&self, geometry: &DesktopGeometry, t: f64) -> Option<GlobalPx> {
        match self.aim {
            Aim::At(p) => Some(p),
            Aim::Lost  => None,
            Aim::Sweep => Some(sweep_point(geometry, t)),
        }
    }
}

/// A Lissajous path across the union of the enabled outputs, snapped onto the nearest
/// panel. Two incommensurate frequencies keep it from retracing itself.
///
/// The snap matters: this desk's three panels cover well under half of their common
/// bounding box, so an unsnapped figure spends most of its time in empty space, where
/// there is no surface to look at and every frame would come out invalid.
fn sweep_point(geometry: &DesktopGeometry, t: f64) -> GlobalPx {
    let mut min = (f64::MAX, f64::MAX);
    let mut max = (f64::MIN, f64::MIN);

    for out in geometry.outputs.iter().filter(|o| o.enabled) {
        min = (min.0.min(out.logical_x), min.1.min(out.logical_y));
        max = (max.0.max(out.logical_x + out.logical_w), max.1.max(out.logical_y + out.logical_h));
    }

    if min.0 > max.0 {
        return GlobalPx { x: 0.0, y: 0.0 };
    }

    let cx = 0.5 * (min.0 + max.0);
    let cy = 0.5 * (min.1 + max.1);
    let ax = 0.42 * (max.0 - min.0);
    let ay = 0.36 * (max.1 - min.1);

    let raw = GlobalPx {
        x : cx + ax * (0.31 * t).sin(),
        y : cy + ay * (0.23 * t).cos(),
    };

    snap_to_desk(geometry, raw)
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum FakeError {
    #[error("fake sidecar needs a desk geometry")]
    MissingGeometry,

    #[error("fake sidecar needs a camera pose")]
    MissingCamera,

    #[error("cannot bind fake sidecar socket {}: {source}", path.display())]
    Bind { path: PathBuf, #[source] source: std::io::Error },

    #[error("cannot spawn the fake sidecar thread: {0}")]
    Spawn(#[source] std::io::Error),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_provider_synthetic::GazeProvider;

    use super::*;
    use crate::provider::WebcamProvider;

    const DESK_TOML: &str = include_str!("../../../config/desk.toml");

    fn desk() -> (DesktopGeometry, CameraPose) {
        (
            DesktopGeometry::from_toml(DESK_TOML).unwrap(),
            CameraPose::from_desk_toml(DESK_TOML).unwrap(),
        )
    }

    #[test]
    fn the_warp_moves_a_point_and_leaves_off_desk_points_alone() {
        let (g, _) = desk();
        let d      = Distortion::default();

        let on  = GlobalPx { x: 4479.0, y: 800.0 };
        let out = d.warp_px(&g, on);

        assert!((out.x - on.x).abs() > 1.0, "the default warp must actually move a point");

        // Nothing on the desk covers this, so there is no normalised frame to warp in.
        let off = GlobalPx { x: -5000.0, y: -5000.0 };
        assert_eq!(d.warp_px(&g, off), off);
    }

    #[test]
    fn the_gain_compresses_the_reported_angle_by_the_requested_factor() {
        let d   = Distortion { gain: 0.5, ..Distortion::none() };
        let dir = gaze_dir_from_yaw_pitch_deg(20.0, -8.0);

        let (yaw, pitch) = gaze_yaw_pitch_deg(d.apply_gain(dir)).unwrap();

        assert!((yaw - 10.0).abs() < 1.0e-9, "yaw = {yaw}");
        assert!((pitch + 4.0).abs() < 1.0e-9, "pitch = {pitch}");

        // A gain of one must be bit-for-bit the identity, not a lossy round trip.
        let unity = Distortion::none();
        assert_eq!(unity.apply_gain(dir), dir);
    }

    #[test]
    fn the_saturating_curve_has_the_requested_gain_at_zero_and_falls_off() {
        let d = Distortion {
            gain            : 3.4,
            curve           : GainCurve::Saturating,
            curve_scale_deg : 45.0,
            ..Distortion::none()
        };

        let gain_at = |deg: f64| {
            let (yaw, _) = gaze_yaw_pitch_deg(d.apply_gain(gaze_dir_from_yaw_pitch_deg(deg, 0.0))).unwrap();

            yaw / deg
        };

        // Near the axis the curve is its own tangent, so the gain is the requested one.
        assert!((gain_at(0.5) - 3.4).abs() < 0.02, "gain near zero = {}", gain_at(0.5));

        // And it falls monotonically with eccentricity, crossing 1 somewhere out in the
        // periphery. This is the shape a single rotation cannot follow.
        let far = gain_at(50.0);
        assert!(far < gain_at(20.0) && gain_at(20.0) < gain_at(5.0), "the gain must fall off");
        assert!(far < 1.5, "gain at 50 deg = {far}");

        // The sign is preserved either side of the axis.
        assert!(gain_at(-30.0) > 0.0);
    }

    #[test]
    fn the_vertical_cross_term_shifts_yaw_with_pitch() {
        let d = Distortion { yaw_shift_per_pitch: -0.3, ..Distortion::none() };

        let at = |pitch: f64| {
            gaze_yaw_pitch_deg(d.apply_gain(gaze_dir_from_yaw_pitch_deg(10.0, pitch))).unwrap().0
        };

        assert!((at(0.0) - 10.0).abs() < 1.0e-9);
        assert!((at(20.0) - 4.0).abs() < 1.0e-6, "yaw at pitch 20 = {}", at(20.0));
        assert!((at(-20.0) - 16.0).abs() < 1.0e-6);
    }

    #[test]
    fn a_distortion_of_none_is_the_identity() {
        let (g, _) = desk();
        let d      = Distortion::none();
        let p      = GlobalPx { x: 4479.0, y: 800.0 };

        let warped = d.warp_px(&g, p);
        assert!((warped.x - p.x).abs() < 1.0e-9 && (warped.y - p.y).abs() < 1.0e-9);

        let ray = g.px_to_ray(p).unwrap();
        assert!(d.bias_ray(&ray).dir.angle_between(ray.dir).to_degrees() < 1.0e-12);
    }

    #[test]
    fn the_sweep_always_lands_on_a_panel() {
        let (g, _) = desk();

        // Every point on the path has to be something a person could actually look at,
        // or the fake spends most of its run reporting invalid frames.
        for i in 0..2000 {
            let p = sweep_point(&g, i as f64 * 0.037);

            assert!(g.output_at(p).is_some(), "sweep left the desk at step {i}: {p:?}");
        }

        // And it has to move: a path that snapped to one corner would be useless.
        let a = sweep_point(&g, 0.0);
        let b = sweep_point(&g, 5.0);

        assert!((a.x - b.x).abs() + (a.y - b.y).abs() > 100.0, "sweep barely moved");
    }

    #[test]
    fn an_undistorted_fake_round_trips_a_commanded_point_through_the_real_provider() {
        let (g, c) = desk();
        let dir    = tempfile::tempdir().unwrap();
        let socket = dir.path().join("gaze-ml.sock");

        let fake = FakeSidecar::create()
            .socket(&socket)
            .geometry(g.clone())
            .camera(c.clone())
            .distortion(Distortion::none())
            .rate_hz(120.0)
            .aim(Aim::At(GlobalPx { x: 4479.0, y: 800.0 }))
            .start()
            .unwrap();

        let mut provider = WebcamProvider::create()
            .socket(fake.path())
            .geometry(g)
            .camera(c)
            .calibration(None::<PathBuf>)
            .start()
            .unwrap();

        // Give the saccade time to settle, then take a sample.
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = None;

        while Instant::now() < deadline {
            let Some(sample) = provider.next() else {
                break;
            };

            if sample.valid && sample.t_s > SACCADE_S + 0.1 {
                last = sample.point;
                break;
            }
        }

        let point = last.expect("the fake must deliver a valid sample within five seconds");

        // No distortion and no calibration: the provider must land on the commanded point.
        assert!((point.x - 4479.0).abs() < 1.0, "landed at {point:?}");
        assert!((point.y - 800.0).abs() < 1.0, "landed at {point:?}");

        provider.stop();
    }

    #[test]
    fn a_lost_aim_produces_invalid_samples() {
        let (g, c) = desk();
        let dir    = tempfile::tempdir().unwrap();

        let fake = FakeSidecar::create()
            .socket(dir.path().join("gaze-ml.sock"))
            .geometry(g.clone())
            .camera(c.clone())
            .rate_hz(120.0)
            .aim(Aim::Lost)
            .start()
            .unwrap();

        let mut provider = WebcamProvider::create()
            .socket(fake.path())
            .geometry(g)
            .camera(c)
            .calibration(None::<PathBuf>)
            .start()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);
        let mut seen = 0;

        while Instant::now() < deadline && seen < 3 {
            let Some(sample) = provider.next() else {
                break;
            };

            assert!(!sample.valid, "a lost sidecar must not produce valid samples");
            assert!(sample.point.is_none());
            seen += 1;
        }

        assert_eq!(seen, 3, "the fake must keep sending invalid frames");

        provider.stop();
    }
}
