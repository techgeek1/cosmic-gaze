//! The pointer look: how a [`Pointer`] intent becomes a dot and a highlight over time.
//!
//! The producer says what is where; this decides how it looks from one frame to the
//! next. The dot follows the gaze on a critically damped spring stepped at the display's
//! frame rate, so it moves with continuous velocity between the tracker's 33 Hz samples
//! instead of stepping, the way a VR laser pointer is smoothed: no overshoot, a settling
//! time to tune, and drift followed at the same pace as everything else. It fades in as
//! the gaze reaches something clickable and out, after a linger, as it leaves, and the
//! highlight crossfades from one element to the next instead of jumping. Everything is
//! driven by a clock the caller advances, so it runs on the overlay thread's frame
//! callbacks and can be stepped by hand in a test.
//!
//! The look is deliberately quiet. Every choice here is about not putting motion or
//! colour next to text the user is reading: the raw gaze point is never marked, and the
//! fades are fast enough not to lag the eyes but slow enough not to blink. A trail
//! behind the dot and a thinner dot on a settled fixation were both tried on 2026-09-09
//! and taken out again: at 33 Hz the trail read as a rendering bug, and a dot that
//! changed weight while the eyes held still looked like it was doing something.

use gaze_core::{GlobalPx, Rect};
use tiny_skia::Pixmap;

use crate::draw::{self, Item, PixelBox};
use crate::mapping::OutputMapping;
use crate::state::{OverlayState, Pointer, Target, Zone};
use crate::theme::Theme;

/// Radius of the dot, logical pixels. Eight across, about four times a period in the
/// UI font, which is enough to find in the periphery and small enough to leave the
/// widget under it legible.
pub const DOT_RADIUS_PX: f64 = 4.0;

/// Width of the halo around the dot and the highlight stroke, logical pixels.
pub const HALO_PX: f64 = 1.0;

/// Alpha of the dot when shown.
pub const DOT_ALPHA: f32 = 0.9;

/// How far the highlight's inner edge sits outside the element's box, logical pixels.
/// One pixel: enough to keep the stroke off the widget's own border without making the
/// box read as bigger than the widget, which it did at three.
pub const HIGHLIGHT_INFLATE_PX: f64 = 1.0;

/// Stroke width of the highlight, logical pixels.
pub const HIGHLIGHT_STROKE_PX: f64 = 2.0;

/// Alpha of the highlight stroke when fully shown.
pub const HIGHLIGHT_STROKE_ALPHA: f32 = 0.85;

/// Alpha of the highlight's interior when fully shown. Enough to see the element is
/// marked, little enough to read its label through.
pub const HIGHLIGHT_FILL_ALPHA: f32 = 0.12;

/// Stroke width of a scroll zone, logical pixels. Thinner than the highlight: the zone
/// is a place, not a thing, and it sits next to the text being read.
pub const ZONE_STROKE_PX: f64 = 1.0;

/// Alpha of the zone's stroke when fully shown.
pub const ZONE_STROKE_ALPHA: f32 = 0.3;

/// Alpha of the zone's interior while the eyes are in or approaching it.
pub const ZONE_FILL_ALPHA: f32 = 0.05;

/// Alpha of the zone's interior while it is scrolling, so the scroll is visibly on.
pub const ZONE_ACTIVE_FILL_ALPHA: f32 = 0.1;

/// Time constant of a fade in, seconds. About three of these to look fully there.
pub const FADE_IN_S: f64 = 0.06;

/// Time constant of a fade out, seconds. Slower than the fade in so a highlight moving
/// to a neighbour reads as sliding, not blinking.
pub const FADE_OUT_S: f64 = 0.2;

/// Spring position error below which the dot is at rest, logical pixels. Under a
/// quarter pixel nothing moves on screen, so frames stop.
const REST_PX: f64 = 0.25;

/// Spring speed below which the dot is at rest, logical pixels per second.
const REST_PX_S: f64 = 2.0;

