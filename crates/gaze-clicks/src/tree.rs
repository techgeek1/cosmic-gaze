//! The accessibility tree, asked on its own thread: once per press what is at the point
//! (the collector), and once per approach to a viewport edge what scrolls there (the
//! edge scroller in `gaze-proto`).
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
use gaze_a11y::{A11y, Answer, Hit, Miss, Surface};
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

/// What is being asked about a point.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Question {
    /// What is at the point: the collector's question, answered in `hit`.
    Hit,
    /// What scrolls at the point: the edge scroller's question, answered in `surface`.
    Surface,
}

/// One question.
#[derive(Clone, Copy, Debug)]
pub struct TreeRequest {
    pub id   : u64,
    pub px   : GlobalPx,
    pub kind : Question,
}

/// One answer. Whichever field the question did not ask for is `None`.
#[derive(Clone, Debug)]
pub struct TreeReply {
    pub id      : u64,
    /// `None` when the point is on no window, the window's application is not on the
    /// bus, or the bus was never there.
    pub hit     : Option<Hit>,
    /// The scroll surface under the point, for a [`Question::Surface`]; `None` for the
    /// same reasons as `hit`, and when nothing above the point overflows.
    pub surface : Option<Surface>,
    /// Why `hit` is `None` for a [`Question::Hit`] when the tree was asked and said
    /// so: the window was found and its application is off the bus, or answered
    /// nothing there. `None` when there was a hit, the point was on no window, or the
    /// tree was never there.
    pub miss    : Option<Miss>,
    /// Round trip on the thread, milliseconds.
    pub ms      : f64,
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
        self.send(TreeRequest { id: id, px: px, kind: Question::Hit });
    }

    /// Asks what scrolls at `px`. Collected the same way; the answer is in `surface`.
    pub fn ask_surface(&mut self, id: u64, px: GlobalPx) {
        self.send(TreeRequest { id: id, px: px, kind: Question::Surface });
    }

    fn send(&mut self, request: TreeRequest) {
        // The progress clock starts at the first outstanding question, so a quiet
        // stretch with nothing asked never reads as a wedge.
        if self.progress.asked == self.progress.seen {
            self.progress.at = Instant::now();
        }

        self.progress.asked += 1;

        // A closed channel means the thread died, which `take` reports as no answer.
        let _ = self.requests.send(request);
    }

    /// The reply for `id`, waiting up to `timeout` for it.
    ///
    /// A timeout here is normal — the answer may just be slow — but a thread that has
    /// answered nothing for [`STUCK_AFTER`] with questions outstanding is wedged on a
    /// blocked D-Bus call, and is replaced before returning.
    pub fn take(&mut self, id: u64, timeout: Duration) -> Option<TreeReply> {
        if let Some(reply) = self.poll(id) {
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
                    if let Some(reply) = self.accept(id, reply) {
                        return Some(reply);
                    }
                }
                Err(_)    => {
                    self.replace_if_stuck();

                    return None;
                }
            }
        }
    }

    /// The reply for `id` if it has already arrived, without waiting. Everything else
    /// that has arrived is parked for its own caller. For a loop that cannot block, such
    /// as the gaze loop asking after a scroll surface; a wedge is still noticed here.
    pub fn poll(&mut self, id: u64) -> Option<TreeReply> {
        if let Some(reply) = self.parked.remove(&id) {
            return Some(reply);
        }

        while let Ok(reply) = self.replies.try_recv() {
            if let Some(reply) = self.accept(id, reply) {
                return Some(reply);
            }
        }

        self.replace_if_stuck();

        None
    }

    /// Books one arrived reply: the watchdog sees progress, and the reply is returned if
    /// it is the one wanted or parked if not.
    fn accept(&mut self, wanted: u64, reply: TreeReply) -> Option<TreeReply> {
        // The thread's first answer proves it is not wedged; a later replacement gets
        // the short leash back.
        if self.progress.seen == 0 {
            self.patience = STUCK_AFTER;
        }

        self.progress.seen += 1;
        self.progress.at    = Instant::now();

        if reply.id == wanted {
            return Some(reply);
        }

        self.parked.insert(reply.id, reply);

        // Keep the park from growing when a caller never collects.
        if self.parked.len() > 16
            && let Some(oldest) = self.parked.keys().min().copied()
        {
            self.parked.remove(&oldest);
        }

        None
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
        let started              = Instant::now();
        let (hit, surface, miss) = answer(&mut windows, &mut a11y, request.px, request.kind);
        let ms                   = started.elapsed().as_secs_f64() * 1000.0;

        debug!(id = request.id, kind = ?request.kind, ms = ms,
               answered = hit.is_some() || surface.is_some(), miss = ?miss, "tree reply");

        let reply = TreeReply { id: request.id, hit: hit, surface: surface, miss: miss, ms: ms };

        if replies.send(reply).is_err() {
            break;
        }
    }
}

/// One query, with every failure turned into "no answer".
fn answer(
    windows : &mut Option<ToplevelTracker>,
    a11y    : &mut Option<A11y>,
    px      : GlobalPx,
    kind    : Question,
)
    -> (Option<Hit>, Option<Surface>, Option<Miss>)
{
    let (Some(windows), Some(a11y)) = (windows.as_mut(), a11y.as_mut()) else {
        return (None, None, None);
    };

    if let Err(e) = windows.pump() {
        warn!(error = %e, "toplevel list stopped");

        return (None, None, None);
    }

    let Some(window) = windows.at(px) else {
        return (None, None, None);
    };

    match kind {
        Question::Hit => match a11y.ask(px, &window) {
            Ok(Answer::Hit(hit))   => (Some(hit), None, None),
            Ok(Answer::Miss(miss)) => {
                debug!(?miss, app_id = %window.app_id, title = %window.title, "tree has no node at the point");

                (None, None, Some(miss))
            }
            Err(e)  => {
                debug!(error = %e, app_id = %window.app_id, "tree query failed");

                (None, None, None)
            }
        },

        Question::Surface => match a11y.scroll_surface(px, &window) {
            Ok(surface) => (None, surface, None),
            Err(e)      => {
                debug!(error = %e, app_id = %window.app_id, "surface query failed");

                (None, None, None)
            }
        },
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
