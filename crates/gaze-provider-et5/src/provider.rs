//! The `GazeProvider` implementation: ET5 frames in, `gaze_core::GazeSample`s out.
//!
//! # Frames
//!
//! The device reports in tracker space (origin at its IR array, +X right, +Y up,
//! +Z toward the user). The desk world frame is defined with its origin at the tracker
//! (`config/desk.toml`), so this provider treats tracker space as the world, with one
//! optional correction: the tracker is physically pitched up at the face, and
//! `tracker_pitch_deg` (from `desk.toml`) rotates device vectors into the desk frame.
//! Display poses come from the desk file; nothing solves them from gaze data.
//!
//! # What a sample carries
//!
//! The per-eye ray is `eye_origin -> gaze_point_3d`; both eyes valid averages the two
//! (midpoint origin, mean direction), one eye valid uses it alone with a wider sigma,
//! none makes the sample invalid. `point` is the desk intersection of that ray when it
//! hits a panel.
//!
//! # Device state
//!
//! The host owns the eye model. The blob file (`config/calibration-et5.bin` by
//! default) is uploaded and verified on every connect, and again on every reconnect
//! after a transport error, so a firmware reboot that resets the flash to its factory
//! model cannot quietly change what the stream means. Without the file the provider
//! still starts, loudly, on whatever the device happens to hold.
//!
//! # Losing the link
//!
//! The ET5 has been observed re-enumerating mid-session (a one-second USB drop). The
//! reader thread ends on the transport error and the frame channel disconnects; the
//! provider then drops its dropout hold, emits explicitly invalid samples for the
//! duration of the gap, and reconnects with the same options on a backoff. A stale
//! held point is never presented as live gaze.

use std::path::PathBuf;
use std::time::{Duration, Instant};

use tracing::{info, trace, warn};

use crossbeam_channel::{RecvTimeoutError, TryRecvError};
use glam::{DQuat, DVec3};
use gaze_core::{DesktopGeometry, GazeSample, GlobalPx, OutputGeometry, Ray};
use gaze_core::GazeProvider;

use crate::blob::BlobReport;
use crate::calibration::{Et5Calibration, VIRTUAL_AREA};
use crate::device::{ConnectOptions, Device, DeviceError};
use crate::gaze::{Et5Frame, EyeCombiner, filtered_ray};
use crate::ttp::DisplayArea;

/// Default 1-sigma angular error for binocular ET5 samples, degrees. The spec figure
/// for the consumer trackers this hardware family ships in.
const SIGMA_BINOCULAR_DEG: f64 = 0.7;

/// Sigma when only one eye is tracked, degrees. Monocular output loses vergence
/// averaging and degrades visibly in practice.
const SIGMA_MONOCULAR_DEG: f64 = 1.2;

/// How far past a panel's edge a ray may point and still be clamped to that edge,
/// degrees beyond the panel's own angular extent as seen from the eye. Past this the
/// user is looking at the keyboard or out the window and a pinned marker would be
/// noise. Measured against the corners rather than the centre: a fixed budget from
/// the centre was 25 degrees, and the 27" panel's corners sit 28 degrees from its
/// centre at the desk's viewing distance, so a look just past a corner was refused.
const CLAMP_MARGIN_DEG: f64 = 6.0;

/// Bisection steps for the edge clamp. 24 halvings resolve the boundary to under a
/// hundredth of a degree.
const EDGE_BISECT_STEPS: usize = 24;

/// How far past the panel bounds the trained 2D output is still taken as a look at
/// the edge, panel uv. The firmware does not clamp its 2D output under a plane
/// declared by corners (measured 2026-09-09: values to 1.68 while looking below the
/// panel), so a reading a little past an edge is a fixation on an edge element plus
/// ordinary noise and maps to the edge at ordinary sigma; further out the user is off
/// the panel and the ray path decides. 0.03 is 18 mm across and 10 mm down the 27".
const DIRECT_EDGE_MARGIN: f64 = 0.03;

/// How long a fully lost track keeps producing a held, dead-reckoned point, seconds.
const HOLD_MAX_S: f64 = 0.6;

/// Sigma the hold ramps to by the end of its window, degrees.
const HOLD_SIGMA_DEG: f64 = 3.0;

/// EMA rate for the gaze-point velocity feeding the hold's dead reckoning.
const VEL_ALPHA: f64 = 0.3;

/// Instantaneous speeds above this are saccadic; extrapolating one overshoots
/// wildly, so the velocity estimate zeroes instead.
const VEL_SACCADE_PX_S: f64 = 3000.0;

/// Screen millimetres of gaze motion per millimetre of lateral eye-origin motion
/// during a hold: a head turning toward an off-envelope target reads as origin
/// translation about the neck pivot. Conservative half of the ~viewing-distance over
/// neck-radius kinematic figure, because pure head translation (where the true gaze
/// point holds still) would be over-steered by the full one.
const HEAD_STEER_GAIN: f64 = 3.0;

