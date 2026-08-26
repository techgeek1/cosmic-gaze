//! A reader for the Lenovo mouse's buttons and wheel.
//!
//! The synthetic provider grabs that mouse and hands its buttons back as provider events.
//! Every other provider needs its own reader, because gaze cannot commit itself (DESIGN.md
//! section 3, principle 1) and there is no keyboard grab here.
//!
//! # Why this grabs by default
//!
//! The Lenovo is a dedicated spare; the real mouse on this desk is the G502 (PLAN.md's
//! environment facts). So `EVIOCGRAB` is the right default: without it the compositor sees
//! every press too, which means the commit button also clicks whatever the pointer happens
//! to be over and the exit button also right-clicks the desktop. That is not theoretical --
//! it ended a webcam test session early on 2026-08-26.
//!
//! Grabbing also hands the wheel to us exclusively, which is what lets the scroll tier warp
//! first and then re-inject the scroll, instead of racing a wheel event the compositor has
//! already dispatched to the old window.
//!
//! `--no-grab` keeps the old behaviour for anyone reading a mouse they still want to use as
//! a mouse. The session then only warps on a wheel event and lets the compositor's own
//! delivery land, at the cost of the first detent of a burst going to the window that was
//! under the pointer before the warp.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use evdev::{Device, EventSummary, KeyCode, RelativeAxisCode};
use gaze_provider_synthetic::WheelAccumulator;
use gaze_provider_synthetic::device::find_device;
use tracing::error;

use crate::source::Control;

/// How long the reader thread sleeps between polls of the non-blocking evdev fd. Same
/// figure the synthetic provider uses: short enough that `stop` returns promptly, long
/// enough not to spin a core on an idle mouse.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// Buttons and wheel from an evdev mouse, grabbed unless the caller says otherwise.
///
/// Motion is read and discarded: this reports presses and whole wheel detents only, so a
/// grabbed device contributes nothing to where the pointer is. Used by every provider mode
/// except `synthetic`, which gets the same events from the device it grabbed itself.
pub struct ButtonSource {
    /// Where the events surface. Unbounded, drained by [`events`](Self::events).
    events : Receiver<Control>,
    /// Set to stop the reader thread. Polled every [`POLL_INTERVAL`].
    stop   : Arc<AtomicBool>,
    /// `None` once joined, which is what makes [`stop`](Self::stop) idempotent.
    join   : Option<JoinHandle<()>>,
    /// The device node that was opened, for logging.
    path   : PathBuf,
    /// Whether the device was `EVIOCGRAB`ed, which decides whether the compositor is also
    /// seeing these events. The scroll tier reads it to decide whether to re-inject the
    /// wheel.
    grabbed : bool,
}

// --- ButtonSource ---

impl ButtonSource {
    /// Opens the device at `path`, or the first `/dev/input/event*` whose name contains
    /// `name_substr`, and starts reading it. Motion is ignored entirely: only buttons and
    /// the wheel come back.
    ///
    /// With `grab` set the device is `EVIOCGRAB`ed, so nothing it reports reaches the
    /// compositor for the life of this source. See the module docs for why that is the
    /// default. The grab is released when the reader thread closes the fd, which
    /// [`stop`](Self::stop) (and so `Drop`) waits for.
    pub fn open(path: Option<&Path>, name_substr: &str, grab: bool) -> Result<ButtonSource> {
        let (path, mut device) = find_device(path, name_substr)
            .with_context(|| format!("opening a button device matching {name_substr:?}"))?;

        if grab {
            device.grab().with_context(|| {
                format!("grabbing {}: another process may already hold it", path.display())
            })?;
        }

        // Non-blocking so the reader can poll the stop flag instead of parking in a read
        // with no way to interrupt it.
        device.set_nonblocking(true)
            .with_context(|| format!("setting {} non-blocking", path.display()))?;

        let stop = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();

        let join = thread::spawn({
            let stop = Arc::clone(&stop);

            move || run(device, &tx, &stop)
        });

        Ok(ButtonSource {
            events  : rx,
            stop    : stop,
            join    : Some(join),
            path    : path,
            grabbed : grab,
        })
    }

    /// The device node being read, for the startup log.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Whether this source has the device to itself. False means the compositor is seeing
    /// the same presses and wheel events, which the scroll tier has to account for.
    pub fn grabbed(&self) -> bool {
        self.grabbed
    }

    /// Drains the controls queued since the last call, without blocking.
    pub fn events(&self) -> impl Iterator<Item = Control> + '_ {
        self.events.try_iter()
    }

    /// Stops the reader thread and closes the device. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for ButtonSource {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- Reader ---

/// Reads `device` until `stop` is set, forwarding button presses and whole wheel detents.
///
/// Releases are dropped: the loop only acts on presses, and nothing here needs to know that
/// a button is being held. Relative motion is dropped too, which is the whole point of this
/// being a button source rather than a provider. Returning drops `device`, and closing the
/// fd is what releases the grab.
fn run(mut device: Device, tx: &Sender<Control>, stop: &AtomicBool) {
    // One accumulator for the life of the device: which wheel axis this mouse reports is a
    // property of the mouse, and sub-detent motion has to survive across read batches.
    let mut wheel = WheelAccumulator::new();

    while !stop.load(Ordering::Relaxed) {
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    match event.destructure() {
                        EventSummary::Key(_, code, value) if value != 0 => {
                            if let Some(control) = map_key(code) {
                                let _ = tx.send(control);
                            }
                        }

                        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_WHEEL, value) => {
                            wheel.notch(value);
                        }

                        EventSummary::RelativeAxis(_, RelativeAxisCode::REL_WHEEL_HI_RES, value) => {
                            wheel.hi_res(value);
                        }

                        _ => {}
                    }
                }

                // Taken once per batch, after both wheel axes have been seen, so the
                // accumulator can decide which of them this device actually uses.
                let detents = wheel.take();

                if detents != 0 {
                    let _ = tx.send(Control::Wheel(detents));
                }
            }

            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }

            Err(e) => {
                error!(error = %e, "button device read error, stopping the reader thread");

                break;
            }
        }
    }
}

/// Maps a pressed key to the control it drives, matching the synthetic provider's
/// assignment so the same three buttons mean the same three things in every mode.
fn map_key(code: KeyCode) -> Option<Control> {
    match code {
        KeyCode::BTN_LEFT   => Some(Control::Commit),
        KeyCode::BTN_RIGHT  => Some(Control::Exit),
        KeyCode::BTN_MIDDLE => Some(Control::Redetect),
        _                   => None,
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The three buttons must mean the same thing here as they do on the grabbed device,
    /// or switching providers would silently move the exit button.
    #[test]
    fn the_three_mouse_buttons_map_to_the_three_controls() {
        assert_eq!(map_key(KeyCode::BTN_LEFT),   Some(Control::Commit));
        assert_eq!(map_key(KeyCode::BTN_RIGHT),  Some(Control::Exit));
        assert_eq!(map_key(KeyCode::BTN_MIDDLE), Some(Control::Redetect));

        // Side buttons and keyboard keys on a combo device are ignored, not guessed at.
        assert_eq!(map_key(KeyCode::BTN_SIDE), None);
        assert_eq!(map_key(KeyCode::KEY_A),    None);
    }
}
