//! The trainer's answers, taken over a Unix socket on their own thread.
//!
//! `gaze-trainer` is a real application whose every control is labelled, and it tells
//! this collector what was under each press it received (`gaze_core::trainer`). The
//! collector matches those answers to the presses it saw itself on evdev by wall-clock
//! time, and for a matched press the trainer's word replaces the tree's and the
//! recogniser's: it is the application itself speaking, which is what the tree is a
//! proxy for, and it costs no inference.
//!
//! The socket is the collector's: it listens, the trainer connects. A trainer started
//! before the collector retries until the socket appears; a collector started without a
//! trainer runs exactly as before. Presses arrive within a few milliseconds of the evdev
//! event and are consumed at the release, so the queue never holds more than a handful.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader};
use std::os::unix::net::UnixListener;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;

use anyhow::{Context, Result};
use gaze_core::{TrainerElement, TrainerMessage, TrainerTag, socket_path};
use tracing::{debug, info, warn};

/// How far apart the trainer's press time and the evdev press time may be and still be
/// the same press, seconds. The compositor delivers the press to the trainer a few
/// milliseconds after evdev; this is generous by an order of magnitude and still well
/// under any double-click gap.
pub const MATCH_TOLERANCE_S: f64 = 0.150;

/// Presses kept waiting for a match. Every press is consumed at its release, so this
/// only fills when the trainer is clicked on with the evdev reader not seeing it.
const QUEUE_CAP: usize = 32;

/// One press the trainer reported.
#[derive(Clone, Debug)]
pub struct TrainerPress {
    /// Unix time the trainer saw the press, seconds.
    pub t_unix_s : f64,
    /// Where the pointer was, window-local logical pixels.
    pub px       : [f64; 2],
    /// The widget under it, or `None` for a press on nothing labelled.
    pub element  : Option<TrainerElement>,
    /// Theme background luminance, [0, 1].
    pub luma     : f64,
    pub tag      : TrainerTag,
}

/// Handle to the listener thread.
pub struct TrainerLink {
    path      : PathBuf,
    presses   : Arc<Mutex<VecDeque<TrainerPress>>>,
    connected : Arc<AtomicBool>,
}

// --- TrainerLink ---

impl TrainerLink {
    /// Binds the socket and starts accepting connections.
    ///
    /// A stale socket file from a killed collector is removed first; two collectors
    /// cannot run at once anyway, since only one can hold the tracker.
    pub fn listen() -> Result<TrainerLink> {
        let path = socket_path();

        if path.exists() {
            std::fs::remove_file(&path)
                .with_context(|| format!("removing the stale socket {}", path.display()))?;
        }

        let listener = UnixListener::bind(&path)
            .with_context(|| format!("binding {}", path.display()))?;

        let presses   = Arc::new(Mutex::new(VecDeque::new()));
        let connected = Arc::new(AtomicBool::new(false));

        {
            let presses   = Arc::clone(&presses);
            let connected = Arc::clone(&connected);

            thread::Builder::new()
                .name("gaze-clicks-trainer".into())
                .spawn(move || accept_loop(listener, presses, connected))
                .context("spawning the trainer listener")?;
        }

        info!(path = %path.display(), "listening for a trainer");

        Ok(TrainerLink {
            path      : path,
            presses   : presses,
            connected : connected,
        })
    }

    /// Whether a trainer is connected right now.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// The trainer press nearest to `t_unix_s` within [`MATCH_TOLERANCE_S`], removed
    /// from the queue, along with everything older than it.
    ///
    /// Older presses are dropped because they belong to presses the collector has
    /// already finished with (or never saw), and keeping them would let a stale answer
    /// match a later click.
    pub fn take_near(&self, t_unix_s: f64) -> Option<TrainerPress> {
        let mut queue = self.presses.lock().ok()?;

        let nearest = queue.iter()
            .enumerate()
            .map(|(i, p)| (i, (p.t_unix_s - t_unix_s).abs()))
            .filter(|(_, dt)| *dt <= MATCH_TOLERANCE_S)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(i, _)| i)?;

        let press = queue.remove(nearest)?;

        queue.drain(..nearest);

