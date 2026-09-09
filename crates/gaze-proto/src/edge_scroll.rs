//! Continuous scrolling from where the eyes are in the viewport (DESIGN.md section 3,
//! principle 4: "continuous scroll on viewport-edge dwell").
//!
//! The loop it is built around: reading moves the eyes down the page; when the line being
//! read is in the lower band of the scroll surface, the surface scrolls, at a speed that
//! grows with how deep in the band the eyes are; the text rises, the line being read
//! leaves the band, and the scroll stops on its own. Nothing has to be looked away from
//! to stop it, and looking away anyway stops it too. Kumar and Winograd's gaze-enhanced
//! scrolling (UIST 2007) is the precedent for the self-regulating band.
//!
//! The surface is the thing the band is measured against, and it is never the window:
//! Discord's message list stops above the composer, a browser's sidebar scrolls apart from
//! the page. `gaze_a11y::A11y::scroll_surface` finds the real clipping node through the
//! accessibility tree; this module takes its viewport and does the rest. No surface, no
//! scroll, and the log says so.
//!
//! Three guards against the Midas touch of reading the last line and having it yanked
//! away: an entry dwell before anything moves, a speed ramp so the first moments of any
//! scroll are slow enough to abort with a glance up, and hysteresis on the band's inner
//! edge so the boundary does not chatter. The top band, where titles and toolbars live,
//! gets a longer dwell.
//!
//! The scroller only sees the eyes while the thumb is off the controller's pad and F14
//! has not latched the pointer look on: a thumb down is input, and input never scrolls
//! (the session hands it nothing in that case). The overlay shows the band the eyes are
//! in or approaching as a faint zone, from [`EdgeScroller::near_band`], so the user can
//! see where a scroll would start before it does.
//!
//! Everything here is pure: timestamps arrive on the samples, and the caller injects.

use gaze_core::{GlobalPx, Rect};

/// Share of the viewport's height that forms the lower band. 0.2 in the first live run
/// was "a little too big" on a 1300 px viewport; 0.1 was a touch slow to catch when
/// reading a PR by eye alone, then 0.115 "still a tad small", so 0.12.
pub const DEFAULT_BAND_FRACTION : f64 = 0.12;

/// Share of the viewport's height that forms the upper band. The top of a surface is
/// looked at for many reasons other than wanting to scroll up, which the longer dwell
/// covers.
pub const DEFAULT_TOP_BAND_FRACTION : f64 = 0.12;

/// Content overflow below which a direction counts as having nothing left to scroll,
/// logical pixels. At the end of a page the lower band is then just page, and the
/// elements in it can be looked at and clicked.
pub const ROOM_MIN_PX : f64 = 2.0;

/// How long the gaze must stay in the lower band before scrolling starts, seconds. Long
/// enough that a saccade landing in the band on its way somewhere else does nothing,
/// short enough not to feel like a wait.
pub const DEFAULT_DWELL_S : f64 = 0.25;

/// Entry dwell for the upper band, seconds.
pub const DEFAULT_TOP_DWELL_S : f64 = 0.5;

/// Speed at the very edge of the viewport, lines per second. One line is one wheel
/// click, 120 high-resolution units, which toolkits render as about three text lines.
pub const DEFAULT_MAX_LINES_S : f64 = 8.0;

/// Time for the speed to ramp from zero to what the depth asks for, seconds. 0.3 in the
/// first live runs, then 0.2, then 0.15: each step was "nav a tad slow" in a live run.
pub const DEFAULT_RAMP_S : f64 = 0.15;

/// Seconds the eyes must hold the outer part of the band before the speed starts to
/// grow. Reading pace leaves the band every few hundred milliseconds and never gets here;
/// a held gaze at the edge is "keep going", and the longer it is held the more it means.
pub const DEFAULT_HOLD_S : f64 = 0.3;

/// Speed multiplier gained per second of hold past [`DEFAULT_HOLD_S`]: the cap doubles
/// every second held.
pub const DEFAULT_HOLD_GAIN : f64 = 2.0;

/// Multiplier on the band speed while the eyes are tracked past the edge of the screen
/// itself. Past the bezel is a stronger "keep going" than the band, and looking above the
/// monitor for a couple of seconds is how "go to the top" is said.
pub const DEFAULT_TURBO : f64 = 3.0;

/// Depth into the band from which the hold clock runs. Below it the eyes are still
/// reading; above it they are parked at the edge.
pub const HOLD_DEPTH : f64 = 0.6;

