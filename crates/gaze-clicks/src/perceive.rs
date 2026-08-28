//! The perception thread: pointer, screen capture and recognition, all on one thread.
//!
//! `Capture`, `CursorTracker` and `Detector` are all built here and never leave: the
//! first two own Wayland event queues and their proxies belong to the thread that made
//! them. `gaze-proto`'s perception thread is the model for that; the difference is what
//! drives the loop. That one captures everything on a slow cadence and re-detects when
//! the picture changes. This one is event driven, because the event that matters is a
//! mouse press and the useful window around it is tens of milliseconds wide:
//!
//! 1. a press arrives on its own channel, straight from the evdev reader, and starts a
//!    capture of the output under the pointer before anything else looks at it;
//! 2. the collector, once the release has told it the press was a click and not a drag,
//!    asks for the crop around the pointer to be recognised;
//! 3. in between, the loop polls the pointer at [`POINTER_HZ`] and takes a slow rolling
//!    capture, the fallback for a press capture that stalled.
//!
//! Choosing between those two frames happens here rather than in the collector, because
//! the capture times belong to the frames and the frames never leave this thread. The
//! collector sends the press and release times and gets back a [`FrameChoice`] saying
//! which was used.

use std::collections::HashMap;
use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender, select};
use gaze_capture::{Capture, CursorTracker};
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

/// Everything the perception thread needs before it starts.
#[derive(Clone, Debug)]
pub struct PerceptionConfig {
    /// Directory holding the ONNX models.
    pub models_dir   : PathBuf,
    /// Rolling fallback capture rate, hertz. The press-triggered capture is the frame
    /// a click is normally recognised from, so this only has to be often enough that a
    /// stalled one has something within [`crate::frames::STALE_FRAME_S`].
    pub capture_hz   : f64,
    /// Half-width of the crop handed to the detector, logical pixels.
    pub crop_half_px : f64,
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
    /// Where the pointer was at the press, global logical pixels. The crop centres
    /// here and the elements are hit-tested against it.
    pub px         : GlobalPx,
    pub t_press    : f64,
    pub t_release  : f64,
}

/// What came back from a recognition request.
#[derive(Clone, Debug)]
pub enum DetectOutcome {
    /// The crop was recognised. `elements` may still be empty, which is a click on
    /// nothing rather than a failure.
    Found {
        elements  : Vec<Element>,
        /// Mean luminance of the crop, [0, 1].
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
    /// The detector itself failed.
    Failed(String),
}

/// One reply from the perception thread.
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

/// Handle to a running perception thread.
pub struct Perception {
    requests : Sender<DetectRequest>,
    replies  : Receiver<DetectReply>,
    pointer  : Arc<Mutex<PointerHistory>>,
    stop     : Arc<AtomicBool>,
    join     : Option<JoinHandle<()>>,
}

/// The thread's own state, split out so the loop body reads as a sequence of steps.
struct State {
    capture      : Capture,
    tracker      : CursorTracker,
    detector     : Detector,
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
    /// Starts the thread.
    ///
    /// Returns once the compositor connections and both models are up, so an error
    /// here means the collector cannot run at all. `presses` carries capture ids from
    /// the mouse reader; every id that arrives on it starts a capture at once.
    pub fn spawn(config: PerceptionConfig, presses: Receiver<u64>) -> Result<Perception> {
        let (ready_tx, ready_rx)     = mpsc::channel();
        let (request_tx, request_rx) = crossbeam_channel::unbounded();
        let (reply_tx, reply_rx)     = crossbeam_channel::unbounded();

        let stop    = Arc::new(AtomicBool::new(false));
        let pointer = Arc::new(Mutex::new(PointerHistory::new(POINTER_SPAN_S)));

        let join = thread::Builder::new()
            .name("gaze-clicks-perception".to_string())
            .spawn({
                let stop    = Arc::clone(&stop);
                let pointer = Arc::clone(&pointer);

                move || {
                    let started = connect(&config, pointer);

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

                    run(&mut state, &presses, &request_rx, &reply_tx, &stop);
                }
            })
            .context("spawning the perception thread")?;

        ready_rx
            .recv()
            .map_err(|_| anyhow!("perception thread died during startup"))??;

        Ok(Perception {
            requests : request_tx,
            replies  : reply_rx,
            pointer  : pointer,
            stop     : stop,
            join     : Some(join),
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

    /// Stops the thread and waits for it. Idempotent.
    ///
    /// Can block for as long as one capture plus one recognition, because neither can
    /// be interrupted once started.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for Perception {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- Thread body ---

/// Opens everything the thread owns. Split out so startup errors reach the caller
/// before the loop begins.
fn connect(config: &PerceptionConfig, pointer: Arc<Mutex<PointerHistory>>) -> Result<State> {
    let capture = Capture::connect()
        .context("connecting to the compositor for screen capture")?;

    let tracker = CursorTracker::connect()
        .context("connecting to the compositor for pointer position")?;

    let detector = Detector::load(&config.models_dir)
        .with_context(|| format!("loading models from {}", config.models_dir.display()))?;

    Ok(State {
        capture      : capture,
        tracker      : tracker,
        detector     : detector,
        config       : config.clone(),
        pointer      : pointer,
        rolling      : RollingCache::new(ROLLING_KEEP),
        press        : HashMap::new(),
        press_order  : VecDeque::new(),
        next_rolling : Instant::now(),
    })
}

/// The loop. Presses and requests wake it immediately; otherwise it ticks at
/// [`POINTER_HZ`] to poll the pointer and take the occasional fallback frame.
fn run(
    state    : &mut State,
    presses  : &Receiver<u64>,
    requests : &Receiver<DetectRequest>,
    replies  : &Sender<DetectReply>,
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

                    let outcome = detect(state, &request);

                    let _ = replies.send(DetectReply { id: request.id, outcome: outcome });
                }
            }

            default(tick) => {
                poll_pointer(state);
                rolling_capture(state);
            }
        }
    }