/// Alpha below which a fade is finished and the item stops being drawn.
const ALPHA_EPSILON: f32 = 1.0 / 255.0;

/// The tunable parts of the pointer look, chosen by the producer at spawn.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct PointerStyle {
    /// How long the dot takes to settle on a new gaze point, seconds: the time for a
    /// critically damped spring to close 95% of a step. Shorter is more responsive and
    /// passes more of the tracker's jitter through.
    pub settle_s : f64,
    /// How long the dot stays up after nothing clickable is near any more, seconds. The
    /// near gate flickers at its edge; without a linger the dot would blink with it.
    pub linger_s : f64,
}

impl Default for PointerStyle {
    fn default() -> Self {
        PointerStyle { settle_s: 0.2, linger_s: 0.3 }
    }
}

/// Presents the pointer look. One per overlay; see the module docs.
pub struct Presenter {
    theme  : Theme,
    style  : PointerStyle,
    /// The last intent observed. `None` after the producer took the pointer down.
    intent : Option<Pointer>,
    /// The dot's opacity.
    dot    : Fade,
    /// The dot's position on its spring, and its velocity, logical pixels. `None`
    /// until the first intent; reset onto the goal whenever the dot is invisible, so
    /// it never sweeps in from wherever it was last hidden.
    spring : Option<Spring>,
    /// When `near` last went false, if it is still false. The dot fades once this is
    /// older than the linger.
    away_s : Option<f64>,
    /// Every highlight still visible: the current target fading in, and any previous
    /// ones fading out behind it.
    boxes  : Vec<Highlight>,
    /// Every scroll zone still visible, likewise.
    zones  : Vec<Band>,
    /// The clock as of the last step, seconds on the caller's timeline.
    now_s  : f64,
}

/// An opacity easing towards a goal.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Fade {
    alpha : f32,
    goal  : f32,
}

/// One element's highlight and its fade.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Highlight {
    target : Target,
    fade   : Fade,
}

/// One scroll band's zone, its fade, and how far it has turned from resting to scrolling.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Band {
    zone     : Zone,
    fade     : Fade,
    /// 0 while the eyes rest in the band, 1 while it scrolls; eased between the two.
    strength : Fade,
}

/// A critically damped spring in two dimensions.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Spring {
    pos  : GlobalPx,
    vel  : (f64, f64),
    goal : GlobalPx,
}

// --- Presenter ---

impl Presenter {
    /// A presenter showing nothing, drawing in `theme`.
    pub fn new(theme: Theme, style: PointerStyle) -> Presenter {
        Presenter {
            theme  : theme,
            style  : style,
            intent : None,
            dot    : Fade { alpha: 0.0, goal: 0.0 },
            spring : None,
            away_s : None,
            boxes  : Vec::new(),
            zones  : Vec::new(),
            now_s  : 0.0,
        }
    }

    /// Replaces the theme. Takes effect on the next frame; nothing fades.
    pub fn set_theme(&mut self, theme: Theme) {
        self.theme = theme;
    }

    /// The theme in use.
    pub fn theme(&self) -> &Theme {
        &self.theme
    }