/// Ceiling on the speed after every multiplier, lines per second, so a runaway is still
/// readable as it happens.
pub const MAX_LINES_S_CAP : f64 = 40.0;

/// Exponent on the band depth. One is linear; Kumar found a slightly quadratic curve,
/// slow near the inner edge and fast at the outer, preferred by some users.
pub const DEFAULT_EXPONENT : f64 = 1.0;

/// Hysteresis on the band's inner edge, as a share of the band's height: once scrolling,
/// the gaze must rise this far above the edge for it to stop.
pub const EXIT_SLACK_FRACTION : f64 = 0.15;

/// A viewport shorter than this is not scrolled: its bands would be a few pixels tall.
/// `gaze_a11y::clip_surface` already walks past clips this short, so this is the belt.
pub const MIN_VIEWPORT_PX : f64 = gaze_a11y::MIN_CLIP_PX;

/// Longest gap without a gaze point (a blink, a dropout) a scroll coasts through before
/// it stops, seconds.
pub const MAX_GAP_S : f64 = 0.25;

/// Longest interval one sample may integrate over, seconds. A stall in the provider must
/// not turn into a lurch when it resumes.
pub const MAX_STEP_S : f64 = 0.1;

/// High-resolution wheel units per line, the kernel's `REL_WHEEL_HI_RES` convention.
pub const UNITS_PER_LINE : f64 = 120.0;

/// How far inside the viewport past a band's inner edge, as a share of the band's height,
/// the eyes count as approaching it: the overlay shows the zone from here so it is up by
/// the time the eyes reach the band, and keeps it up to 1.5x this on the way out.
pub const APPROACH_FRACTION : f64 = 0.75;

/// The scroller's tunables.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct EdgeParams {
    pub band_fraction     : f64,
    pub top_band_fraction : f64,
    pub dwell_s           : f64,
    pub top_dwell_s       : f64,
    pub max_lines_s       : f64,
    pub ramp_s            : f64,
    pub exponent          : f64,
    pub hold_s            : f64,
    pub hold_gain         : f64,
    pub turbo             : f64,
}

impl Default for EdgeParams {
    fn default() -> Self {
        Self {
            band_fraction     : DEFAULT_BAND_FRACTION,
            top_band_fraction : DEFAULT_TOP_BAND_FRACTION,
            dwell_s           : DEFAULT_DWELL_S,
            top_dwell_s       : DEFAULT_TOP_DWELL_S,
            max_lines_s       : DEFAULT_MAX_LINES_S,
            ramp_s            : DEFAULT_RAMP_S,
            exponent          : DEFAULT_EXPONENT,
            hold_s            : DEFAULT_HOLD_S,
            hold_gain         : DEFAULT_HOLD_GAIN,
            turbo             : DEFAULT_TURBO,
        }
    }
}

/// A scroll surface as the scroller sees it: where it is, and how much is left each way.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Scrollable {
    /// The clipping region, global logical pixels.
    pub viewport : Rect,
    /// Content above the viewport's top edge, pixels; zero at the top of the page.
    pub above_px : f64,
    /// Content below the viewport's bottom edge, pixels; zero at the end of the page.
    pub below_px : f64,
}

/// Where the eyes are, as far as the scroller cares.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Eyes {
    /// The filtered gaze point, on a panel.
    On(GlobalPx),
    /// Tracked, but on no panel: the ray projected onto the surface's panel extended past
    /// its edges, so `y` past the viewport's bottom means the eyes went off the bottom.
    /// Sustains a scroll in that direction at full depth; never starts one, and the eyes
    /// off the *other* edge is a look-away.
    Off(GlobalPx),
    /// Not tracked: a blink, a dropout.
    Lost,
}

/// Which way the content moves.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Direction {
    /// Reading on: the lower band, content moves up, wheel units are negative.
    Down,
    /// Going back: the upper band, content moves down, wheel units are positive.
    Up,
}

/// What the caller should do after one update.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Action {
    Nothing,
    /// The dwell completed: put the pointer on `point`, inside the surface, so the
    /// wheel units that follow land on it.
    Start { dir: Direction, point: GlobalPx },
    /// Whole high-resolution wheel units accrued this tick, signed per [`Direction`].
    Scroll { units: i32 },
    /// The gaze left the band, the surface, or the screen.
    Stop,
}

