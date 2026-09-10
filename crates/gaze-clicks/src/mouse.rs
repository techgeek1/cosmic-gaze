//! Reading the real mouse's buttons without taking it away from anybody.
//!
//! This is `gaze-proto`'s button reader with the one decision reversed. That one
//! `EVIOCGRAB`s a dedicated spare mouse, because a gaze prototype's commit button must
//! not also click whatever is under the pointer. This one reads the mouse the user is
//! actually working with, all day, while they work: it opens the nodes read-only, never
//! grabs, and never writes. The compositor keeps receiving every event exactly as it
//! would have.
//!
//! # Which nodes: all of them
//!
//! The first version picked one node by name — the clone input-remapper publishes,
//! because the compositor acts on the clone rather than on the remapped hardware. That
//! bet lost a whole evening of data: the G502 re-enumerated (a new `eventN` appeared),
//! input-remapper kept grabbing the *old* node, the compositor quietly switched to
//! reading the new hardware node directly, and the reader sat on a clone that would
//! never speak again. The desktop worked, so nothing looked wrong; the session file
//! stayed empty.
//!
//! The version that cannot lose that bet reads **every node that looks like a mouse**
//! (has a left button, is not a keyboard), plus anything matching the configured name.
//! Grab semantics make this safe rather than double-counting: a remapper grabs its
//! source node, and a grabbed node delivers nothing to other readers, so for any
//! physical press exactly one of the open nodes speaks — the clone while the remapper
//! is live, the hardware node when it is not. A rescan every [`RESCAN_INTERVAL`] picks
//! up nodes that appear mid-run, which is exactly what a re-enumeration or a wireless
//! reconnect does. `--mouse PATH` still forces a single node and turns the rescan off.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use crossbeam_channel::{Receiver, Sender};
use evdev::{Device, EventSummary, InputEvent, KeyCode};
use tracing::{debug, info, warn};

use crate::click::{Button, ButtonEvent};

/// Name substring that is always included in the read set, whatever its capabilities:
/// the clone input-remapper publishes on this desk. Mouse-shaped nodes are read
/// regardless, so this only matters for a clone that hides its buttons.
pub const DEFAULT_NAME: &str = "input-remapper mouse";

/// Name substring of `gaze-inject`'s virtual pointer, which is never a user's mouse.
pub const INJECTOR_NAME: &str = "gaze-inject";

/// How long the reader sleeps between polls of the non-blocking fds. Two milliseconds
/// caps the error on a press timestamp at the same figure, which is a tenth of the
/// device's frame period and nothing next to the 1.2 s gaze window.
const POLL_INTERVAL: Duration = Duration::from_millis(2);

/// How often the reader looks for mouse nodes that appeared after it started. A
/// re-enumerated device is unreadable for at most this long.
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);

/// One candidate evdev node, for the `devices` listing and the read-set choice.
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

/// One open node the reader thread is polling.
struct Reading {
    path   : PathBuf,
    name   : String,
    device : Device,
}

/// A running reader thread over every mouse-shaped node.
pub struct MouseReader {
    /// Where button events surface, drained by the caller.
    events : Receiver<ButtonEvent>,
    /// Set to stop the thread. Polled every [`POLL_INTERVAL`].
    stop   : Arc<AtomicBool>,
    /// `None` once joined, which makes [`stop`](Self::stop) idempotent.
    join   : Option<JoinHandle<()>>,
    /// The nodes open at startup, path and kernel name, for the startup log.
    nodes  : Vec<(PathBuf, String)>,
    /// Nanoseconds after `t0` of the last motion, wheel or button event on any node,
    /// plus one; zero until the first. A hand on the mouse is read from this.
    input  : Arc<AtomicU64>,
    /// The clock `input` counts from.
    t0     : Instant,
}

// --- MouseReader ---

impl MouseReader {
    /// Opens the mice and starts reading them.
    ///
    /// `path` wins when given: that one node, no rescan. Otherwise every readable node
    /// [`wanted`] accepts is opened, and nodes that appear later are picked up by the
    /// rescan. Nothing is ever grabbed, so every press still reaches the compositor.
    ///
    /// `t0` is the caller's clock: events are stamped with the reader's own arrival
    /// time against it rather than with the kernel's, because the kernel stamps on
    /// `CLOCK_REALTIME` and everything else in a session is monotonic.
    ///
    /// Every press also fires a `capture` request carrying a fresh id, so a screen
    /// capture keyed to the press can start here rather than after the event has
    /// crossed more threads (the session has no use for it and drops the ids).
    pub fn open(
        path        : Option<&Path>,
        name_substr : &str,
        t0          : Instant,
        capture     : Sender<u64>,
    )
        -> Result<MouseReader>
    {
        let (readings, rescan) = {
            match path {
                Some(p) => (vec![open_node(p)?], None),
                None    => (open_read_set(name_substr)?, Some(name_substr.to_string())),
            }
        };

        let nodes = readings.iter()
            .map(|r| (r.path.clone(), r.name.clone()))
            .collect();

        let stop     = Arc::new(AtomicBool::new(false));
        let input    = Arc::new(AtomicU64::new(0));
        let (tx, rx) = crossbeam_channel::unbounded();

        let join = thread::Builder::new()
            .name("gaze-clicks-mouse".to_string())
            .spawn({
                let stop  = Arc::clone(&stop);
                let input = Arc::clone(&input);

                move || run(readings, rescan, &tx, &capture, &stop, &input, t0)
            })
            .context("spawning the mouse reader thread")?;

        Ok(MouseReader {
            events : rx,
            stop   : stop,
            join   : Some(join),
            nodes  : nodes,
            input  : input,
            t0     : t0,
        })
    }

