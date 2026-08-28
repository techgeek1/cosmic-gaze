//! Reading the real mouse's buttons without taking it away from anybody.
//!
//! This is `gaze-proto`'s button reader with the one decision reversed. That one
//! `EVIOCGRAB`s a dedicated spare mouse, because a gaze prototype's commit button must
//! not also click whatever is under the pointer. This one reads the mouse the user is
//! actually working with, all day, while they work: it opens the node read-only, never
//! grabs, and never writes. The compositor keeps receiving every event exactly as it
//! would have.
//!
//! # Which node
//!
//! The daily mouse here is a G502 remapped through input-remapper, and the compositor
//! reads the *clone* input-remapper publishes rather than the hardware node. Reading the
//! hardware node would see presses the compositor never acts on and miss remapped ones,
//! so the default is the clone, by name. [`DEFAULT_NAME`] is that name; the fallback for
//! any other desk is the first device that has a left button and is not a keyboard.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};
use evdev::{Device, EventSummary, KeyCode};
use tracing::error;

use crate::click::{Button, ButtonEvent};

/// Name substring of the node the compositor reads on this desk.
pub const DEFAULT_NAME: &str = "input-remapper mouse";

/// Directory evdev nodes live under.
const DEV_INPUT_DIR: &str = "/dev/input";

/// How long the reader sleeps between polls of the non-blocking fd. Two milliseconds
/// caps the error on a press timestamp at the same figure, which is a tenth of the
/// device's frame period and nothing next to the 1.2 s gaze window.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// One candidate evdev node, for the `devices` listing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub path     : PathBuf,
    /// Kernel-reported device name, empty when the node does not give one.
    pub name     : String,
    /// Whether the node reports a left mouse button.
    pub buttons  : bool,
    /// Whether it also reports letter keys, which makes it a keyboard (or the
    /// keyboard half of a combo device) rather than a mouse.
    pub keyboard : bool,
}

/// A running reader thread on one mouse.
pub struct MouseReader {
    /// Where button events surface, drained by the collector.
    events : Receiver<ButtonEvent>,
    /// Set to stop the thread. Polled every [`POLL_INTERVAL`].
    stop   : Arc<AtomicBool>,
    /// `None` once joined, which makes [`stop`](Self::stop) idempotent.
    join   : Option<JoinHandle<()>>,
    /// The node that was opened, for the startup log.
    path   : PathBuf,
    /// Its kernel name.
    name   : String,
}

// --- MouseReader ---

impl MouseReader {
    /// Opens a mouse and starts reading it.
    ///
    /// `path` wins when given; otherwise the first node whose name contains
    /// `name_substr`, and failing that the first plain mouse on the system. The device
    /// is never grabbed, so every press still reaches the compositor.
    ///
    /// `t0` is the collector's clock: events are stamped with the reader's own arrival
    /// time against it rather than with the kernel's, because the kernel stamps on
    /// `CLOCK_REALTIME` and everything else in a session is monotonic.
    ///
    /// Every press also fires a `capture` request carrying a fresh id, so the screen
    /// capture starts here rather than after the event has crossed two more threads.
    pub fn open(
        path        : Option<&Path>,
        name_substr : &str,
        t0          : std::time::Instant,
        capture     : Sender<u64>,
    )
        -> Result<MouseReader>
    {
        let chosen = {
            match path {
                Some(p) => p.to_path_buf(),
                None    => choose(name_substr)?,
            }
        };

        let device = Device::open(&chosen)
            .with_context(|| open_hint(&chosen))?;

        let name = device.name().unwrap_or_default().to_string();

        // Non-blocking so the reader can poll the stop flag instead of parking in a
        // read with no way to interrupt it.
        device.set_nonblocking(true)
            .with_context(|| format!("setting {} non-blocking", chosen.display()))?;

        let stop     = Arc::new(AtomicBool::new(false));
        let (tx, rx) = crossbeam_channel::unbounded();

        let join = thread::Builder::new()
            .name("gaze-clicks-mouse".to_string())
            .spawn({
                let stop = Arc::clone(&stop);

                move || run(device, &tx, &capture, &stop, t0)
            })
            .context("spawning the mouse reader thread")?;

        Ok(MouseReader {
            events : rx,
            stop   : stop,
            join   : Some(join),
            path   : chosen,
            name   : name,
        })
    }

    /// The node being read.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Its kernel-reported name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// The channel button events arrive on.
    pub fn events(&self) -> &Receiver<ButtonEvent> {
        &self.events
    }

    /// Stops the reader thread and closes the device. Idempotent.
    pub fn stop(&mut self) {
        self.stop.store(true, Ordering::Relaxed);

        if let Some(join) = self.join.take() {
            let _ = join.join();
        }
    }
}

