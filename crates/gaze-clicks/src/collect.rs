//! The collector: the thread that turns presses into labelled gaze samples.
//!
//! Everything expensive happens somewhere else. The evdev reader stamps presses and
//! fires their captures, the perception thread captures and recognises, the tracker
//! feed fills a ring of device frames. This module is the sequencing and the rules:
//! which presses are clicks, which clicks landed on something, which of those had a
//! gaze worth keeping, and what gets written.
//!
//! # The rules, in the order they reject
//!
//! 1. **drag** — held past [`click::DRAG_HOLD_S`] or moved past
//!    [`click::DRAG_MOVE_PX`]. The press and the release are about different places.
//! 2. **off-desk** — the click landed on an output `desk.toml` does not describe, so
//!    there is no surface to put the target on.
//! 3. **stale** — no press capture in time and no fresh enough rolling frame.
//! 4. **no-element** — nothing recognisable under the pointer. This is the "focus
//!    click on nothing" case, and excluding it is most of what makes the rest of the
//!    data worth training on.
//! 5. **blank** — a box did contain the pointer, but it is a large one and the pixels
//!    under the pointer are flat, so the model drew a control over empty space. The same
//!    rejection as `no-element` wearing a different hat, counted apart so a rising count
//!    is legible as "the detector is hallucinating panels".
//! 6. **no-gaze** — under [`frames::GAZE_MIN_FRACTION`] of the approach carried a
//!    usable combined gaze. A blink over the last half second is not a label.
//!
//! A click that survives all six is written, and its firmware offset goes into the
//! running median that the status line reports. That median is the daily "is the model
//! drifting" number: it is measured against where the user clicked rather than against
//! a dot they were told to look at, so it costs nothing and accumulates all day.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use crossbeam_channel::RecvTimeoutError;
use gaze_core::{DesktopGeometry, Element, GlobalPx, OutputGeometry};
use gaze_provider_et5::calibration::VIRTUAL_AREA;
use gaze_provider_et5::record::{ClickElement, ClickRecord, SESSION_FORMAT, SessionMeta};
use gaze_provider_et5::sweep::{StopWindow, TimedFrame};
use gaze_provider_et5::{BlobReport, DisplayArea, Et5Calibration, Et5Provider};
use glam::DVec3;
use tracing::{debug, warn};

use crate::click::{Button, ButtonEvent, MultiCounter, PressKind, classify};
use crate::cursor::{CursorShape, classify as classify_cursor};
use crate::element::{CARET_KIND, Pick, kind_name, pick};
use crate::frames::{
    FrameChoice, GAZE_AFTER_S, GAZE_BEFORE_S, GAZE_MIN_FRACTION, STOP_AFTER_S, STOP_BEFORE_S,
    gaze_fraction, has_combined_gaze,
};
use crate::mouse::MouseReader;
use crate::perceive::{DetectOutcome, DetectRequest, Perception, PerceptionConfig};
use crate::session::ClickSession;
use crate::tracker::{TrackerEvent, TrackerFeed};

/// How often the status line is printed, seconds.
const STATUS_PERIOD_S: u64 = 10;

/// How many recent accepted clicks the running offset median is taken over.
const OFFSET_WINDOW: usize = 20;

/// How long a recognition request may take before the click is given up on.
///
/// A pointer-local detection is well under 200 ms on the ultrawide and up to three of
/// them can be queued ahead of this one, so three seconds is a generous bound; anything
/// near it means the detector is wedged rather than merely busy.
const DETECT_TIMEOUT: Duration = Duration::from_secs(3);

/// Note written into the session meta line, so a click session is identifiable at a
/// glance among the recorded ones.
const SESSION_NOTE: &str = "clicks";