    /// When the mouse last did anything: moved, scrolled, or had a button pressed or
    /// released. `None` until it has.
    pub fn last_input(&self) -> Option<Instant> {
        match self.input.load(Ordering::Relaxed) {
            0 => None,
            n => Some(self.t0 + Duration::from_nanos(n - 1)),
        }
    }

    /// The nodes that were open at startup, path and kernel name. The rescan may add
    /// more later; those are logged as they appear.
    pub fn nodes(&self) -> &[(PathBuf, String)] {
        &self.nodes
    }

    /// The channel button events arrive on.
    pub fn events(&self) -> &Receiver<ButtonEvent> {
        &self.events
    }

    /// Stops the reader thread and closes the devices. Idempotent.
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

/// Whether a run with no `--mouse` reads this node.
///
/// Anything mouse-shaped: a left button and no letter keys, so combo receivers'
/// keyboard halves stay out. The name match additionally admits the configured clone
/// even if it were to hide its buttons. Reading several at once is safe because a
/// grabbed node (a remapper's source) is silent to every other reader — exactly one
/// open node speaks for any physical press.
pub fn wanted(candidate: &Candidate, name_substr: &str) -> bool {
    // `gaze-inject`'s uinput device is mouse-shaped, and every press it makes is one
    // the gaze itself placed: reading it back would label the gaze with the gaze.
    if candidate.name.contains(INJECTOR_NAME) {
        return false;
    }

    let mouse = candidate.buttons && !candidate.keyboard;
    let named = !name_substr.is_empty() && candidate.name.contains(name_substr);

    mouse || named
}

/// The nodes a run with no `--mouse` would read, in numeric order.
pub fn read_set(name_substr: &str) -> Vec<Candidate> {
    candidates().into_iter().filter(|c| wanted(c, name_substr)).collect()
}

// --- Reader ---

/// Opens every node in the read set, tolerating individual failures.
fn open_read_set(name_substr: &str) -> Result<Vec<Reading>> {
    let set = read_set(name_substr);

    let mut readings = Vec::new();

    for candidate in &set {
        match open_node(&candidate.path) {
            Ok(reading) => readings.push(reading),
            Err(e)      => warn!(node = %candidate.path.display(), error = %e,
                                 "skipping an unreadable mouse node"),
        }
    }

    if readings.is_empty() {
        bail!(
            "no readable mouse node (looked for a left button, or a name containing \
             {name_substr:?}); see `gaze-clicks-cli devices`, and if this is a \
             permission error, grant the user an ACL on the node (setfacl -m u:$USER:r \
             /dev/input/eventN)",
        );
    }

    Ok(readings)
}

/// Opens one node read-only and non-blocking.
fn open_node(path: &Path) -> Result<Reading> {
    let device = Device::open(path)
        .with_context(|| open_hint(path))?;

    let name = device.name().unwrap_or_default().to_string();

    // Non-blocking so the reader can poll the stop flag instead of parking in a
    // read with no way to interrupt it.
    device.set_nonblocking(true)
        .with_context(|| format!("setting {} non-blocking", path.display()))?;

    Ok(Reading {
        path   : path.to_path_buf(),
        name   : name,
        device : device,
    })
}

/// Reads every open node until `stop` is set, forwarding left and right press and
/// release and stamping `input` with every event a hand on the mouse produces.
///
/// A press fires its screen capture before the event is queued, so the capture starts
/// as close to the press as this process can manage. A node that errors is dropped
/// alone; with a rescan configured the thread keeps running even with none open,
/// because the next rescan may bring the mouse back.
fn run(
    mut readings : Vec<Reading>,
    rescan       : Option<String>,
    tx           : &Sender<ButtonEvent>,
    capture      : &Sender<u64>,
    stop         : &AtomicBool,
    input        : &AtomicU64,
    t0           : Instant,
)
{
    // Capture ids are unique for the life of the process, so a reply that arrives
    // after the caller gave up on it can never be mistaken for a later click's.
    let mut next_id: u64 = 0;

    let mut next_rescan = Instant::now() + RESCAN_INTERVAL;

    while !stop.load(Ordering::Relaxed) {
        let mut forwarded = false;
        let mut dead      = Vec::new();

        for (i, reading) in readings.iter_mut().enumerate() {
            match reading.device.fetch_events() {
                Ok(events) => {
                    for event in events {
                        note_input(event, input, t0);

                        forwarded |= forward(event, &mut next_id, tx, capture, t0);
                    }
                }

                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {}

                Err(e) => {
                    warn!(node = %reading.path.display(), error = %e,
                          "mouse node stopped; dropping it");

                    dead.push(i);
                }
            }
        }

        for i in dead.into_iter().rev() {
            readings.remove(i);
        }

        match &rescan {
            Some(name_substr) => {
                if Instant::now() >= next_rescan {
                    rescan_nodes(&mut readings, name_substr);

                    next_rescan = Instant::now() + RESCAN_INTERVAL;
                }
            }

            // A forced single node that died is the end of the run: the caller asked
            // for exactly that node and it is gone.
            None => {
                if readings.is_empty() {
                    break;
                }
            }
        }

        if !forwarded {
            thread::sleep(POLL_INTERVAL);
        }
    }
}

/// Stamps `input` when the event is one a hand makes: relative motion (which includes
/// the wheel) or any key. Sync reports and the odd misc event are not a hand.
fn note_input(event: InputEvent, input: &AtomicU64, t0: Instant) {
    let hand = matches!(
        event.destructure(),
        EventSummary::RelativeAxis(..) | EventSummary::Key(..)
    );

    if hand {
        let ns = t0.elapsed().as_nanos().min(u64::MAX as u128 - 1) as u64;

        input.store(ns + 1, Ordering::Relaxed);
    }
}

/// Forwards one event when it is a button press or release worth keeping. Autorepeat
/// (value 2) is dropped: a held button repeats, and each repeat is not a new click.
fn forward(
    event   : InputEvent,
    next_id : &mut u64,
    tx      : &Sender<ButtonEvent>,
    capture : &Sender<u64>,
    t0      : Instant,
)
    -> bool
{
    let EventSummary::Key(_, code, value) = event.destructure() else {
        return false;
    };

    let Some(button) = Button::from_key(code) else {
        return false;
    };

    if value > 1 {
        return false;
    }

    let t_s     = t0.elapsed().as_secs_f64();
    let pressed = value == 1;

    let capture_id = {
        if pressed {
            let id = *next_id;
            *next_id += 1;

            let _ = capture.send(id);

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

    true
}

/// Opens any read-set node that is not already open.
///
/// A node that was dropped for an error but still enumerates is retried here, which
/// costs a debug line every interval and recovers the moment it opens.
fn rescan_nodes(readings: &mut Vec<Reading>, name_substr: &str) {
    for candidate in read_set(name_substr) {
        if readings.iter().any(|r| r.path == candidate.path) {
            continue;
        }

        match open_node(&candidate.path) {
            Ok(reading) => {
                info!(node = %reading.path.display(), name = %reading.name,
                      "a mouse node appeared; reading it");

                readings.push(reading);
            }

            Err(e) => debug!(node = %candidate.path.display(), error = %e,
                             "a new mouse node is not readable"),
        }
    }
}

// --- Helpers ---

/// The `N` of `/dev/input/eventN`.
fn node_number(path: &Path) -> Option<u32> {
    path.file_name()?.to_str()?.strip_prefix("event")?.parse().ok()
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

    /// A candidate with the given shape.
    fn candidate(name: &str, buttons: bool, keyboard: bool) -> Candidate {
        Candidate {
            path     : PathBuf::from("/dev/input/event0"),
            name     : name.to_string(),
            buttons  : buttons,
            keyboard : keyboard,
        }
    }

    #[test]
    fn node_numbers_come_off_the_path() {
        assert_eq!(node_number(Path::new("/dev/input/event23")), Some(23));
        assert_eq!(node_number(Path::new("/dev/input/event0")) , Some(0));
        assert_eq!(node_number(Path::new("/dev/input/mice"))   , None);
        assert_eq!(node_number(Path::new("/dev/input"))        , None);
    }

    #[test]
    fn the_read_set_is_every_mouse_plus_the_named_clone() {
        // Mouse-shaped nodes are read whatever they are called.
        assert!(wanted(&candidate("Logitech Gaming Mouse G502", true, false), DEFAULT_NAME));
        assert!(wanted(&candidate("PixArt Lenovo USB Optical Mouse", true, false), ""));

        // The named clone is read even if it hid its buttons.
        assert!(wanted(&candidate("input-remapper mouse", false, false), DEFAULT_NAME));

        // Keyboards and combo keyboard halves are not, even with buttons.
        assert!(!wanted(&candidate("Keychron Q6 Max Keyboard", false, true), DEFAULT_NAME));
        assert!(!wanted(&candidate("G502 Keyboard", true, true), DEFAULT_NAME));

        // Speakers, HDMI audio and the rest of /dev/input.
        assert!(!wanted(&candidate("PC Speaker", false, false), DEFAULT_NAME));
    }

    #[test]
    fn the_open_hint_names_the_node_and_the_fix() {
        let hint = open_hint(Path::new("/dev/input/event23"));

        assert!(hint.contains("/dev/input/event23"));
        assert!(hint.contains("setfacl"));
    }
}