    /// Takes a new intent. Sets where every fade is heading; nothing moves until
    /// [`Presenter::step`].
    pub fn observe(&mut self, pointer: Option<&Pointer>) {
        let Some(p) = pointer else {
            self.intent   = None;
            self.dot.goal = 0.0;
            self.away_s   = None;

            for h in &mut self.boxes {
                h.fade.goal = 0.0;
            }

            for z in &mut self.zones {
                z.fade.goal = 0.0;
            }

            return;
        };

        // The dot: shown near something clickable, and taken down a linger after
        // nothing is (the fade itself is decided in `step`, which has the clock).
        if p.near {
            self.away_s   = None;
            self.dot.goal = DOT_ALPHA;
        }
        else if self.away_s.is_none() {
            self.away_s = Some(self.now_s);
        }

        // The spring: aim at the new point. An invisible dot is put straight there, so
        // it appears where the eyes are rather than sweeping in from where it was.
        match &mut self.spring {
            Some(spring) if self.dot.alpha > 0.0 => spring.goal = p.gaze,
            _ => self.spring = Some(Spring { pos: p.gaze, vel: (0.0, 0.0), goal: p.gaze }),
        }

        // The highlight: the current target heads for full, everything else for zero.
        // A target already on the list (fading out after a brief switch away, say) is
        // simply turned around, and one reported at a new place follows its element.
        for h in &mut self.boxes {
            h.fade.goal = 0.0;
        }

        if let Some(target) = p.target {
            match self.boxes.iter_mut().find(|h| h.target.id == target.id) {
                Some(h) => {
                    h.target    = target;
                    h.fade.goal = 1.0;
                }
                None    => self.boxes.push(Highlight {
                    target : target,
                    fade   : Fade { alpha: 0.0, goal: 1.0 },
                }),
            }
        }

        // The zone: the same, keyed by the band's place. A band that moved (the
        // surface was resized) is a new one; the old fades out where it was.
        for z in &mut self.zones {
            z.fade.goal = 0.0;
        }

        if let Some(zone) = p.zone {
            let strength = if zone.active { 1.0 } else { 0.0 };

            match self.zones.iter_mut().find(|z| same_rect(z.zone.rect, zone.rect)) {
                Some(z) => {
                    z.zone          = zone;
                    z.fade.goal     = 1.0;
                    z.strength.goal = strength;
                }
                None    => self.zones.push(Band {
                    zone     : zone,
                    fade     : Fade { alpha: 0.0, goal: 1.0 },
                    strength : Fade { alpha: strength, goal: strength },
                }),
            }
        }

        self.intent = Some(*p);
    }

    /// Advances every fade to time `now_s` and drops what has finished. Returns true
    /// while something is still changing, which is the caller's cue to draw again on
    /// the next frame.
    pub fn step(&mut self, now_s: f64) -> bool {
        let dt = (now_s - self.now_s).max(0.0);

        self.now_s = now_s;

        if self.away_s.is_some_and(|t| now_s - t >= self.style.linger_s) {
            self.dot.goal = 0.0;
        }

        self.dot.step(dt);

        if let Some(spring) = &mut self.spring {
            spring.step(dt, self.style.settle_s);
        }

        for h in &mut self.boxes {
            h.fade.step(dt);
        }

        self.boxes.retain(|h| h.fade.goal > 0.0 || h.fade.alpha > 0.0);

        for z in &mut self.zones {
            z.fade.step(dt);
            z.strength.step(dt);
        }

        self.zones.retain(|z| z.fade.goal > 0.0 || z.fade.alpha > 0.0);

        self.active()
    }

    /// True while a fade is in progress, the dot is still moving on its spring, or a
    /// linger is running out.
    pub fn active(&self) -> bool {
        !self.dot.settled()
            || self.boxes.iter().any(|h| !h.fade.settled())
            || self.zones.iter().any(|z| !z.fade.settled() || !z.strength.settled())
            || (self.dot.alpha > 0.0 && self.spring.is_some_and(|s| !s.at_rest()))
            || (self.away_s.is_some() && self.dot.goal > 0.0)
    }

    /// True when nothing is drawn and nothing is about to be.
    pub fn is_idle(&self) -> bool {
        self.dot.alpha <= 0.0 && self.boxes.is_empty() && self.zones.is_empty()
    }

