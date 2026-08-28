//! Which screen frame a click is recognised from, and which gaze frames go with it.
//!
//! # The screen
//!
//! A click is captured **on the press**, not from a rolling buffer. Everything that
//! destroys the target fires on the *release*: the menu item activates, the link
//! navigates, the popup dismisses. Between press and release the target is still there
//! and still under the pointer; a pressed-state highlight or a popup opening below the
//! pointer changes nothing the recogniser cares about. A rolling frame up to a second
//! old, by contrast, is exactly wrong for the common case of a menu item clicked
//! shortly after the menu opened, because it predates the menu.
//!
//! So the press fires a capture, and that frame is used when it lands within
//! [`PRESS_CAPTURE_S`]. The rolling cache is only the fallback for the case where the
//! compositor was slow, and a fallback frame older than [`STALE_FRAME_S`] is no frame
//! at all.
//!
//! # The gaze
//!
//! People look at what they click, before they click it. The window is asymmetric for
//! that reason: it opens well before the press and closes shortly after it.

use std::collections::HashMap;
use std::collections::VecDeque;

use gaze_capture::Frame;
use gaze_provider_et5::Et5Frame;
use gaze_provider_et5::sweep::TimedFrame;

/// How long the press-triggered capture is given to land, seconds. One ultrawide
/// capture measures about 35 ms, so this tolerates a compositor stall of four frames
/// before falling back.
pub const PRESS_CAPTURE_S: f64 = 0.150;

/// Oldest a fallback rolling frame may be, seconds. Past this the screen has had time
/// to become a different screen and the element list is a guess.
pub const STALE_FRAME_S: f64 = 0.500;

/// How far before the press the gaze window opens, seconds.
pub const GAZE_BEFORE_S: f64 = 1.200;

/// How far after the press it closes, seconds.
pub const GAZE_AFTER_S: f64 = 0.400;

/// The window the stop record claims as the fixation, relative to the press. Tighter
/// than the frame window, which carries context either side of it.
pub const STOP_BEFORE_S: f64 = 0.600;

/// How far past the press the stop window runs, seconds.
pub const STOP_AFTER_S: f64 = 0.100;

/// Fraction of the frames in `[t_press - STOP_BEFORE_S, t_press]` that must carry a
/// usable combined gaze before the click is worth writing. A blink or a head turn over
/// the approach leaves nothing to learn from.
pub const GAZE_MIN_FRACTION: f64 = 0.20;

/// Where the screen frame behind a click came from.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum FrameChoice {
    /// The capture the press itself fired, which completed `age_s` after the press.
    /// Always positive and normally 20 to 50 ms.
    Press { age_s : f64 },
    /// The newest rolling frame from before the press, `age_s` seconds old.
    /// Always negative.
    Rolling { age_s : f64 },
    /// Neither was usable: no press capture in time and no fresh enough fallback.
    Stale,
}

/// One captured frame with the host time its capture finished.
///
/// The time is the collector's own clock rather than `Frame::t_s`, which is the
/// compositor's presentation time on a different origin. Everything a click is timed
/// against (the button event, the gaze frames) is in the collector's clock.
#[derive(Clone, Debug)]
pub struct CachedFrame {
    pub frame    : Frame,
    /// When the capture returned, seconds since the collector started.
    pub done_t_s : f64,
}

/// The last few frames of each output, the fallback for a press capture that did not
/// land in time.
#[derive(Debug, Default)]
pub struct RollingCache {
    /// Newest last, per connector name.
    per_output : HashMap<String, VecDeque<CachedFrame>>,
    /// How many frames to keep per output.
    keep       : usize,
}

/// The last few seconds of device frames, so a click can look backward at the gaze
/// that led up to it.
#[derive(Debug)]
pub struct GazeRing {
    /// Oldest first, in host time order.
    frames : VecDeque<TimedFrame>,
    /// How much history to hold, seconds.
    span_s : f64,
}

