//! A connected tracker: the TTP session on top of `crate::transport`, with a reader
//! thread feeding two consumers. Responses (magic 0x52) answer the request methods
//! here; gaze notifications (magic 0x53, op 0x500) fan out to `gaze_stream`
//! subscribers.
//!
//! # Handshake
//!
//! Connecting runs hello, unlocks the device's privilege realm (an HMAC-MD5 challenge
//! with a key shared by every unit), optionally uploads the host's calibration blob,
//! declares the display plane, enables both eyes, unpauses tracking, verifies the
//! upload, and subscribes to the gaze stream last. The enable step matters: a freshly
//! flashed device can boot with eyes disabled or tracking paused and will stream empty
//! frames forever while looking perfectly healthy.
//!
//! The order is the Windows driver's, recovered from nottobii's pcap-derived init
//! sequence: the blob goes up *before* the plane is declared, and eye-enable and
//! unpause come after it. Talon does the same on every attach. The host owns the eye
//! model; the device's flash is a cache refilled on every connect (`PLAN-ET5.md`,
//! Phase A). [`connect_sequence`] is that order as data, so it can be asserted in a
//! test, and [`Device::connect_with`] executes exactly that list.
//!
//! Realm handling is deliberately per-operation rather than one session spanning the
//! whole handshake: `realm_unlock` is safe to repeat, `cal_apply` unlocks and closes
//! around itself, and the plane, eye and pause ops are not realm guarded (they have
//! always been sent outside an open realm here and the device accepts them). nottobii
//! keeps one session across the whole init instead; nothing observed needs that, so
//! this keeps the smaller change.
//!
//! # Calibration
//!
//! The calibration ops are realm guarded; each entry point re-runs the unlock, which
//! the device accepts repeatedly. `cal_begin` opens a session (without it the device
//! acks and silently discards every added point), `cal_add_point` collects raw samples
//! at a fixation target, `cal_finish` fits and commits the on-device eye model and
//! returns the opaque blob for backup.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, bounded};
use hmac::{Hmac, Mac};
use md5::Md5;
use tracing::{debug, info, warn};

use crate::blob::{BlobCheck, BlobReport};
use crate::gaze::Et5Frame;
use crate::transport::{Transport, TransportError, IN_CHUNK};
use crate::ttp::{
    self, DisplayArea, DisplayRect, Frame, FrameAccumulator, MAGIC_RSP, MAGIC_NOTIFY,
    STREAM_GAZE,
};

/// The realm HMAC key, shared by all ET5 units (17 bytes, trailing NUL included).
const REALM_KEY: &[u8] = b"IS2LJC6GIRBBEK2K\x00";

/// Timeout for ordinary request/response round trips.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(2);

/// Timeout for cal_points_apply, which runs the on-device model fit.
const CAL_APPLY_TIMEOUT: Duration = Duration::from_secs(120);

/// Timeout for calibration blob transfers (605 KB on this unit, over bulk).
const CAL_BLOB_TIMEOUT: Duration = Duration::from_secs(60);

/// Timeout for cal_point_add2d, during which the device collects samples.
const CAL_POINT_TIMEOUT: Duration = Duration::from_secs(20);

/// Gaze channel depth. At 133 Hz this is ~2 s of backlog; when a consumer stalls
/// longer, the oldest frames are dropped so it resumes on fresh data.
const GAZE_CHANNEL_DEPTH: usize = 256;

/// Response channel depth. Only one request is ever in flight, so this only needs to
/// absorb stale responses from timed-out requests.
const RESPONSE_CHANNEL_DEPTH: usize = 32;

/// Enabled-eye mask for binocular tracking.
const BOTH_EYES: u32 = 3;

// --- ConnectOptions ---

/// What a connect should do to the device beyond the bare handshake.
#[derive(Clone, Debug, Default)]
pub struct ConnectOptions {
    /// The host's calibration blob, uploaded before the plane is declared and
    /// verified afterwards. `None` connects blob-less: whatever the flash holds is
    /// what gets used, which is only right for diagnostics and a fresh device.
    pub blob          : Option<Vec<u8>>,
    /// The display plane to declare. `None` leaves the device's current one, which
    /// persists across power cycles.
    pub area          : Option<DisplayArea>,
    /// Send the blob a second time after eye-enable and unpause, as the Windows
    /// driver does. Costs a second ~400 KB transfer.
    pub double_upload : bool,
    /// How the retrieved blob is compared against the uploaded one.
    pub check         : BlobCheck,
}