/// Where the gaze is relative to the viewport's bands.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Zone {
    /// Inside a band, `depth` in `(0, 1]` from its inner edge to the viewport's edge.
    Band { dir: Direction, depth: f64 },
    /// In the viewport but in neither band, or outside the viewport altogether.
    Off,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum State {
    Idle,
    /// In a band, waiting out the dwell.
    Armed { dir: Direction, since_s: f64 },
    /// Moving. `depth` is the last one seen, held through a gap.
    Scrolling { dir: Direction, since_s: f64, depth: f64 },
}

/// The dwell, band and speed state machine.
#[derive(Debug)]
pub struct EdgeScroller {
    params     : EdgeParams,
    state      : State,
    /// Fractional wheel units not yet handed out.
    accum      : f64,
    /// When the eyes last entered the outer part of the band (or went past the edge)
    /// without leaving it since; the hold multiplier grows from here.
    hold_since : Option<f64>,
    /// Whether the last sample had the eyes past the screen's edge: the turbo.
    off_screen : bool,
    last_t_s   : Option<f64>,
    /// When a gaze point was last seen, for the gap rule.
    last_seen_s : Option<f64>,
    /// Scrolls started over the session.
    pub starts : u64,
    /// Whole units handed out over the session, by magnitude.
    pub units  : u64,
}

// --- EdgeScroller ---

impl EdgeScroller {
    pub fn new(params: EdgeParams) -> Self {
        Self {
            params      : params,
            state       : State::Idle,
            accum       : 0.0,
            hold_since  : None,
            off_screen  : false,
            last_t_s    : None,
            last_seen_s : None,
            starts      : 0,
            units       : 0,
        }
    }

    /// Whether a scroll is in progress.
    pub fn scrolling(&self) -> bool {
        matches!(self.state, State::Scrolling { .. })
    }

    /// The parameters in force.
    pub fn params(&self) -> EdgeParams {
        self.params
    }

    /// One sample. `eyes` is where the filtered gaze is and `surface` the scroll surface
    /// under it, as the tree last reported it; a surface that vanishes mid-scroll is a
    /// stop, and so is running out of room.
    pub fn update(&mut self, t_s: f64, eyes: Eyes, surface: Option<&Scrollable>) -> Action {
        let dt = self.last_t_s.map_or(0.0, |last| (t_s - last).clamp(0.0, MAX_STEP_S));

        self.last_t_s = Some(t_s);

        let Some(surface) = surface.filter(|s| s.viewport.h >= MIN_VIEWPORT_PX) else {
            return self.stop();
        };

        let viewport = &surface.viewport;

        // Out of room in the direction being scrolled: done, whatever the eyes do.
        if let State::Scrolling { dir, .. } = self.state
            && !surface.room(dir)
        {
            return self.stop();
        }

        self.off_screen = false;

        let (gaze, zone) = match eyes {
            Eyes::On(g) => {
                self.last_seen_s = Some(t_s);

                (Some(g), Some(self.zone(g, surface)))
            }

            // Tracked but off the panel: only a scroll already running reads it, and only
            // past the edge it is scrolling towards. Anywhere else it is a look-away.
            Eyes::Off(g) => {
                self.last_seen_s = Some(t_s);

                let zone = match self.state {
                    State::Scrolling { dir, .. } if surface.beyond(g, dir) => {
                        self.off_screen = true;

                        Zone::Band { dir: dir, depth: 1.0 }
                    }
                    _ => Zone::Off,
                };

                (Some(g), Some(zone))
            }

            Eyes::Lost => (None, None),
        };

        match (self.state, zone) {
            // Nothing to see: a scroll coasts on its last depth through a short gap.
            (State::Scrolling { dir, since_s, depth }, None) => {
                match self.last_seen_s.is_some_and(|seen| t_s - seen <= MAX_GAP_S) {
                    true  => self.step(t_s, dt, dir, since_s, depth),
                    false => self.stop(),
                }
            }

            (_, None) => {
                self.state = State::Idle;

                Action::Nothing
            }

            (State::Idle, Some(Zone::Band { dir, .. })) => {
                self.state = State::Armed { dir: dir, since_s: t_s };

                Action::Nothing
            }

            (State::Idle, Some(Zone::Off)) => Action::Nothing,

            (State::Armed { dir, since_s }, Some(Zone::Band { dir: now, depth })) if now == dir => {
                let dwell = match dir {
                    Direction::Down => self.params.dwell_s,
                    Direction::Up   => self.params.top_dwell_s,
                };

                if t_s - since_s < dwell {
                    return Action::Nothing;
                }

                self.state  = State::Scrolling { dir: dir, since_s: t_s, depth: depth };
                self.accum  = 0.0;
                self.starts += 1;

                // The gaze point is inside the band, so it is inside the surface.
                Action::Start { dir: dir, point: gaze.expect("a zone came from a point") }
            }

            (State::Armed { .. }, Some(_)) => {
                self.state = State::Idle;

                Action::Nothing
            }

            (State::Scrolling { dir, since_s, .. }, Some(Zone::Band { dir: now, depth })) if now == dir => {
                self.state = State::Scrolling { dir: dir, since_s: since_s, depth: depth };

                // The hold clock runs while the eyes stay parked at the edge and resets
                // the moment they come back in to read.
                match depth >= HOLD_DEPTH {
                    true  => { self.hold_since.get_or_insert(t_s); }
                    false => self.hold_since = None,
                }

                self.step(t_s, dt, dir, since_s, depth)
            }

            (State::Scrolling { dir, since_s, depth: held }, Some(Zone::Off)) => {
                // Hysteresis: just above the inner edge still counts as in the band, at
                // the depth last seen, so the boundary does not chatter.
                match gaze.is_some_and(|g| self.within_slack(g, viewport, dir)) {
                    true  => self.step(t_s, dt, dir, since_s, held),
                    false => self.stop(),
                }
            }

            (State::Scrolling { .. }, Some(Zone::Band { .. })) => self.stop(),
        }
    }