/// Conventional path of the host-owned calibration blob, uploaded on every connect.
pub const DEFAULT_DEVICE_BLOB_PATH: &str = "config/calibration-et5.bin";

/// Delay before the first reconnect attempt after a transport error. A USB
/// re-enumeration takes about a second, so an immediate retry only wastes an attempt.
const RECONNECT_BACKOFF_MIN: Duration = Duration::from_millis(250);

/// Cap on the reconnect backoff. Long enough not to hammer the bus, short enough that
/// replugging the tracker recovers within a few seconds.
const RECONNECT_BACKOFF_MAX: Duration = Duration::from_secs(5);

/// Spacing of the explicitly invalid samples emitted while the link is down. Roughly
/// the device's own frame period, so consumers see a continuous stream rather than a
/// stall they have to time out on themselves.
const GAP_TICK: Duration = Duration::from_millis(10);

// --- Provider ---

/// A `GazeProvider` streaming from a connected ET5.
pub struct Et5Provider {
    /// The live session, `None` between a transport error and a successful
    /// reconnect. It has to be droppable: the USB interface stays claimed until it
    /// is, and the replacement session cannot claim it.
    device       : Option<Device>,
    frames       : crossbeam_channel::Receiver<Et5Frame>,
    convert      : Converter,
    /// What every connect and reconnect does to the device: the same blob, the same
    /// plane, the same verification.
    options      : ConnectOptions,
    /// When the next reconnect attempt is due.
    next_attempt : Instant,
    /// Current reconnect delay, doubling per failure up to the cap.
    backoff      : Duration,
    /// Consecutive failed reconnect attempts, for the log line.
    attempts     : u32,
    /// Successful connects over the life of the provider, starting at one. Bumped by
    /// every reconnect, so a caller streaming raw frames can notice that the device
    /// went away and came back without watching the log.
    connects     : u64,
    /// Host time of the last invalid sample emitted during a gap, for pacing.
    last_gap     : Instant,
    t0           : Instant,
    stopped      : bool,
}

/// Builder for `Et5Provider`.
pub struct Et5ProviderBuilder {
    geometry          : Option<DesktopGeometry>,
    calibration       : Option<Et5Calibration>,
    tracker_pitch_deg : f64,
    sigma_deg         : f64,
    device_blob       : PathBuf,
}

impl Et5Provider {
    /// Starts building a provider.
    pub fn create() -> Et5ProviderBuilder {
        Et5ProviderBuilder {
            geometry          : None,
            calibration       : None,
            tracker_pitch_deg : 0.0,
            sigma_deg         : SIGMA_BINOCULAR_DEG,
            device_blob       : PathBuf::from(DEFAULT_DEVICE_BLOB_PATH),
        }
    }

    /// The underlying device session, for display-area and calibration operations.
    /// `None` while the link is down and the provider is reconnecting.
    pub fn device_mut(&mut self) -> Option<&mut Device> {
        self.device.as_mut()
    }

    /// Converts one already-received frame, for callers that stream `Et5Frame`s
    /// directly (the retrain ceremony and health check) but want the standard sample
    /// view too.
    pub fn convert(&mut self, frame: &Et5Frame) -> GazeSample {
        let t_s = self.t0.elapsed().as_secs_f64();

        self.convert.sample(frame, t_s)
    }

    /// The next raw device frame, or `None` when `timeout` expires with nothing to
    /// report.
    ///
    /// Same device story as [`next`](GazeProvider::next): a transport error tears the
    /// session down and the reconnect (blob re-uploaded and re-verified) is attempted
    /// from here on the same backoff. The difference is what a gap looks like. `next`
    /// has to keep a consumer's sample stream flowing, so it emits explicitly invalid
    /// samples; a frame recorder has nothing honest to write, so a gap is simply an
    /// absence of frames and every call during one returns `None`.
    ///
    /// Callers that need to know a gap happened watch [`connects`](Self::connects).
    pub fn next_frame(&mut self, timeout: Duration) -> Option<Et5Frame> {
        if self.stopped {
            return None;
        }

        if self.device.is_none() {
            self.reconnect_if_due();

            // Still down: park for the caller's timeout rather than spinning on it.
            if self.device.is_none() {
                std::thread::sleep(timeout.min(GAP_TICK));

                return None;
            }
        }

        match self.frames.recv_timeout(timeout) {
            Ok(frame)                           => Some(frame),
            Err(RecvTimeoutError::Timeout)      => None,
            Err(RecvTimeoutError::Disconnected) => {
                self.link_lost();

                None
            }
        }
    }

    /// Whether the link is up right now. False between a transport error and the
    /// reconnect that follows it.
    pub fn connected(&self) -> bool {
        self.device.is_some()
    }

    /// Whether a calibration was loaded for this provider.
    pub fn calibrated(&self) -> bool {
        self.convert.calibration.is_some()
    }