// --- Step ---

/// One operation of the connect sequence. The ordering of these is the contract this
/// module owes the rest of the crate, so it exists as data and is asserted in a test
/// rather than living only in the shape of a function body.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Step {
    /// Session hello, first frame after the USB session opens.
    Hello,
    /// Unlock the privilege realm guarding the calibration ops.
    RealmUnlock,
    /// Upload the host's calibration blob.
    UploadBlob,
    /// Declare the display plane by corners.
    SetDisplayArea,
    /// Enable both eyes.
    EnableEyes,
    /// Resume tracking.
    Unpause,
    /// Retrieve the blob and compare it against what was uploaded.
    VerifyBlob,
    /// Subscribe to the gaze notification stream.
    Subscribe,
}

/// The ordered operations a connect with these options performs.
///
/// Upload before the plane declaration, eye-enable and unpause after it, the optional
/// second upload after those, verification before the stream starts (a 400 KB inbound
/// transfer is easier to reassemble with no notifications interleaved), subscribe
/// last.
pub fn connect_sequence(options: &ConnectOptions) -> Vec<Step> {
    let mut steps = vec![Step::Hello, Step::RealmUnlock];

    if options.blob.is_some() {
        steps.push(Step::UploadBlob);
    }

    if options.area.is_some() {
        steps.push(Step::SetDisplayArea);
    }

    steps.push(Step::EnableEyes);
    steps.push(Step::Unpause);

    if options.blob.is_some() && options.double_upload {
        steps.push(Step::UploadBlob);
    }

    if options.blob.is_some() {
        steps.push(Step::VerifyBlob);
    }

    steps.push(Step::Subscribe);

    steps
}

// --- Device ---

/// An open, handshaken tracker session.
pub struct Device {
    transport : Arc<Transport>,
    responses : Receiver<Frame>,
    gaze_rx   : Receiver<Et5Frame>,
    stop      : Arc<AtomicBool>,
    reader    : Option<JoinHandle<()>>,
    next_seq  : u32,
}

impl Device {
    /// Opens the tracker and runs the blob-less handshake: no upload, the device's
    /// stored plane left alone. For diagnostics (`info`, `dump`, `blob-info`) and a
    /// fresh device; a real session connects with a blob.
    pub fn connect() -> Result<Self, DeviceError> {
        Self::connect_with(ConnectOptions::default())
    }

    /// Opens the tracker and runs the handshake described by `options`. On return the
    /// gaze stream is live (frames flow into `gaze_stream` receivers), tracking is
    /// enabled, and any uploaded blob has been read back and verified.
    pub fn connect_with(options: ConnectOptions) -> Result<Self, DeviceError> {
        let transport = Arc::new(Transport::open()?);
        let stop      = Arc::new(AtomicBool::new(false));

        let (response_tx, response_rx) = bounded(RESPONSE_CHANNEL_DEPTH);
        let (gaze_tx, gaze_rx)         = bounded(GAZE_CHANNEL_DEPTH);

        let reader = {
            let transport = Arc::clone(&transport);
            let stop      = Arc::clone(&stop);

            // The reader keeps its own receiver clone so it can drop the oldest
            // frame when the queue is full, keeping the stream fresh.
            let drain = gaze_rx.clone();

            std::thread::Builder::new()
                .name("et5-reader".into())
                .spawn(move || reader_loop(&transport, &stop, &response_tx, &gaze_tx, &drain))
                .expect("spawn reader thread")
        };

        let mut device = Self {
            transport : transport,
            responses : response_rx,
            gaze_rx   : gaze_rx,
            stop      : stop,
            reader    : Some(reader),
            next_seq  : 1,
        };

        device.handshake(&options)?;

        Ok(device)
    }

    /// A receiver of decoded gaze frames. Clones share one queue; a slow consumer
    /// steals frames from the others, so hand each consumer its own via one clone.
    pub fn gaze_stream(&self) -> Receiver<Et5Frame> {
        self.gaze_rx.clone()
    }

    /// Reads the display plane the device currently projects 2D gaze onto.
    pub fn display_area(&mut self) -> Result<DisplayArea, DeviceError> {
        let payload = self.request(ttp::get_display_area, REQUEST_TIMEOUT)?;

        ttp::decode_display_area(&payload).ok_or(DeviceError::BadResponse("display_area"))
    }