        Some(press)
    }

    /// Drops every press older than `t_unix_s` minus the tolerance. Called on presses
    /// the collector refuses before matching, so the queue cannot accumulate.
    pub fn expire_before(&self, t_unix_s: f64) {
        let Ok(mut queue) = self.presses.lock() else {
            return;
        };

        queue.retain(|p| p.t_unix_s >= t_unix_s - MATCH_TOLERANCE_S);
    }
}

impl Drop for TrainerLink {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Accepts connections one at a time. A trainer is one process on one desk; a second
/// connection is served after the first closes.
fn accept_loop(
    listener  : UnixListener,
    presses   : Arc<Mutex<VecDeque<TrainerPress>>>,
    connected : Arc<AtomicBool>,
) {
    for stream in listener.incoming() {
        let stream = match stream {
            Ok(s)  => s,
            Err(e) => {
                warn!(error = %e, "trainer socket accept failed");

                continue;
            }
        };

        connected.store(true, Ordering::Relaxed);

        let reader = BufReader::new(stream);

        for line in reader.lines() {
            let line = match line {
                Ok(l)  => l,
                Err(e) => {
                    debug!(error = %e, "trainer connection read failed");

                    break;
                }
            };

            match serde_json::from_str::<TrainerMessage>(&line) {
                Ok(TrainerMessage::Hello { app, started }) => {
                    println!("trainer connected: {app} (started {started:.0})");
                }

                Ok(TrainerMessage::Press { t_unix_s, px, element, luma, tag, .. }) => {
                    let Ok(mut queue) = presses.lock() else {
                        break;
                    };

                    if queue.len() >= QUEUE_CAP {
                        queue.pop_front();
                    }

                    queue.push_back(TrainerPress {
                        t_unix_s : t_unix_s,
                        px       : px,
                        element  : element,
                        luma     : luma,
                        tag      : tag,
                    });
                }

                Err(e) => {
                    warn!(error = %e, "unreadable line from the trainer");
                }
            }
        }

        connected.store(false, Ordering::Relaxed);
        println!("trainer disconnected");
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// A link with no thread behind it, for the queue rules.
    fn link_with(times: &[f64]) -> TrainerLink {
        let queue = times.iter().map(|t| TrainerPress {
            t_unix_s : *t,
            px       : [0.0, 0.0],
            element  : None,
            luma     : 0.5,
            tag      : TrainerTag {
                task    : 0,
                step    : 0,
                hit     : false,
                posture : "normal".into(),
                theme   : "dark".into(),
            },
        }).collect();

        TrainerLink {
            path      : PathBuf::from("/nonexistent/gaze-clicks-test.sock"),
            presses   : Arc::new(Mutex::new(queue)),
            connected : Arc::new(AtomicBool::new(false)),
        }
    }

    #[test]
    fn the_nearest_press_within_tolerance_is_taken_and_older_ones_dropped() {
        let link = link_with(&[10.0, 11.0, 11.05, 12.0]);

        let press = link.take_near(11.06).expect("a press matches");

        assert_eq!(press.t_unix_s, 11.05);

        // 10.0 and 11.0 were older than the match and are gone; 12.0 survives.
        let left: Vec<f64> = link.presses.lock().unwrap().iter().map(|p| p.t_unix_s).collect();

        assert_eq!(left, vec![12.0]);
    }

    #[test]
    fn a_press_outside_tolerance_does_not_match() {
        let link = link_with(&[10.0]);

        assert!(link.take_near(10.0 + MATCH_TOLERANCE_S + 0.01).is_none());
        assert!(link.take_near(10.0 - MATCH_TOLERANCE_S - 0.01).is_none());

        // Nothing was consumed by the misses.
        assert_eq!(link.presses.lock().unwrap().len(), 1);
    }

    #[test]
    fn expiry_keeps_only_what_could_still_match() {
        let link = link_with(&[10.0, 10.9, 11.0]);

        link.expire_before(11.0);

        let left: Vec<f64> = link.presses.lock().unwrap().iter().map(|p| p.t_unix_s).collect();

        assert_eq!(left, vec![10.9, 11.0]);
    }
}
