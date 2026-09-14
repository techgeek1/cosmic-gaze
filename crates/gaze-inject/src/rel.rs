//! The relative backend: a plain `REL_X`/`REL_Y` mouse. Two modes behind the same
//! `move_to`, chosen per call by whether a cursor tracker is connected:
//!
//! - **Closed-loop** (primary): when `gaze_capture::CursorTracker::connect()` succeeds,
//!   `move_to` reads the real cursor position, emits a relative step capped in magnitude
//!   toward the target, reads the position again, and repeats until it's within a pixel or
//!   the correction budget runs out. Pointer acceleration then only affects how many
//!   iterations convergence takes, never the final accuracy, because every step is
//!   measured against ground truth instead of assumed.
//! - **Open-loop** (fallback): when the tracker is unavailable, `move_to` homes to the
//!   desk layout's top-left corner with a saturating negative move, then applies one
//!   uncorrected relative delta computed from that known corner. This is the original,
//!   imprecise scheme - accurate only if the device happens to be configured for the
//!   `flat` libinput accel profile at unity speed - kept as a last resort so the backend
//!   still does *something* on a cosmic-comp without the cursor-position protocol.
//!
//! The tracker is a Wayland connection, and the compositor hangs up on a client that
//! stops reading its socket: every mouse motion is a `position` event, and a session
//! that only read the tracker around its own warps found the connection dead ("broken
//! pipe") after twenty seconds to a few minutes of ordinary mouse use, and stayed on
//! the last warp's position for the rest of the day. So the tracker is drained on
//! every [`InjectBackend::last_known_position`], which the session calls once a sample,
//! and a tracker that fails is dropped and reconnected after [`TRACKER_RETRY`]: a move
//! in between goes open loop rather than failing.

use std::thread;
use std::time::{Duration, Instant};

use evdev::uinput::VirtualDevice;
use evdev::{AttributeSet, InputEvent, KeyCode, KeyEvent, RelativeAxisCode, RelativeAxisEvent};
use gaze_capture::CursorTracker;
use gaze_core::GlobalPx;

use crate::codes::{button_code, WHEEL_HI_RES_PER_CLICK};
use crate::layout::DeskLayout;
use crate::{Button, InjectBackend, InjectError, Result};

/// Name reported to the compositor/udev for the relative device.
const DEVICE_NAME: &str = "gaze-inject (relative)";

/// Magnitude of the homing move's `REL_X`/`REL_Y`, in counts. Far larger than any real
/// desk layout (the widest configured layout here is under 6400 px), so the cursor
/// saturates against the top-left corner of the output layout regardless of libinput's
/// acceleration curve: acceleration only rescales a delta's magnitude, never its sign, so
/// a sufficiently large negative delta cannot undershoot the edge no matter how the curve
/// compresses it.
const HOME_DELTA: i32 = -1_000_000;

/// Largest single closed-loop correction step, in `REL_X`/`REL_Y` counts.
const MAX_STEP: i32 = 200;

/// Proportional gain applied to the fine-correction loop's commanded step, as a fraction
/// of the measured error. Measured empirically, not guessed: an undamped command (gain
/// 1.0, "correct the full measured error") against a real plant gain close to 1.0 is
/// exactly the marginal-stability case for a proportional controller - a real run
/// oscillated in a stable two-point limit cycle (2836 -> 3164 -> 2836 -> 3164 px, error
/// flipping sign with the same magnitude every step) instead of ever settling. Halving the
/// command turns that into geometric convergence instead (each step roughly halves the
/// remaining error assuming plant gain near 1), at the cost of needing roughly
/// log2(residual_px) extra iterations - well inside `MAX_ITERATIONS` once the coarse jump
/// below has already closed most of the distance.
const STEP_GAIN: f64 = 0.5;