    /// Declares an axis-aligned display plane. Persists on the device across power
    /// cycles. Fire and forget on the wire; read back with `display_area` to confirm.
    pub fn set_display_area(&mut self, rect: DisplayRect) -> Result<(), DeviceError> {
        let seq = self.take_seq();
        self.transport.send(&ttp::set_display_area(seq, rect))?;

        Ok(())
    }

    /// Declares a display plane by three explicit corners, which is how a panel that
    /// is tilted relative to the tracker is described truthfully.
    pub fn set_display_area_corners(&mut self, area: DisplayArea) -> Result<(), DeviceError> {
        let seq = self.take_seq();
        self.transport.send(&ttp::set_display_area_corners(seq, area))?;

        Ok(())
    }

    /// Selects which eyes are tracked: 1 left, 2 right, 3 both.
    pub fn set_enabled_eyes(&mut self, mask: u32) -> Result<(), DeviceError> {
        let seq = self.take_seq();
        self.transport.send(&ttp::set_u32(seq, ttp::OP_ENABLED_EYE_SET, mask))?;

        Ok(())
    }

    /// Pauses (true) or resumes (false) tracking.
    pub fn set_paused(&mut self, paused: bool) -> Result<(), DeviceError> {
        let seq = self.take_seq();
        self.transport.send(&ttp::set_u32(seq, ttp::OP_PAUSE_SET, paused as u32))?;

        Ok(())
    }

    /// Opens a calibration session: realm unlock, cal_start, then cal_clear to drop
    /// stale points. Eye detection activates in this mode even on an unprovisioned
    /// device.
    pub fn cal_begin(&mut self) -> Result<(), DeviceError> {
        self.realm_unlock()?;
        self.request(ttp::cal_start, REQUEST_TIMEOUT)?;

        // A clear with no collected points can be rejected; that is not a problem.
        if let Err(e) = self.request(ttp::cal_clear, REQUEST_TIMEOUT) {
            debug!("cal_clear rejected (harmless with no prior points): {e}");
        }

        Ok(())
    }

    /// Adds a calibration point at normalised display coordinates while the user
    /// fixates it. Blocks while the device collects raw samples. `eye_mask`: 1 left,
    /// 2 right, 3 both.
    pub fn cal_add_point(&mut self, x: f64, y: f64, eye_mask: u32)
        -> Result<(), DeviceError>
    {
        self.request(|seq| ttp::cal_point_add2d(seq, x, y, eye_mask), CAL_POINT_TIMEOUT)?;

        Ok(())
    }

    /// Folds the points collected since the last apply into the on-device eye model.
    ///
    /// The retrain ceremony calls this after every round rather than once at the end
    /// (Talon's shape): a bad point then poisons one round instead of the whole
    /// model, and each round is collected through the model the previous rounds
    /// already improved. Blocks while the firmware runs its fit.
    pub fn cal_points_apply(&mut self) -> Result<(), DeviceError> {
        self.request(ttp::cal_points_apply, CAL_APPLY_TIMEOUT)?;

        Ok(())
    }

    /// Closes the calibration session, fitting nothing further. What the applies
    /// already committed stays committed; this is how an aborted ceremony leaves the
    /// device in an ordinary streaming state rather than silently discarding points.
    pub fn cal_stop(&mut self) -> Result<(), DeviceError> {
        self.request(ttp::cal_stop, REQUEST_TIMEOUT)?;

        Ok(())
    }

    /// Closes the session and downloads the resulting model for backup. The counterpart
    /// to `cal_points_apply` for a ceremony that has already applied its last round.
    pub fn cal_end(&mut self) -> Result<Vec<u8>, DeviceError> {
        self.cal_stop()?;

        self.cal_retrieve()
    }

    /// Asks the device which calibration point it would like next, returning the raw
    /// response payload. Exploratory (`PLAN-ET5.md` A5): the reply's meaning is
    /// unknown, so nothing may depend on it.
    pub fn cal_point_suggestion(&mut self) -> Result<Vec<u8>, DeviceError> {
        self.request(ttp::cal_point_suggestion, REQUEST_TIMEOUT)
    }

