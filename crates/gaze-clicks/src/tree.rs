//! The accessibility tree, asked once per press on its own thread.
//!
//! `gaze_a11y::A11y` answers "what is at this point" in single-digit milliseconds when
//! the application under the pointer is on the bus, and not at all when it is not. The
//! answer is authoritative when it comes (DESIGN.md §2: a11y first, vision for what the
//! tree does not have), so the collector asks at the press, in parallel with the screen
//! capture, and reads the reply when the recogniser's arrives. A stuck application would
//! stall this thread and nothing else: the reply is taken with a timeout and its absence
//! means "the pixels are all there is".
//!
//! The thread owns a `ToplevelTracker` as well, because the point has to be placed in a
//! window before the window can be asked, and both are `!Send`.
//!
//! # The wedged-application hazard
//!
//! A D-Bus call to an application that has stopped answering (suspended, deadlocked)
//! blocks with no timeout — measured at 138 s against a `SIGSTOP`ped process, ending
//! only when the process resumed — and one such application stalls queries for every
//! window, because the application lookup walks the registry's whole list. The thread
//! is therefore watched: when questions are outstanding and no answer has arrived for
//! [`STUCK_AFTER`], [`take`](TreeService::take) abandons the thread and starts a fresh
//! one. The old thread parks on its blocked call; if that call ever returns, its reply
//! channel is closed and it exits on its own.

use std::collections::HashMap;
use std::thread;
use std::time::{Duration, Instant};

use crossbeam_channel::{Receiver, Sender, unbounded};
use gaze_a11y::{A11y, Hit};
use gaze_capture::ToplevelTracker;
use gaze_core::GlobalPx;
use tracing::{debug, info, warn};

/// How long the collector waits for the tree's reply once the recogniser has answered.
/// The query started at the press, so this is normally already satisfied.
pub const TREE_TIMEOUT: Duration = Duration::from_millis(40);

/// How long the thread may sit on outstanding questions before it is declared wedged
/// and replaced. Long enough that main-thread jank in the queried application never
/// trips it; a real wedge lasts until the process is resumed, which can be all day.
const STUCK_AFTER: Duration = Duration::from_secs(5);

/// The watchdog's ceiling. Each replacement that wedges in turn doubles the wait
/// before the next — a permanently stopped application would otherwise cost a parked
/// thread every [`STUCK_AFTER`] all day — and a replacement that answers anything
/// resets it. Recovery after the wedge clears is at worst one ceiling late.
const STUCK_CEILING: Duration = Duration::from_secs(320);

/// One question.
#[derive(Clone, Copy, Debug)]
pub struct TreeRequest {
    pub id : u64,
    pub px : GlobalPx,
}

/// One answer.
#[derive(Clone, Debug)]
pub struct TreeReply {
    pub id  : u64,
    /// `None` when the point is on no window, the window's application is not on the
    /// bus, or the bus was never there.
    pub hit : Option<Hit>,
    /// Round trip on the thread, milliseconds.
    pub ms  : f64,
}

/// Handle to the tree thread.
pub struct TreeService {
    requests : Sender<TreeRequest>,
    replies  : Receiver<TreeReply>,
    /// Replies that arrived while the collector was waiting for a different id.
    parked   : HashMap<u64, TreeReply>,
    /// Whether the thread is still answering; see [`Progress`].
    progress : Progress,
    /// How long the current thread may sit silent before it is replaced. Starts at
    /// [`STUCK_AFTER`], doubles per replacement up to [`STUCK_CEILING`].
    patience : Duration,
}

/// The watchdog's bookkeeping: questions asked, answers seen, and when the thread
/// last did anything.
#[derive(Clone, Copy, Debug)]
struct Progress {
    asked : u64,
    seen  : u64,
    /// The last answer, or the question that started the current outstanding run.
    at    : Instant,
}

// --- TreeService ---

impl TreeService {
    /// Starts the thread. Never fails: without a bus or a toplevel list the thread
    /// answers `None` to everything, and the collector runs on pixels alone, which is
    /// what it did before there was a tree.
    pub fn spawn() -> Self {
        let (requests, replies) = start_thread();

        Self {
            requests : requests,
            replies  : replies,
            parked   : HashMap::new(),
            progress : Progress { asked: 0, seen: 0, at: Instant::now() },
            patience : STUCK_AFTER,
        }
    }

    /// Asks what is at `px`. The answer is collected later with [`take`](Self::take).
    pub fn ask(&mut self, id: u64, px: GlobalPx) {
        // The progress clock starts at the first outstanding question, so a quiet
        // stretch with nothing asked never reads as a wedge.
        if self.progress.asked == self.progress.seen {
            self.progress.at = Instant::now();
        }

        self.progress.asked += 1;

        // A closed channel means the thread died, which `take` reports as no answer.
        let _ = self.requests.send(TreeRequest { id: id, px: px });
    }

