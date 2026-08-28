//! `WebcamProvider`: a `GazeProvider` fed by the Python gaze sidecar over a Unix socket.
//!
//! The provider owns one reader thread. Per line it converts the sidecar's camera-frame
//! eye position and gaze vector into a desk-frame ray (see `crate::camera`), applies the
//! calibration, intersects the desk, and pushes a `GazeSample` down a channel. When the
//! socket goes away it emits one invalid sample and reconnects with backoff, forever,
//! until `stop()`.
//!
//! # Timestamps
//!
//! `GazeSample::t_s` is monotonic seconds since `start()` measured *on this process's
//! clock, when the line was read*. The sidecar's own `t` is on a different clock and its
//! `lat_ms` jitters frame to frame, so subtracting the latency would make `t_s`
//! non-monotonic and break any velocity-based filter downstream. Both sidecar figures are
//! kept on `SampleMeta` for anyone who wants to reason about the true capture time.
//!
//! # Sigma
//!
//! Sigma comes from a `SigmaProfile`, and the webcam preset is flat: an appearance-based
//! model has no cornea-glint constraint to fall off, so there is nothing to justify a
//! ramp against off-axis angle. The default is 2.5 degrees, which is the optimistic end of
//! the 2 to 5 degrees the literature reports for webcam-only gaze (DESIGN.md section 4).
//! It is the snap engine's search radius, so it is deliberately not flattered.
//!
//! Confidence is reported, not folded into sigma. Turning a model's self-reported `conf`
//! into an angular error needs a measured relationship between the two, and there is not
//! one yet; inventing a mapping would make sigma dishonest.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender};
use gaze_core::{DesktopGeometry, GazeSample, GlobalPx, Ray, SigmaProfile};
use gaze_provider_synthetic::GazeProvider;

use crate::calibration::{Calibration, CalibrationError, resolve};
use crate::camera::CameraPose;
use crate::protocol::{ProtocolError, SidecarMessage};
use crate::socket::{Backoff, LineStream};

/// Default sigma for the webcam tier, degrees. See the module docs.
pub const DEFAULT_SIGMA_DEG: f64 = 2.5;

/// Default socket the sidecar listens on.
pub const DEFAULT_SOCKET: &str = "/run/user/1000/gaze-ml.sock";

/// Samples buffered for the consumer. At 30 Hz this is eight seconds of backlog; a
/// consumer further behind than that wants the newest sample, not the oldest, so the
/// reader drops rather than blocks.
const CHANNEL_CAPACITY: usize = 256;

/// How long a socket read waits before returning to the loop so `stop()` can be noticed.
const READ_TIMEOUT: Duration = Duration::from_millis(100);

/// Granularity of the reconnect sleep, so a stop during a two second backoff is still
/// prompt.
const SLEEP_SLICE: Duration = Duration::from_millis(25);

/// A `GazeProvider` backed by the gaze sidecar's Unix socket.
pub struct WebcamProvider {
    sample_rx : Receiver<Reading>,
    status    : Arc<Mutex<Status>>,
    stop_flag : Arc<AtomicBool>,
    thread    : Option<JoinHandle<()>>,
}

/// One sample together with everything behind it: the line it came from and the
/// intermediate quantities the geometry produced.
///
/// Calibration diagnostics need the *raw* sidecar vectors, not just the desk-frame point
/// they turned into: a model that under-reports gaze angle looks like a strange polynomial
/// in pixel space and like an obvious gain below one in camera-frame angle space. Carrying
/// the message through is the only way to see the difference.
#[derive(Clone, Debug)]
pub struct Reading {
    pub sample  : GazeSample,
    /// The line this came from, parsed, or `None` for a sample this crate synthesized
    /// because the socket went away.
    pub message : Option<SidecarMessage>,
    /// The same line, **verbatim**.
    ///
    /// `SidecarMessage` only models the fields this crate uses, so re-serialising it
    /// silently drops anything the sidecar added: a second estimator's vectors, a blink
    /// flag, a model version. Those are exactly the fields an offline experiment wants
    /// months later, and by then the sweep is gone. Keeping the text costs a few hundred
    /// bytes per sample and makes the readings log a complete record rather than a
    /// summary. Shared rather than owned so cloning a reading stays cheap.
    pub raw     : Option<Arc<str>>,
    /// `None` when the sample is invalid.
    pub meta    : Option<SampleMeta>,
}

/// A source of `Reading`s. Separate from `GazeProvider` because only a provider that
/// speaks the sidecar protocol has raw vectors to offer, and because it lets the
/// calibration sweep be driven by a stub in tests.
pub trait RawGaze {
    /// Next reading if one is already available, without blocking.
    fn try_next_reading(&mut self) -> Option<Reading>;
}

