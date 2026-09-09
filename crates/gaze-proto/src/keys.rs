//! The overlay latch key, read from every keyboard on the desk.
//!
//! The pointer look is shown while a thumb rests on the controller's pad; without a
//! controller in hand, or for a stretch of gaze-driven work where the thumb would get
//! tired, F14 latches it on and off. It is read the way the click feed reads the real
//! mouse: every evdev node that reports the key, read-only, never grabbed, so the key
//! still reaches the compositor and anything else that wants it. F14 is chosen because no
//! desktop binds it and most keyboards can emit it from a macro layer.

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result};
use crossbeam_channel::{Receiver, Sender};
use evdev::{Device, EventSummary, KeyCode};
use tracing::{debug, error};

use crate::source::Control;

/// The key that toggles the latch.
pub const LATCH_KEY: KeyCode = KeyCode::KEY_F14;

/// How long each reader thread sleeps between polls of its non-blocking fd.
const POLL_INTERVAL: Duration = Duration::from_millis(4);

/// Every keyboard that can emit the latch key, each on a thread of its own.
pub struct LatchKeys {
    events : Receiver<Control>,
    stop   : Arc<AtomicBool>,
    joins  : Vec<JoinHandle<()>>,
    paths  : Vec<PathBuf>,
}

// --- LatchKeys ---

impl LatchKeys {
    /// Opens every evdev node that reports the latch key. Fails only when none can be
    /// opened, which usually means the user is not in the `input` group.
    pub fn open() -> Result<LatchKeys> {
        let (tx, rx)  = crossbeam_channel::unbounded();
        let stop      = Arc::new(AtomicBool::new(false));
        let mut joins = Vec::new();
        let mut paths = Vec::new();

        for (path, device) in evdev::enumerate() {
            let has_key = device.supported_keys().is_some_and(|k| k.contains(LATCH_KEY));

            if !has_key {
                continue;
            }

            if let Err(e) = device.set_nonblocking(true) {
                debug!(path = %path.display(), error = %e, "latch key node not usable");

                continue;
            }

            let tx   = tx.clone();
            let stop = Arc::clone(&stop);
            let name = format!("gaze-latch-{}", path.display());

            let join = thread::Builder::new()
                .name(name)
                .spawn(move || run(device, &tx, &stop))
                .context("spawning a latch key reader")?;

            joins.push(join);
            paths.push(path);
        }

        if joins.is_empty() {
            anyhow::bail!("no readable input node reports {LATCH_KEY:?}");
        }

        Ok(LatchKeys { events: rx, stop: stop, joins: joins, paths: paths })
    }

    /// The nodes being read.
    pub fn paths(&self) -> &[PathBuf] {
        &self.paths
    }

    /// Presses since the last call.
    pub fn events(&self) -> impl Iterator<Item = Control> + '_ {
        self.events.try_iter()
    }

    /// Stops every reader. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        for join in self.joins.drain(..) {
            let _ = join.join();
        }
    }
}

impl Drop for LatchKeys {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- Internals ---

/// Reads one node until stopped, sending a toggle per press of the latch key.
fn run(mut device: Device, tx: &Sender<Control>, stop: &AtomicBool) {
    while !stop.load(Ordering::Relaxed) {
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    // Value 1 is a press; 2 is autorepeat and 0 a release, neither a
                    // toggle.
                    if let EventSummary::Key(_, code, 1) = event.destructure()
                        && code == LATCH_KEY
                    {
                        let _ = tx.send(Control::ToggleOverlay);
                    }
                }
            }

            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }

            Err(e) => {
                error!(error = %e, "latch key node read error, stopping its reader");

                break;
            }
        }
    }
}