/// Everything one collector run needs.
#[derive(Clone, Debug)]
pub struct CollectConfig {
    /// Session file path, overriding the generated one.
    pub out         : Option<PathBuf>,
    /// Directory sessions are written into when `out` is not given.
    pub out_dir     : PathBuf,
    /// Mouse node to read, overriding the name lookup.
    pub mouse       : Option<PathBuf>,
    /// Name substring the mouse is found by.
    pub mouse_name  : String,
    /// Directory holding the ONNX models.
    pub models_dir  : PathBuf,
    /// Desk geometry file.
    pub desk        : PathBuf,
    /// Client-side calibration, whose trained plane names the tracker's display.
    pub calibration : PathBuf,
    /// Host-owned device blob, uploaded on every connect.
    pub blob        : PathBuf,
    /// Run the whole pipeline without opening the tracker. Clicks and recognition
    /// still happen and the tallies are still real; the records carry no gaze.
    pub no_tracker  : bool,
    /// Rolling fallback capture rate, hertz.
    pub capture_hz  : f64,
    /// Half-width of the window the screen luminance is averaged over, logical pixels.
    /// Recognition is pointer-local; this is only the pupil covariate's window.
    pub luma_px     : f64,
}

/// What the rules did to the presses that arrived.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Tallies {
    /// Written to the session file.
    pub accepted     : u64,
    /// Held too long or moved too far.
    pub drag         : u64,
    /// On an output the desk config does not describe.
    pub off_desk     : u64,
    /// No usable screen frame.
    pub stale        : u64,
    /// A frame, but nothing recognisable under the pointer.
    pub no_element   : u64,
    /// Of the `no_element` and `blank` refusals, those made under a pointing hand or an
    /// I-beam: the application said something clickable or editable was there and the
    /// rules refused it anyway. With the I-beam accepting a click on nothing, this is
    /// hands over cards and links plus I-beams over flat boxes; see [`crate::cursor`].
    pub disputed     : u64,
    /// A large box did contain the pointer, but the pixels there are flat: a control
    /// claimed over empty space, which is a click on nothing by another name.
    pub blank        : u64,
    /// An element, but no gaze over the approach.
    pub no_gaze      : u64,
    /// Accepted on the I-beam's word alone, with no recognised box: an input, a
    /// terminal, a document. Counted alongside `accepted`, not instead of it.
    pub caret        : u64,
    /// Accepted, but recognised from the rolling fallback rather than the press's own
    /// capture. Counted alongside `accepted`, not instead of it.
    pub late_capture : u64,
    /// Clicked faster than recognition runs, so the detector's queue was full and this
    /// click's frame was dropped rather than made to wait. A steady count here means the
    /// detector cannot keep up with how the machine is used.
    pub overrun      : u64,
    /// The recogniser failed outright.
    pub error        : u64,
}

/// What a finished run produced.
#[derive(Clone, Debug)]
pub struct Outcome {
    /// The last session file written.
    pub path    : PathBuf,
    /// Clicks written across every session file this run produced.
    pub clicks  : u64,
    /// How many session files it took, more than one only after a retrain.
    pub files   : u64,
    pub tallies : Tallies,
}

/// A press waiting for its release.
#[derive(Clone, Debug)]
struct Pending {
    t_press    : f64,
    /// The capture the reader fired for this press.
    capture_id : Option<u64>,
    /// Where the pointer was at the press.
    px         : GlobalPx,
    /// Which output that was on.
    output     : String,
    /// Position of this press within its multi-click.
    multi      : u32,
    /// The pointer's shape at the press, when the compositor reported the image.
    cursor     : Option<CursorShape>,
}