    /// Fits and commits the eye model from the collected points, closes the session,
    /// and returns the opaque calibration blob for backup.
    pub fn cal_finish(&mut self) -> Result<Vec<u8>, DeviceError> {
        self.cal_points_apply()?;

        self.cal_end()
    }

    /// Downloads the current calibration blob without recomputing anything.
    pub fn cal_retrieve(&mut self) -> Result<Vec<u8>, DeviceError> {
        let payload = self.request(ttp::cal_retrieve, CAL_BLOB_TIMEOUT)?;

        Ok(strip_status_prefix(payload))
    }

    /// Uploads a previously downloaded calibration blob.
    pub fn cal_apply(&mut self, blob: &[u8]) -> Result<(), DeviceError> {
        let realm_id = self.realm_unlock()?;
        self.request(|seq| ttp::cal_apply(seq, blob), CAL_BLOB_TIMEOUT)?;
        self.request(|seq| ttp::close_realm(seq, realm_id), REQUEST_TIMEOUT)?;

        Ok(())
    }

    /// Stops the reader thread and closes the USB session. Also runs on drop.
    pub fn close(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(reader) = self.reader.take() {
            let _ = reader.join();
        }
    }
}

impl Device {
    /// Runs the connect-time handshake by executing `connect_sequence`. See the
    /// module docs for why the order is what it is.
    fn handshake(&mut self, options: &ConnectOptions) -> Result<(), DeviceError> {
        for step in connect_sequence(options) {
            self.run_step(step, options)?;
        }

        Ok(())
    }

    /// Performs one handshake step. Steps that need a blob or an area are only
    /// emitted by `connect_sequence` when the option carries one, so the unwraps
    /// here cannot fire.
    fn run_step(&mut self, step: Step, options: &ConnectOptions)
        -> Result<(), DeviceError>
    {
        match step {
            Step::Hello          => {
                self.request(ttp::hello, REQUEST_TIMEOUT)?;
            }
            Step::RealmUnlock    => {
                self.realm_unlock()?;
            }
            Step::UploadBlob     => {
                let blob = options.blob.as_ref().expect("upload step without a blob");

                self.cal_apply(blob)?;
            }
            Step::SetDisplayArea => {
                let area = options.area.expect("plane step without an area");

                self.set_display_area_corners(area)?;
            }
            // Both eyes on, tracking unpaused. Without these a freshly flashed
            // device streams validity=4 frames with the illuminators dark.
            Step::EnableEyes     => self.set_enabled_eyes(BOTH_EYES)?,
            Step::Unpause        => self.set_paused(false)?,
            Step::VerifyBlob     => {
                let blob = options.blob.as_ref().expect("verify step without a blob");

                self.verify_blob(blob, options.check)?;
            }
            // Subscribe has no response; gaze notifications simply start arriving.
            Step::Subscribe      => {
                let seq = self.take_seq();
                self.transport.send(&ttp::subscribe(seq, STREAM_GAZE as u16))?;
            }
        }

        Ok(())
    }

    /// Reads the model back and checks it against what was uploaded. A mismatch is an
    /// error: a device that did not take the blob is a device whose gaze output means
    /// something other than what the host's calibration says it means.
    fn verify_blob(&mut self, expected: &[u8], check: BlobCheck)
        -> Result<(), DeviceError>
    {
        let actual = self.cal_retrieve()?;

        if check.agrees(expected, &actual) {
            info!(
                blob = %BlobReport::of(expected),
                check = ?check,
                "on-device eye model verified after upload",
            );

            return Ok(());
        }

        let expected = BlobReport::of(expected);
        let actual   = BlobReport::of(&actual);

        Err(DeviceError::BlobMismatch {
            expected_sha256 : expected.sha256,
            actual_sha256   : actual.sha256,
            expected_len    : expected.len,
            actual_len      : actual.len,
        })
    }