    /// How many times this provider has had a live session, starting at one. A change
    /// means the link dropped and came back, and everything the device holds (the eye
    /// model above all) was re-declared in between.
    pub fn connects(&self) -> u64 {
        self.connects
    }

    /// Retrieves the on-device calibration blob and reports its identity. `None` while
    /// the link is down or when the retrieve fails.
    ///
    /// Costs a full blob transfer, so this is a start-of-session and post-reconnect
    /// question, not a per-frame one.
    pub fn device_blob_report(&mut self) -> Option<BlobReport> {
        let bytes = self.device.as_mut()?.cal_retrieve().ok()?;

        Some(BlobReport::of(&bytes))
    }

    /// The display area currently declared on the device. `None` while the link is
    /// down or when the query fails.
    pub fn device_display_area(&mut self) -> Option<DisplayArea> {
        self.device.as_mut()?.display_area().ok()
    }
}

// --- GazeProvider ---

impl GazeProvider for Et5Provider {
    fn next(&mut self) -> Option<GazeSample> {
        if self.stopped {
            return None;
        }

        loop {
            // While the link is down the caller gets a paced stream of invalid
            // samples; `gap_sample` only declines when the reconnect just succeeded,
            // and then the loop goes straight back to receiving.
            if self.device.is_none() {
                if let Some(sample) = self.gap_sample(true) {
                    return Some(sample);
                }

                continue;
            }

            match self.frames.recv() {
                Ok(frame) => {
                    let t_s = self.t0.elapsed().as_secs_f64();

                    return Some(self.convert.sample(&frame, t_s));
                }
                Err(_)    => self.link_lost(),
            }
        }
    }

    fn try_next(&mut self) -> Option<GazeSample> {
        if self.stopped {
            return None;
        }

        if self.device.is_none() {
            return self.gap_sample(false);
        }

        match self.frames.try_recv() {
            Ok(frame)                 => {
                let t_s = self.t0.elapsed().as_secs_f64();

                Some(self.convert.sample(&frame, t_s))
            }
            Err(TryRecvError::Empty)        => None,
            Err(TryRecvError::Disconnected) => {
                self.link_lost();

                self.gap_sample(false)
            }
        }
    }

    fn stop(&mut self) {
        // Dropping the device joins its reader thread and releases the interface;
        // that drops the channel sender and unblocks any pending `next`.
        self.device = None;
        self.stopped = true;
    }
}

// --- Reconnection ---

impl Et5Provider {
    /// Tears the session down after a transport error. The dropout hold is discarded
    /// with it: a held point is a guess about where the user was looking a moment ago
    /// and there is no reason to believe it survived a USB drop, so the gap is
    /// reported honestly instead.
    fn link_lost(&mut self) {
        warn!("et5 transport error: link lost, samples are invalid until it is back");

        self.device       = None;
        self.next_attempt = Instant::now() + RECONNECT_BACKOFF_MIN;
        self.backoff      = RECONNECT_BACKOFF_MIN;
        self.attempts     = 0;
        self.convert.forget_hold();
    }

    /// What to hand the caller while the link is down: a reconnect attempt when one
    /// is due, then an invalid sample paced at roughly the device's frame rate.
    /// `None` means the caller should go back to receiving (the link is back) or that
    /// nothing is due yet (`block` false).
    fn gap_sample(&mut self, block: bool) -> Option<GazeSample> {
        if self.reconnect_if_due() {
            return None;
        }

        let due = self.last_gap + GAP_TICK;
        let now = Instant::now();

        if now < due {
            if !block {
                return None;
            }

            std::thread::sleep(due - now);
        }

        self.last_gap = Instant::now();

        Some(GazeSample {
            t_s       : self.t0.elapsed().as_secs_f64(),
            ray       : None,
            point     : None,
            sigma_deg : self.convert.sigma_deg,
            valid     : false,
        })
    }

    /// Attempts a reconnect when one is due. Returns true when the link came back.
    fn reconnect_if_due(&mut self) -> bool {
        if Instant::now() < self.next_attempt {
            return false;
        }

        self.reconnect();

        self.device.is_some()
    }

    /// One reconnect attempt with the connect options this provider started with, so
    /// the blob is re-uploaded and re-verified. A failure schedules the next attempt
    /// and doubles the backoff.
    fn reconnect(&mut self) {
        self.attempts += 1;

        match Device::connect_with(self.options.clone()) {
            Ok(device) => {
                info!(
                    attempts = self.attempts,
                    "et5 reconnected; eye model re-uploaded and verified",
                );

                self.frames   = device.gaze_stream();
                self.device   = Some(device);
                self.backoff  = RECONNECT_BACKOFF_MIN;
                self.attempts = 0;
                self.connects += 1;
            }
            Err(e)     => {
                warn!(
                    attempts = self.attempts,
                    retry_in_ms = self.backoff.as_millis(),
                    "et5 reconnect failed: {e}",
                );

                self.next_attempt = Instant::now() + self.backoff;
                self.backoff      = (self.backoff * 2).min(RECONNECT_BACKOFF_MAX);
            }
        }
    }
}