/// Everything about the stream that is not part of a `GazeSample`, kept so a CLI can print
/// it and a supervisor can notice a sidecar that is running but not tracking.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct SampleMeta {
    /// Sidecar frame counter. Gaps are dropped frames.
    pub seq         : u64,
    /// Sidecar-clock timestamp of the frame.
    pub sidecar_t_s : f64,
    /// The model's confidence in the frame, nominally [0, 1].
    pub conf        : f64,
    /// Capture-to-socket-write latency reported by the sidecar, milliseconds.
    pub lat_ms      : f64,
    /// Angle between the corrected gaze ray and the tracker axis, degrees.
    pub off_axis_deg: f64,
    /// True when the ray missed every panel and the point was clamped to an edge.
    pub clamped     : bool,
}

/// Live view of the reader thread, behind one mutex.
#[derive(Clone, Debug, Default)]
struct Status {
    connected  : bool,
    /// Lines received since `start()`, valid or not.
    lines      : u64,
    /// Connections established since `start()`, including the first.
    connects   : u64,
    /// Lines that failed to parse. A steadily rising count means a protocol skew.
    bad_lines  : u64,
    last_meta  : Option<SampleMeta>,
    last_output: Option<String>,
}

/// Counters a CLI can print to show what the socket has been doing.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ProviderStats {
    pub connected  : bool,
    pub lines      : u64,
    pub connects   : u64,
    pub bad_lines  : u64,
}

// --- WebcamProvider ---

impl WebcamProvider {
    /// Entry point for the builder:
    /// `WebcamProvider::create().socket(p).geometry(g).camera(c).calibration(None).start()`.
    pub fn create() -> WebcamProviderBuilder {
        WebcamProviderBuilder::new()
    }

    /// True while the reader thread has a live connection to the sidecar.
    pub fn connected(&self) -> bool {
        self.status.lock().map(|s| s.connected).unwrap_or(false)
    }

    /// Sidecar-side metadata for the most recent valid sample, or `None` before the first
    /// one arrives.
    pub fn last_meta(&self) -> Option<SampleMeta> {
        self.status.lock().ok().and_then(|s| s.last_meta)
    }

    /// Output the most recent valid sample landed on.
    pub fn last_output(&self) -> Option<String> {
        self.status.lock().ok().and_then(|s| s.last_output.clone())
    }

    /// Connection and parse counters since `start()`.
    pub fn stats(&self) -> ProviderStats {
        let Ok(s) = self.status.lock() else {
            return ProviderStats::default();
        };

        ProviderStats {
            connected : s.connected,
            lines     : s.lines,
            connects  : s.connects,
            bad_lines : s.bad_lines,
        }
    }
}

impl GazeProvider for WebcamProvider {
    fn next(&mut self) -> Option<GazeSample> {
        self.sample_rx.recv().ok().map(|r| r.sample)
    }

    fn try_next(&mut self) -> Option<GazeSample> {
        self.sample_rx.try_recv().ok().map(|r| r.sample)
    }

    fn stop(&mut self) {
        // Idempotent: a second call finds the handle already taken and does nothing.
        self.stop_flag.store(true, Ordering::Relaxed);

        if let Some(handle) = self.thread.take() {
            let _ = handle.join();
        }
    }
}

impl RawGaze for WebcamProvider {
    fn try_next_reading(&mut self) -> Option<Reading> {
        self.sample_rx.try_recv().ok()
    }
}

impl WebcamProvider {
    /// Blocks for the next reading. Shares one channel with `next`, so a caller uses one
    /// API or the other, never both.
    pub fn next_reading(&mut self) -> Option<Reading> {
        self.sample_rx.recv().ok()
    }
}

impl Drop for WebcamProvider {
    fn drop(&mut self) {
        // Closes the socket even if the caller forgot, so a dropped provider does not
        // leave a thread polling a dead sidecar for the life of the process.
        self.stop();
    }
}

/// Builds a `WebcamProvider`. Entry point is `WebcamProvider::create()`.
pub struct WebcamProviderBuilder {
    socket           : PathBuf,
    geometry         : Option<DesktopGeometry>,
    camera           : Option<CameraPose>,
    calibration_path : Option<PathBuf>,
    calibration      : Option<Calibration>,
    profile          : Option<SigmaProfile>,
}

// --- WebcamProviderBuilder ---

impl WebcamProviderBuilder {
    fn new() -> Self {
        Self {
            socket           : PathBuf::from(DEFAULT_SOCKET),
            geometry         : None,
            camera           : None,
            calibration_path : None,
            calibration      : None,
            profile          : None,
        }
    }

