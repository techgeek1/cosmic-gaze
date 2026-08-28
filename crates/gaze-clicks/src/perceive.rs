//! Perception: two threads, one that must never be slow and one that is.
//!
//! # Why recognition runs on the whole frame
//!
//! The first version recognised a ±256 px crop around the pointer, on the reasoning
//! that a click only ever lands on something under the pointer and a crop costs tens of
//! milliseconds against the whole frame's few hundred. It measured badly enough to be
//! worth writing down.
//!
//! Take one capture of DP-2 (Discord plus a browser), detect it whole as the reference,
//! then cut a crop out of **those same pixels** around each of the twelve largest
//! widgets and detect each crop. No drift, no timing, the same detector and settings on
//! both sides. The crop's smallest-containing pick agreed with the full frame **0 times
//! out of 12**, at 512 px and again at 640 px. Every one came back as the widget's own
//! inner OCR text, or as nothing at all.
//!
//! The losses are the wide flat widgets: Discord's channel and member rows (~290x45),
//! the browser's URL bar (`Input 636x35`), a link. Small square widgets (server icons,
//! toolbar buttons) survive cropping fine, which is why the first version looked like it
//! worked. So the failure is not resolution, it is **context**: the widget model needs
//! the surrounding layout to know that a run of text with padding either side is a row
//! rather than a label. That is exactly what the user saw as "it's just locking onto the
//! text and icons".
//!
//! Whole-frame recognition through this module's own path, checked live against a fresh
//! reference, agrees 11 or 12 times out of 12; the remaining miss is the screen having
//! changed between the reference and the check, which two captures a second apart put at
//! about one widget in thirty on a working desktop.
//!
//! So the detector gets the whole captured output. The ±`luma_half_px` window around
//! the pointer survives only to measure [`crate::element::mean_luma`], the pupil
//! covariate. `examples/recognition_check.rs` is the measurement, kept runnable.
//!
//! # Why two threads
//!
//! A full-frame detection is 250 to 400 ms on the ultrawide. A press capture has to
//! happen within tens of milliseconds of the press, and a double click's second press
//! arrives while the first click is still being recognised. One thread cannot do both.
//!
//! - The **capture thread** owns `Capture` and `CursorTracker`. It polls the pointer,
//!   takes press and rolling captures, and chooses which frame a click is recognised
//!   from (the capture times live here, so the choice does too). **Nothing on it may
//!   block for longer than one capture**, which is the invariant that keeps a press
//!   capture prompt. Handing work to the detector is a `try_send` on a bounded queue,
//!   never a blocking one: see [`dispatch`].
//! - The **detect thread** owns the `Detector` and is fed already-chosen frames. It is
//!   allowed to be slow. It also builds the models, so a missing model file is reported
//!   from the thread that will use them.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, RecvTimeoutError, Sender, TrySendError, select};
use gaze_capture::{Capture, CursorTracker, Frame};
use gaze_core::{Element, GlobalPx};
use gaze_detect::Detector;
use tracing::{debug, warn};

use crate::element::{Crop, crop_around, extract, mean_luma};
use crate::frames::{CachedFrame, FrameChoice, RollingCache, select_frame};

/// How often the pointer is polled, hertz. The pointer only has to be accurate at the
/// moment of a press, and a press polls it again on the spot, so this is really the
/// resolution of the history a release looks back through.
pub const POINTER_HZ: f64 = 50.0;

/// How much pointer history is kept, seconds. Long enough to cover the longest press
/// that is still a click, with room for the release to be looked up afterwards.
const POINTER_SPAN_S: f64 = 3.0;

/// Frames kept per output in the rolling fallback cache.
const ROLLING_KEEP: usize = 2;

/// Press captures held waiting for a recognition request. Each is a whole output
/// framebuffer (about 25 MB for the ultrawide), so this is a memory figure as much as
/// a queue depth; three covers a double click with one still in flight.
const PRESS_KEEP: usize = 3;

/// Frames the detect thread may have queued behind the one it is working on.
///
/// The same figure as [`PRESS_KEEP`] and for the same reason: each queued job holds a
/// whole framebuffer, and clicking faster than the detector can keep up is a burst that
/// ends, not a rate to buffer for. A full queue costs the newest click, not the
/// collector's responsiveness.
const DETECT_QUEUE: usize = PRESS_KEEP;

