//! The `GazeProvider` implementation: ET5 frames in, `gaze_core::GazeSample`s out.
//!
//! # Frames
//!
//! The device reports in tracker space (origin at its IR array, +X right, +Y up,
//! +Z toward the user). The desk world frame is defined with its origin at the tracker
//! (`config/desk.toml`), so this provider treats tracker space as the world, with one
//! optional correction: the tracker is physically pitched up at the face, and
//! `tracker_pitch_deg` rotates device vectors into the desk frame until the
//! calibration sweep solves display poses in tracker space directly (which makes the
//! correction moot; solved poses live in the same frame as the rays).
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

use std::collections::VecDeque;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use tracing::{info, warn};

use crossbeam_channel::{RecvTimeoutError, TryRecvError};
use glam::{DQuat, DVec3};
use gaze_core::{DesktopGeometry, GazeSample, GlobalPx, Ray};
use gaze_provider_synthetic::GazeProvider;

use crate::blob::BlobReport;
use crate::calibration::{Et5Calibration, VIRTUAL_AREA};
use crate::dataset::{firmware_ray, local_yaw_pitch_deg};
use crate::device::{ConnectOptions, Device, DeviceError};
use crate::gaze::{Et5Frame, EyeCombiner, filtered_ray};
use crate::model::{Features, HeadHistory, Prediction, ResidualModel, correct_direction};
use crate::offset::{ClickFeedback, OffsetParams, OnlineOffset};
use crate::sweep::desk_to_sensor;
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

/// How far back a click looks for the eye, seconds. The label study on the click
/// sessions (`model/label_timing.py`) found the firmware residual flat from 0.3 s before
/// the press to the press itself and rising steeply earlier, as the eye is still
/// arriving; this window matched the collector's own label to within 0.05 degrees.
const CLICK_LOOKBACK_S: f64 = 0.4;

/// Corrected rays kept for click attribution, seconds. Covers the lookback plus the
/// latency between a press on the bus and the call that attributes it.
const RAY_HISTORY_S: f64 = 1.5;