    /// The draw items for one output as of the last step. Like [`draw::scene`], items
    /// are emitted whether or not they touch this output and clipped by the caller.
    pub fn scene(&self, map: &OutputMapping) -> Vec<Item> {
        let mut items = Vec::new();

        // Zones under everything: they are wide and faint, and a highlight inside one
        // (a link in the last lines of a page) must still read as the thing marked.
        for z in &self.zones {
            let alpha = z.fade.alpha;

            if alpha < ALPHA_EPSILON {
                continue;
            }

            let fill         = ZONE_FILL_ALPHA + (ZONE_ACTIVE_FILL_ALPHA - ZONE_FILL_ALPHA) * z.strength.alpha;
            let (x, y, w, h) = map.buffer_rect(z.zone.rect);

            items.push(Item::RoundBox {
                x      : x,
                y      : y,
                w      : w,
                h      : h,
                radius : map.buffer_len(self.theme.radius_px),
                stroke : map.buffer_len(ZONE_STROKE_PX),
                halo   : 0.0,
                color  : self.theme.accent_at(alpha * ZONE_STROKE_ALPHA),
                fill   : self.theme.accent_at(alpha * fill),
                halo_color : self.theme.halo_at(0.0),
            });
        }

        // Highlights next so the dot stays on top of them. Older ones first too, so a
        // target fading in paints over the one it replaces.
        for hl in &self.boxes {
            let alpha = hl.fade.alpha;

            if alpha < ALPHA_EPSILON {
                continue;
            }

            let (x, y, w, h) = map.buffer_rect(inflate(hl.target.rect, HIGHLIGHT_INFLATE_PX));

            items.push(Item::RoundBox {
                x      : x,
                y      : y,
                w      : w,
                h      : h,
                radius : map.buffer_len(self.theme.radius_px + HIGHLIGHT_INFLATE_PX),
                stroke : map.buffer_len(HIGHLIGHT_STROKE_PX),
                halo   : map.buffer_len(HALO_PX),
                color  : self.theme.accent_at(alpha * HIGHLIGHT_STROKE_ALPHA),
                fill   : self.theme.accent_at(alpha * HIGHLIGHT_FILL_ALPHA),
                halo_color : self.theme.halo_at(alpha),
            });
        }

        let alpha = self.dot.alpha;

        if alpha < ALPHA_EPSILON {
            return items;
        }

        if let (Some(_), Some(spring)) = (self.intent, self.spring) {
            let (cx, cy) = map.buffer(spring.pos);

            items.push(Item::Marker {
                cx         : cx,
                cy         : cy,
                radius     : map.buffer_len(DOT_RADIUS_PX),
                halo       : map.buffer_len(HALO_PX),
                color      : self.theme.accent_at(alpha),
                halo_color : self.theme.halo_at(alpha),
            });
        }

        items
    }

    /// Renders one output with both the debug items of `state` and this look, into a
    /// standalone pixmap. The offline twin of what the overlay commits.
    pub fn render(&self, state: &OverlayState, map: &OutputMapping) -> Option<Pixmap> {
        let (w, h)     = map.buffer_size();
        let mut pixmap = Pixmap::new(w, h)?;
        let whole      = PixelBox { x: 0, y: 0, w: w as i32, h: h as i32 };
        let mut items  = draw::scene(state, map);

        items.extend(self.scene(map));

        draw::clear(&mut pixmap.as_mut(), whole, state.background);
        draw::draw(&mut pixmap.as_mut(), &items);

        Some(pixmap)
    }

    /// The dot's current alpha. For tests and the CLI's report.
    pub fn dot_alpha(&self) -> f32 {
        self.dot.alpha
    }

    /// Where the dot is drawn right now, on its spring. `None` before the first intent.
    pub fn dot_at(&self) -> Option<GlobalPx> {
        self.spring.map(|s| s.pos)
    }

    /// The highlights currently visible with their alphas, oldest first.
    pub fn highlights(&self) -> impl Iterator<Item = (Target, f32)> + '_ {
        self.boxes.iter().map(|h| (h.target, h.fade.alpha))
    }

    /// The scroll zones currently visible with their alphas, oldest first.
    pub fn zones(&self) -> impl Iterator<Item = (Zone, f32)> + '_ {
        self.zones.iter().map(|z| (z.zone, z.fade.alpha))
    }
}

