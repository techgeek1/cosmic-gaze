//! The socket to the collector, on its own thread.
//!
//! The trainer never blocks on the collector: presses go into a channel and the thread
//! writes them out, reconnecting whenever the socket is missing or drops. A press sent
//! while disconnected is lost, and the count of those is shown in the window so a
//! session run without the collector is obviously not being recorded.

use std::io::Write;
use std::os::unix::net::UnixStream;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::thread;
use std::time::Duration;

use gaze_core::{TrainerMessage, socket_path};
use tracing::{debug, info, warn};

/// How long the thread waits between connection attempts.
const RETRY: Duration = Duration::from_secs(1);

/// Handle to the writer thread.
#[derive(Clone)]
pub struct Link {
    tx        : Sender<String>,
    connected : Arc<AtomicBool>,
    /// Presses dropped because nothing was listening.
    dropped   : Arc<AtomicU64>,
    /// Presses handed to the socket.
    sent      : Arc<AtomicU64>,
}

// --- Link ---

impl Link {
    /// Starts the thread. Never fails; a missing collector shows as `connected() ==
    /// false` and a rising `dropped()`.
    pub fn spawn(started_unix_s: f64) -> Link {
        let (tx, rx)  = channel::<String>();
        let connected = Arc::new(AtomicBool::new(false));
        let dropped   = Arc::new(AtomicU64::new(0));
        let sent      = Arc::new(AtomicU64::new(0));

        {
            let connected = Arc::clone(&connected);
            let dropped   = Arc::clone(&dropped);
            let sent      = Arc::clone(&sent);

            thread::Builder::new()
                .name("gaze-trainer-link".into())
                .spawn(move || run(rx, connected, dropped, sent, started_unix_s))
                .expect("spawning the link thread");
        }

        Link {
            tx        : tx,
            connected : connected,
            dropped   : dropped,
            sent      : sent,
        }
    }

    /// Queues one message.
    pub fn send(&self, message: &TrainerMessage) {
        match serde_json::to_string(message) {
            Ok(line) => {
                let _ = self.tx.send(line);
            }
            Err(e)   => warn!(error = %e, "could not serialise a trainer message"),
        }
    }

    /// Whether the collector is on the other end right now.
    pub fn connected(&self) -> bool {
        self.connected.load(Ordering::Relaxed)
    }

    /// Presses lost to a missing collector.
    pub fn dropped(&self) -> u64 {
        self.dropped.load(Ordering::Relaxed)
    }

    /// Presses delivered.
    pub fn sent(&self) -> u64 {
        self.sent.load(Ordering::Relaxed)
    }
}

/// The thread body: connect, greet, forward lines, repeat.
fn run(
    rx        : Receiver<String>,
    connected : Arc<AtomicBool>,
    dropped   : Arc<AtomicU64>,
    sent      : Arc<AtomicU64>,
    started   : f64,
) {
    let hello = serde_json::to_string(&TrainerMessage::Hello {
        app     : format!("gaze-trainer {}", env!("CARGO_PKG_VERSION")),
        started : started,
    })
    .expect("the hello line serialises");

    loop {
        let path = socket_path();

        let mut stream = match UnixStream::connect(&path) {
            Ok(s)  => s,
            Err(e) => {
                debug!(error = %e, path = %path.display(), "collector not listening");

                // Drain what arrived meanwhile; there is nobody to send it to.
                while rx.try_recv().is_ok() {
                    dropped.fetch_add(1, Ordering::Relaxed);
                }

                thread::sleep(RETRY);

                continue;
            }
        };

        if writeln!(stream, "{hello}").is_err() {
            thread::sleep(RETRY);

            continue;
        }

        info!(path = %path.display(), "connected to the collector");
        connected.store(true, Ordering::Relaxed);

        for line in rx.iter() {
            if writeln!(stream, "{line}").and_then(|_| stream.flush()).is_err() {
                warn!("the collector went away; reconnecting");
                dropped.fetch_add(1, Ordering::Relaxed);

                break;
            }

            sent.fetch_add(1, Ordering::Relaxed);
        }

        connected.store(false, Ordering::Relaxed);

        // The channel closing means the application is exiting.
        if rx.try_recv().is_err() && Arc::strong_count(&connected) == 1 {
            return;
        }
    }
}