/// Fewest rays a click needs in its window to count. At the ET5's 33 Hz the window
/// holds about thirteen; a window this thin means the eyes were untracked for most
/// of it.
const CLICK_MIN_RAYS: usize = 3;

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
    /// The residual model run on every direct-mode frame, when one is fitted.
    model             : Option<ResidualModel>,
    /// Where the online offset persists, `None` to keep it in memory for the run.
    offset_path       : Option<PathBuf>,
    /// The online offset's tunables.
    offset_params     : OffsetParams,
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
            model             : None,
            offset_path       : None,
            offset_params     : OffsetParams::default(),
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
    /// directly (the calibration sweep) but want the standard sample view too.
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

    /// Whether a residual model is running, and with it the online offset.
    pub fn has_model(&self) -> bool {
        self.convert.model.is_some()
    }

    /// The instant the provider's clock started: every sample's `t_s` is seconds since
    /// this, so a caller stamping events from another thread can share the clock.
    pub fn started_at(&self) -> Instant {
        self.t0
    }

    /// Offers a real click to the online offset: the user pressed a physical button
    /// with the pointer at `px`, at host time `t_s` on this provider's clock. The eye
    /// over the [`CLICK_LOOKBACK_S`] before the press is compared with the clicked
    /// point and the leftover, if believable, nudges the offset every later sample is
    /// corrected by. `None` when there is no model, the point is on no configured
    /// panel, or too few corrected rays fell in the window.
    pub fn observe_click(&mut self, px: GlobalPx, t_s: f64) -> Option<ClickFeedback> {
        self.convert.model.as_mut()?.observe_click(px, t_s)
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

    /// Client-side calibration from a sweep: solved display poses replace the
    /// configured ones, and each intersected point runs through the display's
    /// correction field.
    pub fn calibration(mut self, calibration: Option<Et5Calibration>) -> Self {
        self.calibration = calibration;

        self
    }

    /// Where the online offset persists across runs. `None` (the default) keeps the
    /// offset in memory for the life of the provider.
    pub fn offset_path(mut self, path: Option<PathBuf>) -> Self {
        self.offset_path = path;

        self
    }

    /// Overrides the online offset's gain, clip and gate.
    pub fn offset_params(mut self, params: OffsetParams) -> Self {
        self.offset_params = params;

        self
    }

    /// A fitted residual model (`gaze-et5-cli fit`). Applied to the firmware's ray in
    /// direct mode, in place of the correction field and the head gain; ignored, with
    /// a warning, when its blob hash is not the calibration's or there is no direct
    /// mode to run it in.
    pub fn model(mut self, model: Option<ResidualModel>) -> Self {
        self.model = model;

        self
    }

    /// Rotation about +X mapping device vectors into the desk frame, for a tracker
    /// pitched up at the face. Zero once display poses are solved in tracker space.
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

        // The model's labels were measured against the configured desk, rotated into
        // the sensor frame by the mount pitch, so that is the geometry its corrected
        // ray is intersected with: the copy before any solved pose replaces one.
        let desk = geometry.clone();

        if let Some(calibration) = &self.calibration {
            calibration.apply_poses(&mut geometry);

            // A display the sweep did not cover keeps a configured pose in a frame
            // that need not match the tracker's (and is certainly stale after a
            // remount); letting it catch rays would steal intersections from the
            // displays that are actually calibrated. The direct-mode display is the
            // exception: its plane is the one declared to the device, and it is the
            // per-eye ray's landing surface whenever the combined 2D drops out at an
            // edge, whether or not a sweep ever wrote an entry for it.
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

        let model = self.model.and_then(|model| {
            let wanted = self.calibration.as_ref().and_then(|c| c.device_blob_sha256.clone());

            if model.device_blob_sha256.is_some() && model.device_blob_sha256 != wanted {
                warn!(model = ?model.device_blob_sha256, calibration = ?wanted,
                      "residual model ignored: fitted under a different device blob");

                return None;
            }

            let Some((_, area)) = &direct else {
                warn!("residual model ignored: no direct-mode calibration to run it under");

                return None;
            };

            // The offset is what is left after this model under this blob, so it is
            // keyed to the same hash the model is.
            let offset = {
                match &self.offset_path {
                    Some(path) => OnlineOffset::persisted(
                        self.offset_params, path, model.device_blob_sha256.clone(),
                    ),
                    None       => OnlineOffset::new(
                        self.offset_params, model.device_blob_sha256.clone(),
                    ),
                }
            };

            Some(ModelState::new(model, *area, &desk, offset))
        });

        if let Some(state) = &model {
            let [yaw, pitch] = state.offset.global_deg();

            info!(centers = state.model.centers(),
                  pitch_deg = state.model.tracker_pitch_deg,
                  offset_anchors = state.offset.state().anchors.len(),
                  offset_yaw_deg = yaw, offset_pitch_deg = pitch,
                  "residual model loaded");
        }

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
                model          : model,
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

/// Frame-to-sample conversion state, separate from the provider so the sweep can run
/// the identical mapping on frames it drained itself.
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
    /// from the fitted lag ago (the model's head error trails the head by ~300 ms).
    origin_log     : std::collections::VecDeque<(f64, DVec3)>,
    /// The residual model and what it needs around it, when one is loaded.
    model          : Option<ModelState>,
}

/// A loaded residual model with the frame it predicts in.
struct ModelState {
    model          : ResidualModel,
    /// The plane the firmware's 2D output is declared against, for its ray.
    area           : DisplayArea,
    /// The configured desk, whose panels the corrected ray is intersected with.
    desk           : DesktopGeometry,
    /// The tracker axis in the sensor frame: the nominal eye seen from the tracker.
    axis           : DVec3,
    /// Sensor frame to desk frame: the inverse of the mount pitch the labels used.
    sensor_to_desk : DQuat,
    /// Head states for the lagged features.
    heads          : HeadHistory,
    /// The day's bias on top of the model, fed by real clicks.
    offset         : OnlineOffset,
    /// Half the vector from the left eye to the right, from the last frame that had
    /// both, so a frame with one eye still yields the midpoint (see [`posture_origin`]).
    half_ipd       : Option<DVec3>,
    /// Recent corrected rays in the sensor frame, oldest first: host time, ray origin,
    /// corrected direction and the posture origin the offset was read at. What a click
    /// is attributed against.
    recent         : VecDeque<(f64, DVec3, DVec3, DVec3)>,
}

// --- ModelState ---

impl ModelState {
    /// Wraps a model for the desk it will run on.
    fn new(
        model  : ResidualModel,
        area   : DisplayArea,
        desk   : &DesktopGeometry,
        offset : OnlineOffset,
    )
        -> Self
    {
        let pitch = model.tracker_pitch_deg;
        let axis  = DVec3::from_array(desk_to_sensor((desk.eye() - desk.tracker()).to_array(), pitch));

        Self {
            model          : model,
            area           : area,
            desk           : desk.clone(),
            axis           : axis,
            sensor_to_desk : DQuat::from_axis_angle(DVec3::X, -pitch.to_radians()),
            heads          : HeadHistory::default(),
            offset         : offset,
            half_ipd       : None,
            recent         : VecDeque::new(),
        }
    }

    /// The corrected desk-frame ray for a frame, with the model's prediction, or
    /// `None` when the firmware produced no usable ray. The sensor-frame ray is kept
    /// for click attribution.
    fn correct(&mut self, frame: &Et5Frame, t_s: f64) -> Option<(Ray, Prediction)> {
        let (origin, dir) = firmware_ray(frame, &self.area)?;
        let lagged        = self.heads.lagged(t_s);
        let features      = Features::of_frame(frame, &lagged, self.axis, dir);
        let p             = self.model.predict(&features);

        // Away from the training data the model's part fades to nothing and the ray
        // is the firmware's own; the offset is the day's bias for the head where it is
        // and applies everywhere on the desk. The head is keyed by the binocular
        // midpoint, not the ray origin: a monocular frame moves the latter by half an
        // interpupillary distance and would read as a change of posture.
        let posture = posture_origin(frame, &mut self.half_ipd).unwrap_or(origin);

        let [off_yaw, off_pitch] = self.offset.offset_deg(posture.to_array());
        let corrected = correct_direction(
            dir,
            p.yaw_deg * p.fade + off_yaw,
            p.pitch_deg * p.fade + off_pitch,
        );

        self.recent.push_back((t_s, origin, corrected, posture));

        while self.recent.front().is_some_and(|(t, _, _, _)| t_s - t > RAY_HISTORY_S) {
            self.recent.pop_front();
        }

        let ray = Ray {
            origin : self.sensor_to_desk * origin,
            dir    : (self.sensor_to_desk * corrected).normalize(),
        };

        Some((ray, p))
    }

    /// Attributes a click at `px`, pressed at `t_s`, to the corrected rays just before
    /// it and offers the median leftover to the offset. See
    /// [`Et5Provider::observe_click`].
    fn observe_click(&mut self, px: GlobalPx, t_s: f64) -> Option<ClickFeedback> {
        // The clicked point in the frame the labels were measured in: the configured
        // desk, rotated by the mount pitch, exactly as `dataset::rows` builds targets.
        let pitch  = self.model.tracker_pitch_deg;
        let target = DVec3::from_array(desk_to_sensor(self.desk.px_to_world(px)?.to_array(), pitch));

        // Each ray's leftover: where the corrected ray still points relative to the
        // clicked point, in the tangent frame at the truth, the label's own convention.
        let mut yaws    = Vec::with_capacity(64);
        let mut pitchs  = Vec::with_capacity(64);
        let mut origins = DVec3::ZERO;

        for (t, origin, dir, posture) in &self.recent {
            if *t < t_s - CLICK_LOOKBACK_S || *t > t_s {
                continue;
            }

            let (yaw, pitch) = local_yaw_pitch_deg(*dir, target - origin);

            yaws.push(yaw);
            pitchs.push(pitch);
            origins += *posture;
        }

        if yaws.len() < CLICK_MIN_RAYS {
            return None;
        }

        // The eyes' mean position over the same window keys the click to its posture.
        let leftover = [median(&mut yaws), median(&mut pitchs)];
        let origin   = origins / yaws.len() as f64;

        Some(self.offset.observe(leftover, origin.to_array()))
    }

    /// Drops the ray history, for a link that went away.
    fn forget(&mut self) {
        self.heads.clear();
        self.recent.clear();
    }
}

/// Median of a slice, which it sorts. Empty slices are the caller's problem.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);

    let n = values.len();

    if n % 2 == 1 { values[n / 2] } else { 0.5 * (values[n / 2 - 1] + values[n / 2]) }
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
        if let Some(state) = &mut self.model {
            state.heads.push(t_s, frame);
        }

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

        // With a residual model the firmware's ray is corrected in angle space and
        // intersected with the configured desk; the field and the head gain were
        // fitted on the same firmware error and would double-correct it.
        if let Some(sample) = self.model_sample(frame, t_s) {
            return Some(sample);
        }

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

        let point = {
            match &self.calibration {
                Some(cal) => cal.correct_point(out, px),
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

    /// The model-corrected sample, `None` without a model or when the firmware's 2D
    /// output made no ray (no eye origin).
    fn model_sample(&mut self, frame: &Et5Frame, t_s: f64) -> Option<GazeSample> {
        let state    = self.model.as_mut()?;
        let (ray, p) = state.correct(frame, t_s)?;

        let hit_px = state.desk.intersect(&ray)
            .map(|hit| hit.px)
            .or_else(|| edge_point(&state.desk, &ray));

        let binocular = frame.left_valid() && frame.right_valid();

        // The model's own uncertainty widens the base sigma.
        let base = if binocular { self.sigma_deg } else { SIGMA_MONOCULAR_DEG };

        let sigma = (base * base + p.var_deg2).sqrt();

        if let Some(px) = hit_px {
            self.note_good(t_s, px, mean_origin(frame));
        }

        Some(GazeSample {
            t_s       : t_s,
            ray       : Some(ray),
            point     : hit_px,
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

        if let Some(state) = &mut self.model {
            state.forget();
        }
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

/// The binocular midpoint of a frame, tracker space mm, for keying the offset to the
/// head. Both eyes give it directly and refresh `half_ipd`; one eye gives it through
/// the remembered half spacing, or the eye itself before any binocular frame has been
/// seen; no eyes give nothing.
fn posture_origin(frame: &Et5Frame, half_ipd: &mut Option<DVec3>) -> Option<DVec3> {
    let l = frame.left_valid().then_some(frame.eye_origin_l_mm).flatten().map(DVec3::from_array);
    let r = frame.right_valid().then_some(frame.eye_origin_r_mm).flatten().map(DVec3::from_array);

    match (l, r) {
        (Some(l), Some(r)) => {
            *half_ipd = Some((r - l) * 0.5);

            Some((l + r) * 0.5)
        }
        (Some(l), None) => Some(l + half_ipd.unwrap_or(DVec3::ZERO)),
        (None, Some(r)) => Some(r - half_ipd.unwrap_or(DVec3::ZERO)),
        (None, None)    => None,
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
fn edge_point(geometry: &DesktopGeometry, ray: &Ray) -> Option<gaze_core::GlobalPx> {
    let mut candidates: Vec<(f64, DVec3)> = geometry.outputs.iter()
        .filter(|o| o.enabled)
        .map(|o| {
            let centre = o.uv_to_world(0.5, 0.5) - ray.origin;
            let extent = [(0.0, 0.0), (1.0, 0.0), (0.0, 1.0), (1.0, 1.0)].iter()
                .map(|(u, v)| (o.uv_to_world(*u, *v) - ray.origin).angle_between(centre))
                .fold(0.0_f64, f64::max);
            let beyond = centre.angle_between(ray.dir) - extent;

            (beyond, centre)
        })
        .filter(|(beyond, _)| beyond.is_finite())
        .collect();

    candidates.sort_by(|a, b| a.0.partial_cmp(&b.0).unwrap_or(std::cmp::Ordering::Equal));

    for (beyond, centre) in candidates {
        if beyond.to_degrees() > CLAMP_MARGIN_DEG {
            break;
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
                noise      : None,
            },
            calibration    : None,
            direct_output  : Some("DP-9".into()),
            device_to_desk : DQuat::IDENTITY,
            sigma_deg      : SIGMA_BINOCULAR_DEG,
            combiner       : EyeCombiner::new(),
            hold           : None,
            origin_log     : std::collections::VecDeque::new(),
            model          : None,
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
    fn the_model_state_undoes_the_mount_pitch_the_labels_used() {
        // A desk point rotated into the sensor frame by the exporter's rule must come
        // back to itself through the state's rotation, or the corrected ray would be
        // intersected with a desk it was not measured against.
        let model = ResidualModel {
            format             : crate::model::MODEL_FORMAT,
            created_unix_s     : 0.0,
            device_blob_sha256 : None,
            tracker_pitch_deg  : 13.0,
            features           : vec!["angle_axis_deg".into()],
            explicit           : vec![],
            impute             : vec![0.0],
            mean               : vec![0.0],
            std                : vec![1.0],
            length_scale       : vec![1.0],
            centers            : vec![vec![0.0]],
            kernel_weights     : vec![[0.0, 0.0]],
            explicit_weights   : vec![],
            variance           : vec![vec![0.0]],
            var_fade_lo        : 1.0,
            var_fade_hi        : 2.0,
            report             : None,
        };

        let desk  = converter().geometry;
        let state = ModelState::new(
            model, DisplayArea::from_rect(VIRTUAL_AREA), &desk,
            OnlineOffset::new(OffsetParams::default(), None),
        );

        let p      = DVec3::new(-38.0, 200.0, 687.0);
        let sensor = DVec3::from_array(desk_to_sensor(p.to_array(), 13.0));
        let back   = state.sensor_to_desk * sensor;

        assert!(back.abs_diff_eq(p, 1e-9), "{back:?} vs {p:?}");

        // The axis is the nominal eye seen from the tracker, in the sensor frame.
        let axis = DVec3::from_array(desk_to_sensor((desk.eye() - desk.tracker()).to_array(), 13.0));
        assert!(state.axis.abs_diff_eq(axis, 1e-9));
    }

    /// A converter running a do-nothing model under the fixture desk, so the corrected
    /// ray is the firmware's and every effect seen is the offset's.
    fn converter_with_model() -> Converter {
        let model = ResidualModel {
            format             : crate::model::MODEL_FORMAT,
            created_unix_s     : 0.0,
            device_blob_sha256 : None,
            tracker_pitch_deg  : 0.0,
            features           : vec!["angle_axis_deg".into()],
            explicit           : vec![],
            impute             : vec![0.0],
            mean               : vec![0.0],
            std                : vec![1.0],
            length_scale       : vec![1.0],
            centers            : vec![vec![0.0]],
            kernel_weights     : vec![[0.0, 0.0]],
            explicit_weights   : vec![],
            variance           : vec![vec![0.0]],
            var_fade_lo        : 1.0,
            var_fade_hi        : 2.0,
            report             : None,
        };

        let mut c = converter();
        let area  = DisplayArea::from_rect(VIRTUAL_AREA);

        c.model = Some(ModelState::new(
            model, area, &c.geometry, OnlineOffset::new(OffsetParams::default(), None),
        ));

        c
    }

    #[test]
    fn a_click_on_the_gaze_point_is_accepted_with_no_leftover() {
        let mut c = converter_with_model();

        let mut point = None;

        for i in 0..40 {
            point = c.sample(&frame_2d(0.5, 0.5), i as f64 * 0.01).point;
        }

        let px = point.expect("the model path lands on the panel");
        let fed = c.model.as_mut().unwrap().observe_click(px, 0.4).expect("attributed");

        match fed {
            ClickFeedback::Accepted { leftover_deg, .. } => {
                assert!(leftover_deg[0].abs() < 0.05 && leftover_deg[1].abs() < 0.05,
                        "{leftover_deg:?}");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_click_beside_the_gaze_point_pulls_the_next_sample_towards_it() {
        let mut c = converter_with_model();

        let mut point = None;

        for i in 0..40 {
            point = c.sample(&frame_2d(0.5, 0.5), i as f64 * 0.01).point;
        }

        // 30 px right is 18 mm on this panel, about 1.6 degrees at the fixture's eye:
        // inside the gate, past nothing.
        let before = point.expect("point");
        let click  = GlobalPx { x: before.x + 30.0, y: before.y };

        let fed = c.model.as_mut().unwrap().observe_click(click, 0.4).expect("attributed");

        let ClickFeedback::Accepted { leftover_deg, offset_deg, anchors } = fed else {
            panic!("{fed:?}");
        };

        let size = leftover_deg[0].hypot(leftover_deg[1]);

        assert!((1.3..2.0).contains(&size), "leftover {leftover_deg:?}");
        assert!(leftover_deg[1].abs() < 0.1, "a horizontal click moved pitch: {leftover_deg:?}");

        // The first click at a posture founds its anchor and enters over the prior count.
        let first = leftover_deg[0] / (crate::offset::PRIOR_CLICKS as f64 + 1.0);

        assert_eq!(anchors, 1);
        assert!((offset_deg[0] - first).abs() < 1e-6, "{offset_deg:?} vs {leftover_deg:?}");

        // The same frame now lands to the right of where it did: the offset is live.
        let after = c.sample(&frame_2d(0.5, 0.5), 0.5).point.expect("point");

        assert!(after.x > before.x + 1.0, "before {before:?} after {after:?}");
        assert!((after.y - before.y).abs() < 0.5, "before {before:?} after {after:?}");

        // Well past the gate: refused, and the offset stays put.
        let far = GlobalPx { x: before.x + 200.0, y: before.y };

        assert!(matches!(c.model.as_mut().unwrap().observe_click(far, 0.5),
                         Some(ClickFeedback::Rejected { .. })));
        let held = c.model.as_ref().unwrap().offset.global_deg();

        assert!((held[0] - offset_deg[0]).abs() < 1e-9 && (held[1] - offset_deg[1]).abs() < 1e-9,
                "{held:?} vs {offset_deg:?}");

        // A click with no rays in its window is not attributed at all.
        assert!(c.model.as_mut().unwrap().observe_click(click, 9.0).is_none());
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