    debug!("perception thread exiting");
}

/// Reads the pointer and files the reading. Returns the reading it took.
fn poll_pointer(state: &mut State) -> Option<PointerSample> {
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
fn press_capture(state: &mut State, id: u64) {
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
fn store_press(state: &mut State, id: u64, cached: CachedFrame) {
    state.press.insert(id, cached);
    state.press_order.push_back(id);

    while state.press_order.len() > PRESS_KEEP {
        if let Some(old) = state.press_order.pop_front() {
            state.press.remove(&old);
        }
    }
}

/// Takes the slow fallback frame when one is due.
fn rolling_capture(state: &mut State) {
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

/// Picks the frame for a click, crops it around the pointer and recognises it.
fn detect(state: &mut State, request: &DetectRequest) -> DetectOutcome {
    let press_done = request.capture_id
        .and_then(|id| state.press.get(&id))
        .map(|cached| cached.done_t_s);

    let rolling_done = state.rolling
        .newest_before(&request.output, request.t_press)
        .map(|cached| cached.done_t_s);

    let choice = select_frame(press_done, rolling_done, request.t_press, request.t_release);

    // A press frame is consumed either way: one press is one recognition, and holding
    // a whole framebuffer past that is pure cost.
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

    let Some(cached) = cached else {
        return DetectOutcome::Stale;
    };

    let frame = &cached.frame;

    let Some(crop) = crop_around(
        frame.logical,
        frame.width,
        frame.height,
        request.px,
        state.config.crop_half_px,
    )
    else {
        return DetectOutcome::OffFrame;
    };

    let pixels = extract(&frame.rgba, frame.width, &crop);

    if pixels.is_empty() {
        return DetectOutcome::OffFrame;
    }

    recognise(&state.detector, &pixels, &crop, choice)
}

/// Runs the detector over one crop.
fn recognise(
    detector : &Detector,
    pixels   : &[u8],
    crop     : &Crop,
    choice   : FrameChoice,
)
    -> DetectOutcome
{
    match detector.detect(pixels, crop.w, crop.h, crop.origin, crop.scale) {
        Ok(elements) => DetectOutcome::Found {
            elements  : elements,
            crop_luma : mean_luma(pixels),
            choice    : choice,
        },
        Err(e)       => DetectOutcome::Failed(e.to_string()),
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A reading on `output` at `t_s`.
    fn sample(t_s: f64, x: f64, output: &str) -> PointerSample {
        PointerSample {
            t_s    : t_s,
            global : GlobalPx { x: x, y: 100.0 },
            output : output.to_string(),
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
}