    /// The direction of the scroll in progress, if one is.
    pub fn direction(&self) -> Option<Direction> {
        match self.state {
            State::Scrolling { dir, .. } => Some(dir),
            _                            => None,
        }
    }

    /// The band for `dir` on a surface, global logical pixels. `None` when the band has
    /// no height or its direction has no room left: at the end of the page the lower
    /// band is page, with things to click in it.
    pub fn band(&self, s: &Scrollable, dir: Direction) -> Option<Rect> {
        let v        = &s.viewport;
        let fraction = match dir {
            Direction::Down => self.params.band_fraction,
            Direction::Up   => self.params.top_band_fraction,
        };
        let h = v.h * fraction;

        if h <= 0.0 || !s.room(dir) {
            return None;
        }

        let y = match dir {
            Direction::Down => v.y + v.h - h,
            Direction::Up   => v.y,
        };

        Some(Rect { x: v.x, y: y, w: v.w, h: h })
    }

    /// The band the eyes are in or approaching: within the surface's width, and no
    /// further inside the viewport than `slack` band heights past the band's inner edge.
    /// A scroll in progress names its own band whatever the eyes do, since they may be
    /// past the screen's edge sustaining it. This is what the overlay draws.
    pub fn near_band(&self, g: Option<GlobalPx>, s: &Scrollable, slack: f64) -> Option<(Direction, Rect)> {
        if let Some(dir) = self.direction() {
            return self.band(s, dir).map(|r| (dir, r));
        }

        let g = g?;
        let v = &s.viewport;

        if g.x < v.x || g.x > v.x + v.w {
            return None;
        }

        for dir in [Direction::Down, Direction::Up] {
            let Some(r) = self.band(s, dir) else {
                continue;
            };

            let (lo, hi) = match dir {
                Direction::Down => (r.y - r.h * slack, v.y + v.h),
                Direction::Up   => (v.y, r.y + r.h + r.h * slack),
            };

            if g.y >= lo && g.y <= hi {
                return Some((dir, r));
            }
        }

        None
    }

    /// The band the point is in, if any (see [`EdgeScroller::band`]).
    fn zone(&self, g: GlobalPx, s: &Scrollable) -> Zone {
        let v = &s.viewport;

        if g.x < v.x || g.x > v.x + v.w || g.y < v.y || g.y > v.y + v.h {
            return Zone::Off;
        }

        if let Some(r) = self.band(s, Direction::Down)
            && g.y > r.y
        {
            return Zone::Band { dir: Direction::Down, depth: ((g.y - r.y) / r.h).min(1.0) };
        }

        if let Some(r) = self.band(s, Direction::Up)
            && g.y < r.y + r.h
        {
            return Zone::Band { dir: Direction::Up, depth: ((r.y + r.h - g.y) / r.h).min(1.0) };
        }

        Zone::Off
    }