// --- Fade ---

impl Fade {
    /// Moves `alpha` towards `goal` by `dt` seconds of exponential easing, snapping
    /// the last fraction of a level so a fade actually finishes.
    fn step(&mut self, dt: f64) {
        let tau = if self.goal > self.alpha { FADE_IN_S } else { FADE_OUT_S };
        let k   = (1.0 - (-dt / tau).exp()) as f32;

        self.alpha += (self.goal - self.alpha) * k;

        if (self.goal - self.alpha).abs() < ALPHA_EPSILON {
            self.alpha = self.goal;
        }
    }

    /// True when the fade has reached its goal.
    fn settled(&self) -> bool {
        self.alpha == self.goal
    }
}

// --- Spring ---

impl Spring {
    /// Advances the spring by `dt` seconds towards its goal, critically damped, settling
    /// 95% of a step in `settle_s`.
    ///
    /// The closed form (position and velocity both scaled by `exp(-w dt)`) is exact for
    /// a constant goal over the step, so it is stable at any frame interval, including
    /// the 200 ms the overlay allows itself when a frame callback never comes.
    fn step(&mut self, dt: f64, settle_s: f64) {
        if dt <= 0.0 {
            return;
        }

        // A critically damped step response reaches 95% at about 4.75 / w.
        let w = 4.75 / settle_s.max(1e-3);
        let e = (-w * dt).exp();

        let axis = |x: f64, v: f64, goal: f64| {
            let dx   = x - goal;
            let temp = (v + w * dx) * dt;

            (goal + (dx + temp) * e, (v - w * temp) * e)
        };

        let (x, vx) = axis(self.pos.x, self.vel.0, self.goal.x);
        let (y, vy) = axis(self.pos.y, self.vel.1, self.goal.y);

        self.pos = GlobalPx { x: x, y: y };
        self.vel = (vx, vy);

        if self.at_rest() {
            self.pos = self.goal;
            self.vel = (0.0, 0.0);
        }
    }

    /// True when the spring is close enough to its goal, and slow enough, that nothing
    /// visible would change by stepping it.
    fn at_rest(&self) -> bool {
        (self.pos.x - self.goal.x).hypot(self.pos.y - self.goal.y) < REST_PX
            && self.vel.0.hypot(self.vel.1) < REST_PX_S
    }
}

// --- Internals ---

/// Whether two rectangles are the same place to within a pixel fraction.
fn same_rect(a: Rect, b: Rect) -> bool {
    (a.x - b.x).abs() < 0.5 && (a.y - b.y).abs() < 0.5 && (a.w - b.w).abs() < 0.5 && (a.h - b.h).abs() < 0.5
}

