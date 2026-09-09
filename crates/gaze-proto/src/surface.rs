//! The scroll surface under the gaze, kept current without blocking the gaze loop.
//!
//! `gaze_a11y` answers "what scrolls here" in ten to thirty milliseconds over D-Bus, and a
//! stopped application can hold that call for as long as it is stopped. Neither belongs on
//! the sample loop, so the question goes to `gaze_clicks::TreeService`'s thread, which has
//! the watchdog for the second problem, and the answer is polled for on later samples.
//!
//! The cache is keyed by where it asked: the answer stands while the gaze stays within
//! [`MOVE_REASK_PX`] of the point it was asked at and the viewport still contains it, and
//! is asked again once the gaze has moved further, because a viewport that contains the
//! point may hold a smaller one that also does (YouTube's mix list inside its page, whose
//! lower bands overlap; keyed by containment alone the page stayed the answer for two
//! seconds after the eyes reached the list, and the list's band scrolled the page). A
//! scroll in progress freezes it, because the content moves and the viewport does not.

use std::time::{Duration, Instant};

use gaze_a11y::Surface;
use gaze_clicks::TreeService;
use gaze_core::GlobalPx;
use tracing::debug;

use crate::edge_scroll::Scrollable;

/// Shortest interval between two questions while the gaze is on nothing the cache knows
/// about. A point over a non-scrolling area re-asks at this rate; each ask costs the tree
/// thread ten to thirty milliseconds.
pub const REASK_AFTER: Duration = Duration::from_millis(300);

/// A viewport that still contains the gaze is re-asked about after this long, so a moved
/// or resized window does not keep serving its old rectangle.
pub const REFRESH_AFTER: Duration = Duration::from_secs(2);

/// How far the gaze may move from the point the current answer was asked at before it
/// is asked again. A fixation's worth: reading along a line stays within it, a look to
/// another region does not.
pub const MOVE_REASK_PX: f64 = 48.0;

/// Shortest interval before a moved gaze is asked about. Well under the scroller's entry
/// dwell, so a band reached from elsewhere on the page is asked about before it can
/// start anything.
pub const MOVED_REASK_AFTER: Duration = Duration::from_millis(100);

/// While a scroll is running the viewport holds but the content moves under it, so the
/// room left each way is re-asked at this rate: it is what ends a scroll at the page's end.
pub const REFRESH_SCROLLING: Duration = Duration::from_millis(400);

/// The current surface and the question in flight.
pub struct SurfaceCache {
    tree     : TreeService,
    next_id  : u64,
    /// The outstanding question and where it was asked.
    pending  : Option<u64>,
    /// The last answer, which may have been "nothing scrolls here".
    current  : Option<Surface>,
    asked_at : Option<Instant>,
    /// Where the outstanding or last question was asked. The answer is only served near
    /// it.
    asked_for : Option<GlobalPx>,
    /// Questions asked over the session.
    pub asked    : u64,
    /// Answers that named a surface.
    pub found    : u64,
}

// --- SurfaceCache ---

impl SurfaceCache {
    /// Starts the tree thread. Never fails; without a bus every answer is "no surface".
    pub fn spawn() -> Self {
        Self {
            tree     : TreeService::spawn(),
            next_id  : 1,
            pending  : None,
            current  : None,
            asked_at : None,
            asked_for : None,
            asked    : 0,
            found    : 0,
        }
    }

