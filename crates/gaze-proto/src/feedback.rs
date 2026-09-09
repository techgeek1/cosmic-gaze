//! Real clicks as gaze labels for the running session (PLAN-ET5 D5's feed, the first
//! slice of E1).
//!
//! People look at what they click. `gaze-clicks` collects that as training data; this
//! feeds it straight back into the live provider instead, so the day's bias is learnt
//! while the session runs. The real mouse is read read-only, the pointer is polled from
//! the compositor, and every press is handed to the source with the pointer position at
//! that moment. The source decides what to make of it.
//!
//! Two kinds of press are never offered. Presses on `gaze-inject`'s own virtual pointer
//! are dropped at the reader ([`gaze_clicks::mouse::wanted`]), because those are the
//! gaze clicking. Presses within [`WARP_NEAR_PX`] of the last point this session warped
//! the pointer to are dropped here, because the user clicking where the gaze just put
//! the pointer says nothing the gaze did not already say.

use std::time::Instant;
use anyhow::{Context, Result};
use crossbeam_channel::Receiver;
use gaze_capture::CursorTracker;
use gaze_clicks::click::ButtonEvent;
use gaze_clicks::mouse::{DEFAULT_NAME, MouseReader};
use gaze_core::GlobalPx;
use tracing::{debug, info, warn};

/// A press this close to the last warp destination is the pointer the gaze placed,
/// not one the user aimed, and is not a label. The compositor's cursor-shape hotspot
/// and a warp's rounding both move the reported position by a few pixels.
pub const WARP_NEAR_PX: f64 = 12.0;

/// The real mouse and the pointer, and what they have yielded so far.
pub struct ClickFeed {
    mouse        : MouseReader,
    /// The reader fires a capture id per press for `gaze-clicks`'s screen capture;
    /// here there is nothing to capture, so the ids are drained and dropped.
    captures     : Receiver<u64>,
    cursor       : CursorTracker,
    /// The last pointer position the compositor reported, global logical pixels.
    pointer      : Option<GlobalPx>,
    /// Presses handed to the source.
    pub offered  : u64,
    /// Presses the source folded into its offset.
    pub accepted : u64,
    /// Presses the source attributed but refused.
    pub rejected : u64,
    /// Presses the source could not attribute at all.
    pub unplaced : u64,
    /// Presses dropped here before the source saw them: on the warp point, or with no
    /// pointer position to go with them.
    pub skipped  : u64,
}

/// One press ready to be offered.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Press {
    /// Host time on the source's clock, seconds.
    pub t_s : f64,
    /// Where the pointer was.
    pub px  : GlobalPx,
}

// --- ClickFeed ---

impl ClickFeed {
    /// Opens every real mouse and a cursor session on every output. `t0` is the
    /// source's clock, so presses land on the same timeline as its samples.
    pub fn open(t0: Instant) -> Result<ClickFeed> {
        let (tx, rx) = crossbeam_channel::unbounded();

        let mouse = MouseReader::open(None, DEFAULT_NAME, t0, tx)
            .context("opening the real mouse for click feedback")?;

        let cursor = CursorTracker::connect()
            .context("connecting the cursor tracker for click feedback")?;

        info!(
            nodes = ?mouse.nodes().iter().map(|(p, n)| format!("{} ({n})", p.display())).collect::<Vec<_>>(),
            "click feedback: reading the real mouse",
        );

        Ok(ClickFeed {
            mouse    : mouse,
            captures : rx,
            cursor   : cursor,
            pointer  : None,
            offered  : 0,
            accepted : 0,
            rejected : 0,
            unplaced : 0,
            skipped  : 0,
        })
    }

    /// Refreshes the pointer position and returns the presses since the last call,
    /// each with the pointer as of now. Called once per loop iteration, which is one
    /// sample period; the pointer is at rest at a press, so the staleness is harmless.
    ///
    /// `avoid` is the last warp destination, whose neighbourhood is not a label.
    pub fn presses(&mut self, avoid: Option<GlobalPx>) -> Vec<Press> {
        match self.cursor.position() {
            Ok(Some(p)) => self.pointer = Some(p),
            Ok(None)    => {}
            Err(e)      => debug!(error = %e, "cursor tracker read failed"),
        }

        for _ in self.captures.try_iter() {}

        let mut out = Vec::new();

        for event in self.mouse.events().try_iter().collect::<Vec<ButtonEvent>>() {
            if !event.pressed {
                continue;
            }

            let Some(px) = self.pointer else {
                self.skipped += 1;

                continue;
            };

            if avoid.is_some_and(|w| (w.x - px.x).hypot(w.y - px.y) < WARP_NEAR_PX) {
                self.skipped += 1;

                continue;
            }

            out.push(Press { t_s: event.t_s, px: px });
        }

        out
    }

    /// When the real mouse last moved, scrolled or clicked. `None` until it has.
    pub fn last_mouse_input(&self) -> Option<Instant> {
        self.mouse.last_input()
    }

    /// Stops the mouse reader. The cursor session closes with the drop.
    pub fn stop(&mut self) {
        self.mouse.stop();

        if self.offered > 0 || self.skipped > 0 {
            info!(
                offered  = self.offered,
                accepted = self.accepted,
                rejected = self.rejected,
                unplaced = self.unplaced,
                skipped  = self.skipped,
                "click feedback",
            );
        }
    }
}

impl Drop for ClickFeed {
    fn drop(&mut self) {
        self.mouse.stop();
    }
}

/// Opens the feed when something can use it (a source that learns from clicks, or a
/// controller that shares the pointer with the mouse), logging rather than failing when
/// the mouse or the cursor session is unavailable: the session is no worse off than
/// before the feed existed.
pub fn open_if_useful(t0: Option<Instant>) -> Option<ClickFeed> {
    let t0 = t0?;

    match ClickFeed::open(t0) {
        Ok(feed) => Some(feed),
        Err(e)   => {
            warn!("click feedback disabled: {e:#}");

            None
        }
    }
}
