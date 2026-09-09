//! The perception thread: capture every enabled output, re-detect when the picture
//! changes, publish one merged element list.
//!
//! The numbers this is built around, from `PLAN.md`'s environment facts: capture is about
//! 35 ms per ultrawide frame and detection about 400 ms. So capture runs on a slow loop
//! and detection runs only when it has to, which is when the frame differs from the last
//! one that was detected on by more than a threshold, or when the interval since that
//! detection expired.
//!
//! The Wayland connection and both ONNX sessions are created on this thread and never
//! leave it, but startup failures are reported back to the caller so a missing model file
//! or a dead compositor kills the process immediately rather than leaving a gaze loop
//! running against an element list that will never fill.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, RwLock, mpsc};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow};
use crossbeam_channel::{Receiver, Sender};
use gaze_capture::{Capture, Frame, changed_fraction};
use gaze_core::{Element, GlobalPx};
use gaze_detect::Detector;
use tracing::{debug, error, info, warn};

/// Id space reserved per output.
///
/// `Detector::detect` numbers its boxes from zero on every call, so the same id comes back
/// for a button on DP-1 and an unrelated one on DP-2. Offsetting by the output's index
/// keeps ids unique across the merged list, which is what the snap engine's hysteresis and
/// the scoring log both assume. A single output will never hold a million boxes.
const ID_STRIDE: u64 = 1_000_000;

/// The published element list, shared with the gaze loop.
///
/// Readers compare [`generation`](ElementStore::generation) before touching the lock, so
/// a steady desktop costs one relaxed atomic load per gaze sample and no contention at
/// all. When it does change they clone the `Arc` and drop the lock immediately, rather
/// than holding a read guard across a snap update.
#[derive(Debug, Default)]
pub struct ElementStore {
    elements   : RwLock<Arc<Vec<Element>>>,
    generation : AtomicU64,
}

/// Everything the perception thread needs to know before it starts.
#[derive(Clone, Debug)]
pub struct PerceptionConfig {
    /// Connector names of the outputs to watch, in the order the desk config lists them.
    /// The index into this list is what offsets element ids, so it must stay stable for
    /// the life of the run.
    pub outputs            : Vec<String>,
    /// Directory holding the ONNX models.
    pub models_dir         : PathBuf,
    /// Fraction of the frame that must change to trigger a detection.
    pub redetect_threshold : f32,
    /// Longest an output may go without a detection.
    pub redetect_interval  : Duration,
    /// Minimum time between capture passes. Caps the loop when nothing is triggering.
    pub period             : Duration,
}

/// What a forced re-detection covers: every output, or one by connector name.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Redetect {
    All,
    Output(String),
}

/// Handle to a running perception thread.
pub struct Perception {
    redetect : Sender<Redetect>,
    stop     : Arc<AtomicBool>,
    join     : Option<JoinHandle<()>>,
}

/// Per-output state carried between passes.
#[derive(Default)]
struct OutputState {
    /// The frame the current element list was detected from. The diff baseline is the last
    /// *detected* frame, not the last captured one, so a desktop that drifts slowly still
    /// eventually crosses the threshold instead of never triggering.
    detected_frame : Option<Frame>,
    /// When that detection ran.
    detected_at    : Option<Instant>,
    /// This output's contribution to the merged list, ids already offset.
    elements       : Vec<Element>,
}

/// Why a detection ran, for the log line.
#[derive(Clone, Copy, Debug)]
enum Trigger {
    /// No detection has run on this output yet.
    First,
    /// The middle mouse button asked for one.
    Forced,
    /// The redetect interval expired.
    Interval,
    /// The frame changed by `fraction`.
    Changed { fraction : f32 },
}

// --- ElementStore ---

impl ElementStore {
    /// Creates an empty store.
    pub fn new() -> Arc<ElementStore> {
        Arc::new(ElementStore::default())
    }

    /// Counter bumped on every publish. Cheap enough to poll per gaze sample.
    pub fn generation(&self) -> u64 {
        // Acquire pairs with the release store in `publish`, so a reader that sees the new
        // generation is guaranteed to see the list that goes with it.
        self.generation.load(Ordering::Acquire)
    }