/// Everything the perception threads need before they start.
#[derive(Clone, Debug)]
pub struct PerceptionConfig {
    /// Directory holding the ONNX models.
    pub models_dir   : PathBuf,
    /// Rolling fallback capture rate, hertz. The press-triggered capture is the frame
    /// a click is normally recognised from, so this only has to be often enough that a
    /// stalled one has something within [`crate::frames::STALE_FRAME_S`].
    pub capture_hz   : f64,
    /// Half-width of the window the screen luminance is averaged over, logical pixels.
    /// Recognition sees the whole frame; this is only the pupil covariate's window.
    pub luma_half_px : f64,
    /// The collector's clock, which every time in this module is measured against.
    pub t0           : Instant,
}

/// One recognition request from the collector.
#[derive(Clone, Debug, PartialEq)]
pub struct DetectRequest {
    /// Echoed back on the reply so the collector can match it.
    pub id         : u64,
    /// The capture the press fired, when the reader managed to fire one.
    pub capture_id : Option<u64>,
    /// Connector the click landed on, for the rolling fallback.
    pub output     : String,
    /// Where the pointer was at the press, global logical pixels. The luminance window
    /// centres here and the elements are hit-tested against it.
    pub px         : GlobalPx,
    pub t_press    : f64,
    pub t_release  : f64,
}

/// What came back from a recognition request.
#[derive(Clone, Debug)]
pub enum DetectOutcome {
    /// The frame was recognised. `elements` may still be empty, which is a click on
    /// nothing rather than a failure.
    Found {
        /// Every element on the whole output, in global logical pixels.
        elements  : Vec<Element>,
        /// Mean luminance of the window around the pointer, [0, 1].
        crop_luma : f64,
        /// Which frame it came from and how old that frame was.
        choice    : FrameChoice,
    },
    /// No usable frame: the press capture was late or failed and the fallback was
    /// stale or absent.
    Stale,
    /// A frame was chosen but the pointer was not on it, which means the pointer
    /// crossed to another output between the capture and the request.
    OffFrame,
    /// The detector was still busy with [`DETECT_QUEUE`] earlier frames. Clicking
    /// faster than recognition runs costs the newest click; the alternative is making
    /// the capture thread wait, which would cost the next press its capture.
    Overrun,
    /// The detector itself failed.
    Failed(String),
}

/// One reply from the perception threads.
#[derive(Clone, Debug)]
pub struct DetectReply {
    /// The request's id.
    pub id      : u64,
    pub outcome : DetectOutcome,
}

/// One pointer reading.
#[derive(Clone, Debug, PartialEq)]
pub struct PointerSample {
    /// Host time the reading was taken, seconds since the collector started.
    pub t_s    : f64,
    pub global : GlobalPx,
    /// Connector the pointer was on.
    pub output : String,
}

/// The last few seconds of pointer readings.
#[derive(Debug)]
pub struct PointerHistory {
    /// Oldest first.
    samples : VecDeque<PointerSample>,
    /// How much history to hold, seconds.
    span_s  : f64,
}

/// Handle to the running perception threads.
pub struct Perception {
    requests : Sender<DetectRequest>,
    replies  : Receiver<DetectReply>,
    pointer  : Arc<Mutex<PointerHistory>>,
    stop     : Arc<AtomicBool>,
    /// The capture thread, joined first: dropping its state closes the job queue,
    /// which is what tells the detect thread to finish.
    capture  : Option<JoinHandle<()>>,
    detect   : Option<JoinHandle<()>>,
}

/// One frame on its way to the detector, with everything already decided about it.
///
/// The frame is owned rather than borrowed because it crosses a thread: a press frame
/// is moved out of the store (one press is one recognition) and a rolling frame is
/// cloned, since it stays in the cache as a fallback for the next click. A 25 MB clone
/// is a few milliseconds of memcpy on the rare path.
struct DetectJob {
    request : DetectRequest,
    frame   : Frame,
    choice  : FrameChoice,
    /// The luminance window around the pointer, in the frame's buffer pixels.
    luma    : Crop,
}