/// The running state of one collector run.
struct Collector<'a> {
    config     : &'a CollectConfig,
    geometry   : DesktopGeometry,
    /// Nominal seated eye from the desk config, the vertex the firmware offset is
    /// measured at. The same stand-in `retrain::run_health` uses.
    eye        : DVec3,
    /// The display whose plane the device has declared, and whose uv the firmware's
    /// 2D output is in.
    device_out : Option<OutputGeometry>,
    perception : &'a Perception,
    feed       : Option<&'a TrackerFeed>,
    session    : ClickSession,
    t0         : Instant,
    tallies    : Tallies,
    /// Firmware offsets of the recent accepted clicks, degrees.
    offsets    : Vec<f64>,
    /// What the detector cost on the recent recognised frames, milliseconds. Kept
    /// alongside the offsets and over the same window, because both answer "is this
    /// still healthy" at a glance.
    detects    : Vec<f64>,
    /// Presses waiting for their release, one per button.
    pending    : HashMap<Button, Pending>,
    multi      : MultiCounter,
    /// Ids handed to recognition requests.
    next_id    : u64,
    /// Total clicks written, across session rotations.
    written    : u64,
    /// How many session files this run has opened.
    files      : u64,
}

// --- Running ---

/// Runs the collector until `stop` is set.
///
/// Opens the mouse, the compositor and (unless `--no-tracker`) the device, then reads
/// presses until it is asked to stop. Returns what was written.
pub fn run(config: &CollectConfig, stop: Arc<AtomicBool>) -> Result<Outcome> {
    let t0 = Instant::now();

    let desk = std::fs::read_to_string(&config.desk)
        .with_context(|| format!("reading {}", config.desk.display()))?;
    let geometry = DesktopGeometry::from_toml(&desk).context("parsing desk geometry")?;
    let pitch    = tracker_pitch_deg(&config.desk);

    // The calibration names the display whose plane the on-device model was trained
    // against. Without one the firmware's 2D output is in an arbitrary virtual plane
    // and the offset readout has nothing to compare against, which is a warning rather
    // than a failure: the labels are still good.
    let calibration = Et5Calibration::load(&config.calibration).ok();

    if calibration.is_none() {
        warn!(path = %config.calibration.display(),
              "no client calibration; the firmware offset readout will be unavailable");
    }

    let (press_tx, press_rx) = crossbeam_channel::unbounded();

    let mut perception = Perception::spawn(
        PerceptionConfig {
            models_dir   : config.models_dir.clone(),
            capture_hz   : config.capture_hz,
            luma_half_px : config.luma_px,
            t0           : t0,
        },
        press_rx,
    )?;

    let mut mouse = MouseReader::open(config.mouse.as_deref(), &config.mouse_name, t0, press_tx)?;

    println!("reading {} ({}), read-only, not grabbed",
             mouse.path().display(), mouse.name());

    // The tracker, when there is one. It owns the device for the life of the run: no
    // other tool in this workspace can open it at the same time.
    let mut feed = {
        match config.no_tracker {
            true  => None,
            false => Some(start_tracker(config, &geometry, &calibration, pitch, t0)?),
        }
    };

    let device_display = calibration.as_ref()
        .and_then(|c| c.device_output.clone())
        .unwrap_or_else(|| "unknown".to_string());

    let device_area = {
        match (&feed, calibration.as_ref().and_then(|c| c.device_area)) {
            (Some(feed), _)     => feed.area(),
            (None, Some(area))  => area,
            (None, None)        => DisplayArea::from_rect(VIRTUAL_AREA),
        }
    };

    let blob = feed.as_ref().map(|f| f.blob().clone()).unwrap_or_else(no_tracker_blob);

    let meta = session_meta(&blob, &device_display, device_area, &desk, pitch);
    let session = ClickSession::create(&config.out_dir, config.out.as_deref(), &meta)?;

    println!("writing {}", session.path().display());

    let mut collector = Collector {
        config     : config,
        device_out : geometry.outputs.iter().find(|o| o.name == device_display).cloned(),
        eye        : geometry.eye(),
        geometry   : geometry,
        perception : &perception,
        feed       : feed.as_ref(),
        session    : session,
        t0         : t0,
        tallies    : Tallies::default(),
        offsets    : Vec::new(),
        detects    : Vec::new(),
        pending    : HashMap::new(),
        multi      : MultiCounter::new(),
        next_id    : 0,
        written    : 0,
        files      : 1,
    };

    let period      = Duration::from_secs(STATUS_PERIOD_S);
    let mut next_status = Instant::now() + period;

    while !stop.load(Ordering::Relaxed) {
        collector.drain_tracker_events(&desk, pitch)?;

        if Instant::now() >= next_status {
            collector.print_status();

            next_status = Instant::now() + period;
        }

        match mouse.events().recv_timeout(Duration::from_millis(100)) {
            Ok(event)                        => collector.on_button(event)?,
            Err(RecvTimeoutError::Timeout)   => {}
            Err(RecvTimeoutError::Disconnected) => {
                warn!("the mouse reader stopped; ending the run");

                break;
            }
        }
    }

    let Collector { session, written, files, tallies, .. } = collector;

    let path = session.path().to_path_buf();

    // The end line takes the blob the feed last saw. A retrain mid-run has already
    // rotated the file, so a difference here is the "the firmware mutated its own
    // model" signal the reader checks for.
    let end_blob = feed.as_ref()
        .map(|feed| feed.blob().body_sha256.clone())
        .unwrap_or_else(|| session.blob_sha256().to_string());

    session.finish(&end_blob)?;

    mouse.stop();
    perception.stop();

    if let Some(feed) = feed.as_mut() {
        feed.stop();
    }

    Ok(Outcome {
        path    : path,
        clicks  : written,
        files   : files,
        tallies : tallies,
    })
}