    /// Takes an owned handle to the current list.
    pub fn snapshot(&self) -> Arc<Vec<Element>> {
        Arc::clone(&self.elements.read().expect("element store poisoned"))
    }

    /// Replaces the list and bumps the generation.
    fn publish(&self, elements: Vec<Element>) {
        *self.elements.write().expect("element store poisoned") = Arc::new(elements);

        self.generation.fetch_add(1, Ordering::Release);
    }
}

// --- Perception ---

impl Perception {
    /// Starts the perception thread.
    ///
    /// Returns once the capture connection and both models are up, so an error here means
    /// the prototype cannot run at all. `store` is shared with the gaze loop.
    pub fn spawn(config: PerceptionConfig, store: Arc<ElementStore>) -> Result<Perception> {
        let (ready_tx, ready_rx)       = mpsc::channel();
        let (redetect_tx, redetect_rx) = crossbeam_channel::unbounded();

        let stop = Arc::new(AtomicBool::new(false));

        let join = thread::Builder::new()
            .name("gaze-perception".to_string())
            .spawn({
                let stop = Arc::clone(&stop);

                move || {
                    let started = connect(&config);

                    let (capture, detector) = match started {
                        Ok(pair) => {
                            let _ = ready_tx.send(Ok(()));

                            pair
                        }

                        Err(e) => {
                            let _ = ready_tx.send(Err(e));

                            return;
                        }
                    };

                    run(config, capture, detector, store, redetect_rx, stop);
                }
            })
            .context("spawning the perception thread")?;

        ready_rx
            .recv()
            .map_err(|_| anyhow!("perception thread died during startup"))??;

        Ok(Perception {
            redetect : redetect_tx,
            stop     : stop,
            join     : Some(join),
        })
    }

    /// Asks for a detection on every output on the next pass, regardless of the frame diff.
    pub fn force_redetect(&self) {
        // A full queue would only mean a redetect is already pending, which is the same
        // outcome, so a failed send is not worth reporting.
        let _ = self.redetect.try_send(Redetect::All);
    }

    /// Asks for a detection on one output on the next pass, ahead of the others: what a
    /// scroll that just stopped wants, so the boxes under the gaze are fresh in one
    /// detection's time rather than a whole pass's.
    pub fn force_redetect_output(&self, name: &str) {
        let _ = self.redetect.try_send(Redetect::Output(name.to_string()));
    }