/// The capture thread's own state, split out so the loop body reads as a sequence of
/// steps.
struct Capturing {
    capture      : Capture,
    tracker      : CursorTracker,
    config       : PerceptionConfig,
    pointer      : Arc<Mutex<PointerHistory>>,
    /// The slow fallback cache.
    rolling      : RollingCache,
    /// Press captures waiting to be recognised.
    press        : HashMap<u64, CachedFrame>,
    /// Insertion order of `press`, for eviction.
    press_order  : VecDeque<u64>,
    /// When the next rolling capture is due.
    next_rolling : Instant,
    /// Where chosen frames go. Bounded, and only ever written with `try_send`.
    jobs         : Sender<DetectJob>,
    /// Where this thread answers a request the detector never sees.
    replies      : Sender<DetectReply>,
}

// --- PointerHistory ---

impl PointerHistory {
    /// A history holding `span_s` seconds of readings.
    pub fn new(span_s: f64) -> PointerHistory {
        PointerHistory {
            samples : VecDeque::new(),
            span_s  : span_s,
        }
    }

    /// Appends a reading and drops everything older than the span.
    pub fn push(&mut self, sample: PointerSample) {
        let cutoff = sample.t_s - self.span_s;

        self.samples.push_back(sample);

        while self.samples.front().is_some_and(|s| s.t_s < cutoff) {
            self.samples.pop_front();
        }
    }

    /// The reading closest in time to `t_s`.
    ///
    /// Closest rather than latest-before: a press is stamped by the reader thread and
    /// the pointer by this one, so which of the two is a millisecond ahead is not
    /// something either can rely on.
    pub fn nearest(&self, t_s: f64) -> Option<&PointerSample> {
        self.samples
            .iter()
            .min_by(|a, b| (a.t_s - t_s).abs().total_cmp(&(b.t_s - t_s).abs()))
    }

    /// The most recent reading.
    pub fn latest(&self) -> Option<&PointerSample> {
        self.samples.back()
    }
}

// --- Perception ---

impl Perception {
    /// Starts both threads.
    ///
    /// Returns once the models are loaded and the compositor connections are up, so an
    /// error here means the collector cannot run at all. `presses` carries capture ids
    /// from the mouse reader; every id that arrives on it starts a capture at once.
    pub fn spawn(config: PerceptionConfig, presses: Receiver<u64>) -> Result<Perception> {
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx)     = crossbeam_channel::unbounded();
        let (job_tx, job_rx)         = crossbeam_channel::bounded(DETECT_QUEUE);

        let stop    = Arc::new(AtomicBool::new(false));
        let pointer = Arc::new(Mutex::new(PointerHistory::new(POINTER_SPAN_S)));

        // The detector is built on the thread that runs it. That sidesteps any
        // question about moving an `ort` session across threads, and puts the model
        // load where its failure belongs.
        let detect = spawn_detect(&config, job_rx, reply_tx.clone(), Arc::clone(&stop))?;

        let capture = spawn_capture(
            config, presses, request_rx, reply_tx, job_tx, Arc::clone(&pointer),
            Arc::clone(&stop),
        )?;