// --- Collector ---

impl Collector<'_> {
    /// Handles one press or release.
    fn on_button(&mut self, event: ButtonEvent) -> Result<()> {
        if event.pressed {
            self.on_press(event);

            return Ok(());
        }

        let Some(pending) = self.pending.remove(&event.button) else {
            // A release with no press means the collector started with the button
            // already down, or the press was dropped while the channel was full.
            return Ok(());
        };

        self.on_release(event, pending)
    }

    /// Records a press and where the pointer was for it.
    fn on_press(&mut self, event: ButtonEvent) {
        let multi = self.multi.press(event.button, event.t_s);

        let Some(sample) = self.perception.pointer_at(event.t_s) else {
            debug!("a press arrived before the pointer was ever reported");

            return;
        };

        self.pending.insert(event.button, Pending {
            t_press    : event.t_s,
            capture_id : event.capture_id,
            px         : sample.global,
            output     : sample.output,
            multi      : multi,
            cursor     : sample.cursor.map(classify_cursor),
        });
    }

    /// Counts a refusal the pointer's own shape disagreed with.
    fn tally_dispute(&mut self, pending: &Pending) {
        if pending.cursor.is_some_and(CursorShape::says_something_is_there) {
            self.tallies.disputed += 1;
        }
    }

    /// Applies every rule to a completed press and writes it when it survives.
    fn on_release(&mut self, event: ButtonEvent, pending: Pending) -> Result<()> {
        let moved_px = self.perception
            .pointer_at(event.t_s)
            .map(|sample| distance(pending.px, sample.global))
            .unwrap_or(0.0);

        let hold_s = event.t_s - pending.t_press;

        if classify(hold_s, moved_px) == PressKind::Drag {
            self.tallies.drag += 1;

            return Ok(());
        }

        // The target has to sit on a surface the desk config describes, or there is no
        // world point to measure an angle to.
        let Some(out) = self.geometry.outputs.iter()
            .find(|o| o.name == pending.output)
            .cloned()
        else {
            self.tallies.off_desk += 1;

            return Ok(());
        };

        let Some((element, crop_luma, choice)) = self.recognise(&pending, event.t_s)? else {
            return Ok(());
        };

        if matches!(choice, FrameChoice::Rolling { .. }) {
            self.tallies.late_capture += 1;
        }

        // The gaze window runs past the press, so it does not exist yet. Waiting for
        // it here rather than deferring the write keeps the whole click in one place;
        // the cost is a few hundred milliseconds of collector latency per click, and
        // the presses that arrive meanwhile have already fired their own captures.
        self.wait_for(pending.t_press + GAZE_AFTER_S);

        let window = self.feed
            .map(|feed| feed.window(pending.t_press - GAZE_BEFORE_S,
                                    pending.t_press + GAZE_AFTER_S))
            .unwrap_or_default();

        if self.feed.is_some() {
            let fraction = gaze_fraction(&window, pending.t_press - STOP_BEFORE_S,
                                         pending.t_press);

            if fraction < GAZE_MIN_FRACTION {
                self.tallies.no_gaze += 1;

                return Ok(());
            }
        }

        self.write(&pending, event, &out, &element, crop_luma, choice, &window)
    }

    /// Asks the perception thread to recognise what is around the press.
    ///
    /// `Ok(None)` means a rule rejected the click and the tally has already been
    /// bumped.
    fn recognise(&mut self, pending: &Pending, t_release: f64)
        -> Result<Option<(ClickElement, f64, FrameChoice)>>
    {
        let id = self.next_id;
        self.next_id += 1;

        self.perception.requests().send(DetectRequest {
            id         : id,
            capture_id : pending.capture_id,
            output     : pending.output.clone(),
            px         : pending.px,
            t_press    : pending.t_press,
            t_release  : t_release,
        })
        .context("the perception thread stopped accepting requests")?;

        let Some(outcome) = self.wait_reply(id) else {
            self.tallies.error += 1;
            warn!("the recogniser did not answer within {DETECT_TIMEOUT:?}");

            return Ok(None);
        };

        match outcome {
            DetectOutcome::Found { elements, crop_luma, pointer_sd, detect_ms, choice } => {
                debug!(detect_ms = detect_ms, boxes = elements.len(),
                       pointer_sd = pointer_sd, "recognised the frame under a click");

                self.detects.push(detect_ms);

                if self.detects.len() > OFFSET_WINDOW {
                    self.detects.remove(0);
                }

                match pick(&elements, pending.px, pointer_sd, pending.cursor) {
                    Pick::Element(element) => {
                        Ok(Some((click_element(element), crop_luma, choice)))
                    }

                    // The I-beam's word, not the recogniser's; see `crate::cursor`.
                    Pick::Caret(bbox)      => {
                        self.tallies.caret += 1;

                        let element = ClickElement {
                            kind  : CARET_KIND.to_string(),
                            bbox  : bbox,
                            text  : None,
                            score : 0.0,
                        };

                        Ok(Some((element, crop_luma, choice)))
                    }

                    Pick::Nothing          => {
                        self.tallies.no_element += 1;
                        self.tally_dispute(pending);

                        Ok(None)
                    }

                    Pick::Blank            => {
                        self.tallies.blank += 1;
                        self.tally_dispute(pending);

                        Ok(None)
                    }
                }
            }

            DetectOutcome::Stale    => {
                self.tallies.stale += 1;

                Ok(None)
            }

            DetectOutcome::OffFrame => {
                // The pointer crossed to another output between the capture and the
                // request: the frame is real but it is not the one under the click.
                self.tallies.stale += 1;

                Ok(None)
            }

            DetectOutcome::Overrun  => {
                // Refusing the click is the price of the capture thread never waiting
                // on the detector; see `perceive::dispatch`.
                self.tallies.overrun += 1;

                Ok(None)
            }

            DetectOutcome::Failed(e) => {
                self.tallies.error += 1;
                warn!("recognition failed: {e}");

                Ok(None)
            }
        }
    }

    /// Waits for the reply to `id`, discarding replies to requests already given up on.
    fn wait_reply(&self, id: u64) -> Option<DetectOutcome> {
        let deadline = Instant::now() + DETECT_TIMEOUT;

        loop {
            let left = deadline.checked_duration_since(Instant::now())?;

            match self.perception.replies().recv_timeout(left) {
                Ok(reply) if reply.id == id => return Some(reply.outcome),
                Ok(_)                       => continue,
                Err(_)                      => return None,
            }
        }
    }

    /// Writes an accepted click and logs it.
    #[allow(clippy::too_many_arguments)]
    fn write(
        &mut self,
        pending   : &Pending,
        event     : ButtonEvent,
        out       : &OutputGeometry,
        element   : &ClickElement,
        crop_luma : f64,
        choice    : FrameChoice,
        window    : &[TimedFrame],
    )
        -> Result<()>
    {
        let n     = self.session.next_index();
        let (u, v) = out.px_to_uv(pending.px);

        let click = ClickRecord {
            n           : n,
            button      : event.button.as_str().to_string(),
            output      : out.name.clone(),
            px          : pending.px,
            t_press     : pending.t_press,
            t_release   : event.t_s,
            moved_px    : self.perception
                .pointer_at(event.t_s)
                .map(|sample| distance(pending.px, sample.global))
                .unwrap_or(0.0),
            multi       : pending.multi,
            element     : element.clone(),
            crop_luma   : crop_luma,
            frame_age_s : age_of(choice),
            cursor      : pending.cursor.map(|c| c.name().to_string()),
        };

        let stop = self.feed.map(|_| StopWindow {
            u        : u,
            v        : v,
            px       : pending.px,
            t_start  : pending.t_press - STOP_BEFORE_S,
            t_end    : pending.t_press + STOP_AFTER_S,
            parallax : false,
        });

        self.session.write_click(&click, stop.as_ref(), window)?;

        self.written        += 1;
        self.tallies.accepted += 1;

        let offset = self.firmware_offset(pending, out, window);

        if let Some(offset) = offset {
            self.offsets.push(offset);

            if self.offsets.len() > OFFSET_WINDOW {
                self.offsets.remove(0);
            }
        }

        self.log_click(&click, offset, window.len());

        Ok(())
    }

    /// Angle between the firmware's own filtered gaze and where the user clicked.
    ///
    /// Measured exactly as `retrain::run_health` measures it: both points on their
    /// panels, both seen from the desk config's nominal eye, and the angle between the
    /// two directions. The nominal eye stands in for the real one because the quantity
    /// is an angle at the head and the head sits within a few centimetres of it.
    ///
    /// `None` without a calibration (the firmware's uv is then in a virtual plane) or
    /// when nothing in the window carried a combined gaze.
    fn firmware_offset(
        &self,
        pending : &Pending,
        out     : &OutputGeometry,
        window  : &[TimedFrame],
    )
        -> Option<f64>
    {
        let device_out = self.device_out.as_ref()?;

        let mut us = Vec::new();
        let mut vs = Vec::new();

        for frame in window {
            if frame.t_s < pending.t_press - STOP_BEFORE_S || frame.t_s > pending.t_press {
                continue;
            }

            if !has_combined_gaze(&frame.frame) {
                continue;
            }

            let [nx, ny] = frame.frame.gaze_2d_norm?;

            us.push(nx);
            vs.push(ny);
        }

        if us.is_empty() {
            return None;
        }

        let want = out.px_to_world(pending.px) - self.eye;
        let got  = device_out.uv_to_world(median(&mut us), median(&mut vs)) - self.eye;

        Some(want.angle_between(got).to_degrees())
    }

    /// One line per accepted click, which is the feedback that makes a background
    /// collector worth leaving running.
    fn log_click(&self, click: &ClickRecord, offset: Option<f64>, frames: usize) {
        let label = click.element.text.as_deref()
            .map(|text| format!(" \"{}\"", text.trim()))
            .unwrap_or_default();

        let gaze = {
            match offset {
                Some(deg) => format!("firmware gaze {deg:.1} deg off"),
                None      => "firmware gaze n/a".to_string(),
            }
        };

        let multi = {
            if click.multi > 1 {
                format!(" x{}", click.multi)
            }
            else {
                String::new()
            }
        };

        println!(
            "click #{} {} ({:.0}, {:.0}) {}{}{} — {}, {} frames",
            click.n, click.output, click.px.x, click.px.y,
            click.element.kind, label, multi, gaze, frames,
        );
    }

    /// The ten-second summary.
    fn print_status(&self) {
        let t = self.tallies;

        let offset = {
            match self.offsets.is_empty() {
                true  => "n/a".to_string(),
                false => format!("{:.2} deg", median(&mut self.offsets.clone())),
            }
        };

        let detect = {
            match self.detects.is_empty() {
                true  => "n/a".to_string(),
                false => format!("{:.0} ms", median(&mut self.detects.clone())),
            }
        };

        println!(
            "status: {} accepted ({} caret) / {} drag / {} no-element / {} blank ({} under \
             a hand or I-beam) / {} no-gaze / {} stale ({} late-capture, {} overrun, \
             {} off-desk, {} error) — median offset {} over the last {}, median detect {}",
            t.accepted, t.caret, t.drag, t.no_element, t.blank, t.disputed, t.no_gaze,
            t.stale, t.late_capture, t.overrun, t.off_desk, t.error, offset,
            self.offsets.len(), detect,
        );
    }

    /// Rotates the session file when the device came back holding a different model.
    fn drain_tracker_events(&mut self, desk: &str, pitch: f64) -> Result<()> {
        let Some(feed) = self.feed else {
            return Ok(());
        };

        for event in feed.events().try_iter() {
            let TrackerEvent::Reconnected { report } = event;

            if report.body_sha256 == self.session.blob_sha256() {
                continue;
            }

            println!("the tracker came back with a different eye model ({}); \
                      starting a new session file", report.short());

            let display = self.device_out.as_ref()
                .map(|o| o.name.clone())
                .unwrap_or_else(|| "unknown".to_string());

            let meta = session_meta(&report, &display, feed.area(), desk, pitch);
            let new  = ClickSession::create(&self.config.out_dir, None, &meta)?;

            println!("writing {}", new.path().display());

            let old = std::mem::replace(&mut self.session, new);
            let _   = old.finish(&report.body_sha256);

            self.files += 1;
        }

        Ok(())
    }

    /// Sleeps until the collector's clock reaches `t_s`.
    fn wait_for(&self, t_s: f64) {
        let deadline = self.t0 + Duration::from_secs_f64(t_s.max(0.0));

        if let Some(rest) = deadline.checked_duration_since(Instant::now()) {
            std::thread::sleep(rest);
        }
    }
}

