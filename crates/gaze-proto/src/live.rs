//! What crosses between the session loop and whoever runs it, while it runs.
//!
//! The daemon holds one [`Live`] per session: it pushes tuning in when the config
//! changes, flips the pause flag and asks for an offset reset from its D-Bus methods,
//! and reads the [`Status`] the loop keeps current for its properties. The loop polls
//! it once per sample. Everything is a flag or a small copy behind a mutex, so the loop
//! never blocks on the daemon and the daemon never blocks on the loop.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use gaze_config::{Status, Tuning};

/// The session's live controls and its status, shared between the loop and its owner.
pub struct Live {
    /// The tuning to run under, replaced whole.
    tuning         : Mutex<Tuning>,
    /// Set by [`Live::set_tuning`], cleared when the loop takes the change.
    tuning_changed : AtomicBool,
    paused         : AtomicBool,
    popup_open     : AtomicBool,
    /// Set by [`Live::request_calibrate`]; the loop ends when it sees it, the owner
    /// clears it with [`Live::take_calibrate`] once the loop has returned.
    calibrate      : AtomicBool,
    stop           : AtomicBool,
    status         : Mutex<Status>,
}

// --- Live ---

impl Live {
    /// A session about to run under `tuning`, not paused, with nothing to report yet.
    pub fn new(tuning: Tuning) -> Arc<Live> {
        Arc::new(Live {
            tuning         : Mutex::new(tuning),
            tuning_changed : AtomicBool::new(false),
            paused         : AtomicBool::new(false),
            popup_open     : AtomicBool::new(false),
            calibrate      : AtomicBool::new(false),
            stop           : AtomicBool::new(false),
            status         : Mutex::new(Status::default()),
        })
    }

    /// Replaces the tuning; the loop applies it at its next sample.
    pub fn set_tuning(&self, tuning: Tuning) {
        *self.lock_tuning() = tuning;

        self.tuning_changed.store(true, Ordering::Release);
    }

    /// The tuning as last set.
    pub fn tuning(&self) -> Tuning {
        self.lock_tuning().clone()
    }

    /// The new tuning, once per change: `Some` on the first call after
    /// [`Live::set_tuning`], `None` until the next one.
    pub fn take_tuning_change(&self) -> Option<Tuning> {
        self.tuning_changed
            .swap(false, Ordering::Acquire)
            .then(|| self.tuning())
    }

    /// Pauses or resumes: paused, the loop draws and injects nothing.
    pub fn set_paused(&self, paused: bool) {
        self.paused.store(paused, Ordering::Relaxed);
    }

    pub fn paused(&self) -> bool {
        self.paused.load(Ordering::Relaxed)
    }

    /// Whether the applet's popup is open. Open, it covers whatever scrolls under it
    /// and the compositor cannot say where, so the loop starts no edge scroll and
    /// shows no band until it closes.
    pub fn set_popup_open(&self, open: bool) {
        self.popup_open.store(open, Ordering::Relaxed);
    }

    pub fn popup_open(&self) -> bool {
        self.popup_open.load(Ordering::Relaxed)
    }

    /// Asks for the quick calibration. The loop ends as for a stop, and the owner runs
    /// the ceremony on the released tracker before starting the next session.
    pub fn request_calibrate(&self) {
        self.calibrate.store(true, Ordering::Relaxed);
    }

    /// Whether a calibration is wanted. The loop's exit condition; not cleared here.
    pub fn calibrate_requested(&self) -> bool {
        self.calibrate.load(Ordering::Relaxed)
    }

    /// Whether a calibration was asked for since the last call, clearing it.
    pub fn take_calibrate(&self) -> bool {
        self.calibrate.swap(false, Ordering::Relaxed)
    }

    /// Asks the loop to end. It notices within one sample interval.
    pub fn stop(&self) {
        self.stop.store(true, Ordering::Relaxed);
    }

    pub fn stopped(&self) -> bool {
        self.stop.load(Ordering::Relaxed)
    }

    /// What the loop last reported.
    pub fn status(&self) -> Status {
        self.status.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Reports the loop's state. Called every sample; stores only on a change, so the
    /// common case is one lock and a compare.
    pub fn set_status(&self, status: Status) {
        let mut current = self.status.lock().unwrap_or_else(|e| e.into_inner());

        if *current != status {
            *current = status;
        }
    }
}

impl Live {
    /// The tuning lock, poison ignored: a panic elsewhere holding it leaves a valid
    /// tuning behind.
    fn lock_tuning(&self) -> std::sync::MutexGuard<'_, Tuning> {
        self.tuning.lock().unwrap_or_else(|e| e.into_inner())
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A tuning change is delivered exactly once, and the current tuning is readable
    /// whether or not it has been taken.
    #[test]
    fn a_tuning_change_is_taken_once() {
        let live = Live::new(Tuning::default());

        assert_eq!(live.take_tuning_change(), None);

        let tuning = Tuning { snap_deg: 3.0, ..Tuning::default() };

        live.set_tuning(tuning.clone());

        assert_eq!(live.tuning().snap_deg, 3.0);
        assert_eq!(live.take_tuning_change(), Some(tuning));
        assert_eq!(live.take_tuning_change(), None);
    }

    #[test]
    fn flags_are_taken_once_and_status_is_what_was_last_set() {
        let live = Live::new(Tuning::default());

        live.request_calibrate();

        assert!(live.calibrate_requested());
        assert!(live.take_calibrate());
        assert!(!live.take_calibrate());
        assert!(!live.calibrate_requested());

        let status = Status { tracker: true, ..Status::default() };

        live.set_status(status.clone());
        live.set_status(status.clone());

        assert_eq!(live.status(), status);
        assert!(!live.paused());

        live.set_paused(true);
        live.stop();

        assert!(live.paused());
        assert!(live.stopped());
    }
}