// --- Et5ProviderBuilder ---

impl Et5ProviderBuilder {
    /// Desk geometry used to intersect rays with the panels. Required.
    pub fn geometry(mut self, geometry: DesktopGeometry) -> Self {
        self.geometry = Some(geometry);

        self
    }

    /// Client-side calibration (`config/calibration-et5.toml`): its display poses
    /// replace the configured ones, and each intersected point runs through the
    /// display's correction field.
    pub fn calibration(mut self, calibration: Option<Et5Calibration>) -> Self {
        self.calibration = calibration;

        self
    }

    /// Rotation about +X mapping device vectors into the desk frame, for a tracker
    /// pitched up at the face. Zero when the tracker sits level with the desk frame.
    pub fn tracker_pitch_deg(mut self, deg: f64) -> Self {
        self.tracker_pitch_deg = deg;

        self
    }

    /// Base 1-sigma angular error reported on binocular samples.
    pub fn sigma_deg(mut self, deg: f64) -> Self {
        self.sigma_deg = deg;

        self
    }

    /// The host-owned calibration blob uploaded to the device on every connect.
    /// Defaults to `DEFAULT_DEVICE_BLOB_PATH`; a missing file is a warning, not an
    /// error, and the session then runs on whatever the flash holds.
    pub fn device_blob(mut self, path: impl Into<PathBuf>) -> Self {
        self.device_blob = path.into();

        self
    }

    /// Connects to the tracker and starts streaming.
    pub fn start(self) -> Result<Et5Provider, DeviceError> {
        let mut geometry = self.geometry.expect("Et5ProviderBuilder requires geometry");

        if let Some(calibration) = &self.calibration {
            calibration.apply_poses(&mut geometry);

            // A display the calibration does not cover keeps a configured pose in a frame
            // that need not match the tracker's (and is certainly stale after a
            // remount); letting it catch rays would steal intersections from the
            // displays that are actually calibrated. The direct-mode display is the
            // exception: its plane is the one declared to the device, and it is the
            // per-eye ray's landing surface whenever the combined 2D drops out at an
            // edge, whether or not the calibration has an entry for it.
            for out in &mut geometry.outputs {
                let direct = calibration.device_output.as_deref() == Some(out.name.as_str());

                if calibration.output(&out.name).is_none() && !direct {
                    out.enabled = false;
                }
            }
        }

        // A direct-mode calibration carries the exact plane the on-device model was
        // trained against; re-declaring it verbatim makes gaze_2d_norm the trained
        // end-to-end mapping (target uv in, reported uv out). Without one, run under
        // the oversized virtual plane so reconstructed rays stay unclamped.
        let direct = self.calibration.as_ref().and_then(|c| {
            Some((c.device_output.clone()?, c.device_area?))
        });

        let area = {
            match &direct {
                Some((_, area)) => *area,
                None            => DisplayArea::from_rect(VIRTUAL_AREA),
            }
        };

        let options = ConnectOptions {
            blob          : load_device_blob(&self.device_blob),
            area          : Some(area),
            double_upload : false,
            check         : Default::default(),
        };

        let device = Device::connect_with(options.clone())?;
        let frames = device.gaze_stream();

        Ok(Et5Provider {
            device       : Some(device),
            frames       : frames,
            options      : options,
            next_attempt : Instant::now(),
            backoff      : RECONNECT_BACKOFF_MIN,
            attempts     : 0,
            connects     : 1,
            last_gap     : Instant::now(),
            convert      : Converter {
                geometry       : geometry,
                calibration    : self.calibration,
                direct_output  : direct.map(|(name, _)| name),
                device_to_desk : DQuat::from_axis_angle(
                    DVec3::X,
                    self.tracker_pitch_deg.to_radians(),
                ),
                sigma_deg      : self.sigma_deg,
                combiner       : EyeCombiner::new(),
                hold           : None,
                origin_log     : std::collections::VecDeque::new(),
            },
            t0           : Instant::now(),
            stopped      : false,
        })
    }
}

// --- Connect helpers ---

/// Reads the host-owned calibration blob, logging its identity. A missing file is the
/// normal state of a machine that has never calibrated, so it warns and returns `None`
/// rather than failing the start; anything else about the file (unreadable, empty) is
/// also only worth a warning, because a blob-less session still tracks.
fn load_device_blob(path: &std::path::Path) -> Option<Vec<u8>> {
    let bytes = {
        match std::fs::read(path) {
            Ok(bytes) => bytes,
            Err(e)    => {
                warn!(
                    path = %path.display(),
                    "no host calibration blob to upload ({e}); the tracker keeps \
                     whatever eye model its flash holds, which is exactly the state \
                     that drifts between sessions. `gaze-et5-cli calibrate` \
                     writes one",
                );

                return None;
            }
        }
    };

    if bytes.is_empty() {
        warn!(path = %path.display(), "host calibration blob is empty, ignoring it");

        return None;
    }

    info!(
        path = %path.display(),
        blob = %BlobReport::of(&bytes),
        "uploading the host calibration blob",
    );

    Some(bytes)
}