/// Minimum wall-clock spacing enforced between consecutive closed-loop correction emits.
/// Measured empirically, not guessed: capping a step's *magnitude* at `MAX_STEP` alone
/// does not keep libinput's acceleration curve in a sane regime, because it responds to
/// implied velocity (counts per unit time), not raw count magnitude. Once
/// `wait_position` starts returning immediately (events already queued), successive
/// `emit_rel` calls landed under a millisecond apart and a commanded -200 count step
/// measured as roughly -400 actual px - about 2x gain - while the same -200 count step
/// sent ~200-350 ms apart earlier in the same run measured close to -177 to -190 (about
/// 0.9x gain, close to unity). Forcing at least this much time between emits keeps a
/// step's implied velocity in the same realistic, mouse-like range regardless of how fast
/// the tracker happens to answer, so the per-step gain stays close to that ~0.9x baseline
/// instead of drifting toward the burst-mode ~2x that made the naive version overshoot
/// and, at 50 ms polling, coalesce multiple resent commands into one wildly-oversized
/// correction.
const MIN_STEP_INTERVAL: Duration = Duration::from_millis(20);

/// Closed-loop convergence tolerance, in global px. Cursor position is reported in
/// integer device pixels, so on one output a pixel is as exact as measurement allows;
/// at the seam between outputs of different scales a one-count step lands on a
/// different pixel grid, and a 1 px tolerance had the pointer hopping across the seam
/// for the whole correction budget. Two pixels is under what a hand notices.
const CONVERGENCE_PX: f64 = 2.0;

/// Maximum closed-loop correction steps before giving up and returning
/// `InjectError::Unreachable`.
const MAX_ITERATIONS: u32 = 8;

/// How long each closed-loop iteration waits for a new measured position before retrying
/// with the same estimate. Measured empirically, not guessed: at 50 ms, cosmic-comp's
/// `ext_image_copy_capture_cursor_session_v1` reports did not keep pace with individual
/// steps at all - seven consecutive 50 ms waits timed out with no new report while the
/// same correction kept getting resent, then a single coalesced report landed reflecting
/// the cumulative effect of all of them at once, causing wild overshoot (a target 1508 px
/// away came back "unreachable" at 1508 px past it). 200 ms reliably gets a fresh report
/// on the following step instead of stacking blind resends.
const POLL_TIMEOUT: Duration = Duration::from_millis(200);

/// How long after a cursor tracker fails before another connection is tried. Short
/// enough that a warp rarely goes open loop twice in a row, long enough not to hammer a
/// compositor that is refusing the capture protocol.
const TRACKER_RETRY: Duration = Duration::from_secs(2);

/// `REL_X`/`REL_Y` device: a plain mouse. Which `move_to` strategy is used is decided per
/// call by whether `tracker` is `Some`; see the module docs.
pub(crate) struct RelativeInjector {
    device   : VirtualDevice,
    /// Top-left corner of the desk layout's union bounding box, in global px. Used as the
    /// open-loop fallback's homing target, and as the closed-loop path's best guess for
    /// "just homed, no position observed yet".
    origin   : GlobalPx,
    /// `Some` while a cursor tracker is connected: closed-loop mode. `None` after a
    /// connection failed or died: open-loop fallback until the next reconnect succeeds.
    tracker  : Option<CursorTracker>,
    /// When the tracker may next be reconnected, after a failure. `None` when one is
    /// connected, or none has ever failed.
    retry_at : Option<Instant>,
}

// --- RelativeInjector ---

impl RelativeInjector {
    /// Opens `/dev/uinput` and registers the relative device, then tries to connect a
    /// cursor tracker. A tracker failure is not fatal here - it selects the open-loop
    /// fallback until a later reconnect succeeds - so it's logged and swallowed rather
    /// than propagated.
    pub(crate) fn create(layout: &DeskLayout) -> Result<RelativeInjector> {
        let mut keys = AttributeSet::<KeyCode>::new();
        keys.insert(KeyCode::BTN_LEFT);
        keys.insert(KeyCode::BTN_RIGHT);
        keys.insert(KeyCode::BTN_MIDDLE);

        let mut rel_axes = AttributeSet::<RelativeAxisCode>::new();
        rel_axes.insert(RelativeAxisCode::REL_X);
        rel_axes.insert(RelativeAxisCode::REL_Y);
        rel_axes.insert(RelativeAxisCode::REL_WHEEL);
        rel_axes.insert(RelativeAxisCode::REL_WHEEL_HI_RES);

        let device = VirtualDevice::builder()
            .map_err(InjectError::UinputOpen)?
            .name(DEVICE_NAME)
            .with_keys(&keys)
            .map_err(InjectError::UinputSetup)?
            .with_relative_axes(&rel_axes)
            .map_err(InjectError::UinputSetup)?
            .build()
            .map_err(InjectError::UinputCreate)?;

        let (min_x, min_y, _, _) = layout.bounds();
        let origin = GlobalPx { x: min_x, y: min_y };

        let tracker = match CursorTracker::connect() {
            Ok(tracker) => Some(tracker),
            Err(err) => {
                tracing::warn!(
                    "gaze-capture cursor tracker unavailable ({err}), falling back to \
                     open-loop relative motion - accuracy will depend on the device's \
                     libinput accel profile"
                );
                None
            }
        };

        let retry_at = tracker.is_none().then(|| Instant::now() + TRACKER_RETRY);

        Ok(RelativeInjector { device: device, origin: origin, tracker: tracker, retry_at: retry_at })
    }

