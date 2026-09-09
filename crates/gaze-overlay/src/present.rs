//! The pointer look: how a [`Pointer`] intent becomes a dot and a highlight over time.
//!
//! The producer says what is where; this decides how it looks from one frame to the
//! next. The dot fades in as the gaze reaches something clickable and out as it leaves,
//! and the highlight crossfades from one element to the next instead of jumping.
//! Everything is driven by a clock the caller advances, so it runs on the overlay
//! thread's frame callbacks and can be stepped by hand in a test.
//!
//! The look is deliberately quiet. Every choice here is about not putting motion or
//! colour next to text the user is reading: the raw gaze point is never marked, and the
//! fades are fast enough not to lag the eyes but slow enough not to blink. A trail
//! behind the dot and a thinner dot on a settled fixation were both tried on 2026-09-09
//! and taken out again: at 33 Hz the trail read as a rendering bug, and a dot that
//! changed weight while the eyes held still looked like it was doing something.

use gaze_core::Rect;
use tiny_skia::Pixmap;

use crate::draw::{self, Item, PixelBox};
use crate::mapping::OutputMapping;
use crate::state::{OverlayState, Pointer, Target};
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

/// Time constant of a fade in, seconds. About three of these to look fully there.
pub const FADE_IN_S: f64 = 0.06;

/// Time constant of a fade out, seconds. Slower than the fade in so a highlight moving
/// to a neighbour reads as sliding, not blinking.
pub const FADE_OUT_S: f64 = 0.14;

/// Alpha below which a fade is finished and the item stops being drawn.
const ALPHA_EPSILON: f32 = 1.0 / 255.0;

/// Presents the pointer look. One per overlay; see the module docs.
pub struct Presenter {
    theme  : Theme,
    /// The last intent observed. `None` after the producer took the pointer down.
    intent : Option<Pointer>,
    /// The dot's opacity.
    dot    : Fade,
    /// Every highlight still visible: the current target fading in, and any previous
    /// ones fading out behind it.
    boxes  : Vec<Highlight>,
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

// --- Presenter ---

impl Presenter {
    /// A presenter showing nothing, drawing in `theme`.
    pub fn new(theme: Theme) -> Presenter {
        Presenter {
            theme  : theme,
            intent : None,
            dot    : Fade { alpha: 0.0, goal: 0.0 },
            boxes  : Vec::new(),
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

            for h in &mut self.boxes {
                h.fade.goal = 0.0;
            }

            return;
        };

        // The dot: shown near something clickable, and not otherwise.
        self.dot.goal = if p.near { DOT_ALPHA } else { 0.0 };

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

        self.intent = Some(*p);
    }

    /// Advances every fade to time `now_s` and drops what has finished. Returns true
    /// while something is still changing, which is the caller's cue to draw again on
    /// the next frame.
    pub fn step(&mut self, now_s: f64) -> bool {
        let dt = (now_s - self.now_s).max(0.0);

        self.now_s = now_s;
        self.dot.step(dt);

        for h in &mut self.boxes {
            h.fade.step(dt);
        }

        self.boxes.retain(|h| h.fade.goal > 0.0 || h.fade.alpha > 0.0);

        self.active()
    }

    /// True while a fade is in progress.
    pub fn active(&self) -> bool {
        !self.dot.settled() || self.boxes.iter().any(|h| !h.fade.settled())
    }

    /// True when nothing is drawn and nothing is about to be.
    pub fn is_idle(&self) -> bool {
        self.dot.alpha <= 0.0 && self.boxes.is_empty()
    }