    /// Unlocks the privilege realm: query type, open, answer the HMAC-MD5 challenge.
    /// Returns the realm id (needed by close_realm). Safe to run repeatedly.
    fn realm_unlock(&mut self) -> Result<u32, DeviceError> {
        let query      = self.request(ttp::query_realm, REQUEST_TIMEOUT)?;
        let realm_type = ttp::realm_u32_at(&query, 0);

        let open = self.request(|seq| ttp::open_realm(seq, realm_type), REQUEST_TIMEOUT)?;

        // Realm type zero needs no authentication; the open response carries the id.
        if realm_type == 0 {
            return Ok(ttp::realm_u32_at(&open, 0));
        }

        let realm_id  = ttp::realm_u32_at(&open, 0);
        let field_210 = ttp::realm_u32_at(&open, 1);
        let challenge = ttp::realm_challenge(&open)
            .ok_or(DeviceError::BadResponse("realm challenge"))?;

        let digest = hmac_md5(REALM_KEY, challenge);
        self.request(|seq| ttp::realm_response(seq, realm_id, field_210, &digest),
                     REQUEST_TIMEOUT)?;

        Ok(realm_id)
    }

    /// Sends one request built by `build` and waits for the response with a matching
    /// sequence number. Stale responses from earlier timed-out requests are discarded.
    fn request(
        &mut self,
        build   : impl FnOnce(u32) -> Vec<u8>,
        timeout : Duration,
    )
        -> Result<Vec<u8>, DeviceError>
    {
        let seq = self.take_seq();
        self.transport.send(&build(seq))?;

        let deadline = Instant::now() + timeout;

        loop {
            let now = Instant::now();
            if now >= deadline {
                return Err(DeviceError::Timeout { seq: seq });
            }

            match self.responses.recv_timeout(deadline - now) {
                Ok(frame) if frame.seq == seq => return Ok(frame.payload),
                Ok(frame)                     => {
                    debug!("dropping stale response seq={} op={:#x}", frame.seq, frame.op);
                }
                Err(_)                        => {
                    return Err(DeviceError::Timeout { seq: seq });
                }
            }
        }
    }

    /// Next request sequence number. Skips zero, mirroring the reference counter.
    fn take_seq(&mut self) -> u32 {
        let seq = self.next_seq;
        self.next_seq = self.next_seq.wrapping_add(1);

        if self.next_seq == 0 {
            self.next_seq = 1;
        }

        seq
    }
}

impl Drop for Device {
    fn drop(&mut self) {
        self.close();
    }
}

// --- Reader thread ---

/// Pulls IN transfers, reassembles frames, and routes them: responses to the request
/// path, gaze notifications to the fan-out channel. Runs until `stop` is set.
fn reader_loop(
    transport : &Transport,
    stop      : &AtomicBool,
    responses : &Sender<Frame>,
    gaze      : &Sender<Et5Frame>,
    drain     : &Receiver<Et5Frame>,
)
{
    let mut buf    = vec![0u8; IN_CHUNK];
    let mut acc    = FrameAccumulator::new();
    let mut frames = Vec::new();

    while !stop.load(Ordering::Relaxed) {
        let n = {
            match transport.recv(&mut buf, Duration::from_millis(100)) {
                Ok(Some(n)) => n,
                Ok(None)    => continue,
                Err(e)      => {
                    warn!("usb read failed, reader stopping: {e}");
                    break;
                }
            }
        };

        if let Err(e) = acc.feed(&buf[..n], &mut frames) {
            warn!("inbound framing error (buffer reset): {e}");
        }

        for frame in frames.drain(..) {
            if frame.magic == MAGIC_RSP {
                // A full queue means nobody is waiting; drop rather than block.
                let _ = responses.try_send(frame);
            }
            else if frame.magic == MAGIC_NOTIFY && frame.op == STREAM_GAZE {
                let Some(decoded) = crate::gaze::decode(&frame.payload) else {
                    continue;
                };

                // Keep the stream fresh under backpressure: drop the oldest.
                if gaze.try_send(decoded).is_err() {
                    let _ = drain.try_recv();
                    let _ = gaze.try_send(decoded);
                }
            }
        }
    }
}

// --- Helpers ---

/// Every response payload starts with a two-byte status prefix; the calibration blob
/// must be stored without it (cal_apply prepends its own).
fn strip_status_prefix(mut payload: Vec<u8>) -> Vec<u8> {
    if payload.len() >= 2 {
        payload.drain(..2);
    }

    payload
}

/// HMAC-MD5 digest of `msg` under `key`.
fn hmac_md5(key: &[u8], msg: &[u8]) -> [u8; 16] {
    let mut mac = <Hmac<Md5> as Mac>::new_from_slice(key).expect("hmac accepts any key size");
    mac.update(msg);

    mac.finalize().into_bytes().into()
}

// --- Errors ---