    /// Unix socket the sidecar listens on. Defaults to `DEFAULT_SOCKET`.
    pub fn socket(mut self, path: impl Into<PathBuf>) -> Self {
        self.socket = path.into();
        self
    }

    /// Desk geometry used to intersect the corrected ray. Required.
    pub fn geometry(mut self, geometry: DesktopGeometry) -> Self {
        self.geometry = Some(geometry);
        self
    }

    /// Camera pose used to bring the sidecar's vectors into the desk frame. Required.
    pub fn camera(mut self, camera: CameraPose) -> Self {
        self.camera = Some(camera);
        self
    }

    /// Calibration file to load, or `None` to run uncalibrated. The file is read in
    /// `start()` so a missing or malformed one is reported there.
    pub fn calibration(mut self, path: Option<impl Into<PathBuf>>) -> Self {
        self.calibration_path = path.map(Into::into);
        self
    }

    /// An already fitted calibration, bypassing the file. Takes precedence over
    /// `calibration`.
    pub fn calibration_model(mut self, calibration: Calibration) -> Self {
        self.calibration = Some(calibration);
        self
    }

    /// Flat sigma in degrees. Shorthand for `profile(webcam_profile(sigma_deg))`.
    pub fn sigma_deg(mut self, sigma_deg: f64) -> Self {
        self.profile = Some(webcam_profile(sigma_deg));
        self
    }

    /// Full sigma profile, for a caller that wants the error to grow off axis. Defaults to
    /// `webcam_profile(DEFAULT_SIGMA_DEG)`.
    pub fn profile(mut self, profile: SigmaProfile) -> Self {
        self.profile = Some(profile);
        self
    }

    /// Starts the reader thread.
    ///
    /// Does *not* require the sidecar to be up: a socket that is not there yet is just the
    /// first reconnect attempt, and samples start flowing when it appears. Only a missing
    /// geometry or camera pose, or an unreadable calibration file, is an error here.
    pub fn start(self) -> Result<WebcamProvider, ProviderError> {
        let geometry = self.geometry.ok_or(ProviderError::MissingGeometry)?;
        let camera   = self.camera.ok_or(ProviderError::MissingCamera)?;
        let profile  = self.profile.unwrap_or_else(|| webcam_profile(DEFAULT_SIGMA_DEG));

        let calibration = {
            match (self.calibration, self.calibration_path) {
                (Some(cal), _)   => Some(cal),
                (None, Some(p))  => Some(Calibration::load(&p).map_err(ProviderError::Calibration)?),
                (None, None)     => None,
            }
        };

        let status    = Arc::new(Mutex::new(Status::default()));
        let stop_flag = Arc::new(AtomicBool::new(false));

        let (sample_tx, sample_rx) = crossbeam_channel::bounded(CHANNEL_CAPACITY);

        let thread = thread::Builder::new()
            .name("gaze-webcam-reader".to_string())
            .spawn({
                let status    = Arc::clone(&status);
                let stop_flag = Arc::clone(&stop_flag);

                let ctx = ReaderContext {
                    socket      : self.socket,
                    geometry    : geometry,
                    camera      : camera,
                    calibration : calibration,
                    profile     : profile,
                };

                move || run_reader(ctx, &sample_tx, &status, &stop_flag)
            })
            .map_err(ProviderError::Spawn)?;

        Ok(WebcamProvider {
            sample_rx : sample_rx,
            status    : status,
            stop_flag : stop_flag,
            thread    : Some(thread),
        })
    }
}

/// The flat sigma profile the webcam tier uses. Flat all the way out, and never invalid,
/// so the only things that can make a sample invalid are the sidecar saying so and the
/// socket going away.
pub fn webcam_profile(sigma_deg: f64) -> SigmaProfile {
    SigmaProfile {
        sigma_deg      : sigma_deg,
        flat_to_deg    : 180.0,
        ramp_to_deg    : 180.0,
        ramp_factor    : 1.0,
        invalid_at_deg : f64::INFINITY,
    }
}

/// Everything the reader thread needs, bundled so the spawn closure stays readable.
struct ReaderContext {
    socket      : PathBuf,
    geometry    : DesktopGeometry,
    camera      : CameraPose,
    calibration : Option<Calibration>,
    profile     : SigmaProfile,
}