// --- Converter ---

/// Frame-to-sample conversion state, separate from the provider so a caller that drains
/// frames itself can run the identical mapping on them.
struct Converter {
    geometry       : DesktopGeometry,
    calibration    : Option<Et5Calibration>,
    /// In direct mode, the display whose trained 2D output drives the point.
    direct_output  : Option<String>,
    device_to_desk : DQuat,
    sigma_deg      : f64,
    combiner       : EyeCombiner,
    /// The last good landing point, for the hold that bridges tracking dropouts.
    hold           : Option<HoldAnchor>,
    /// Recent mean eye origins, so the head correction can be driven by the origin
    /// from the fitted lag ago (the head error trails the head by ~300 ms).
    origin_log     : std::collections::VecDeque<(f64, DVec3)>,
}

/// A good sample's landing point and motion state, kept for dropout bridging.
struct HoldAnchor {
    /// Host time of the sample, seconds.
    t_s    : f64,
    /// Corrected landing point.
    point  : GlobalPx,
    /// Smoothed point velocity, pixels per second.
    vel    : [f64; 2],
    /// Mean eye origin at the sample, when eyes were reported.
    origin : Option<DVec3>,
}

impl Converter {
    /// Builds the `GazeSample` for one frame at host time `t_s`.
    fn sample(&mut self, frame: &Et5Frame, t_s: f64) -> GazeSample {
        if let Some(o) = mean_origin(frame) {
            self.origin_log.push_back((t_s, o));

            while self.origin_log.front().is_some_and(|(t, _)| t_s - t > 1.5) {
                self.origin_log.pop_front();
            }
        }

        // Direct mode: the trained 2D output is panel coordinates; map it straight to
        // pixels and run the correction field. This is the firmware's end-to-end
        // calibrated mapping, the most accurate signal the device produces.
        if let Some(sample) = self.direct_sample(frame, t_s) {
            return sample;
        }

        // The firmware's own filtered ray (its combination and temporal filter) is
        // the best signal when present; the per-eye fusion covers the frames where
        // the combined 2D is invalid or pinned at the declared-area edge. The
        // combiner still sees every frame so its offsets stay warm for handoffs.
        let fused    = self.combiner.combine(frame);
        let firmware = filtered_ray(frame, &VIRTUAL_AREA);

        let (origin, dir, binocular) = {
            match (firmware, &fused) {
                (Some(f), _)       => f,
                (None, Some(f))    => (f.origin_mm, f.dir, f.binocular),
                (None, None)       => return self.hold_sample(frame, t_s),
            }
        };

        let ray = Ray {
            origin : self.device_to_desk * origin,
            dir    : (self.device_to_desk * dir).normalize(),
        };

        let sigma = if binocular { self.sigma_deg } else { SIGMA_MONOCULAR_DEG };

        // Intersect (with the edge clamp as fallback), then run the landing point
        // through the display's correction field.
        let hit_px = self.geometry.intersect(&ray)
            .map(|hit| hit.px)
            .or_else(|| edge_point(&self.geometry, &ray));

        let point = hit_px.map(|px| {
            match (&self.calibration, self.geometry.output_at(px)) {
                (Some(cal), Some(out)) => cal.correct_point(out, px),
                _                      => px,
            }
        });

        if let Some(p) = point {
            self.note_good(t_s, p, mean_origin(frame));
        }

        GazeSample {
            t_s       : t_s,
            ray       : Some(ray),
            point     : point,
            sigma_deg : sigma,
            valid     : true,
        }
    }