    /// The cursor tracker, reconnecting it first if one failed and the retry is due.
    /// `None` means open loop for now.
    fn tracker(&mut self) -> Option<&mut CursorTracker> {
        if self.tracker.is_none()
            && self.retry_at.is_some_and(|at| Instant::now() >= at)
        {
            match CursorTracker::connect() {
                Ok(tracker) => {
                    tracing::info!("cursor tracker reconnected; pointer moves are closed loop again");

                    self.tracker  = Some(tracker);
                    self.retry_at = None;
                }

                Err(err) => {
                    tracing::debug!("cursor tracker still unavailable ({err})");

                    self.retry_at = Some(Instant::now() + TRACKER_RETRY);
                }
            }
        }

        self.tracker.as_mut()
    }

    /// Drops a tracker whose connection failed and schedules the reconnect. The error
    /// carries the compositor's own reason when it sent one.
    fn lose_tracker(&mut self, err: &InjectError) {
        tracing::warn!(
            "cursor tracker lost ({err}); pointer moves are open loop until it reconnects"
        );

        self.tracker  = None;
        self.retry_at = Some(Instant::now() + TRACKER_RETRY);
    }

    /// Closed-loop `move_to`: repeatedly measures the real cursor position and steps
    /// toward `target`, capping each step so acceleration stays in a sane regime.
    fn move_to_closed_loop(
        device  : &mut VirtualDevice,
        tracker : &mut CursorTracker,
        origin  : GlobalPx,
        target  : GlobalPx,
    )
        -> Result<()>
    {
        let mut current = Self::measured_or_homed_position(device, tracker, origin)?;

        // Coarse stage: one uncapped jump straight at the target before the paced,
        // capped fine loop below. This is not optional for a large correction - measured
        // empirically, a long burst of repeated MAX_STEP-capped emits (needed when the
        // distance is many multiples of MAX_STEP) accumulates libinput's velocity-based
        // acceleration across the *whole burst*, not just per step, in a way
        // MIN_STEP_INTERVAL's per-step pacing does not fully tame: a real run targeting a
        // point ~1700 px away wandered unpredictably over 8 capped steps and never
        // converged, including the y axis briefly reversing direction while every command
        // sent was signed correctly toward the target - almost certainly the cursor
        // overshooting past an output boundary under compounding acceleration. A single
        // big relative jump does not have that failure mode (it's exactly the open-loop
        // scheme, just measured afterward instead of trusted blindly), so it's used to
        // close most of the distance before the small-step loop, which only then has to
        // clean up a short residual - exactly the regime the fine loop was confirmed to
        // converge in.
        if !converged(target, current) {
            let dx = (target.x - current.x).round() as i32;
            let dy = (target.y - current.y).round() as i32;

            Self::emit_rel(device, dx, dy)?;

            if let Some(p) = tracker.wait_position(POLL_TIMEOUT * 2).map_err(InjectError::CursorTracker)? {
                current = p;
            }
        }

        let mut last_emit: Option<Instant> = None;

        for _ in 0..MAX_ITERATIONS {
            if converged(target, current) {
                return Ok(());
            }

            // See `MIN_STEP_INTERVAL`'s docs: without this, a `wait_position` that
            // returns immediately (events already queued) lets consecutive emits land
            // under a millisecond apart, which reads as a much higher implied velocity to
            // libinput's acceleration curve than the same step spaced out realistically.
            if let Some(prev) = last_emit {
                let elapsed = prev.elapsed();
                if elapsed < MIN_STEP_INTERVAL {
                    thread::sleep(MIN_STEP_INTERVAL - elapsed);
                }
            }

            let dx = clamp_step(target.x - current.x);
            let dy = clamp_step(target.y - current.y);

            Self::emit_rel(device, dx, dy)?;
            last_emit = Some(Instant::now());

            if let Some(p) = tracker.wait_position(POLL_TIMEOUT).map_err(InjectError::CursorTracker)? {
                current = p;
            }
            tracing::debug!("closed loop: sent ({dx}, {dy}), now at {current:?}, target {target:?}");
            // Else: no new position event landed within the timeout - the compositor
            // hasn't reported one yet, or the step was too small to register a change.
            // Keep the previous estimate and let the next iteration's step (computed
            // against the same `current`) push further in the same direction.
        }

        if converged(target, current) {
            Ok(())
        }
        else {
            Err(InjectError::Unreachable { got: current, wanted: target })
        }
    }