/// Connect, read, reconnect. Runs until `stop_flag` is set.
fn run_reader(
    ctx       : ReaderContext,
    sample_tx : &Sender<Reading>,
    status    : &Mutex<Status>,
    stop_flag : &AtomicBool,
)
{
    let start        = Instant::now();
    let mut backoff  = Backoff::new();
    let mut lines    = Vec::new();

    while !stop_flag.load(Ordering::Relaxed) {
        let stream = LineStream::connect(&ctx.socket, READ_TIMEOUT);

        let mut stream = {
            match stream {
                Ok(s) => s,

                Err(e) => {
                    // The sidecar not being up is the normal state at boot, so this is
                    // debug rather than a warning that would flood the log.
                    tracing::debug!("sidecar socket {} unavailable: {e}", ctx.socket.display());
                    emit_invalid(sample_tx, start.elapsed().as_secs_f64(), None, None);
                    sleep_interruptibly(backoff.next_delay(), stop_flag);

                    continue;
                }
            }
        };

        backoff.reset();
        tracing::info!("connected to sidecar at {}", ctx.socket.display());

        if let Ok(mut s) = status.lock() {
            s.connected = true;
            s.connects += 1;
        }

        // Drain until the peer goes away or the caller stops us.
        while !stop_flag.load(Ordering::Relaxed) {
            lines.clear();

            match stream.poll(&mut lines) {
                Ok(true)  => {}

                Ok(false) => {
                    tracing::info!("sidecar closed the connection");
                    break;
                }

                Err(e) => {
                    tracing::warn!("sidecar socket read failed: {e}");
                    break;
                }
            }

            for line in &lines {
                handle_line(&ctx, line, sample_tx, status, start);
            }
        }

        if let Ok(mut s) = status.lock() {
            s.connected = false;
        }

        // Tell the consumer the stream is gone rather than letting it sit on a stale
        // sample: an invalid sample is the contract for "do not use the last point".
        emit_invalid(sample_tx, start.elapsed().as_secs_f64(), None, None);

        if !stop_flag.load(Ordering::Relaxed) {
            sleep_interruptibly(backoff.next_delay(), stop_flag);
        }
    }
}

/// Turns one sidecar line into a `Reading`, exactly as the reader thread does.
///
/// **This is the runtime path.** `handle_line` calls it and does nothing else of
/// consequence, and `gaze-webcam-cli check` calls it to prove that what the provider
/// applies at run time is what the fitter evaluated at fit time. If those two ever
/// disagree, the calibration is a fiction and the marker will be somewhere the numbers say
/// it is not, so the equality is worth a public function to test against.
///
/// `t_s` is stamped onto the sample; the caller owns the clock. A line that fails to parse
/// is an error, and a line the sidecar marked invalid comes back as an invalid `Reading`.
pub fn sample_from_line(
    geometry    : &DesktopGeometry,
    camera      : &CameraPose,
    calibration : Option<&Calibration>,
    profile     : &SigmaProfile,
    line        : &str,
    t_s         : f64,
)
    -> Result<Option<Reading>, ProtocolError>
{
    let raw_line: Arc<str> = Arc::from(line);

    let Some(message) = SidecarMessage::parse(line)? else {
        return Ok(None);
    };

    let invalid = |message: Option<SidecarMessage>| Reading {
        sample  : GazeSample {
            t_s       : t_s,
            ray       : None,
            point     : None,
            sigma_deg : f64::MAX,
            valid     : false,
        },
        message : message,
        raw     : Some(Arc::clone(&raw_line)),
        meta    : None,
    };

    let Some(gaze) = message.gaze() else {
        return Ok(Some(invalid(Some(message))));
    };

    // Camera frame to desk frame, then through the calibration and onto a panel.
    let raw = Ray {
        origin : camera.point_to_desk(gaze.eye_mm),
        dir    : camera.dir_to_desk(gaze.gaze),
    };

    // Clamping is on: a gaze that wanders off the desk should report where it left, not
    // vanish. The calibration sweep is the one caller that turns it off.
    let resolved     = resolve(geometry, camera, calibration, &raw, true);
    let off_axis_deg = geometry.off_axis_deg(&resolved.ray);

    // The webcam preset never reports lost tracking, but a caller that supplied its own
    // profile may have asked for it, and an unknown sigma is not something to guess at.
    let Some(sigma_deg) = profile.sigma_at(off_axis_deg) else {
        return Ok(Some(invalid(Some(message))));
    };

    let meta = SampleMeta {
        seq          : gaze.seq,
        sidecar_t_s  : gaze.t,
        conf         : gaze.conf,
        lat_ms       : gaze.lat_ms,
        off_axis_deg : off_axis_deg,
        clamped      : resolved.clamped,
    };

    Ok(Some(Reading {
        sample  : GazeSample {
            t_s       : t_s,
            ray       : Some(resolved.ray),
            point     : resolved.point,
            sigma_deg : sigma_deg,
            // A ray aimed entirely away from the desk has no point even after the edge
            // clamp. That is rarer than the two cases the contract names, and it is still
            // a sample the consumer must not use.
            valid     : resolved.point.is_some(),
        },
        message : Some(message),
        raw     : Some(raw_line),
        meta    : Some(meta),
    }))
}