    /// Stops the thread and waits for it. Idempotent.
    ///
    /// Can block for as long as one detection pass over every output, because the thread
    /// only checks the stop flag between outputs and an ONNX session cannot be interrupted.
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

/// Opens everything the thread owns. Split out so startup errors can be shipped back to
/// the spawning thread before the loop begins.
fn connect(config: &PerceptionConfig) -> Result<(Capture, Detector)> {
    let capture = Capture::connect()
        .context("connecting to the compositor for screen capture")?;

    let detector = Detector::load(&config.models_dir)
        .with_context(|| format!("loading models from {}", config.models_dir.display()))?;

    Ok((capture, detector))
}

/// The capture and detect loop. Runs until the stop flag is set.
fn run(
    config   : PerceptionConfig,
    capture  : Capture,
    detector : Detector,
    store    : Arc<ElementStore>,
    redetect : Receiver<Redetect>,
    stop     : Arc<AtomicBool>,
)
{
    let mut capture = capture;
    let mut states  : HashMap<String, OutputState> = HashMap::new();
    let mut missing : HashSet<String>              = HashSet::new();

    while !stop.load(Ordering::Relaxed) {
        let pass_start = Instant::now();

        // Drain the whole channel: several requests between passes still mean one pass.
        let mut forced_all = false;
        let mut forced     = HashSet::new();

        for request in redetect.try_iter() {
            match request {
                Redetect::All          => forced_all = true,
                Redetect::Output(name) => { forced.insert(name); }
            }
        }

        // Output identity is re-queried every pass because HDMI-A-1 on this desk drops off
        // the list and comes back (see PLAN.md's environment facts).
        let present : HashSet<String> = capture
            .outputs()
            .into_iter()
            .map(|info| info.name)
            .collect();

        // Forced outputs first: the one a scroll just stopped on should not wait behind
        // two others that happened to change. The id offset stays the config index.
        let mut order : Vec<(usize, &String)> = config.outputs.iter().enumerate().collect();

        order.sort_by_key(|(_, name)| !forced.contains(*name));

        for (index, name) in order {
            if stop.load(Ordering::Relaxed) {
                break;
            }

            // A configured output the compositor does not have is a warning once, not a
            // failure: it may well come back mid-session.
            if !present.contains(name) {
                if missing.insert(name.clone()) {
                    warn!(output = %name, "configured output is not present, skipping it");
                }

                continue;
            }

            if missing.remove(name) {
                info!(output = %name, "configured output came back");
            }

            let frame = match capture.capture_output(name) {
                Ok(frame) => frame,

                Err(e) => {
                    warn!(output = %name, error = %e, "capture failed");

                    continue;
                }
            };

            let state = states.entry(name.clone()).or_default();

            let force = forced_all || forced.contains(name);

            let Some(trigger) = trigger_for(state, &frame, &config, force) else {
                continue;
            };

            let origin = GlobalPx { x: frame.logical.x, y: frame.logical.y };
            let start  = Instant::now();

            let detected = detector.detect(
                &frame.rgba,
                frame.width,
                frame.height,
                origin,
                frame.scale(),
            );

            let elapsed_ms = start.elapsed().as_secs_f64() * 1000.0;

            let mut elements = match detected {
                Ok(elements) => elements,

                Err(e) => {
                    error!(output = %name, error = %e, "detection failed");

                    continue;
                }
            };

            // Ids are per-call indices, so offset them into this output's own range.
            let base = index as u64 * ID_STRIDE;

            for element in &mut elements {
                element.id += base;
            }

            info!(
                output  = %name,
                trigger = %trigger,
                ms      = elapsed_ms,
                boxes   = elements.len(),
                px      = format_args!("{}x{}", frame.width, frame.height),
                "detected"
            );

            state.elements       = elements;
            state.detected_frame = Some(frame);
            state.detected_at    = Some(Instant::now());

            // Publish per output, not per pass: a reader waiting on this output's boxes
            // gets them now instead of after every other output's detection.
            let merged : Vec<Element> = config
                .outputs
                .iter()
                .filter_map(|name| states.get(name))
                .flat_map(|state| state.elements.iter().cloned())
                .collect();

            debug!(count = merged.len(), output = %name, "publishing merged element list");

            store.publish(merged);
        }

        // Cap the loop rather than spinning: capture alone would otherwise run flat out.
        if let Some(rest) = config.period.checked_sub(pass_start.elapsed()) {
            thread::sleep(rest);
        }
    }

    debug!("perception thread exiting");
}

/// Decides whether this output needs a fresh detection, and why.
///
/// Returns `None` when the cached element list still stands.
fn trigger_for(
    state  : &OutputState,
    frame  : &Frame,
    config : &PerceptionConfig,
    forced : bool,
)
    -> Option<Trigger>
{
    let Some(previous) = &state.detected_frame else {
        return Some(Trigger::First);
    };

    if forced {
        return Some(Trigger::Forced);
    }

    if state.detected_at.is_none_or(|at| at.elapsed() >= config.redetect_interval) {
        return Some(Trigger::Interval);
    }

    // The diff is the cheap part of the pass, one strided walk over both buffers, so it
    // runs on every captured frame and only the detector is gated.
    let fraction = changed_fraction(previous, frame);

    if fraction > config.redetect_threshold {
        return Some(Trigger::Changed { fraction: fraction });
    }

    None
}

// --- Display ---

impl std::fmt::Display for Trigger {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Trigger::First                => write!(f, "first"),
            Trigger::Forced               => write!(f, "forced"),
            Trigger::Interval             => write!(f, "interval"),
            Trigger::Changed { fraction } => write!(f, "changed {fraction:.3}"),
        }
    }
}