    /// Open-loop `move_to`: home to the layout corner, then one uncorrected relative
    /// delta from there. The path taken while there is no tracker.
    fn move_to_open_loop(&mut self, target: GlobalPx) -> Result<()> {
        Self::home(&mut self.device)?;

        let dx = (target.x - self.origin.x).round() as i32;
        let dy = (target.y - self.origin.y).round() as i32;

        Self::emit_rel(&mut self.device, dx, dy)
    }

    /// Best known starting position for a closed-loop move: the tracker's last observed
    /// position if it has one, or - on a fresh session that hasn't seen the cursor move
    /// yet - home to the corner and wait for the resulting position event.
    fn measured_or_homed_position(
        device  : &mut VirtualDevice,
        tracker : &mut CursorTracker,
        origin  : GlobalPx,
    )
        -> Result<GlobalPx>
    {
        if let Some(p) = tracker.position().map_err(InjectError::CursorTracker)? {
            return Ok(p);
        }

        Self::home(device)?;

        let homed = tracker.wait_position(POLL_TIMEOUT).map_err(InjectError::CursorTracker)?;

        Ok(homed.unwrap_or(origin))
    }

    /// Emits the saturating negative `REL_X`/`REL_Y` pair that homes the cursor to the
    /// layout's top-left corner (see `HOME_DELTA`'s docs for why this is deterministic).
    fn home(device: &mut VirtualDevice) -> Result<()> {
        let home = [
            InputEvent::from(RelativeAxisEvent::new(RelativeAxisCode::REL_X, HOME_DELTA)),
            InputEvent::from(RelativeAxisEvent::new(RelativeAxisCode::REL_Y, HOME_DELTA)),
        ];

        device.emit(&home).map_err(InjectError::Emit)
    }

    /// Emits one `REL_X`/`REL_Y` delta pair.
    fn emit_rel(device: &mut VirtualDevice, dx: i32, dy: i32) -> Result<()> {
        let delta = [
            InputEvent::from(RelativeAxisEvent::new(RelativeAxisCode::REL_X, dx)),
            InputEvent::from(RelativeAxisEvent::new(RelativeAxisCode::REL_Y, dy)),
        ];

        device.emit(&delta).map_err(InjectError::Emit)
    }
}

impl InjectBackend for RelativeInjector {
    fn move_to(&mut self, target: GlobalPx) -> Result<()> {
        // Reconnect first if due; then the closed loop runs on the disjoint borrows of
        // the device and the tracker, so a failure can drop the tracker afterwards.
        let _ = self.tracker();

        let origin  = self.origin;
        let outcome = self.tracker.as_mut().map(|tracker| {
            Self::move_to_closed_loop(&mut self.device, tracker, origin, target)
        });

        match outcome {
            // The tracker died mid-move (the compositor hung up): this move goes open
            // loop, and the next one tries the tracker again.
            Some(Err(err @ InjectError::CursorTracker(_))) => {
                self.lose_tracker(&err);

                self.move_to_open_loop(target)
            }

            Some(result) => result,
            None         => self.move_to_open_loop(target),
        }
    }

    fn move_by(&mut self, dx: i32, dy: i32) -> Result<()> {
        Self::emit_rel(&mut self.device, dx, dy)
    }