/// Suppresses frames that two overlapping click windows both cover.
///
/// Clicks are processed in press order and every window ends at its own press, so a
/// frame that has already been written can only be older than everything still to
/// come. One watermark is the whole rule.
#[derive(Clone, Copy, Debug)]
pub struct FrameDedup {
    /// Host time of the newest frame written so far.
    last_t_s : f64,
}

// --- RollingCache ---

impl RollingCache {
    /// A cache keeping `keep` frames per output.
    pub fn new(keep: usize) -> RollingCache {
        RollingCache {
            per_output : HashMap::new(),
            keep       : keep.max(1),
        }
    }

    /// Files a frame under its output, evicting the oldest when the cache is full.
    pub fn push(&mut self, frame: Frame, done_t_s: f64) {
        let queue = self.per_output.entry(frame.output.clone()).or_default();

        queue.push_back(CachedFrame { frame: frame, done_t_s: done_t_s });

        while queue.len() > self.keep {
            queue.pop_front();
        }
    }

    /// The newest frame of `output` whose capture finished at or before `t_s`.
    ///
    /// Never returns a frame from after the press: a frame captured after the button
    /// went down may already show the pressed state of a menu that has closed.
    pub fn newest_before(&self, output: &str, t_s: f64) -> Option<&CachedFrame> {
        self.per_output
            .get(output)?
            .iter()
            .rev()
            .find(|cached| cached.done_t_s <= t_s)
    }

    /// Total frames held, over every output. For the memory line in the status log.
    pub fn len(&self) -> usize {
        self.per_output.values().map(VecDeque::len).sum()
    }

    /// True when nothing has been captured yet.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

// --- GazeRing ---

impl GazeRing {
    /// A ring holding `span_s` seconds of history.
    pub fn new(span_s: f64) -> GazeRing {
        GazeRing {
            frames : VecDeque::new(),
            span_s : span_s,
        }
    }

    /// Appends a frame and drops everything older than the span.
    pub fn push(&mut self, frame: TimedFrame) {
        let cutoff = frame.t_s - self.span_s;

        self.frames.push_back(frame);

        while self.frames.front().is_some_and(|f| f.t_s < cutoff) {
            self.frames.pop_front();
        }
    }

    /// Every frame in `[t0, t1]`, oldest first.
    pub fn window(&self, t0: f64, t1: f64) -> Vec<TimedFrame> {
        self.frames
            .iter()
            .filter(|f| f.t_s >= t0 && f.t_s <= t1)
            .copied()
            .collect()
    }

    /// How many frames the ring holds.
    pub fn len(&self) -> usize {
        self.frames.len()
    }

    /// True before the first frame arrives, and during a link gap long enough to
    /// empty the ring.
    pub fn is_empty(&self) -> bool {
        self.frames.is_empty()
    }
}

// --- FrameDedup ---

impl FrameDedup {
    /// A deduplicator that has written nothing.
    pub fn new() -> FrameDedup {
        FrameDedup { last_t_s: f64::NEG_INFINITY }
    }

    /// The frames of `window` that have not been written yet, and marks them written.
    pub fn take<'a>(&mut self, window: &'a [TimedFrame]) -> Vec<&'a TimedFrame> {
        let fresh: Vec<&TimedFrame> = window.iter()
            .filter(|f| f.t_s > self.last_t_s)
            .collect();

        if let Some(last) = fresh.last() {
            self.last_t_s = last.t_s;
        }

        fresh
    }
}

impl Default for FrameDedup {
    fn default() -> Self {
        FrameDedup::new()
    }
}

// --- Selection ---