// --- Helpers ---

/// Opens the tracker and starts its feed.
fn start_tracker(
    config      : &CollectConfig,
    geometry    : &DesktopGeometry,
    calibration : &Option<Et5Calibration>,
    pitch       : f64,
    t0          : Instant,
)
    -> Result<TrackerFeed>
{
    let provider = Et5Provider::create()
        .geometry(geometry.clone())
        .calibration(calibration.clone())
        .tracker_pitch_deg(pitch)
        .device_blob(&config.blob)
        .start()
        .context("connecting to the ET5; another process may already hold it")?;

    TrackerFeed::start(provider, t0)
}

/// The meta line for a click session.
fn session_meta(
    blob    : &BlobReport,
    display : &str,
    area    : DisplayArea,
    desk    : &str,
    pitch   : f64,
)
    -> SessionMeta
{
    let created = now_unix_s();

    SessionMeta {
        kind              : "meta".into(),
        format            : SESSION_FORMAT,
        session_id        : format!("{}-{}-clicks", created as u64, blob.short()),
        created_unix_s    : created,
        blob_sha256       : blob.body_sha256.clone(),
        blob_bytes        : blob.len,
        display           : display.to_string(),
        display_area      : area,
        desk_sha256       : sha256_of(desk),
        tracker_pitch_deg : pitch,
        // Not prompted: this runs all day and a prompt the user answers once in the
        // morning would be a lie by lunchtime. The click rows are labelled by their
        // element, not by an operator's diary.
        glasses           : false,
        note              : SESSION_NOTE.into(),
    }
}