    fn click(&mut self, button: Button) -> Result<()> {
        let code = button_code(button);

        self.device.emit(&[InputEvent::from(KeyEvent::new(code, 1))]).map_err(InjectError::Emit)?;
        self.device.emit(&[InputEvent::from(KeyEvent::new(code, 0))]).map_err(InjectError::Emit)
    }

    fn scroll(&mut self, dy: i32) -> Result<()> {
        let events = [
            InputEvent::from(RelativeAxisEvent::new(RelativeAxisCode::REL_WHEEL, dy)),
            InputEvent::from(RelativeAxisEvent::new(
                RelativeAxisCode::REL_WHEEL_HI_RES,
                dy * WHEEL_HI_RES_PER_CLICK,
            )),
        ];

        self.device.emit(&events).map_err(InjectError::Emit)
    }

    fn wheel(&mut self, clicks: i32, hi_res: i32) -> Result<()> {
        let mut events = Vec::with_capacity(2);

        if clicks != 0 {
            events.push(InputEvent::from(RelativeAxisEvent::new(RelativeAxisCode::REL_WHEEL, clicks)));
        }

        if hi_res != 0 {
            events.push(InputEvent::from(RelativeAxisEvent::new(RelativeAxisCode::REL_WHEEL_HI_RES, hi_res)));
        }

        if events.is_empty() {
            return Ok(());
        }

        self.device.emit(&events).map_err(InjectError::Emit)
    }

    fn measures(&self) -> bool {
        self.tracker.is_some()
    }

    /// Also the tracker's drain: see the module docs on why it must be called often.
    /// A tracker that fails here is dropped for reconnection and reads as no position,
    /// which every caller already handles.
    fn last_known_position(&mut self) -> Result<Option<GlobalPx>> {
        let Some(tracker) = self.tracker() else {
            return Ok(None);
        };

        match tracker.position() {
            Ok(position) => Ok(position),

            Err(e) => {
                self.lose_tracker(&InjectError::CursorTracker(e));

                Ok(None)
            }
        }
    }
}

/// True when `current` is within `CONVERGENCE_PX` of `target` on both axes.
fn converged(target: GlobalPx, current: GlobalPx) -> bool {
    (target.x - current.x).abs() <= CONVERGENCE_PX && (target.y - current.y).abs() <= CONVERGENCE_PX
}

/// Applies `STEP_GAIN` damping to a single-axis error, then rounds and clamps to
/// `[-MAX_STEP, MAX_STEP]` counts.
fn clamp_step(err: f64) -> i32 {
    (err * STEP_GAIN).round().clamp(-(MAX_STEP as f64), MAX_STEP as f64) as i32
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converged_true_within_tolerance() {
        let target = GlobalPx { x: 100.0, y: 100.0 };

        assert!(converged(target, GlobalPx { x: 100.0, y: 100.0 }));
        assert!(converged(target, GlobalPx { x: 100.5, y: 99.5 }));
    }

    #[test]
    fn converged_false_outside_tolerance() {
        let target = GlobalPx { x: 100.0, y: 100.0 };

        assert!(!converged(target, GlobalPx { x: 102.5, y: 100.0 }));
        assert!(!converged(target, GlobalPx { x: 100.0, y: 97.5 }));
    }

    #[test]
    fn clamp_step_applies_gain_below_cap() {
        // Regression test for the marginal-stability bug this replaced: an undamped
        // (gain 1.0) command against a real plant gain near 1.0 oscillated in a stable
        // two-point limit cycle instead of converging (measured live: 2836 -> 3164 ->
        // 2836 -> 3164 px). STEP_GAIN halves the commanded error so repeated correction
        // converges geometrically instead.
        assert_eq!(clamp_step(164.0), 82);
        assert_eq!(clamp_step(-164.0), -82);
    }

    #[test]
    fn clamp_step_saturates_at_max_step() {
        // A large residual (e.g. right after a coarse jump that undershot badly) must
        // still be capped, even after gain is applied.
        assert_eq!(clamp_step(10_000.0), MAX_STEP);
        assert_eq!(clamp_step(-10_000.0), -MAX_STEP);
    }

    #[test]
    fn clamp_step_rounds_small_errors_to_nonzero() {
        // A 1 px residual times a 0.5 gain rounds to a 1-count step, not a 0-count step
        // that would silently stall the loop just outside CONVERGENCE_PX.
        assert_eq!(clamp_step(1.0), 1);
    }
}