/// Which frame a click should be recognised from.
///
/// `press_done_t_s` is when the capture fired by the press finished, absent when it
/// failed or has not landed. `rolling_t_s` is when the newest pre-press rolling frame
/// of the same output finished.
pub fn select_frame(
    press_done_t_s : Option<f64>,
    rolling_t_s    : Option<f64>,
    t_press        : f64,
    t_release      : f64,
)
    -> FrameChoice
{
    // The press capture is the wanted frame whenever it is timely. The release bound
    // matters for a click so short that the capture outlived it: past the release the
    // menu has already acted and the target may be gone.
    if let Some(done) = press_done_t_s
        && done <= t_press + PRESS_CAPTURE_S
        && done <= t_release
    {
        return FrameChoice::Press { age_s: done - t_press };
    }

    if let Some(rolling) = rolling_t_s
        && rolling <= t_press
        && t_press - rolling <= STALE_FRAME_S
    {
        return FrameChoice::Rolling { age_s: rolling - t_press };
    }

    FrameChoice::Stale
}

// --- Gaze validity ---

/// True when the firmware produced a usable combined gaze point for this frame.
///
/// The same test `dataset::firmware_ray` applies: the device reports (-1, -1) for an
/// invalid combined gaze and clamps to the declared area otherwise, so a value on or
/// outside the bounds is a clamp rather than a measurement, and at least one eye has
/// to have been tracked for the point to have an origin.
pub fn has_combined_gaze(frame: &Et5Frame) -> bool {
    let Some([nx, ny]) = frame.gaze_2d_norm else {
        return false;
    };

    (0.0..1.0).contains(&nx) && (0.0..1.0).contains(&ny) && frame.any_valid()
}