    /// The direct-mode sample: trained 2D to pixels, `None` when not in direct mode
    /// or the 2D output is invalid or beyond [`DIRECT_EDGE_MARGIN`] past the panel
    /// (the caller falls back to the ray path). A reading within the margin is a look
    /// at the edge and maps to it.
    fn direct_sample(&mut self, frame: &Et5Frame, t_s: f64) -> Option<GazeSample> {
        let name     = self.direct_output.clone()?;
        let [nx, ny] = frame.gaze_2d_norm?;

        // The firmware reports (-1, -1) when the combined gaze is invalid, and past
        // the bounds when the gaze is off the panel; it does not clamp.
        let allowed = -DIRECT_EDGE_MARGIN..=1.0 + DIRECT_EDGE_MARGIN;

        if !allowed.contains(&nx) || !allowed.contains(&ny) {
            return None;
        }

        let (nx, ny) = (nx.clamp(0.0, 1.0), ny.clamp(0.0, 1.0));

        // The ray still comes from the eye fields, for consumers that reason in
        // angles (off-axis sigma, the snap engine's degree-based radii).
        let ray = self.combiner.combine(frame).map(|fused| Ray {
            origin : self.device_to_desk * fused.origin_mm,
            dir    : (self.device_to_desk * fused.dir).normalize(),
        });

        let origin = mean_origin(frame);
        let out    = self.geometry.outputs.iter().find(|o| o.name == name)?;

        // Head-translation residual first (in uv space), then the correction field.
        let (mut u, mut v) = (nx, ny);

        let gain = self.calibration.as_ref()
            .and_then(|c| c.output(&name))
            .and_then(|e| e.head_gain);

        if let (Some(gain), Some(o)) = (gain, origin) {
            // Drive the correction with the state from the fitted lag ago (zero
            // once the rotation channel carries the signal). The interocular
            // vector is the rotation channel: head roll/yaw as the eyes see it.
            let o = self.origin_near(t_s - gain.lag_s).unwrap_or(o);

            let inter = {
                let l = frame.left_valid().then_some(frame.eye_origin_l_mm).flatten();
                let r = frame.right_valid().then_some(frame.eye_origin_r_mm).flatten();

                match (l, r) {
                    (Some(l), Some(r)) => {
                        let d = DVec3::from_array(r) - DVec3::from_array(l);

                        Some([d.y, d.z, d.length()])
                    }
                    _                  => None,
                }
            };

            (u, v) = gain.apply(u, v, o.to_array(), inter);
            u = u.clamp(0.0, 1.0);
            v = v.clamp(0.0, 1.0);
        }

        let px = out.uv_to_px(u, v);

        // The correction field extrapolates past the panel it was fitted on, and a
        // point pushed over the edge would be claimed by whatever panel's rect lies
        // there. The trained plane is this panel; the point stays on it.
        let point = {
            match &self.calibration {
                Some(cal) => clamp_to_output(out, cal.correct_point(out, px)),
                None      => px,
            }
        };

        let binocular = frame.left_valid() && frame.right_valid();
        let sigma     = if binocular { self.sigma_deg } else { SIGMA_MONOCULAR_DEG };

        self.note_good(t_s, point, origin);

        Some(GazeSample {
            t_s       : t_s,
            ray       : ray,
            point     : Some(point),
            sigma_deg : sigma,
            valid     : true,
        })
    }

    /// Drops the dropout hold and the head-state history. Called when the link goes
    /// away: both describe a moment that is now arbitrarily far in the past, and the
    /// hold in particular would otherwise present a stale point as live gaze the
    /// instant the stream came back.
    fn forget_hold(&mut self) {
        self.hold = None;
        self.origin_log.clear();
    }

    /// The logged origin nearest to time `t`, within a small pairing gap.
    fn origin_near(&self, t: f64) -> Option<DVec3> {
        self.origin_log.iter()
            .min_by(|a, b| (a.0 - t).abs().partial_cmp(&(b.0 - t).abs()).unwrap())
            .filter(|(tn, _)| (tn - t).abs() <= 0.08)
            .map(|(_, o)| *o)
    }

    /// Records a good landing point for the dropout hold, updating the smoothed
    /// velocity estimate from the previous anchor.
    fn note_good(&mut self, t_s: f64, point: GlobalPx, origin: Option<DVec3>) {
        let vel = {
            match &self.hold {
                Some(prev) => {
                    let dt = t_s - prev.t_s;

                    if dt > 0.0 && dt < 0.2 {
                        let vx    = (point.x - prev.point.x) / dt;
                        let vy    = (point.y - prev.point.y) / dt;
                        let speed = (vx * vx + vy * vy).sqrt();

                        if speed > VEL_SACCADE_PX_S {
                            [0.0, 0.0]
                        }
                        else {
                            [
                                prev.vel[0] + VEL_ALPHA * (vx - prev.vel[0]),
                                prev.vel[1] + VEL_ALPHA * (vy - prev.vel[1]),
                            ]
                        }
                    }
                    else {
                        [0.0, 0.0]
                    }
                }
                None       => [0.0, 0.0],
            }
        };

        self.hold = Some(HoldAnchor {
            t_s    : t_s,
            point  : point,
            vel    : vel,
            origin : origin,
        });
    }