    /// The reply for `id`, waiting up to `timeout` for it.
    ///
    /// A timeout here is normal — the answer may just be slow — but a thread that has
    /// answered nothing for [`STUCK_AFTER`] with questions outstanding is wedged on a
    /// blocked D-Bus call, and is replaced before returning.
    pub fn take(&mut self, id: u64, timeout: Duration) -> Option<TreeReply> {
        if let Some(reply) = self.parked.remove(&id) {
            return Some(reply);
        }

        let deadline = Instant::now() + timeout;

        loop {
            let Some(left) = deadline.checked_duration_since(Instant::now()) else {
                self.replace_if_stuck();

                return None;
            };

            match self.replies.recv_timeout(left) {
                Ok(reply) => {
                    // The thread's first answer proves it is not wedged; a later
                    // replacement gets the short leash back.
                    if self.progress.seen == 0 {
                        self.patience = STUCK_AFTER;
                    }

                    self.progress.seen += 1;
                    self.progress.at    = Instant::now();

                    if reply.id == id {
                        return Some(reply);
                    }

                    self.parked.insert(reply.id, reply);

                    // Keep the park from growing when a caller never collects.
                    if self.parked.len() > 16 {
                        let oldest = *self.parked.keys().min()?;

                        self.parked.remove(&oldest);
                    }
                }
                Err(_)    => {
                    self.replace_if_stuck();

                    return None;
                }
            }
        }
    }

    /// Replaces a wedged thread with a fresh one.
    fn replace_if_stuck(&mut self) {
        if !self.progress.stuck(self.patience) {
            return;
        }

        warn!(unanswered = self.progress.asked - self.progress.seen,
              patience = ?self.patience,
              "the tree thread is wedged on a blocked call; replacing it");

        let (requests, replies) = start_thread();

        self.requests = requests;
        self.replies  = replies;
        self.parked.clear();
        self.progress = Progress { asked: 0, seen: 0, at: Instant::now() };
        self.patience = (self.patience * 2).min(STUCK_CEILING);
    }
}

// --- Progress ---

impl Progress {
    /// Whether the thread counts as wedged: questions outstanding and nothing heard
    /// for `after`.
    fn stuck(&self, after: Duration) -> bool {
        self.asked > self.seen && self.at.elapsed() >= after
    }
}

/// Spawns one tree thread and returns its channels.
fn start_thread() -> (Sender<TreeRequest>, Receiver<TreeReply>) {
    let (req_tx, req_rx) = unbounded::<TreeRequest>();
    let (rep_tx, rep_rx) = unbounded::<TreeReply>();

    thread::Builder::new()
        .name("gaze-clicks-tree".into())
        .spawn(move || run(req_rx, rep_tx))
        .expect("spawning the tree thread");

    (req_tx, rep_rx)
}

/// The thread body.
fn run(requests: Receiver<TreeRequest>, replies: Sender<TreeReply>) {
    let mut windows = match ToplevelTracker::connect() {
        Ok(t)  => Some(t),
        Err(e) => {
            warn!(error = %e, "no toplevel list; the tree is unavailable");

            None
        }
    };

    let mut a11y = match A11y::connect() {
        Ok(a)  => Some(a),
        Err(e) => {
            warn!(error = %e, "no accessibility bus; the tree is unavailable");

            None
        }
    };

    if windows.is_some() && a11y.is_some() {
        info!("accessibility tree available");
    }

    for request in requests {
        let started = Instant::now();
        let hit     = answer(&mut windows, &mut a11y, request.px);
        let ms      = started.elapsed().as_secs_f64() * 1000.0;

        debug!(id = request.id, ms = ms, hit = hit.is_some(), "tree reply");

        if replies.send(TreeReply { id: request.id, hit: hit, ms: ms }).is_err() {
            break;
        }
    }
}

/// One query, with every failure turned into "no answer".
fn answer(windows: &mut Option<ToplevelTracker>, a11y: &mut Option<A11y>, px: GlobalPx)
    -> Option<Hit>
{
    let windows = windows.as_mut()?;
    let a11y    = a11y.as_mut()?;

    if let Err(e) = windows.pump() {
        warn!(error = %e, "toplevel list stopped");

        return None;
    }

    let window = windows.at(px)?;

    match a11y.at(px, &window) {
        Ok(hit) => hit,
        Err(e)  => {
            debug!(error = %e, app_id = %window.app_id, "tree query failed");

            None
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stuck_means_outstanding_questions_and_a_silent_thread() {
        let old = Instant::now() - Duration::from_secs(6);
        let now = Instant::now();

        // Outstanding and silent past the threshold: wedged.
        assert!(Progress { asked: 3, seen: 1, at: old }.stuck(STUCK_AFTER));

        // Nothing outstanding: a quiet stretch, however long, is not a wedge.
        assert!(!Progress { asked: 5, seen: 5, at: old }.stuck(STUCK_AFTER));

        // Outstanding but recent: the answer may just be slow.
        assert!(!Progress { asked: 1, seen: 0, at: now }.stuck(STUCK_AFTER));
    }
}