impl Drop for MouseReader {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- Enumeration ---

/// Every `/dev/input/event*` node this user can open, in numeric order.
///
/// Nodes that cannot be opened are omitted rather than reported: on a normal desktop
/// most of them belong to root and none of them is the mouse.
pub fn candidates() -> Vec<Candidate> {
    let mut found: Vec<(u32, Candidate)> = evdev::enumerate()
        .filter_map(|(path, device)| {
            let n = node_number(&path)?;

            let keys     = device.supported_keys();
            let buttons  = keys.is_some_and(|k| k.contains(KeyCode::BTN_LEFT));
            let keyboard = keys.is_some_and(|k| k.contains(KeyCode::KEY_A));

            Some((n, Candidate {
                path     : path,
                name     : device.name().unwrap_or_default().to_string(),
                buttons  : buttons,
                keyboard : keyboard,
            }))
        })
        .collect();

    found.sort_by_key(|(n, _)| *n);

    found.into_iter().map(|(_, c)| c).collect()
}

/// The node a run with no `--mouse` would open.
///
/// First the name match, which is how the right clone is picked out of a remapped
/// setup, then the first plain mouse. A device that also has letter keys is skipped:
/// combo receivers publish one node for the keyboard and one for the mouse, and only
/// the second one's buttons mean anything about the pointer.
pub fn choose(name_substr: &str) -> Result<PathBuf> {
    // The name pass reads sysfs rather than opening nodes, so a device can be found by
    // name even where only the wanted node is readable.
    if !name_substr.is_empty()
        && let Some(path) = find_by_sysfs_name(name_substr)
    {
        return Ok(path);
    }

    let all = candidates();

    if let Some(candidate) = all.iter().find(|c| c.buttons && !c.keyboard) {
        return Ok(candidate.path.clone());
    }

    bail!(
        "no evdev device named {name_substr:?} and no plain mouse among the {} \
         readable nodes; pass --mouse PATH (see `gaze-clicks-cli devices`)",
        all.len(),
    )
}

// --- Reader ---

/// Reads `device` until `stop` is set, forwarding left and right press and release.
///
/// A press fires its screen capture before the event is queued, so the capture starts
/// as close to the press as this process can manage. Autorepeat (value 2) is dropped:
/// a held button repeats, and each repeat is not a new click.
fn run(
    mut device : Device,
    tx         : &Sender<ButtonEvent>,
    capture    : &Sender<u64>,
    stop       : &AtomicBool,
    t0         : std::time::Instant,
)
{
    // Capture ids are unique for the life of the process, so a reply that arrives
    // after the collector gave up on it can never be mistaken for a later click's.
    let next_id = AtomicU64::new(0);

    while !stop.load(Ordering::Relaxed) {
        match device.fetch_events() {
            Ok(events) => {
                for event in events {
                    let EventSummary::Key(_, code, value) = event.destructure() else {
                        continue;
                    };

                    let Some(button) = Button::from_key(code) else {
                        continue;
                    };

                    if value > 1 {
                        continue;
                    }

                    let t_s     = t0.elapsed().as_secs_f64();
                    let pressed = value == 1;

                    let capture_id = {
                        if pressed {
                            let id = next_id.fetch_add(1, Ordering::Relaxed);
                            let _  = capture.send(id);

                            Some(id)
                        }
                        else {
                            None
                        }
                    };

                    let _ = tx.send(ButtonEvent {
                        button     : button,
                        pressed    : pressed,
                        t_s        : t_s,
                        capture_id : capture_id,
                    });
                }
            }

            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                thread::sleep(POLL_INTERVAL);
            }

            Err(e) => {
                error!(error = %e, "mouse read error, stopping the reader thread");

                break;
            }
        }
    }
}

// --- Helpers ---

/// The `N` of `/dev/input/eventN`.
fn node_number(path: &Path) -> Option<u32> {
    path.file_name()?.to_str()?.strip_prefix("event")?.parse().ok()
}

/// The first node whose sysfs name contains `substr`, in numeric order.
fn find_by_sysfs_name(substr: &str) -> Option<PathBuf> {
    let mut nodes: Vec<(u32, PathBuf)> = fs::read_dir(DEV_INPUT_DIR)
        .ok()?
        .filter_map(|entry| entry.ok())
        .filter_map(|entry| {
            let path = entry.path();

            Some((node_number(&path)?, path))
        })
        .collect();

    nodes.sort_by_key(|(n, _)| *n);

    for (n, path) in nodes {
        let sysfs = PathBuf::from(format!("/sys/class/input/event{n}/device/name"));

        if fs::read_to_string(&sysfs).is_ok_and(|name| name.trim().contains(substr)) {
            return Some(path);
        }
    }

    None
}

/// The context line for a failed open, which is nearly always a permissions problem.
fn open_hint(path: &Path) -> String {
    format!(
        "opening {} read-only; if this is a permission error, grant the user an ACL on \
         the node (setfacl -m u:$USER:r {})",
        path.display(),
        path.display(),
    )
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn node_numbers_come_off_the_path() {
        assert_eq!(node_number(Path::new("/dev/input/event23")), Some(23));
        assert_eq!(node_number(Path::new("/dev/input/event0")) , Some(0));
        assert_eq!(node_number(Path::new("/dev/input/mice"))   , None);
        assert_eq!(node_number(Path::new("/dev/input"))        , None);
    }

    #[test]
    fn the_open_hint_names_the_node_and_the_fix() {
        let hint = open_hint(Path::new("/dev/input/event23"));

        assert!(hint.contains("/dev/input/event23"));
        assert!(hint.contains("setfacl"));
    }
}