/// Parses one line, updates the status counters, and pushes the resulting sample.
fn handle_line(
    ctx       : &ReaderContext,
    line      : &str,
    sample_tx : &Sender<Reading>,
    status    : &Mutex<Status>,
    start     : Instant,
)
{
    let t_s = start.elapsed().as_secs_f64();

    let reading = {
        match sample_from_line(&ctx.geometry, &ctx.camera, ctx.calibration.as_ref(), &ctx.profile, line, t_s) {
            Ok(Some(r)) => r,

            // A blank line is not a message and not a problem.
            Ok(None) => return,

            Err(e) => {
                tracing::warn!("{e}");

                if let Ok(mut s) = status.lock() {
                    s.lines     += 1;
                    s.bad_lines += 1;
                }

                return;
            }
        }
    };

    if let Ok(mut s) = status.lock() {
        s.lines += 1;

        if let Some(meta) = reading.meta {
            s.last_meta   = Some(meta);
            s.last_output = reading
                .sample
                .point
                .and_then(|p| ctx.geometry.output_at(p))
                .map(|o| o.name.clone());
        }
    }

    send(sample_tx, reading);
}

/// Pushes an invalid sample, with the sigma convention the rest of the workspace uses for
/// "no idea".
fn emit_invalid(
    sample_tx : &Sender<Reading>,
    t_s       : f64,
    message   : Option<SidecarMessage>,
    raw       : Option<Arc<str>>,
)
{
    send(sample_tx, Reading {
        sample  : GazeSample {
            t_s       : t_s,
            ray       : None,
            point     : None,
            sigma_deg : f64::MAX,
            valid     : false,
        },
        message : message,
        raw     : raw,
        meta    : None,
    });
}

/// Sends without ever blocking the reader. A full channel means the consumer has fallen
/// seconds behind, and the newest sample is worth more than the oldest.
fn send(sample_tx: &Sender<Reading>, reading: Reading) {
    if sample_tx.try_send(reading).is_err() {
        tracing::trace!("gaze sample dropped: consumer is not keeping up");
    }
}

/// Sleeps in slices so `stop()` does not have to wait out a full backoff.
fn sleep_interruptibly(total: Duration, stop_flag: &AtomicBool) {
    let mut left = total;

    while left > Duration::ZERO && !stop_flag.load(Ordering::Relaxed) {
        let slice = left.min(SLEEP_SLICE);

        thread::sleep(slice);
        left -= slice;
    }
}

/// Where a corrected sample would land, for a caller that has a ray but no provider.
pub fn point_for_ray(
    geometry    : &DesktopGeometry,
    camera      : &CameraPose,
    calibration : Option<&Calibration>,
    ray         : &Ray,
)
    -> Option<GlobalPx>
{
    resolve(geometry, camera, calibration, ray, true).point
}

// --- Error ---

#[derive(Debug, thiserror::Error)]
pub enum ProviderError {
    #[error("webcam provider needs a desk geometry")]
    MissingGeometry,

    #[error("webcam provider needs a camera pose")]
    MissingCamera,