    /// The draw items for one output as of the last step. Like [`draw::scene`], items
    /// are emitted whether or not they touch this output and clipped by the caller.
    pub fn scene(&self, map: &OutputMapping) -> Vec<Item> {
        let mut items = Vec::new();

        // Highlights first so the dot stays on top of them. Older ones first too, so a
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

        if let Some(p) = self.intent {
            let (cx, cy) = map.buffer(p.gaze);

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

    /// The highlights currently visible with their alphas, oldest first.
    pub fn highlights(&self) -> impl Iterator<Item = (Target, f32)> + '_ {
        self.boxes.iter().map(|h| (h.target, h.fade.alpha))
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

// --- Internals ---

/// Grows a rectangle by `by` on every side.
fn inflate(r: Rect, by: f64) -> Rect {
    Rect { x: r.x - by, y: r.y - by, w: r.w + by * 2.0, h: r.h + by * 2.0 }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use gaze_core::GlobalPx;

    use super::*;

    fn map() -> OutputMapping {
        OutputMapping::new("DP-1", Rect { x: 0.0, y: 0.0, w: 1000.0, h: 600.0 }, 1)
    }

    fn pointer(x: f64, near: bool, target: Option<u64>) -> Pointer {
        Pointer {
            gaze   : GlobalPx { x: x, y: 300.0 },
            near   : near,
            target : target.map(|id| Target {
                id   : id,
                rect : Rect { x: x - 50.0, y: 280.0, w: 100.0, h: 40.0 },
            }),
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
        let mut p = Presenter::new(Theme::fallback());

        p.observe(Some(&pointer(100.0, false, None)));
        run(&mut p, 0.5);

        assert!(p.scene(&map()).is_empty());
        assert!(p.is_idle());
    }

    /// The dot fades in near a target and the highlight comes up with it; the dot's
    /// weight does not change afterwards.
    #[test]
    fn the_dot_appears_near_a_target_at_one_weight() {
        let mut p = Presenter::new(Theme::fallback());

        p.observe(Some(&pointer(100.0, true, Some(1))));

        assert!(p.step(0.001) , "a fade in is in progress");
        assert!(p.dot_alpha() > 0.0 && p.dot_alpha() < DOT_ALPHA);

        run(&mut p, 0.5);

        assert_eq!(p.dot_alpha(), DOT_ALPHA);
        assert_eq!(p.highlights().map(|(_, a)| a).collect::<Vec<_>>(), vec![1.0]);
        assert!(!p.active(), "a settled look asks for no frames");
    }

    /// Moving from one element to the next crossfades: for a while both are drawn, the
    /// old one fading out behind the new one, and then only the new one is left.
    #[test]
    fn a_new_target_crossfades_from_the_old_one() {
        let mut p = Presenter::new(Theme::fallback());

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

        run(&mut p, 1.0);

        assert_eq!(p.highlights().map(|(t, _)| t.id).collect::<Vec<_>>(), vec![2]);
    }

    /// Taking the pointer down fades everything out and leaves the presenter idle, and
    /// idle means it stops asking for frames.
    #[test]
    fn taking_the_pointer_down_fades_out_and_goes_idle() {
        let mut p = Presenter::new(Theme::fallback());

        p.observe(Some(&pointer(100.0, true, Some(1))));
        run(&mut p, 0.5);
        p.observe(None);

        assert!(p.step(p.now_s + 0.01), "fading out");
        assert!(!p.scene(&map()).is_empty());

        run(&mut p, 1.0);

        assert!(p.is_idle());
        assert!(!p.active());
        assert!(p.scene(&map()).is_empty());
    }

    /// The highlight follows an element that is reported at a new place under the same
    /// id without restarting its fade.
    #[test]
    fn a_target_that_moves_keeps_its_fade() {
        let mut p = Presenter::new(Theme::fallback());

        p.observe(Some(&pointer(100.0, true, Some(1))));
        run(&mut p, 0.5);
        p.observe(Some(&pointer(140.0, true, Some(1))));
        p.step(p.now_s + 0.001);

        let (target, alpha) = p.highlights().next().unwrap();

        assert_eq!(alpha, 1.0);
        assert_eq!(target.rect.x, 90.0);
    }
}