/// Session failure.
#[derive(Debug, thiserror::Error)]
pub enum DeviceError {
    #[error(transparent)]
    Transport(#[from] TransportError),
    #[error("request seq {seq} timed out")]
    Timeout { seq: u32 },
    #[error("malformed response: {0}")]
    BadResponse(&'static str),
    #[error(
        "the tracker did not take the calibration blob: uploaded {expected_len} bytes \
         (sha256 {expected_sha256}), read back {actual_len} bytes \
         (sha256 {actual_sha256})"
    )]
    BlobMismatch {
        expected_sha256 : String,
        actual_sha256   : String,
        expected_len    : usize,
        actual_len      : usize,
    },
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hmac_md5_matches_rfc_2202() {
        // Vector 1: key = 0x0b * 16, data = "Hi There".
        let d = hmac_md5(&[0x0b; 16], b"Hi There");
        assert_eq!(d, [
            0x92, 0x94, 0x72, 0x7a, 0x36, 0x38, 0xbb, 0x1c,
            0x13, 0xf4, 0x8e, 0xf8, 0x15, 0x8b, 0xfc, 0x9d,
        ]);

        // Vector 2: key = "Jefe", data = "what do ya want for nothing?".
        let d = hmac_md5(b"Jefe", b"what do ya want for nothing?");
        assert_eq!(d, [
            0x75, 0x0c, 0x78, 0x3e, 0x6a, 0xb0, 0xb5, 0x03,
            0xea, 0xa8, 0x6e, 0x31, 0x0a, 0x5d, 0xb7, 0x38,
        ]);
    }

    /// Options carrying a blob and a plane, the shape a real session uses.
    fn full_options() -> ConnectOptions {
        ConnectOptions {
            blob          : Some(vec![0xab; 64]),
            area          : Some(DisplayArea {
                tl_mm : [-300.0, 200.0, 0.0],
                tr_mm : [300.0, 200.0, 0.0],
                bl_mm : [-300.0, -140.0, 0.0],
            }),
            double_upload : false,
            check         : BlobCheck::Exact,
        }
    }

    #[test]
    fn blob_less_connect_touches_neither_the_model_nor_the_plane() {
        assert_eq!(connect_sequence(&ConnectOptions::default()), vec![
            Step::Hello,
            Step::RealmUnlock,
            Step::EnableEyes,
            Step::Unpause,
            Step::Subscribe,
        ]);
    }

    #[test]
    fn connect_uploads_before_the_plane_and_subscribes_last() {
        assert_eq!(connect_sequence(&full_options()), vec![
            Step::Hello,
            Step::RealmUnlock,
            Step::UploadBlob,
            Step::SetDisplayArea,
            Step::EnableEyes,
            Step::Unpause,
            Step::VerifyBlob,
            Step::Subscribe,
        ]);
    }

    #[test]
    fn the_second_upload_lands_after_eye_enable_and_unpause() {
        let steps = connect_sequence(&ConnectOptions {
            double_upload : true,
            ..full_options()
        });

        assert_eq!(steps, vec![
            Step::Hello,
            Step::RealmUnlock,
            Step::UploadBlob,
            Step::SetDisplayArea,
            Step::EnableEyes,
            Step::Unpause,
            Step::UploadBlob,
            Step::VerifyBlob,
            Step::Subscribe,
        ]);
    }

    #[test]
    fn a_blob_with_no_plane_still_uploads_and_verifies() {
        let steps = connect_sequence(&ConnectOptions { area: None, ..full_options() });

        assert!(!steps.contains(&Step::SetDisplayArea));
        assert_eq!(steps.iter().filter(|s| **s == Step::UploadBlob).count(), 1);
        assert!(steps.contains(&Step::VerifyBlob));
    }

    #[test]
    fn a_plane_with_no_blob_never_verifies() {
        let steps = connect_sequence(&ConnectOptions { blob: None, ..full_options() });

        assert!(steps.contains(&Step::SetDisplayArea));
        assert!(!steps.contains(&Step::UploadBlob));
        assert!(!steps.contains(&Step::VerifyBlob));
    }

    #[test]
    fn realm_key_has_reference_shape() {
        // 16 characters plus the explicit trailing NUL the reference key carries.
        assert_eq!(REALM_KEY.len(), 17);
        assert_eq!(REALM_KEY[16], 0);
    }
}