    #[error("cannot load calibration: {0}")]
    Calibration(#[source] CalibrationError),

    #[error("cannot spawn the sidecar reader thread: {0}")]
    Spawn(#[source] std::io::Error),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use std::io::Write;
    use std::os::unix::net::UnixListener;

    use gaze_core::GlobalPx;
    use glam::DVec3;

    use super::*;
    use crate::protocol::{SidecarGaze, SidecarMessage};

    /// The frozen 2026-08-25 desk snapshot these assertions were written against;
    /// the live `config/desk.toml` drifts with the physical desk.
    const FIXTURE_TOML: &str = include_str!("../../../config/desk-fixture.toml");

    /// Point on the LG used as the thing the fake stream looks at.
    const LOOK_AT: GlobalPx = GlobalPx { x: 4479.0, y: 800.0 };

    fn desk() -> (DesktopGeometry, CameraPose) {
        (
            DesktopGeometry::from_toml(FIXTURE_TOML).unwrap(),
            CameraPose::from_desk_toml(FIXTURE_TOML).unwrap(),
        )
    }

    /// One line aimed at `LOOK_AT`, phrased in the camera frame the way the sidecar would.
    fn line(geometry: &DesktopGeometry, camera: &CameraPose, seq: u64) -> String {
        let ray = geometry.px_to_ray(LOOK_AT).expect("the look-at point is on a panel");

        SidecarMessage::from_gaze(&SidecarGaze {
            t      : seq as f64 / 30.0,
            seq    : seq,
            eye_mm : camera.point_to_camera(ray.origin),
            gaze   : camera.dir_to_camera(ray.dir),
            conf   : 0.75,
            lat_ms : 42.0,
        })
        .to_line()
        .unwrap()
    }

    /// Collects up to `want` valid samples, or gives up after `timeout`.
    fn drain_valid(provider: &mut WebcamProvider, want: usize, timeout: Duration)
        -> Vec<GazeSample>
    {
        let deadline = Instant::now() + timeout;
        let mut out  = Vec::new();

        while out.len() < want && Instant::now() < deadline {
            let Some(sample) = provider.next() else {
                break;
            };

            if sample.valid {
                out.push(sample);
            }
        }

        out
    }

    #[test]
    fn a_sidecar_that_closes_the_socket_is_reconnected_to() {
        let (geometry, camera) = desk();
        let dir    = tempfile::tempdir().unwrap();
        let socket = dir.path().join("gaze-ml.sock");

        // A server that serves two short-lived connections: a few lines each, then close.
        // The provider must come back for the second one on its own.
        let server = {
            let socket   = socket.clone();
            let geometry = geometry.clone();
            let camera   = camera.clone();

            thread::spawn(move || {
                let listener = UnixListener::bind(&socket).unwrap();

                for round in 0..2_u64 {
                    let (mut stream, _) = listener.accept().unwrap();

                    for i in 0..5 {
                        let seq = round * 100 + i;

                        writeln!(stream, "{}", line(&geometry, &camera, seq)).unwrap();
                    }

                    stream.flush().unwrap();
                    drop(stream);
                }
            })
        };

        let mut provider = WebcamProvider::create()
            .socket(&socket)
            .geometry(geometry)
            .camera(camera)
            .calibration(None::<PathBuf>)
            .start()
            .unwrap();

        let samples = drain_valid(&mut provider, 10, Duration::from_secs(10));

        assert!(samples.len() >= 6, "only got {} samples across two connections", samples.len());

        // Every sample must have landed where the stream was aiming.
        for s in &samples {
            let p = s.point.expect("a valid sample carries a point");

            assert!((p.x - LOOK_AT.x).abs() < 1.0 && (p.y - LOOK_AT.y).abs() < 1.0, "landed at {p:?}");
        }

        let stats = provider.stats();
        assert!(stats.connects >= 2, "the provider did not reconnect: {stats:?}");
        assert_eq!(stats.bad_lines, 0);

        provider.stop();
        let _ = server.join();
    }

    #[test]
    fn a_socket_that_does_not_exist_yet_is_waited_for_rather_than_failing() {
        let (geometry, camera) = desk();
        let dir    = tempfile::tempdir().unwrap();
        let socket = dir.path().join("late.sock");

        // Starting against a socket that is not there must succeed: the sidecar coming up
        // later is the normal case at boot.
        let mut provider = WebcamProvider::create()
            .socket(&socket)
            .geometry(geometry.clone())
            .camera(camera.clone())
            .calibration(None::<PathBuf>)
            .start()
            .unwrap();

        assert!(!provider.connected());

        // While disconnected the provider must still be producing invalid samples rather
        // than going silent.
        assert!(provider.next().is_some_and(|s| !s.valid));

        let server = {
            let socket   = socket.clone();
            let geometry = geometry.clone();
            let camera   = camera.clone();

            thread::spawn(move || {
                thread::sleep(Duration::from_millis(150));

                let listener        = UnixListener::bind(&socket).unwrap();
                let (mut stream, _) = listener.accept().unwrap();

                for i in 0..5 {
                    writeln!(stream, "{}", line(&geometry, &camera, i)).unwrap();
                }

                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(200));
            })
        };

        let samples = drain_valid(&mut provider, 3, Duration::from_secs(10));
        assert!(samples.len() >= 3, "the provider did not pick up the late sidecar");

        provider.stop();
        let _ = server.join();
    }

    #[test]
    fn stop_is_prompt_and_idempotent_while_disconnected() {
        let (geometry, camera) = desk();
        let dir = tempfile::tempdir().unwrap();

        let mut provider = WebcamProvider::create()
            .socket(dir.path().join("never.sock"))
            .geometry(geometry)
            .camera(camera)
            .calibration(None::<PathBuf>)
            .start()
            .unwrap();

        // Let it get a couple of failed attempts in, so the stop lands mid-backoff.
        thread::sleep(Duration::from_millis(300));

        let start = Instant::now();
        provider.stop();
        provider.stop();

        assert!(start.elapsed() < Duration::from_secs(1), "stop took {:?}", start.elapsed());
    }

    #[test]
    fn a_malformed_line_is_counted_and_skipped_without_dropping_the_connection() {
        let (geometry, camera) = desk();
        let dir    = tempfile::tempdir().unwrap();
        let socket = dir.path().join("noisy.sock");

        let server = {
            let socket   = socket.clone();
            let geometry = geometry.clone();
            let camera   = camera.clone();

            thread::spawn(move || {
                let listener        = UnixListener::bind(&socket).unwrap();
                let (mut stream, _) = listener.accept().unwrap();

                writeln!(stream, "{{ not json").unwrap();

                for i in 0..5 {
                    writeln!(stream, "{}", line(&geometry, &camera, i)).unwrap();
                }

                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(300));
            })
        };

        let mut provider = WebcamProvider::create()
            .socket(&socket)
            .geometry(geometry)
            .camera(camera)
            .calibration(None::<PathBuf>)
            .start()
            .unwrap();

        let samples = drain_valid(&mut provider, 3, Duration::from_secs(10));

        assert!(samples.len() >= 3, "a bad line must not stop the stream");
        assert_eq!(provider.stats().bad_lines, 1);
        assert_eq!(provider.stats().connects, 1, "a bad line must not force a reconnect");

        provider.stop();
        let _ = server.join();
    }