        Ok(Perception {
            requests : request_tx,
            replies  : reply_rx,
            pointer  : pointer,
            stop     : stop,
            capture  : Some(capture),
            detect   : Some(detect),
        })
    }

    /// Where to send recognition requests.
    pub fn requests(&self) -> &Sender<DetectRequest> {
        &self.requests
    }

    /// Where replies arrive.
    pub fn replies(&self) -> &Receiver<DetectReply> {
        &self.replies
    }

    /// The pointer reading closest to `t_s`.
    pub fn pointer_at(&self, t_s: f64) -> Option<PointerSample> {
        self.pointer.lock().ok()?.nearest(t_s).cloned()
    }

    /// The most recent pointer reading.
    pub fn pointer_now(&self) -> Option<PointerSample> {
        self.pointer.lock().ok()?.latest().cloned()
    }

    /// Stops both threads and waits for them. Idempotent.
    ///
    /// Can block for as long as one capture plus one full-frame detection, because
    /// neither can be interrupted once started. The capture thread is joined first:
    /// dropping its state closes the job queue, which ends the detect thread's wait.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(join) = self.capture.take() {
            let _ = join.join();
        }

        if let Some(join) = self.detect.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Perception {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- Detect thread ---

/// Builds the detector and starts the thread that runs it.
fn spawn_detect(
    config  : &PerceptionConfig,
    jobs    : Receiver<DetectJob>,
    replies : Sender<DetectReply>,
    stop    : Arc<AtomicBool>,
)
    -> Result<JoinHandle<()>>
{
    let (ready_tx, ready_rx) = mpsc::channel();
    let models_dir           = config.models_dir.clone();

    let join = thread::Builder::new()
        .name("gaze-clicks-detect".to_string())
        .spawn(move || {
            let loaded = Detector::load(&models_dir)
                .with_context(|| format!("loading models from {}", models_dir.display()));

            let detector = {
                match loaded {
                    Ok(detector) => {
                        let _ = ready_tx.send(Ok(()));

                        detector
                    }

                    Err(e) => {
                        let _ = ready_tx.send(Err(e));

                        return;
                    }
                }
            };

            run_detect(&detector, &jobs, &replies, &stop);
        })
        .context("spawning the detect thread")?;

    ready_rx
        .recv()
        .map_err(|_| anyhow!("detect thread died during startup"))??;

    Ok(join)
}

/// Recognises whole frames until the job queue closes.
fn run_detect(
    detector : &Detector,
    jobs     : &Receiver<DetectJob>,
    replies  : &Sender<DetectReply>,
    stop     : &AtomicBool,
)
{
    // A timeout rather than a plain `recv` so a stop between jobs is noticed even
    // while the capture thread is mid-capture and has not dropped its sender yet.
    while !stop.load(Ordering::Relaxed) {
        match jobs.recv_timeout(Duration::from_millis(100)) {
            Ok(job) => {
                let outcome = recognise(detector, &job);

                let _ = replies.send(DetectReply { id: job.request.id, outcome: outcome });
            }

            Err(RecvTimeoutError::Timeout)      => continue,

            // The capture thread went away and took the queue with it.
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }

    debug!("detect thread exiting");
}

/// Runs the detector over one whole frame and measures the pointer's luminance window.
fn recognise(detector: &Detector, job: &DetectJob) -> DetectOutcome {
    let frame  = &job.frame;
    let origin = GlobalPx { x: frame.logical.x, y: frame.logical.y };

    let detected = detector.detect(
        &frame.rgba,
        frame.width,
        frame.height,
        origin,
        frame.scale(),
    );

    // The luminance window is a small copy out of the same buffer; NaN rather than a
    // failure if the crop and the frame disagree, because the click is still a label.
    let pixels    = extract(&frame.rgba, frame.width, &job.luma);
    let crop_luma = {
        if pixels.is_empty() {
            f64::NAN
        }
        else {
            mean_luma(&pixels)
        }
    };

    match detected {
        Ok(elements) => DetectOutcome::Found {
            elements  : elements,
            crop_luma : crop_luma,
            choice    : job.choice,
        },
        Err(e)       => DetectOutcome::Failed(e.to_string()),
    }
}

// --- Capture thread ---

/// Opens the compositor connections and starts the capture thread.
fn spawn_capture(
    config   : PerceptionConfig,
    presses  : Receiver<u64>,
    requests : Receiver<DetectRequest>,
    replies  : Sender<DetectReply>,
    jobs     : Sender<DetectJob>,
    pointer  : Arc<Mutex<PointerHistory>>,
    stop     : Arc<AtomicBool>,
)
    -> Result<JoinHandle<()>>
{
    let (ready_tx, ready_rx) = mpsc::channel();

    let join = thread::Builder::new()
        .name("gaze-clicks-capture".to_string())
        .spawn(move || {
            let started = connect(config, pointer, jobs, replies.clone());

            let mut state = {
                match started {
                    Ok(state) => {
                        let _ = ready_tx.send(Ok(()));

                        state
                    }

                    Err(e) => {
                        let _ = ready_tx.send(Err(e));

                        return;
                    }
                }
            };

            run_capture(&mut state, &presses, &requests, &stop);
        })
        .context("spawning the capture thread")?;

    ready_rx
        .recv()
        .map_err(|_| anyhow!("capture thread died during startup"))??;

    Ok(join)
}

/// Opens everything the capture thread owns. Split out so startup errors reach the
/// caller before the loop begins.
fn connect(
    config  : PerceptionConfig,
    pointer : Arc<Mutex<PointerHistory>>,
    jobs    : Sender<DetectJob>,
    replies : Sender<DetectReply>,
)
    -> Result<Capturing>
{
    let capture = Capture::connect()
        .context("connecting to the compositor for screen capture")?;

    let tracker = CursorTracker::connect()
        .context("connecting to the compositor for pointer position")?;

    Ok(Capturing {
        capture      : capture,
        tracker      : tracker,
        config       : config,
        pointer      : pointer,
        rolling      : RollingCache::new(ROLLING_KEEP),
        press        : HashMap::new(),
        press_order  : VecDeque::new(),
        next_rolling : Instant::now(),
        jobs         : jobs,
        replies      : replies,
    })
}

/// The capture loop. Presses and requests wake it immediately; otherwise it ticks at
/// [`POINTER_HZ`] to poll the pointer and take the occasional fallback frame.
///
/// Every arm is bounded by one screen capture. Nothing here waits on the detector.
fn run_capture(
    state    : &mut Capturing,
    presses  : &Receiver<u64>,
    requests : &Receiver<DetectRequest>,
    stop     : &AtomicBool,
)
{
    let tick = Duration::from_secs_f64(1.0 / POINTER_HZ);

    while !stop.load(Ordering::Relaxed) {
        select! {
            recv(presses) -> id => {
                if let Ok(id) = id {
                    press_capture(state, id);
                }
            }

            recv(requests) -> request => {
                if let Ok(request) = request {
                    // `select!` picks at random among ready channels, so a press whose
                    // capture has not run yet could otherwise be recognised from the
                    // fallback. Draining first makes the ordering the obvious one.
                    for id in presses.try_iter() {
                        press_capture(state, id);
                    }

                    hand_off(state, request);
                }
            }

            default(tick) => {
                poll_pointer(state);
                rolling_capture(state);
            }
        }
    }

    debug!("capture thread exiting");
}

/// Reads the pointer and files the reading. Returns the reading it took.
fn poll_pointer(state: &mut Capturing) -> Option<PointerSample> {
    // `position` is what pumps the Wayland queue; `last_report` is the same reading
    // with the connector name attached, which the capture path needs.
    if let Err(e) = state.tracker.position() {
        warn!(error = %e, "pointer poll failed");

        return None;
    }

    let report = state.tracker.last_report()?;

    let sample = PointerSample {
        t_s    : state.config.t0.elapsed().as_secs_f64(),
        global : report.global,
        output : report.output,
    };

    if let Ok(mut history) = state.pointer.lock() {
        history.push(sample.clone());
    }

    Some(sample)
}

/// Captures the output under the pointer for a press.
///
/// The pointer is re-read first: this runs within a millisecond or two of the press
/// and the last scheduled poll may be a whole tick old.
fn press_capture(state: &mut Capturing, id: u64) {
    let output = poll_pointer(state)
        .or_else(|| state.pointer.lock().ok()?.latest().cloned())
        .map(|sample| sample.output);

    let Some(output) = output else {
        warn!("a press arrived with the pointer on no known output");

        return;
    };

    let result   = state.capture.capture_output(&output);
    let done_t_s = state.config.t0.elapsed().as_secs_f64();

    match result {
        Ok(frame) => store_press(state, id, CachedFrame { frame: frame, done_t_s: done_t_s }),
        Err(e)    => warn!(output = %output, error = %e, "press capture failed"),
    }
}

/// Files a press capture, evicting the oldest when the store is full.
fn store_press(state: &mut Capturing, id: u64, cached: CachedFrame) {
    state.press.insert(id, cached);
    state.press_order.push_back(id);

    while state.press_order.len() > PRESS_KEEP {
        if let Some(old) = state.press_order.pop_front() {
            state.press.remove(&old);
        }
    }
}

/// Takes the slow fallback frame when one is due.
fn rolling_capture(state: &mut Capturing) {
    if Instant::now() < state.next_rolling {
        return;
    }

    let period = Duration::from_secs_f64(1.0 / state.config.capture_hz.max(0.05));
    state.next_rolling = Instant::now() + period;

    let output = state.pointer.lock().ok()
        .and_then(|history| history.latest().map(|s| s.output.clone()));

    let Some(output) = output else {
        return;
    };

    match state.capture.capture_output(&output) {
        Ok(frame) => {
            let done_t_s = state.config.t0.elapsed().as_secs_f64();

            state.rolling.push(frame, done_t_s);
        }

        // An output that came and went mid-session is normal on this desk; the next
        // pass picks up whatever the pointer is on then.
        Err(e) => debug!(output = %output, error = %e, "rolling capture failed"),
    }
}

/// Chooses the frame for a click and hands it to the detector, or answers the request
/// here when there is nothing to hand over.
fn hand_off(state: &mut Capturing, request: DetectRequest) {
    let id = request.id;

    let Some(job) = choose(state, request) else {
        return;
    };

    if let Some(outcome) = dispatch(&state.jobs, job) {
        let _ = state.replies.send(DetectReply { id: id, outcome: outcome });
    }
}

/// Picks the frame a click is recognised from and packages it as a job.
///
/// Answers the request itself and returns `None` when no frame qualifies, because the
/// caller has nothing to hand over in that case.
fn choose(state: &mut Capturing, request: DetectRequest) -> Option<DetectJob> {
    let press_done = request.capture_id
        .and_then(|id| state.press.get(&id))
        .map(|cached| cached.done_t_s);

    let rolling_done = state.rolling
        .newest_before(&request.output, request.t_press)
        .map(|cached| cached.done_t_s);

    let choice = select_frame(press_done, rolling_done, request.t_press, request.t_release);

    // A press frame is consumed either way: one press is one recognition, and holding
    // a whole framebuffer past that is pure cost. A rolling frame is cloned, because
    // it stays in the cache as the next click's fallback.
    let cached = {
        match choice {
            FrameChoice::Press { .. }   => {
                let id = request.capture_id.expect("a press choice implies a capture id");

                state.press_order.retain(|held| *held != id);

                state.press.remove(&id)
            }
            FrameChoice::Rolling { .. } => {
                state.rolling.newest_before(&request.output, request.t_press).cloned()
            }
            FrameChoice::Stale          => None,
        }
    };

    // Drop a press frame the choice rejected, so a late capture does not sit in the
    // store until three more clicks push it out.
    if let (FrameChoice::Rolling { .. } | FrameChoice::Stale, Some(id)) =
        (choice, request.capture_id)
    {
        state.press_order.retain(|held| *held != id);
        state.press.remove(&id);
    }

    let id = request.id;

    let Some(cached) = cached else {
        let _ = state.replies.send(DetectReply { id: id, outcome: DetectOutcome::Stale });

        return None;
    };

    // The luminance window is pure geometry here; the pixels are copied on the detect
    // thread. A point off the frame means the pointer crossed outputs between the
    // capture and the request, which is not something to recognise.
    let Some(luma) = crop_around(
        cached.frame.logical,
        cached.frame.width,
        cached.frame.height,
        request.px,
        state.config.luma_half_px,
    )
    else {
        let _ = state.replies.send(DetectReply { id: id, outcome: DetectOutcome::OffFrame });

        return None;
    };

    Some(DetectJob {
        request : request,
        frame   : cached.frame,
        choice  : choice,
        luma    : luma,
    })
}

/// Hands `job` to the detect thread without ever blocking.
///
/// This is the invariant the two-thread split exists for. A full-frame detection runs
/// for 250 to 400 ms and a double click's second press has to be captured within tens
/// of milliseconds of arriving, so the capture thread refuses to wait: a full queue
/// costs the newest click and nothing else.
///
/// Returns the outcome the caller must answer with, or `None` when the detect thread
/// has taken the job and will answer itself.
fn dispatch(jobs: &Sender<DetectJob>, job: DetectJob) -> Option<DetectOutcome> {
    match jobs.try_send(job) {
        Ok(())                             => None,
        Err(TrySendError::Full(_))         => Some(DetectOutcome::Overrun),
        Err(TrySendError::Disconnected(_)) => {
            Some(DetectOutcome::Failed("the detect thread stopped".to_string()))
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::Rect;

    use super::*;

    /// A reading on `output` at `t_s`.
    fn sample(t_s: f64, x: f64, output: &str) -> PointerSample {
        PointerSample {
            t_s    : t_s,
            global : GlobalPx { x: x, y: 100.0 },
            output : output.to_string(),
        }
    }

    /// A job carrying a one-pixel frame, which is all `dispatch` looks at.
    fn job(id: u64) -> DetectJob {
        DetectJob {
            request : DetectRequest {
                id         : id,
                capture_id : Some(id),
                output     : "DP-1".to_string(),
                px         : GlobalPx { x: 0.0, y: 0.0 },
                t_press    : 0.0,
                t_release  : 0.1,
            },
            frame   : Frame {
                output  : "DP-1".to_string(),
                logical : Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 },
                width   : 1,
                height  : 1,
                rgba    : vec![0, 0, 0, 255],
                t_s     : 0.0,
            },
            choice  : FrameChoice::Press { age_s: 0.03 },
            luma    : Crop {
                x0     : 0,
                y0     : 0,
                w      : 1,
                h      : 1,
                origin : GlobalPx { x: 0.0, y: 0.0 },
                scale  : 1.0,
            },
        }
    }

    #[test]
    fn the_nearest_reading_can_be_on_either_side_of_the_press() {
        let mut history = PointerHistory::new(3.0);

        history.push(sample(9.90 , 100.0, "DP-1"));
        history.push(sample(9.98 , 110.0, "DP-1"));
        history.push(sample(10.04, 120.0, "DP-1"));

        // 10.00 is 20 ms after the second and 40 ms before the third.
        assert_eq!(history.nearest(10.00).map(|s| s.global.x), Some(110.0));
        assert_eq!(history.nearest(10.03).map(|s| s.global.x), Some(120.0));
        assert_eq!(history.latest().map(|s| s.global.x)      , Some(120.0));
    }

    #[test]
    fn history_past_the_span_is_dropped() {
        let mut history = PointerHistory::new(1.0);

        for i in 0..100 {
            history.push(sample(i as f64 * 0.05, i as f64, "DP-1"));
        }

        // The last reading is at 4.95 s, so nothing before 3.95 survives.
        assert!(history.nearest(0.0).is_some_and(|s| s.t_s >= 3.95));
    }

    #[test]
    fn an_empty_history_has_no_answer() {
        let history = PointerHistory::new(1.0);

        assert!(history.nearest(1.0).is_none());
        assert!(history.latest().is_none());
    }

    /// The whole point of the split: a detector that never drains the queue must not
    /// slow the capture thread down by even one job.
    #[test]
    fn a_full_detect_queue_is_refused_rather_than_waited_on() {
        let (tx, _rx) = crossbeam_channel::bounded(DETECT_QUEUE);

        for id in 0..DETECT_QUEUE as u64 {
            assert!(dispatch(&tx, job(id)).is_none(), "job {id} was taken");
        }

        // Nothing is reading `_rx`, so the queue is full and the next job has to come
        // straight back rather than parking the caller.
        let start   = Instant::now();
        let outcome = dispatch(&tx, job(99));

        assert!(matches!(outcome, Some(DetectOutcome::Overrun)));
        assert!(start.elapsed() < Duration::from_millis(50),
                "dispatch blocked for {:?}", start.elapsed());
    }

    #[test]
    fn a_dead_detect_thread_is_reported_rather_than_hung() {
        let (tx, rx) = crossbeam_channel::bounded(DETECT_QUEUE);
        drop(rx);

        let outcome = dispatch(&tx, job(0));

        assert!(matches!(outcome, Some(DetectOutcome::Failed(_))));
    }
}