    /// Whether a point just outside `dir`'s band is within the exit hysteresis.
    fn within_slack(&self, g: GlobalPx, v: &Rect, dir: Direction) -> bool {
        if g.x < v.x || g.x > v.x + v.w {
            return false;
        }

        match dir {
            Direction::Down => {
                let band_h = v.h * self.params.band_fraction;
                let edge   = v.y + v.h - band_h;

                g.y > edge - band_h * EXIT_SLACK_FRACTION && g.y <= v.y + v.h
            }

            Direction::Up => {
                let band_h = v.h * self.params.top_band_fraction;
                let edge   = v.y + band_h;

                g.y < edge + band_h * EXIT_SLACK_FRACTION && g.y >= v.y
            }
        }
    }

    /// The speed in lines per second the eyes are asking for right now, before the ramp.
    fn lines_s(&self, t_s: f64, depth: f64) -> f64 {
        let base = self.params.max_lines_s * depth.clamp(0.0, 1.0).powf(self.params.exponent);
        let held = self.hold_since.map_or(0.0, |since| (t_s - since - self.params.hold_s).max(0.0));
        let hold = self.params.hold_gain.max(1.0).powf(held);
        let turbo = match self.off_screen {
            true  => self.params.turbo.max(1.0),
            false => 1.0,
        };

        (base * hold * turbo).min(MAX_LINES_S_CAP)
    }

    /// Integrates one tick of scrolling and hands out the whole units.
    fn step(&mut self, t_s: f64, dt: f64, dir: Direction, since_s: f64, depth: f64) -> Action {
        let ramp  = match self.params.ramp_s > 0.0 {
            true  => ((t_s - since_s) / self.params.ramp_s).clamp(0.0, 1.0),
            false => 1.0,
        };
        let lines_s = self.lines_s(t_s, depth) * ramp;
        let sign    = match dir {
            Direction::Down => -1.0,
            Direction::Up   =>  1.0,
        };

        self.accum += sign * lines_s * dt * UNITS_PER_LINE;

        let whole = self.accum.trunc();

        self.accum -= whole;

        match whole as i32 {
            0     => Action::Nothing,
            units => {
                self.units += units.unsigned_abs() as u64;

                Action::Scroll { units: units }
            }
        }
    }

    /// Back to idle, reporting a stop if something was moving.
    fn stop(&mut self) -> Action {
        let was = self.scrolling();

        self.state      = State::Idle;
        self.accum      = 0.0;
        self.hold_since = None;
        self.off_screen = false;

        match was {
            true  => Action::Stop,
            false => Action::Nothing,
        }
    }
}

// --- Scrollable ---

impl Scrollable {
    /// Whether there is anything left to scroll in `dir`.
    pub fn room(&self, dir: Direction) -> bool {
        match dir {
            Direction::Down => self.below_px > ROOM_MIN_PX,
            Direction::Up   => self.above_px > ROOM_MIN_PX,
        }
    }

    /// Whether `g` is past the viewport's edge in `dir`'s direction, within its width.
    pub fn beyond(&self, g: GlobalPx, dir: Direction) -> bool {
        let v = &self.viewport;

        if g.x < v.x || g.x > v.x + v.w {
            return false;
        }

        match dir {
            Direction::Down => g.y > v.y + v.h,
            Direction::Up   => g.y < v.y,
        }
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Discord's message list as the tree reported it (2026-09-04): the viewport, with
    /// the 4305 px list mostly above it and a little below.
    fn discord() -> Scrollable {
        Scrollable {
            viewport : Rect { x: 381.0, y: 248.0, w: 898.0, h: 1296.0 },
            above_px : 3009.0,
            below_px : 400.0,
        }
    }

    fn at(x: f64, y: f64) -> Eyes {
        Eyes::On(GlobalPx { x: x, y: y })
    }

    fn off(x: f64, y: f64) -> Eyes {
        Eyes::Off(GlobalPx { x: x, y: y })
    }

    /// Runs the scroller at 100 Hz from `t0` for `seconds` with the gaze fixed, summing
    /// the units and returning the actions that were not plain scrolls.
    fn run(e: &mut EdgeScroller, t0: f64, seconds: f64, gaze: Eyes, v: &Scrollable)
        -> (i64, Vec<Action>)
    {
        let mut total  = 0i64;
        let mut events = Vec::new();
        let steps      = (seconds * 100.0).round() as usize;

        for i in 0..=steps {
            match e.update(t0 + i as f64 * 0.01, gaze, Some(v)) {
                Action::Scroll { units } => total += i64::from(units),
                Action::Nothing          => {}
                other                    => events.push(other),
            }
        }

        (total, events)
    }

    #[test]
    fn a_dwell_in_the_lower_band_starts_a_downward_scroll_that_reaches_full_speed() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());