/// The stand-in blob identity for a `--no-tracker` run, which describes no model.
fn no_tracker_blob() -> BlobReport {
    BlobReport::of(&[])
}

/// SHA-256 of the desk config text, so a desk change is detectable in the file.
fn sha256_of(text: &str) -> String {
    gaze_provider_et5::blob::sha256_hex(text.as_bytes())
}

/// The sensor-frame pitch from the desk config, degrees.
///
/// Lives in `desk.toml` as a top-level `tracker_pitch_deg`; the core geometry parser
/// ignores keys it does not know, so it is read separately, the same way
/// `gaze-et5-cli` reads it.
fn tracker_pitch_deg(path: &Path) -> f64 {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|text| toml::from_str::<toml::Value>(&text).ok())
        .and_then(|v| v.get("tracker_pitch_deg").and_then(toml::Value::as_float))
        .unwrap_or(0.0)
}

/// Distance between two points, logical pixels.
fn distance(a: GlobalPx, b: GlobalPx) -> f64 {
    ((a.x - b.x).powi(2) + (a.y - b.y).powi(2)).sqrt()
}

/// How old the frame behind a click was, relative to the press.
fn age_of(choice: FrameChoice) -> f64 {
    match choice {
        FrameChoice::Press { age_s }   => age_s,
        FrameChoice::Rolling { age_s } => age_s,
        FrameChoice::Stale             => f64::NAN,
    }
}