/// Fraction of the frames in `[t0, t1]` that carry a usable combined gaze.
///
/// Zero for an empty window, which is the answer that rejects the click: no frames is
/// not evidence that the user was looking anywhere.
pub fn gaze_fraction(frames: &[TimedFrame], t0: f64, t1: f64) -> f64 {
    let in_window: Vec<&TimedFrame> = frames.iter()
        .filter(|f| f.t_s >= t0 && f.t_s <= t1)
        .collect();

    if in_window.is_empty() {
        return 0.0;
    }

    let good = in_window.iter().filter(|f| has_combined_gaze(&f.frame)).count();

    good as f64 / in_window.len() as f64
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::Rect;

    use super::*;

    /// A one-pixel frame on `output`, which is all the selection rules read.
    fn frame(output: &str) -> Frame {
        Frame {
            output  : output.to_string(),
            logical : Rect { x: 0.0, y: 0.0, w: 1.0, h: 1.0 },
            width   : 1,
            height  : 1,
            rgba    : vec![0, 0, 0, 255],
            t_s     : 0.0,
        }
    }

    /// A device frame at `t_s` whose combined gaze is `uv`, or invalid for `None`.
    fn gaze(t_s: f64, uv: Option<[f64; 2]>) -> TimedFrame {
        TimedFrame {
            t_s   : t_s,
            frame : Et5Frame {
                validity_l   : Some(0),
                validity_r   : Some(0),
                gaze_2d_norm : uv,
                ..Et5Frame::default()
            },
        }
    }

    #[test]
    fn a_timely_press_capture_wins() {
        let choice = select_frame(Some(10.03), Some(9.80), 10.0, 10.09);

        assert_eq!(choice, FrameChoice::Press { age_s: 10.03 - 10.0 });
    }

    #[test]
    fn a_late_press_capture_falls_back_to_the_rolling_frame() {
        // 200 ms is past the deadline; the 300 ms old rolling frame is still fresh.
        let choice = select_frame(Some(10.20), Some(9.70), 10.0, 10.09);

        match choice {
            FrameChoice::Rolling { age_s } => assert!((age_s + 0.30).abs() < 1e-9),
            other                          => panic!("expected a fallback, got {other:?}"),
        }
    }

    #[test]
    fn a_press_capture_that_outlived_the_release_is_not_used() {
        // The click was 20 ms long and the capture took 40: by the time the pixels
        // exist the menu has already acted.
        let choice = select_frame(Some(10.04), Some(9.90), 10.0, 10.02);

        match choice {
            FrameChoice::Rolling { age_s } => assert!((age_s + 0.10).abs() < 1e-9),
            other                          => panic!("expected a fallback, got {other:?}"),
        }
    }

    #[test]
    fn a_stale_fallback_is_no_frame_at_all() {
        assert_eq!(select_frame(None, Some(9.40), 10.0, 10.09), FrameChoice::Stale);
        assert_eq!(select_frame(None, None      , 10.0, 10.09), FrameChoice::Stale);

        // And a rolling frame from after the press is never eligible, however fresh.
        assert_eq!(select_frame(None, Some(10.05), 10.0, 10.09), FrameChoice::Stale);
    }

    #[test]
    fn the_rolling_cache_never_hands_back_a_frame_from_after_the_press() {
        let mut cache = RollingCache::new(2);

        cache.push(frame("DP-1"), 9.0);
        cache.push(frame("DP-1"), 9.8);
        cache.push(frame("DP-1"), 10.4);

        // Only two are kept, and the newest is past the press.
        assert_eq!(cache.len(), 2);
        assert_eq!(cache.newest_before("DP-1", 10.0).map(|c| c.done_t_s), Some(9.8));

        // A different output has its own history and no cross-talk.
        assert!(cache.newest_before("DP-2", 10.0).is_none());
    }

    #[test]
    fn the_gaze_ring_drops_history_past_its_span() {
        let mut ring = GazeRing::new(1.0);

        // An eighth of a second is exact in binary, so the span boundary lands where
        // the arithmetic says it does rather than a float epsilon either side.
        for i in 0..32 {
            ring.push(gaze(f64::from(i) * 0.125, Some([0.5, 0.5])));
        }

        // The last frame is at 3.875 s, so 2.875 and later survive: nine frames.
        assert_eq!(ring.len(), 9);
        assert!(ring.window(0.0, 2.8).is_empty());
        assert_eq!(ring.window(2.875, 4.0).len(), 9);
    }

    #[test]
    fn the_gaze_window_needs_a_fifth_of_its_frames_valid() {
        // Ten frames over the 600 ms before the press, two of them tracked.
        let frames: Vec<TimedFrame> = (0..10)
            .map(|i| {
                let uv = if i < 2 { Some([0.4, 0.6]) } else { None };

                gaze(9.4 + i as f64 * 0.06, uv)
            })
            .collect();

        assert!((gaze_fraction(&frames, 9.4, 10.0) - 0.2).abs() < 1e-9);

        // A clamped point is a bound, not a measurement, and counts as invalid.
        let clamped = vec![gaze(9.5, Some([1.0, 0.5])), gaze(9.6, Some([0.5, 0.5]))];
        assert!((gaze_fraction(&clamped, 9.4, 10.0) - 0.5).abs() < 1e-9);

        // No frames at all is a rejection, not a free pass.
        assert_eq!(gaze_fraction(&[], 9.4, 10.0), 0.0);
    }

    #[test]
    fn overlapping_click_windows_write_each_frame_once() {
        let all: Vec<TimedFrame> = (0..30)
            .map(|i| gaze(i as f64 * 0.1, Some([0.5, 0.5])))
            .collect();

        let mut dedup = FrameDedup::new();

        // Two clicks 500 ms apart, so their 1.6 s windows overlap heavily.
        let first  = all.iter().filter(|f| f.t_s >= 0.8 && f.t_s <= 2.4).copied()
            .collect::<Vec<_>>();
        let second = all.iter().filter(|f| f.t_s >= 1.3 && f.t_s <= 2.9).copied()
            .collect::<Vec<_>>();

        // What the two windows cover between them, counted once.
        let union = all.iter()
            .filter(|f| first.iter().chain(second.iter()).any(|g| g.t_s == f.t_s))
            .count();

        let a = dedup.take(&first).len();
        let b = dedup.take(&second).len();

        assert_eq!(a, first.len());
        assert!(b < second.len(), "the overlap was skipped the second time");
        assert_eq!(a + b, union, "the overlap is written once, not twice");

        // Everything the second pass kept is strictly newer than the first pass's end.
        let again = dedup.take(&second);
        assert!(again.is_empty(), "a repeated window writes nothing new");
    }
}