        // Half way into the band, below the hold depth, well above the composer.
        let band = v.h * DEFAULT_BAND_FRACTION;
        let gaze = at(800.0, v.y + v.h - band * 0.5);

        // The dwell: nothing moves for the first quarter second.
        let (units, events) = run(&mut e, 0.0, 0.24, gaze, &s);

        assert_eq!(units, 0);
        assert!(events.is_empty(), "{events:?}");

        // Then it starts, ramps, and settles at depth * max lines per second.
        let (units, events) = run(&mut e, 0.25, 2.0, gaze, &s);

        let Eyes::On(point) = gaze else { unreachable!() };

        assert_eq!(events, vec![Action::Start { dir: Direction::Down, point: point }]);
        assert!(units < 0, "reading down scrolls with negative units, got {units}");

        // 2 s at 0.5 * 8 lines/s * 120, less half the ramp.
        let expected = -(0.5 * 8.0 * 120.0 * (2.0 - DEFAULT_RAMP_S / 2.0));

        assert!((units as f64 - expected).abs() < 0.05 * expected.abs(),
                "units {units}, expected about {expected:.0}");
        assert_eq!(e.starts, 1);
    }

    #[test]
    fn looking_back_up_the_page_stops_it() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, v.y + v.h - 10.0);

        run(&mut e, 0.0, 1.0, deep, &s);
        assert!(e.scrolling());

        // Just above the inner edge: hysteresis keeps it going.
        let inner = v.y + v.h * (1.0 - DEFAULT_BAND_FRACTION);
        let close = at(800.0, inner - 5.0);

        assert_ne!(e.update(1.01, close, Some(&s)), Action::Stop);
        assert!(e.scrolling());

        // Well above it: stop, once, then quiet.
        let far = at(800.0, v.y + v.h * 0.5);

        assert_eq!(e.update(1.02, far, Some(&s)), Action::Stop);
        assert_eq!(e.update(1.03, far, Some(&s)), Action::Nothing);
        assert!(!e.scrolling());
    }

    #[test]
    fn a_glance_through_the_band_does_not_scroll() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, v.y + v.h - 10.0);
        let mid   = at(800.0, v.y + v.h * 0.5);

        // 100 ms in the band on the way somewhere else.
        let (units, events) = run(&mut e, 0.0, 0.1, deep, &s);

        assert_eq!((units, events.len()), (0, 0));
        assert_eq!(e.update(0.11, mid, Some(&s)), Action::Nothing);

        // The dwell restarts from scratch on the next visit.
        let (units, events) = run(&mut e, 0.2, 0.2, deep, &s);

        assert_eq!((units, events.len()), (0, 0));
    }

    #[test]
    fn the_top_band_waits_longer_and_scrolls_up() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let top   = at(800.0, v.y + 5.0);

        let (units, events) = run(&mut e, 0.0, 0.45, top, &s);

        assert_eq!((units, events.len()), (0, 0), "the lower band's dwell must not apply");

        let (units, events) = run(&mut e, 0.5, 1.0, top, &s);

        let Eyes::On(point) = top else { unreachable!() };

        assert_eq!(events, vec![Action::Start { dir: Direction::Up, point: point }]);
        assert!(units > 0, "scrolling up is positive units, got {units}");
    }

    #[test]
    fn a_blink_coasts_and_a_dropout_stops() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, v.y + v.h - 10.0);

        run(&mut e, 0.0, 1.0, deep, &s);

        // 150 ms without a point: still scrolling on the held depth.
        let (units, events) = run(&mut e, 1.01, 0.15, Eyes::Lost, &s);

        assert!(units < 0, "coasting should still scroll, got {units}");
        assert!(events.is_empty());

        // Past the gap: stop.
        let (_, events) = run(&mut e, 1.17, 0.2, Eyes::Lost, &s);

        assert_eq!(events, vec![Action::Stop]);
    }

    #[test]
    fn no_surface_or_a_tiny_one_never_scrolls() {
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, 1500.0);

        for i in 0..100 {
            assert_eq!(e.update(i as f64 * 0.01, deep, None), Action::Nothing);
        }

        let tiny = Scrollable {
            viewport : Rect { x: 700.0, y: 1400.0, w: 200.0, h: 100.0 },
            above_px : 500.0,
            below_px : 500.0,
        };

        for i in 0..100 {
            assert_eq!(e.update(1.0 + i as f64 * 0.01, deep, Some(&tiny)), Action::Nothing);
        }
    }

    #[test]
    fn a_surface_change_mid_scroll_stops_it() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, v.y + v.h - 10.0);

        run(&mut e, 0.0, 1.0, deep, &s);

        // The tree now says the point is in the composer, which does not overflow.
        assert_eq!(e.update(1.01, deep, None), Action::Stop);
    }

    #[test]
    fn a_stalled_provider_does_not_lurch() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, v.y + v.h - 10.0);

        run(&mut e, 0.0, 1.0, deep, &s);

        // Two seconds of silence, then one sample: at most MAX_STEP_S worth of units,
        // even at the capped speed the hold has by now reached.
        let action = e.update(3.0, deep, Some(&s));
        let cap    = (MAX_LINES_S_CAP * UNITS_PER_LINE * MAX_STEP_S).ceil() as i32;

        match action {
            Action::Scroll { units } => assert!(units.abs() <= cap, "{units} > {cap}"),
            Action::Nothing          => {}
            other                    => panic!("{other:?}"),
        }
    }

    #[test]
    fn at_the_end_of_the_page_the_lower_band_is_just_page() {
        let mut s = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, v.y + v.h - 10.0);

        s.below_px = 0.0;

        // Nothing arms, so the snap engine keeps the band's elements.
        let (units, events) = run(&mut e, 0.0, 1.0, deep, &s);

        assert_eq!((units, events.len()), (0, 0));
        assert!(!e.scrolling());

        // Whereas the top still has room.
        let top = at(800.0, v.y + 5.0);
        let (_, events) = run(&mut e, 1.0, 1.0, top, &s);

        assert_eq!(events.len(), 1, "{events:?}");
    }

    #[test]
    fn running_out_of_room_mid_scroll_stops_it() {
        let mut s = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, v.y + v.h - 10.0);

        run(&mut e, 0.0, 1.0, deep, &s);
        assert!(e.scrolling());

        // The tree's refreshed answer: the list now ends at the viewport's bottom.
        s.below_px = 0.0;

        assert_eq!(e.update(1.01, deep, Some(&s)), Action::Stop);
        assert_eq!(e.update(1.02, deep, Some(&s)), Action::Nothing);
    }

    #[test]
    fn eyes_past_the_bottom_of_the_screen_keep_a_downward_scroll_at_full_speed() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let deep  = at(800.0, v.y + v.h - 10.0);

        run(&mut e, 0.0, 1.0, deep, &s);
        assert!(e.scrolling());

        // Tracked, 200 px below the viewport's bottom, for two seconds: still going, at
        // least the turbo multiple of the edge speed and never past the cap.
        let (units, events) = run(&mut e, 1.01, 2.0, off(800.0, v.y + v.h + 200.0), &s);

        assert!(events.is_empty(), "{events:?}");

        let floor = DEFAULT_TURBO * DEFAULT_MAX_LINES_S * UNITS_PER_LINE * 2.0;
        let cap   = MAX_LINES_S_CAP * UNITS_PER_LINE * 2.0 * 1.01;

        assert!((units as f64) <= -floor, "units {units}, turbo floor {floor:.0}");
        assert!((units as f64) >= -cap, "units {units}, cap {cap:.0}");

        // Off the top instead: that is a look-away.
        assert_eq!(e.update(3.02, off(800.0, v.y - 200.0), Some(&s)), Action::Stop);
    }

    #[test]
    fn eyes_off_the_screen_never_start_a_scroll() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());

        let (units, events) = run(&mut e, 0.0, 2.0, off(800.0, v.y + v.h + 200.0), &s);

        assert_eq!((units, events.len()), (0, 0));
        assert!(!e.scrolling());
    }

    /// Units per second over a one-second window at a fixed gaze, magnitude.
    fn rate(e: &mut EdgeScroller, t0: f64, gaze: Eyes, s: &Scrollable) -> f64 {
        let (units, _) = run(e, t0, 1.0, gaze, s);

        units.abs() as f64
    }

    #[test]
    fn holding_the_edge_accelerates_and_reading_back_in_resets_it() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams::default());
        let edge  = at(800.0, v.y + v.h - 5.0);

        run(&mut e, 0.0, 1.0, edge, &s);

        // Second one after the start: the hold second has just elapsed, so this window
        // is close to the base edge speed.
        let first = rate(&mut e, 1.01, edge, &s);

        // Two seconds later the cap has doubled twice.
        run(&mut e, 2.02, 1.0, edge, &s);

        let later = rate(&mut e, 3.03, edge, &s);

        assert!(later > 2.5 * first, "held: {first:.0} then {later:.0} units/s");
        assert!(later <= MAX_LINES_S_CAP * UNITS_PER_LINE * 1.01, "{later:.0} over the cap");

        // Back in to the inner half of the band, still scrolling, then out to the edge
        // again: the hold clock restarted, so the speed is back near base.
        let inner = at(800.0, v.y + v.h * (1.0 - DEFAULT_BAND_FRACTION * 0.7));

        run(&mut e, 4.04, 0.3, inner, &s);
        assert!(e.scrolling());

        let reset = rate(&mut e, 4.35, edge, &s);

        assert!(reset < 1.5 * first, "reset: {reset:.0} vs first {first:.0} units/s");
    }

    #[test]
    fn the_speed_never_passes_the_cap() {
        let s     = discord();
        let v     = s.viewport;
        let mut e = EdgeScroller::new(EdgeParams { hold_gain: 10.0, turbo: 10.0, ..EdgeParams::default() });

        run(&mut e, 0.0, 1.0, at(800.0, v.y + v.h - 5.0), &s);
        run(&mut e, 1.01, 3.0, off(800.0, v.y + v.h + 300.0), &s);

        let capped = rate(&mut e, 4.02, off(800.0, v.y + v.h + 300.0), &s);

        assert!((capped - MAX_LINES_S_CAP * UNITS_PER_LINE).abs() < 0.02 * MAX_LINES_S_CAP * UNITS_PER_LINE,
                "{capped:.0} units/s vs cap");
    }

    /// The zone the overlay draws: the lower band from three quarters of its height above
    /// its inner edge, the upper band likewise, nothing in the middle or off the surface,
    /// and no band in a direction with no room.
    #[test]
    fn near_band_covers_the_approach_to_each_band() {
        let s = EdgeScroller::new(EdgeParams::default());
        let d = discord();
        let v = d.viewport;

        let band_h    = v.h * DEFAULT_BAND_FRACTION;
        let lower_top = v.y + v.h - band_h;

        let at = |y: f64| s.near_band(Some(GlobalPx { x: 500.0, y: y }), &d, APPROACH_FRACTION);

        assert_eq!(at(lower_top + 1.0).map(|(d, _)| d), Some(Direction::Down));
        assert_eq!(at(lower_top - band_h * 0.5).map(|(d, _)| d), Some(Direction::Down), "approaching");
        assert_eq!(at(lower_top - band_h * 1.0), None, "still reading");
        assert_eq!(at(v.y + 1.0).map(|(d, _)| d), Some(Direction::Up));
        assert_eq!(at(v.y + v.h * 0.5), None);
        assert_eq!(s.near_band(Some(GlobalPx { x: 10.0, y: lower_top + 1.0 }), &d, 1.0), None, "off the surface");

        let top = Scrollable { above_px: 0.0, ..d };

        assert_eq!(s.near_band(Some(GlobalPx { x: 500.0, y: v.y + 1.0 }), &top, 1.0), None, "nothing above");

        let (_, r) = at(lower_top + 1.0).unwrap();

        assert!((r.y - lower_top).abs() < 1e-9 && (r.h - band_h).abs() < 1e-9);
    }

    /// A scroll in progress names its band even with the eyes lost or off the screen.
    #[test]
    fn near_band_follows_a_running_scroll() {
        let mut s = EdgeScroller::new(EdgeParams::default());
        let d     = discord();
        let v     = d.viewport;
        let deep  = at(500.0, v.y + v.h - 5.0);

        s.update(0.0, deep, Some(&d));
        s.update(1.0, deep, Some(&d));

        assert!(s.scrolling());
        assert_eq!(s.near_band(None, &d, 0.0).map(|(dir, _)| dir), Some(Direction::Down));
    }
}