    /// The sample produced while no signal path has anything: a brief hold at the
    /// last good point, eased along its recent velocity and steered by whatever eye
    /// origins survive, with sigma ramping up so the snap tier goes coarse. Past the
    /// hold window the sample is honestly invalid.
    fn hold_sample(&mut self, frame: &Et5Frame, t_s: f64) -> GazeSample {
        let invalid = GazeSample {
            t_s       : t_s,
            ray       : None,
            point     : None,
            sigma_deg : self.sigma_deg,
            valid     : false,
        };

        let Some(anchor) = &self.hold else {
            return invalid;
        };

        let dt = t_s - anchor.t_s;

        if dt <= 0.0 || dt > HOLD_MAX_S {
            return invalid;
        }

        // Dead reckoning with a decaying velocity: the marker eases out instead of
        // flying on, and a stale velocity cannot carry it far.
        let ease  = 1.0 - dt / HOLD_MAX_S;
        let mut x = anchor.point.x + anchor.vel[0] * dt * ease;
        let mut y = anchor.point.y + anchor.vel[1] * dt * ease;

        // Eye origins often outlive the gaze during a dropout; a head turning toward
        // an off-envelope target reads as lateral origin motion. Tracker +Y is up,
        // pixel y is down.
        if let (Some(then), Some(now)) = (anchor.origin, mean_origin(frame))
            && let Some(name) = &self.direct_output
            && let Some(out) = self.geometry.outputs.iter().find(|o| o.name == *name)
        {
            let px_per_mm = out.logical_w / out.physical_w_mm;
            let d         = now - then;

            x += HEAD_STEER_GAIN * d.x * px_per_mm;
            y -= HEAD_STEER_GAIN * d.y * px_per_mm;

            x = x.clamp(out.logical_x, out.logical_x + out.logical_w);
            y = y.clamp(out.logical_y, out.logical_y + out.logical_h);
        }

        let ramp = dt / HOLD_MAX_S;

        GazeSample {
            t_s       : t_s,
            ray       : None,
            point     : Some(GlobalPx { x: x, y: y }),
            sigma_deg : SIGMA_MONOCULAR_DEG
                + ramp * (HOLD_SIGMA_DEG - SIGMA_MONOCULAR_DEG),
            valid     : true,
        }
    }
}

/// `p` moved onto `out`'s logical rect: the nearest pixel of the panel, the last
/// pixel on each axis being one short of the far edge as `contains_px` counts it.
fn clamp_to_output(out: &OutputGeometry, p: GlobalPx) -> GlobalPx {
    GlobalPx {
        x : p.x.clamp(out.logical_x, out.logical_x + out.logical_w - 1.0),
        y : p.y.clamp(out.logical_y, out.logical_y + out.logical_h - 1.0),
    }
}

/// Mean of the valid eye origins of a frame, tracker space.
fn mean_origin(frame: &Et5Frame) -> Option<DVec3> {
    let l = frame.left_valid().then_some(frame.eye_origin_l_mm).flatten();
    let r = frame.right_valid().then_some(frame.eye_origin_r_mm).flatten();

    match (l, r) {
        (Some(l), Some(r)) => Some((DVec3::from_array(l) + DVec3::from_array(r)) * 0.5),
        (Some(l), None)    => Some(DVec3::from_array(l)),
        (None, Some(r))    => Some(DVec3::from_array(r)),
        (None, None)       => None,
    }
}

