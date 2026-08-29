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
}

// --- TreeService ---

impl TreeService {
    /// Starts the thread. Never fails: without a bus or a toplevel list the thread
    /// answers `None` to everything, and the collector runs on pixels alone, which is
    /// what it did before there was a tree.
    pub fn spawn() -> Self {
        let (req_tx, req_rx) = unbounded::<TreeRequest>();
        let (rep_tx, rep_rx) = unbounded::<TreeReply>();

        thread::Builder::new()
            .name("gaze-clicks-tree".into())
            .spawn(move || run(req_rx, rep_tx))
            .expect("spawning the tree thread");

        Self {
            requests : req_tx,
            replies  : rep_rx,
            parked   : HashMap::new(),
        }
    }

    /// Asks what is at `px`. The answer is collected later with [`take`](Self::take).
    pub fn ask(&self, id: u64, px: GlobalPx) {
        // A closed channel means the thread died, which `take` reports as no answer.
        let _ = self.requests.send(TreeRequest { id: id, px: px });
    }

    /// The reply for `id`, waiting up to `timeout` for it.
    pub fn take(&mut self, id: u64, timeout: Duration) -> Option<TreeReply> {
        if let Some(reply) = self.parked.remove(&id) {
            return Some(reply);
        }

        let deadline = Instant::now() + timeout;

        loop {
            let left = deadline.checked_duration_since(Instant::now())?;

            match self.replies.recv_timeout(left) {
                Ok(reply) if reply.id == id => return Some(reply),
                Ok(reply)                   => {
                    self.parked.insert(reply.id, reply);

                    // Keep the park from growing when a caller never collects.
                    if self.parked.len() > 16 {
                        let oldest = *self.parked.keys().min()?;

                        self.parked.remove(&oldest);
                    }
                }
                Err(_)                      => return None,
            }
        }
    }
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