    #[test]
    fn the_runtime_path_reproduces_the_in_sample_rms_the_fitter_reported() {
        use crate::camera::gaze_yaw_pitch_deg;
        use crate::sweep::{self, AngleSample, SweepTarget};

        let (geometry, camera) = desk();

        // A sweep with a saturating gain, the failure mode that matters, fitted the way
        // `calibrate` fits it.
        let saturate = |a: f64, k: f64| {
            let scale = 45.0_f64.to_radians();

            (scale * (k * a.to_radians() / scale).atan()).to_degrees()
        };

        let observations: Vec<_> = sweep::default_targets(&geometry, 3, 0.12, true)
            .into_iter()
            .filter_map(|target: SweepTarget| {
                let world   = geometry.px_to_world(target.px)?;
                let eye_cam = camera.point_to_camera(geometry.eye());
                let want    = gaze_yaw_pitch_deg(camera.point_to_camera(world) - eye_cam)?;

                // Gentle enough that a sane correction exists, so a model is actually
                // chosen and there is something to reproduce.
                let yaw   = saturate(want.0, 1.3) - 0.1 * want.1;
                let pitch = saturate(want.1, 1.2);

                let sample = AngleSample {
                    eye_cam_mm     : eye_cam,
                    gaze_cam       : crate::camera::gaze_dir_from_yaw_pitch_deg(yaw, pitch),
                    yaw_deg        : yaw,
                    pitch_deg      : pitch,
                    want_yaw_deg   : want.0,
                    want_pitch_deg : want.1,
                    missed         : false,
                    head_rot       : Some([0.0, 0.0, 0.0]),
                    raw            : None,
                };

                sweep::rebuild(&geometry, &camera, target, vec![sample])
            })
            .collect();

        assert!(observations.len() >= 20);

        let report  = sweep::fit(&geometry, &camera, &observations, DEFAULT_SIGMA_DEG);

        assert!(report.chosen.is_some(), "the test sweep must produce a usable model");

        let cal     = report.calibration;
        let profile = webcam_profile(DEFAULT_SIGMA_DEG);

        // Now replay every target through the *runtime* path and measure the same thing.
        // This is what `gaze-webcam-cli check` does, and it is the only evidence that the
        // numbers in a calibration file describe the system that will actually run.
        let mut residuals = Vec::new();

        for obs in &observations {
            let s    = &obs.samples[0];
            let line = SidecarMessage::from_gaze(&SidecarGaze {
                t      : 0.0,
                seq    : 0,
                eye_mm : s.eye_cam_mm,
                gaze   : s.gaze_cam,
                conf   : 1.0,
                lat_ms : 0.0,
            })
            .to_line()
            .unwrap();

            let reading = sample_from_line(&geometry, &camera, Some(&cal), &profile, &line, 0.0)
                .unwrap()
                .expect("a valid line must produce a reading");

            let ray   = reading.sample.ray.expect("a valid sample carries a ray");
            let world = geometry.px_to_world(obs.target.px).unwrap();
            let want  = (world - ray.origin).normalize();

            // The fitter scores without the edge clamp and the runtime applies it, so a
            // ray that misses the desk is scored on its direction by both. See the same
            // rule in `gaze-webcam-cli check`.
            let clamped  = reading.meta.is_some_and(|m| m.clamped);
            let believed = {
                match reading.sample.point.filter(|_| !clamped).and_then(|p| geometry.px_to_world(p)) {
                    Some(w) => (w - ray.origin).normalize(),
                    None    => ray.dir,
                }
            };

            residuals.push(believed.angle_between(want).to_degrees());
        }

        let replayed = (residuals.iter().map(|v| v * v).sum::<f64>() / residuals.len() as f64).sqrt();

        assert!(
            (replayed - cal.rms_deg).abs() < 1.0e-6,
            "runtime path gives {replayed} deg, the fit claimed {}",
            cal.rms_deg,
        );
    }