/// Clamps a ray that misses every panel to the nearest point on the nearest panel's
/// edge, so tracing along a display border slides instead of vanishing. Bisects
/// between the panel-centre anchor (which hits) and the miss direction. A panel is a
/// candidate while the ray is within [`CLAMP_MARGIN_DEG`] of its angular extent: the
/// angle to its centre less the angle its farthest corner subtends from that centre.
///
/// A ray that lands on another configured display when carried past the panel's edge
/// is not clamped to that edge: the eyes are on the other display, calibrated or not
/// (`DesktopGeometry::output_beyond`). The portable panel below the tracker's display
/// has no calibration, so it catches no rays, and without this a look at its top rows
/// was a fixation pinned to the bottom edge above it, which started edge scrolls there.
fn edge_point(geometry: &DesktopGeometry, ray: &Ray) -> Option<gaze_core::GlobalPx> {
    let mut candidates: Vec<(f64, DVec3, &gaze_core::OutputGeometry)> = geometry.outputs.iter()
        .filter(|o| o.enabled)
        .map(|o| {
            let centre = o.uv_to_world(0.5, 0.5) - ray.origin;
            let extent = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].iter()
                .map(|(u, v)| (o.uv_to_world(*u, *v) - ray.origin).angle_between(centre))
                .fold(0.0_f64, f64::max);
            let beyond = centre.angle_between(ray.dir) - extent;

            (beyond, centre, o)
        })
        .filter(|(beyond, _, _)| beyond.is_finite())
        .collect();

    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    for (beyond, centre, out) in candidates {
        if beyond.to_degrees() > CLAMP_MARGIN_DEG {
            break;
        }

        // Another display under the extended ray owns the gaze; no edge of this one does.
        if let Some(projected) = out.project_px(ray)
            && let Some(other) = geometry.output_beyond(&out.name, projected)
        {
            trace!(panel = %out.name, on = %other.name, "ray past the edge lands on another display, not clamped");

            continue;
        }

        let anchor = centre.normalize();

        // The anchor must hit for the bisection to have a bracket; an occluded panel
        // centre does not, so try the next panel.
        if geometry.intersect(&Ray { origin: ray.origin, dir: anchor }).is_none() {
            continue;
        }

        let mut lo   = 0.0_f64;
        let mut hi   = 1.0_f64;
        let mut best = None;

        for _ in 0..EDGE_BISECT_STEPS {
            let mid = 0.5 * (lo + hi);
            let dir = anchor.lerp(ray.dir, mid).normalize();

            match geometry.intersect(&Ray { origin: ray.origin, dir: dir }) {
                Some(hit) => {
                    best = Some(hit.px);
                    lo   = mid;
                }
                None      => hi = mid,
            }
        }

        if best.is_some() {
            return best;
        }
    }

    None
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use gaze_core::OutputGeometry;

    fn converter() -> Converter {
        let out = OutputGeometry {
            name          : "DP-9".into(),
            enabled       : true,
            detect        : true,
            logical_x     : 0.0,
            logical_y     : 0.0,
            logical_w     : 1000.0,
            logical_h     : 500.0,
            physical_w_mm : 600.0,
            physical_h_mm : 340.0,
            radius_mm     : 0.0,
            position_mm   : [0.0, 150.0, 0.0],
            yaw_deg       : 0.0,
            pitch_deg     : 0.0,
            roll_deg      : 0.0,
        };

        Converter {
            geometry       : DesktopGeometry {
                eye_mm     : [0.0, 180.0, 650.0],
                tracker_mm : [0.0, 0.0, 0.0],
                outputs    : vec![out],
            },
            calibration    : None,
            direct_output  : Some("DP-9".into()),
            device_to_desk : DQuat::IDENTITY,
            sigma_deg      : SIGMA_BINOCULAR_DEG,
            combiner       : EyeCombiner::new(),
            hold           : None,
            origin_log     : std::collections::VecDeque::new(),
        }
    }

    fn frame_2d(nx: f64, ny: f64) -> Et5Frame {
        Et5Frame {
            validity_l      : Some(0),
            validity_r      : Some(0),
            eye_origin_l_mm : Some([-32.0, 150.0, 620.0]),
            eye_origin_r_mm : Some([32.0, 150.0, 620.0]),
            gaze_2d_norm    : Some([nx, ny]),
            ..Default::default()
        }
    }

    #[test]
    fn a_2d_reading_just_past_the_edge_maps_to_the_edge_at_ordinary_sigma() {
        let mut c = converter();

        let s = c.sample(&frame_2d(1.0 + DIRECT_EDGE_MARGIN * 0.5, 0.4), 0.0);
        let p = s.point.expect("point");

        assert!(s.valid);
        assert_eq!(p.x, 1000.0);
        assert!((p.y - 200.0).abs() < 1e-9);
        assert!(s.sigma_deg < 1.5, "sigma {}", s.sigma_deg);
    }

    /// A field-corrected point past the panel's edge lands on the panel's last pixel,
    /// not on the neighbour whose rect begins there.
    #[test]
    fn a_corrected_point_never_leaves_the_trained_panel() {
        let c   = converter();
        let out = c.geometry.outputs.iter().find(|o| o.name == "DP-9").expect("fixture output");

        let inside = GlobalPx { x: out.logical_x + 10.0, y: out.logical_y + 10.0 };
        let beyond = GlobalPx { x: out.logical_x + out.logical_w + 40.0, y: out.logical_y - 5.0 };

        assert_eq!(clamp_to_output(out, inside), inside);

        let clamped = clamp_to_output(out, beyond);

        assert!(out.contains_px(clamped), "{clamped:?} is off the panel");
        assert_eq!(clamped.x, out.logical_x + out.logical_w - 1.0);
        assert_eq!(clamped.y, out.logical_y);
    }

    #[test]
    fn a_2d_reading_well_past_the_edge_is_left_to_the_ray_path() {
        let mut c = converter();

        let s = c.sample(&frame_2d(1.0 + DIRECT_EDGE_MARGIN * 2.0, 0.4), 0.0);

        // The synthetic frame carries eye origins but no plane points, so the ray
        // path has nothing either: the sample is a dropout, not a point at the edge.
        assert!(!s.valid);
        assert!(s.point.is_none());
    }

    #[test]
    fn dropout_holds_briefly_then_goes_invalid() {
        let mut c = converter();

        let good = c.sample(&frame_2d(0.5, 0.5), 0.0);
        assert!(good.valid);

        // Nothing at all in the frame: held point, wider sigma, still valid.
        let lost = c.sample(&Et5Frame::default(), 0.2);
        assert!(lost.valid);

        let p = lost.point.expect("held point");
        assert!((p.x - 500.0).abs() < 1.0);
        assert!(lost.sigma_deg > SIGMA_BINOCULAR_DEG);

        // Past the hold window the sample is honestly invalid.
        let gone = c.sample(&Et5Frame::default(), 1.0);
        assert!(!gone.valid);
    }

    #[test]
    fn a_lost_link_forgets_the_hold() {
        let mut c = converter();

        assert!(c.sample(&frame_2d(0.5, 0.5), 0.0).valid);

        // Within the hold window a dropout would normally still produce a point.
        c.forget_hold();

        let after = c.sample(&Et5Frame::default(), 0.1);
        assert!(!after.valid);
        assert!(after.point.is_none());
    }
}