    /// The scroll surface under `gaze` as far as the tree has said. `scrolling` keeps the
    /// current answer whether or not it contains the point, and refreshes it at
    /// [`REFRESH_SCROLLING`] so the room left tracks the moving content.
    ///
    /// Non-blocking: a fresh point, or one that has moved [`MOVE_REASK_PX`] from the last
    /// one asked about, gets a question sent and `None` back until the reply lands on a
    /// later call, which the scroller's entry dwell comfortably covers.
    pub fn scrollable(&mut self, gaze: Option<GlobalPx>, scrolling: bool) -> Option<Scrollable> {
        self.collect(scrolling);

        let age = self.asked_at.map_or(Duration::MAX, |at| at.elapsed());

        if scrolling {
            if let Some(gaze) = gaze
                && age >= REFRESH_SCROLLING
                && self.pending.is_none()
            {
                self.ask(gaze);
            }

            return self.current.as_ref().map(scrollable);
        }

        let gaze = gaze?;

        let contains = self.current.as_ref().is_some_and(|s| s.viewport.contains(gaze));
        let moved    = self.asked_for.is_none_or(|p| (p.x - gaze.x).hypot(p.y - gaze.y) > MOVE_REASK_PX);

        let due = match (moved, contains) {
            (true, _)      => age >= MOVED_REASK_AFTER,
            (false, true)  => age >= REFRESH_AFTER,
            (false, false) => age >= REASK_AFTER,
        };

        if due && self.pending.is_none() {
            self.ask(gaze);
        }

        // An answer is served only near the point it was asked at, and not while the
        // question for a new point is still out: the old answer may be an outer surface
        // of the one under the eyes now.
        let near = self.asked_for.is_some_and(|p| (p.x - gaze.x).hypot(p.y - gaze.y) <= MOVE_REASK_PX);

        (contains && near && self.pending.is_none())
            .then(|| self.current.as_ref().map(scrollable))
            .flatten()
    }

    /// Forgets when the current answer was fetched, so the next call asks again: after a
    /// scroll stops the content has moved and the room left is whatever it now is.
    pub fn invalidate(&mut self) {
        self.asked_at = None;
    }

    /// The surface behind the current viewport, for logging.
    pub fn current(&self) -> Option<&Surface> {
        self.current.as_ref()
    }

    fn ask(&mut self, gaze: GlobalPx) {
        let id = self.next_id;

        self.next_id  += 1;
        self.asked    += 1;
        self.pending   = Some(id);
        self.asked_at  = Some(Instant::now());
        self.asked_for = Some(gaze);

        self.tree.ask_surface(id, gaze);
    }

    /// Takes the reply to the outstanding question, if it has arrived.
    ///
    /// During a scroll only the room is wanted, so a reply about a different clipping
    /// node, or no node, is dropped: the hit under a moving page can land on a fixed
    /// header, a nested scrollable or a list row that has since been recycled, and
    /// swapping the surface on that would stop the scroll the user is in the middle of.
    fn collect(&mut self, scrolling: bool) {
        let Some(id) = self.pending else {
            return;
        };

        let Some(reply) = self.tree.poll(id) else {
            return;
        };

        self.pending = None;

        if scrolling
            && let Some(current) = &self.current
        {
            let same = reply.surface.as_ref().is_some_and(|s| {
                s.clip.bus == current.clip.bus && s.clip.path == current.clip.path
            });

            if !same {
                debug!(
                    role = reply.surface.as_ref().map(|s| s.clip.role.as_str()).unwrap_or("none"),
                    ms   = reply.ms,
                    "mid-scroll answer is not the surface being scrolled; kept the current one",
                );

                return;
            }
        }

        if let Some(surface) = &reply.surface {
            self.found += 1;

            debug!(
                role  = %surface.clip.role,
                x     = surface.viewport.x,
                y     = surface.viewport.y,
                w     = surface.viewport.w,
                h     = surface.viewport.h,
                above = surface.viewport.y - surface.content.y,
                below = (surface.content.y + surface.content.h) - (surface.viewport.y + surface.viewport.h),
                ms    = reply.ms,
                "scroll surface",
            );
        }
        else {
            debug!(ms = reply.ms, "no scroll surface under the gaze");
        }

        self.current = reply.surface;
    }
}

/// The scroller's view of a surface: the viewport and how far the content overflows it.
fn scrollable(s: &Surface) -> Scrollable {
    let above = (s.viewport.y - s.content.y).max(0.0);
    let below = ((s.content.y + s.content.h) - (s.viewport.y + s.viewport.h)).max(0.0);

    Scrollable { viewport: s.viewport, above_px: above, below_px: below }
}