/// Median of a slice, which it sorts in place. NaN for an empty one.
pub fn median(values: &mut [f64]) -> f64 {
    if values.is_empty() {
        return f64::NAN;
    }

    values.sort_by(f64::total_cmp);

    let mid = values.len() / 2;

    if values.len() % 2 == 1 {
        return values[mid];
    }

    (values[mid - 1] + values[mid]) * 0.5
}

/// Current unix time, seconds.
fn now_unix_s() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// A recognised element as the session file records it.
fn click_element(element: &Element) -> ClickElement {
    ClickElement {
        kind  : kind_name(element.kind).to_string(),
        bbox  : element.bbox,
        text  : element.text.clone(),
        score : element.score,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_median_handles_both_parities_and_the_empty_case() {
        assert_eq!(median(&mut [3.0, 1.0, 2.0])          , 2.0);
        assert_eq!(median(&mut [4.0, 1.0, 3.0, 2.0])     , 2.5);
        assert_eq!(median(&mut [7.0])                    , 7.0);
        assert!(median(&mut []).is_nan());
    }

    #[test]
    fn distance_is_euclidean() {
        let a = GlobalPx { x: 0.0, y: 0.0 };
        let b = GlobalPx { x: 3.0, y: 4.0 };

        assert_eq!(distance(a, b), 5.0);
        assert_eq!(distance(a, a), 0.0);
    }

    #[test]
    fn the_frame_age_carries_the_sign_of_its_source() {
        assert!(age_of(FrameChoice::Press { age_s: 0.031 }) > 0.0);
        assert!(age_of(FrameChoice::Rolling { age_s: -0.3 }) < 0.0);
        assert!(age_of(FrameChoice::Stale).is_nan());
    }

    #[test]
    fn the_desk_config_carries_a_tracker_pitch() {
        // The real desk config, which every session's rows are resolved against.
        let pitch = tracker_pitch_deg(Path::new("../../config/desk.toml"));

        assert!(pitch.abs() < 45.0, "an implausible mount pitch: {pitch}");
    }
}