    #[test]
    fn a_reading_keeps_the_verbatim_line_including_fields_this_crate_ignores() {
        let (geometry, camera) = desk();
        let profile            = webcam_profile(DEFAULT_SIGMA_DEG);

        // A field no version of this crate has ever modelled. Re-serialising the parsed
        // message would lose it, and it is exactly the kind of thing a later experiment
        // wants.
        let line = r#"{"t":1.0,"seq":7,"valid":true,"eye_mm":[0,0,600],"gaze":[0,0,-1],"head_rot":[0.1,0.2,0.3],"conf":0.9,"lat_ms":40,"gaze_iris":[0.01,0.02,-0.99]}"#;

        let reading = sample_from_line(&geometry, &camera, None, &profile, line, 0.0)
            .unwrap()
            .unwrap();

        let raw = reading.raw.expect("the verbatim line must be kept");
        assert!(raw.contains("gaze_iris"), "unknown fields must survive: {raw}");
        assert_eq!(&*raw, line);

        assert_eq!(reading.message.unwrap().head_rot, Some([0.1, 0.2, 0.3]));
    }

    #[test]
    fn the_webcam_sigma_profile_is_flat_and_never_reports_lost_tracking() {
        let p = webcam_profile(DEFAULT_SIGMA_DEG);

        for off in [0.0, 10.0, 25.0, 40.0, 75.0, 120.0] {
            assert_eq!(p.sigma_at(off), Some(DEFAULT_SIGMA_DEG), "off-axis {off}");
        }
    }

    #[test]
    fn the_provider_needs_a_geometry_and_a_camera() {
        let (geometry, camera) = desk();

        assert!(matches!(
            WebcamProvider::create().camera(camera.clone()).start(),
            Err(ProviderError::MissingGeometry),
        ));

        assert!(matches!(
            WebcamProvider::create().geometry(geometry).start(),
            Err(ProviderError::MissingCamera),
        ));

        // A calibration path that is not there is an error at start, not a silent
        // fallback to running uncalibrated.
        let (geometry, camera) = desk();
        assert!(matches!(
            WebcamProvider::create()
                .geometry(geometry)
                .camera(camera)
                .calibration(Some("/nonexistent/calibration.toml"))
                .start(),
            Err(ProviderError::Calibration(_)),
        ));
    }

    #[test]
    fn a_direction_the_desk_cannot_see_produces_an_invalid_sample() {
        let (geometry, camera) = desk();
        let dir    = tempfile::tempdir().unwrap();
        let socket = dir.path().join("away.sock");

        let server = {
            let socket = socket.clone();
            let camera = camera.clone();

            thread::spawn(move || {
                let listener        = UnixListener::bind(&socket).unwrap();
                let (mut stream, _) = listener.accept().unwrap();

                // Looking straight at the user, away from every panel.
                let message = SidecarMessage::from_gaze(&SidecarGaze {
                    t      : 0.0,
                    seq    : 0,
                    eye_mm : camera.point_to_camera(DVec3::new(0.0, 180.0, 650.0)),
                    gaze   : camera.dir_to_camera(DVec3::new(0.0, 0.0, 1.0)),
                    conf   : 0.9,
                    lat_ms : 40.0,
                });

                for _ in 0..5 {
                    writeln!(stream, "{}", message.to_line().unwrap()).unwrap();
                }

                stream.flush().unwrap();
                thread::sleep(Duration::from_millis(300));
            })
        };

        let mut provider = WebcamProvider::create()
            .socket(&socket)
            .geometry(geometry)
            .camera(camera)
            .calibration(None::<PathBuf>)
            .start()
            .unwrap();

        let deadline = Instant::now() + Duration::from_secs(5);

        while Instant::now() < deadline {
            let Some(sample) = provider.next() else {
                break;
            };

            assert!(!sample.valid, "a gaze pointing off the desk must not be reported valid");

            if provider.stats().lines >= 5 {
                break;
            }
        }

        assert!(provider.stats().lines >= 1, "the stream never arrived");

        provider.stop();
        let _ = server.join();
    }
}