/// Grows a rectangle by `by` on every side.
fn inflate(r: Rect, by: f64) -> Rect {
    Rect { x: r.x - by, y: r.y - by, w: r.w + by * 2.0, h: r.h + by * 2.0 }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    fn map() -> OutputMapping {
        OutputMapping::new("DP-1", Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 }, 1)
    }

    fn presenter() -> Presenter {
        Presenter::new(Theme::fallback(), PointerStyle::default())
    }

    fn pointer(x: f64, near: bool, target: Option<u64>) -> Pointer {
        Pointer {
            gaze   : GlobalPx { x: x, y: 300.0 },
            near   : near,
            target : target.map(|id| Target {
                id   : id,
                rect : Rect { x: x - 50.0, y: 280.0, w: 100.0, h: 40.0 },
            }),
            zone   : None,
        }
    }

    fn scrolling(active: bool) -> Pointer {
        Pointer {
            gaze   : GlobalPx { x: 500.0, y: 560.0 },
            near   : true,
            target : None,
            zone   : Some(Zone { rect: Rect { x: 0.0, y: 520.0, w: 1000.0, h: 80.0 }, active: active }),
        }
    }

    /// Runs the presenter for `seconds` in frame-sized steps from its current clock.
    fn run(p: &mut Presenter, seconds: f64) -> bool {
        let mut active = false;
        let start      = p.now_s;
        let mut t      = start;

        while t < start + seconds {
            t     += 1.0 / 60.0;
            active = p.step(t);
        }

        active
    }

    /// Nothing near, nothing drawn: a gaze wandering over prose leaves the screen alone.
    #[test]
    fn the_dot_is_hidden_away_from_interactive_elements() {
        let mut p = presenter();

        p.observe(Some(&pointer(100.0, false, None)));
        run(&mut p, 0.5);

        assert!(p.scene(&map()).is_empty());
        assert!(p.is_idle());
    }

    /// The dot fades in near a target and the highlight comes up with it; the dot's
    /// weight does not change afterwards, and a settled look asks for no frames.
    #[test]
    fn the_dot_appears_near_a_target_at_one_weight() {
        let mut p = presenter();

        p.observe(Some(&pointer(100.0, true, Some(1))));

        assert!(p.step(0.001) , "a fade in is in progress");
        assert!(p.dot_alpha() > 0.0 && p.dot_alpha() < DOT_ALPHA);

        run(&mut p, 0.5);

        assert_eq!(p.dot_alpha(), DOT_ALPHA);
        assert_eq!(p.highlights().map(|(_, a)| a).collect::<Vec<_>>(), vec![1.0]);
        assert!(!p.active(), "a settled look asks for no frames");
    }

    /// A blink of the near gate does not blink the dot: it stays up through the linger
    /// and only fades once nothing has been near for that long.
    #[test]
    fn the_dot_lingers_through_a_gap_in_near() {
        let mut p = presenter();

        p.observe(Some(&pointer(100.0, true, None)));
        run(&mut p, 0.5);

        p.observe(Some(&pointer(100.0, false, None)));
        run(&mut p, 0.1);

        assert_eq!(p.dot_alpha(), DOT_ALPHA, "still up inside the linger");

        p.observe(Some(&pointer(100.0, true, None)));
        run(&mut p, 0.5);

        assert_eq!(p.dot_alpha(), DOT_ALPHA, "near again, never faded");

        let linger = p.style.linger_s;

        p.observe(Some(&pointer(100.0, false, None)));
        run(&mut p, linger + 1.5);

        assert!(p.is_idle(), "gone once nothing was near for the linger");
    }

    /// The dot follows a moved gaze without overshoot and comes to rest on it, and an
    /// invisible dot appears on the new point rather than sweeping in.
    #[test]
    fn the_dot_springs_to_the_gaze_without_overshoot() {
        let mut p = presenter();

        p.observe(Some(&pointer(100.0, true, None)));
        run(&mut p, 0.5);
        p.observe(Some(&pointer(400.0, true, None)));

        let mut last_x = 100.0;
        let mut t      = p.now_s;

        for _ in 0..60 {
            t += 1.0 / 60.0;
            p.step(t);

            let x = p.dot_at().unwrap().x;

            assert!(x >= last_x - 1e-6 && x <= 400.0 + 1e-6, "monotone, no overshoot: {x}");
            last_x = x;
        }

        assert!((last_x - 400.0).abs() < 1.0, "settled within a second: {last_x}");
        assert!(!p.active());

        // Hidden, then shown somewhere else: no sweep.
        p.observe(Some(&pointer(400.0, false, None)));
        run(&mut p, 2.0);
        p.observe(Some(&pointer(50.0, true, None)));

        assert_eq!(p.dot_at().unwrap().x, 50.0);
    }

    /// The spring closes 95% of a step in its settle time.
    #[test]
    fn the_spring_settles_in_its_settle_time() {
        let mut s = Spring {
            pos  : GlobalPx { x: 0.0, y: 0.0 },
            vel  : (0.0, 0.0),
            goal : GlobalPx { x: 100.0, y: 0.0 },
        };

        let mut t = 0.0;

        while t < 0.2 {
            s.step(1.0 / 144.0, 0.2);
            t += 1.0 / 144.0;
        }

        assert!((s.pos.x - 95.0).abs() < 2.0, "{}", s.pos.x);
    }

    /// Moving from one element to the next crossfades: for a while both are drawn, the
    /// old one fading out behind the new one, and then only the new one is left.
    #[test]
    fn a_new_target_crossfades_from_the_old_one() {
        let mut p = presenter();

        p.observe(Some(&pointer(100.0, true, Some(1))));
        run(&mut p, 0.5);

        p.observe(Some(&pointer(300.0, true, Some(2))));
        p.step(p.now_s + 0.03);

        let mid: Vec<(u64, f32)> = p.highlights().map(|(t, a)| (t.id, a)).collect();

        assert_eq!(mid.len(), 2);
        assert_eq!(mid[0].0, 1);
        assert!(mid[0].1 < 1.0 && mid[0].1 > 0.0, "old target fading out: {mid:?}");
        assert!(mid[1].1 > 0.0 && mid[1].1 < 1.0, "new target fading in: {mid:?}");

        let items = p.scene(&map());

        assert_eq!(items.iter().filter(|i| matches!(i, Item::RoundBox { .. })).count(), 2);

        run(&mut p, 1.5);

        assert_eq!(p.highlights().map(|(t, _)| t.id).collect::<Vec<_>>(), vec![2]);
    }

    /// Taking the pointer down fades everything out and leaves the presenter idle, and
    /// idle means it stops asking for frames.
    #[test]
    fn taking_the_pointer_down_fades_out_and_goes_idle() {
        let mut p = presenter();

        p.observe(Some(&pointer(100.0, true, Some(1))));
        run(&mut p, 0.5);
        p.observe(None);

        assert!(p.step(p.now_s + 0.01), "fading out");
        assert!(!p.scene(&map()).is_empty());

        run(&mut p, 1.5);

        assert!(p.is_idle());
        assert!(!p.active());
        assert!(p.scene(&map()).is_empty());
    }

    /// A scroll zone fades in with the dot, deepens when the scroll starts without
    /// restarting its fade, and fades out with the pointer.
    #[test]
    fn a_zone_fades_in_deepens_while_scrolling_and_fades_out() {
        let mut p = presenter();

        p.observe(Some(&scrolling(false)));
        run(&mut p, 0.5);

        assert_eq!(p.dot_alpha(), DOT_ALPHA);
        assert_eq!(p.zones().map(|(_, a)| a).collect::<Vec<_>>(), vec![1.0]);
        assert!(!p.active());

        let resting = p.scene(&map());

        p.observe(Some(&scrolling(true)));

        assert!(p.step(p.now_s + 0.01), "strength easing");

        run(&mut p, 0.5);

        let fill_of = |items: &[Item]| match items[0] {
            Item::RoundBox { fill, .. } => fill[3],
            _                           => panic!("zone first"),
        };

        assert!(fill_of(&p.scene(&map())) > fill_of(&resting), "deeper while scrolling");
        assert_eq!(p.zones().count(), 1, "same band, no crossfade");

        p.observe(None);
        run(&mut p, 1.5);

        assert!(p.is_idle());
    }

    /// The highlight follows an element that is reported at a new place under the same
    /// id without restarting its fade.
    #[test]
    fn a_target_that_moves_keeps_its_fade() {
        let mut p = presenter();

        p.observe(Some(&pointer(100.0, true, Some(1))));
        run(&mut p, 0.5);
        p.observe(Some(&pointer(140.0, true, Some(1))));
        p.step(p.now_s + 0.001);

        let (target, alpha) = p.highlights().next().unwrap();

        assert_eq!(alpha, 1.0);
        assert_eq!(target.rect.x, 90.0);
    }
}
