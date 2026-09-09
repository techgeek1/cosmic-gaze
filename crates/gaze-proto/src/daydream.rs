//! The Daydream controller as the session's commit and fine channel.
//!
//! [`gaze_daydream::Controller`] delivers raw reports; this turns them into the same
//! [`Control`] stream the mouse buttons produce, plus the [`Refine`] gesture the mouse has
//! no equivalent of. Roles follow DESIGN.md's fine-channel plan:
//!
//! * the touchpad **click** commits, a **tap** on the pad (down and up within
//!   [`TAP_MAX`], no travel, no click) commits with the secondary button, **Home** exits,
//!   **App** is the voice stack's push-to-talk for as long as it is held (forwarded as
//!   F13, see [`Control::PushToTalk`]), and the **volume** keys are a wheel, one detent
//!   per press and repeating while held;
//! * the thumb resting on the pad is the **clutch**: landing on it arms the pointer look
//!   ([`Control::Arm`]), so the eyes' target shows while the thumb rests; once the thumb
//!   (touch) or the wrist (gyro) has travelled [`REFINE_START_PX`] the refine begins and
//!   from then on moves the commit point until the thumb lifts. Pressing the pad to click
//!   implies touching it, so a refine always ends in a commit if one is wanted.
//!
//! The controller used to arbitrate the pointer against the mouse from its gyro (held or
//! on the desk, picked up since the mouse last moved). That went on 2026-09-09 with the
//! modes: a thumb on the pad is the whole statement of intent, and with the thumb up
//! nothing on the gaze side moves the pointer except a scroll that borrows it and gives
//! it back.
//!
//! The touchpad is the default; the gyro was the bet (no pad-edge problem) but on the desk
//! it was jittery and its yaw barely registered, so it stays as an option behind
//! `--refine gyro`. Drift is irrelevant either way because every gesture starts from
//! wherever gaze put the point.

use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use gaze_daydream::{Button, Buttons, Controller, Report};
use tracing::warn;

use crate::source::{Control, Refine};

/// Which sensor moves the commit point while the thumb is on the pad.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum RefineMode {
    /// Thumb motion across the pad.
    #[default]
    Touch,
    /// Angular rate of the controller: turn the wrist, the point moves.
    Gyro,
    /// No refinement; the pad only commits.
    Off,
}

/// Default gyro gain: logical pixels per radian of wrist turn. At 1500 a one degree turn
/// is 26 px, so the ±1° of residual gaze error is a few degrees of wrist.
pub const DEFAULT_GYRO_GAIN_PX_PER_RAD: f64 = 1500.0;

/// Default touch gain: logical pixels per full pad width. 250 makes one count of the pad's
/// 8-bit resolution about one pixel, and puts the whole refine box
/// ([`DEFAULT_RANGE_PX`]) inside half a pad.
pub const DEFAULT_TOUCH_GAIN_PX: f64 = 250.0;

/// Default size of the box the refined point may move within, centred on the anchor,
/// logical pixels. Gaze puts the point within a degree or two; the fine channel only has
/// to cover that, and a bounded box keeps a stray sweep from flinging the pointer away.
pub const DEFAULT_RANGE_PX: f64 = 100.0;

/// Default axis mapping: pointer x from minus gyro y, pointer y from minus gyro x. The
/// controller's y axis is the pad normal (gravity reads +g on it lying flat), so yaw is
/// about y; turning left is positive there and must move the pointer left. Pitch is about
/// x with nose-up positive, which must move the pointer up, so both are negated.
pub const DEFAULT_AXES: &str = "-y,-x";

/// A thumb down and up within this long, without travel or a click, is a tap.
pub const TAP_MAX: Duration = Duration::from_millis(250);

/// Pad travel (in pad widths) past which a touch is a drag, not a tap. The pad's 8-bit
/// resolution puts one count at 0.004; a thumb landing and lifting moves a few counts.
pub const TAP_MAX_PAD: f32 = 0.04;

/// How far the thumb must have moved from where it landed, as a fraction of the pad,
/// before a touch becomes a refine. A thumb settling onto the pad shifts its centroid a
/// good deal as the pad of the finger flattens, so this is well past the tap tolerance;
/// with the default touch gain it is 30 logical pixels. The gyro mode accumulates its
/// own motion against the same figure in pixels.
pub const REFINE_START_PAD: f32 = 0.12;

/// Shortest time after the thumb lands before travel counts, so the settling of the
/// first few reports never starts a refine.
const REFINE_START_DELAY: Duration = Duration::from_millis(120);

/// Angular rate below which the gyro is noise, rad/s. At rest on the desk the peak over a
/// second was 0.01 to 0.03.
const GYRO_DEADZONE_RAD_S: f32 = 0.03;

/// Longest gap between two reports that still integrates as motion. Longer means the
/// controller went away, and the motion in between is not known.
const MAX_DT: Duration = Duration::from_millis(50);

/// A held volume key starts repeating after this long ...
const REPEAT_DELAY: Duration = Duration::from_millis(350);

/// ... and repeats at this interval.
const REPEAT_INTERVAL: Duration = Duration::from_millis(120);

/// How long without a report before every held button is treated as released. The
/// controller streams at about 62 Hz for as long as the link is up, so this much silence
/// means the link dropped or the reader died, and a button that was down at the time will
/// never send its release edge. Push-to-talk forwarded down and a wheel key repeating
/// are what would otherwise be stuck. If the link comes back with the button still held,
/// the next report reads as a fresh press, which is right.
const SILENCE_MAX: Duration = Duration::from_millis(500);

/// One signed gyro axis.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Axis {
    /// 0, 1 or 2 for x, y, z.
    index : usize,
    /// Multiply by minus one.
    negate : bool,
}

impl Axis {
    /// Reads this axis out of a gyro sample.
    fn of(self, v: glam::Vec3) -> f32 {
        let value = v[self.index];

        if self.negate { -value } else { value }
    }
}

/// Parses `--refine-axes`: two comma-separated axes, each `x`, `y` or `z` with an optional
/// leading minus, pointer x first.
pub fn parse_axes(s: &str) -> Result<(Axis, Axis)> {
    let parse_one = |t: &str| -> Result<Axis> {
        let (negate, name) = match t.strip_prefix('-') {
            Some(rest) => (true, rest),
            None       => (false, t.strip_prefix('+').unwrap_or(t)),
        };

        let index = match name {
            "x" => 0,
            "y" => 1,
            "z" => 2,
            _   => anyhow::bail!("axis {t:?} is not one of x, y, z with an optional sign"),
        };

        Ok(Axis { index: index, negate: negate })
    };

    let (a, b) = s.split_once(',')
        .with_context(|| format!("axes {s:?} must be two comma-separated axes, like {DEFAULT_AXES:?}"))?;

    Ok((parse_one(a.trim())?, parse_one(b.trim())?))
}

/// How the controller's reports become controls.
#[derive(Clone, Copy, Debug)]
pub struct DaydreamConfig {
    /// Which sensor refines.
    pub mode              : RefineMode,
    /// Pixels per radian, gyro mode.
    pub gyro_gain_px_rad  : f64,
    /// Pixels per pad width, touch mode.
    pub touch_gain_px     : f64,
    /// Which gyro axes drive pointer x and y.
    pub axes              : (Axis, Axis),
}

/// The controller plus the mapping from its reports to controls.
pub struct Daydream {
    controller  : Controller,
    mapper      : Mapper,
    /// Reports seen, for the exit summary.
    pub reports : u64,
}

/// Turns reports into controls: the state the edges are detected against. Separate from
/// the controller so it can be driven by synthetic reports in tests.
struct Mapper {
    config        : DaydreamConfig,
    /// Buttons in the last report.
    buttons       : Buttons,
    /// Thumb position in the last report, if it was on the pad.
    touch         : Option<glam::Vec2>,
    /// When and where the current touch began, for telling a tap from a drag.
    touch_since   : Option<(Instant, glam::Vec2)>,
    /// Whether the pad was clicked during the current touch, which makes it not a tap.
    touch_clicked : bool,
    /// Gyro travel since the thumb landed, before the refine began, logical pixels;
    /// `None` once it has begun (or between touches). Touch mode measures the thumb's
    /// displacement from where it landed instead and only uses this as the flag.
    pending_move  : Option<(f64, f64)>,
    /// When the last report arrived, for the gyro's `dt`.
    last_at       : Option<Instant>,
    /// Held volume keys and when each next repeats.
    repeats       : Vec<(Button, Instant)>,
}

// --- Daydream ---

impl Daydream {
    /// Connects the controller (see [`Controller::open`]) and starts reading it.
    pub fn open(address: Option<&str>, config: DaydreamConfig) -> Result<Daydream> {
        let controller = Controller::open(address).context("opening the Daydream controller")?;

        Ok(Daydream {
            controller : controller,
            mapper     : Mapper::new(config),
            reports    : 0,
        })
    }

    /// The controller's address, for the startup log.
    pub fn address(&self) -> &str {
        self.controller.address()
    }

    /// Drains the reports queued since the last call into controls, in order.
    pub fn controls(&mut self) -> Vec<Control> {
        let mut out = Vec::new();

        let reports: Vec<Report> = self.controller.reports().collect();

        for report in reports {
            self.reports += 1;
            self.mapper.step(report, &mut out);
        }

        let now = Instant::now();

        self.mapper.silence(now, &mut out);
        self.mapper.repeat(now, &mut out);

        out
    }

    /// Stops the reader. Idempotent.
    pub fn stop(&mut self) {
        self.controller.stop();
    }
}

impl Drop for Daydream {
    fn drop(&mut self) {
        self.stop();
    }
}

// --- Mapper ---

impl Mapper {
    fn new(config: DaydreamConfig) -> Mapper {
        Mapper {
            config        : config,
            buttons       : Buttons::default(),
            touch         : None,
            touch_since   : None,
            touch_clicked : false,
            pending_move  : None,
            last_at       : None,
            repeats       : Vec::new(),
        }
    }

    /// One report's worth of edges and motion.
    fn step(&mut self, report: Report, out: &mut Vec<Control>) {
        let p  = report.packet;
        let dt = self.last_at.map(|t| report.at.saturating_duration_since(t)).unwrap_or(MAX_DT);

        // A tap: the thumb lifting soon after it landed, having gone nowhere and clicked
        // nothing. Emitted before the refine ends so the commit still has the anchor.
        match (self.touch, p.touch) {
            (None, Some(at)) => {
                self.touch_since   = Some((report.at, at));
                self.touch_clicked = false;
            }

            (Some(_), None) => {
                if let Some((since, from)) = self.touch_since.take()
                    && !self.touch_clicked
                    && report.at.saturating_duration_since(since) <= TAP_MAX
                    && let Some(last) = self.touch
                    && (last - from).length() <= TAP_MAX_PAD
                {
                    out.push(Control::Context);
                }
            }

            _ => {}
        }

        if p.buttons.pressed_since(self.buttons).any(|b| b == Button::Click) {
            self.touch_clicked = true;
        }

        // The thumb landing arms the pointer look; lifting disarms it, and ends the
        // refine if one began. Before the buttons, so a press in the same report lands
        // after the motion that led up to it.
        match (self.touch, p.touch) {
            (None, Some(_)) => {
                out.push(Control::Arm { down: true });
                self.pending_move = Some((0.0, 0.0));
            }
            (Some(_), None) => {
                if self.pending_move.take().is_none() && self.config.mode != RefineMode::Off {
                    out.push(Control::Refine(Refine::End));
                }

                out.push(Control::Arm { down: false });
            }
            _ => {}
        }

        if self.config.mode != RefineMode::Off
            && let Some(now) = p.touch
        {
            let (dx, dy) = match self.config.mode {
                RefineMode::Gyro => {
                    let dt = dt.min(MAX_DT).as_secs_f64();
                    let (ax, ay) = self.config.axes;
                    let (rx, ry) = (ax.of(p.gyro), ay.of(p.gyro));

                    let rx = if rx.abs() < GYRO_DEADZONE_RAD_S { 0.0 } else { rx };
                    let ry = if ry.abs() < GYRO_DEADZONE_RAD_S { 0.0 } else { ry };

                    (f64::from(rx) * dt * self.config.gyro_gain_px_rad,
                     f64::from(ry) * dt * self.config.gyro_gain_px_rad)
                }

                RefineMode::Touch => match self.touch {
                    Some(prev) => {
                        let d = now - prev;

                        (f64::from(d.x) * self.config.touch_gain_px,
                         f64::from(d.y) * self.config.touch_gain_px)
                    }

                    None => (0.0, 0.0),
                },

                RefineMode::Off => (0.0, 0.0),
            };

            // Motion counts for nothing until it amounts to a nudge: the thumb a pad
            // fraction from where it landed, or the wrist the same distance in pixels,
            // and not within the settling time either way. Then the refine begins from
            // where the point is, the way a joystick's dead zone begins from its rim,
            // and every later move passes straight on.
            match self.pending_move.as_mut() {
                Some((px, py)) => {
                    *px += dx;
                    *py += dy;

                    let settled = self
                        .touch_since
                        .is_some_and(|(since, _)| report.at.saturating_duration_since(since) >= REFINE_START_DELAY);

                    let travelled = match self.config.mode {
                        RefineMode::Touch => self
                            .touch_since
                            .is_some_and(|(_, from)| (now - from).length() >= REFINE_START_PAD),
                        _ => px.hypot(*py) >= f64::from(REFINE_START_PAD) * self.config.touch_gain_px,
                    };

                    if settled && travelled {
                        self.pending_move = None;
                        out.push(Control::Refine(Refine::Begin));
                    }
                }

                None if dx != 0.0 || dy != 0.0 => {
                    out.push(Control::Refine(Refine::Move { dx_px: dx, dy_px: dy }));
                }

                None => {}
            }
        }

        for button in p.buttons.pressed_since(self.buttons) {
            if let Some(control) = map_button(button) {
                out.push(control);
            }

            if matches!(button, Button::VolumeUp | Button::VolumeDown) {
                self.repeats.push((button, report.at + REPEAT_DELAY));
            }
        }

        for button in p.buttons.released_since(self.buttons) {
            self.release(button, out);
        }

        self.buttons = p.buttons;
        self.touch   = p.touch;
        self.last_at = Some(report.at);
    }

    /// One button's release edge: its repeat stops, and a held push-to-talk goes back up.
    fn release(&mut self, button: Button, out: &mut Vec<Control>) {
        self.repeats.retain(|(b, _)| *b != button);

        if button == Button::App {
            out.push(Control::PushToTalk { down: false });
        }
    }

    /// The failsafe for a link that dropped mid-press: after [`SILENCE_MAX`] without a
    /// report, every button still recorded as down is released as if the edge had
    /// arrived. Nothing to do while no button is down, however long the silence.
    fn silence(&mut self, now: Instant, out: &mut Vec<Control>) {
        let quiet = self.last_at.is_some_and(|at| now.saturating_duration_since(at) > SILENCE_MAX);

        if !quiet || self.buttons.bits() == 0 {
            return;
        }

        warn!(
            silent_ms = now.saturating_duration_since(self.last_at.unwrap_or(now)).as_millis(),
            buttons   = ?self.buttons,
            "daydream controller went quiet with buttons held; releasing them",
        );

        for button in self.buttons.pressed_since(Buttons::default()).collect::<Vec<_>>() {
            self.release(button, out);
        }

        self.buttons = Buttons::default();
    }

    /// Fires the repeats that are due.
    fn repeat(&mut self, now: Instant, out: &mut Vec<Control>) {
        for (button, due) in &mut self.repeats {
            while *due <= now {
                if let Some(control) = map_button(*button) {
                    out.push(control);
                }

                *due += REPEAT_INTERVAL;
            }
        }
    }
}

/// What each button does on its press edge. The pad commits because it is the one under
/// the thumb; Home exits because it is the one a thumb does not stray onto; App is
/// push-to-talk because it is the one the thumb can hold while the pad is free. Its
/// release edge is `step`'s business, since this table only sees presses. A redetect is
/// left to the mouse's middle button.
fn map_button(button: Button) -> Option<Control> {
    match button {
        Button::Click      => Some(Control::Commit),
        Button::Home       => Some(Control::Exit),
        Button::App        => Some(Control::PushToTalk { down: true }),
        Button::VolumeUp   => Some(Control::Wheel(1)),
        Button::VolumeDown => Some(Control::Wheel(-1)),
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;
    use gaze_daydream::Packet;

    #[test]
    fn axes_parse_with_signs_and_default_is_valid() {
        let (x, y) = parse_axes(DEFAULT_AXES).unwrap();

        assert_eq!(x, Axis { index: 1, negate: true });
        assert_eq!(y, Axis { index: 0, negate: true });

        let (x, y) = parse_axes(" z , +x").unwrap();

        assert_eq!(x, Axis { index: 2, negate: false });
        assert_eq!(y, Axis { index: 0, negate: false });

        assert!(parse_axes("w,x").is_err());
        assert!(parse_axes("x").is_err());
    }

    #[test]
    fn a_negated_axis_reads_the_negative() {
        let v = glam::Vec3::new(1.0, 2.0, 3.0);

        assert_eq!(Axis { index: 1, negate: true }.of(v), -2.0);
        assert_eq!(Axis { index: 2, negate: false }.of(v), 3.0);
    }

    /// A report with the thumb at `touch`, nothing else happening.
    fn touch_report(at: Instant, touch: Option<(f32, f32)>) -> Report {
        Report {
            at     : at,
            packet : Packet {
                time        : 0,
                seq         : 0,
                orientation : glam::Vec3::ZERO,
                accel       : glam::Vec3::ZERO,
                gyro        : glam::Vec3::ZERO,
                touch       : touch.map(|(x, y)| glam::Vec2::new(x, y)),
                buttons     : Buttons::default(),
            },
        }
    }

    #[test]
    fn a_thumb_drag_moves_both_axes_by_the_pad_gain() {
        let config = DaydreamConfig {
            mode              : RefineMode::Touch,
            gyro_gain_px_rad  : DEFAULT_GYRO_GAIN_PX_PER_RAD,
            touch_gain_px     : 1000.0,
            axes              : parse_axes(DEFAULT_AXES).unwrap(),
        };

        let mut mapper = Mapper::new(config);
        let mut out    = Vec::new();
        let t0         = Instant::now();
        let step       = Duration::from_millis(16);

        // Landing arms; the refine waits for the thumb to settle and travel, then
        // begins, and only the motion after that moves the point.
        mapper.step(touch_report(t0, Some((0.20, 0.50))), &mut out);
        assert_eq!(out, vec![Control::Arm { down: true }]);

        out.clear();
        mapper.step(touch_report(t0 + step, Some((0.30, 0.50))), &mut out);
        assert!(out.is_empty(), "travel inside the settling time: {out:?}");

        let step = REFINE_START_DELAY;

        mapper.step(touch_report(t0 + step, Some((0.35, 0.50))), &mut out);
        assert_eq!(out, vec![Control::Refine(Refine::Begin)]);

        out.clear();
        mapper.step(touch_report(t0 + step + Duration::from_millis(16), Some((0.45, 0.50))), &mut out);

        let [Control::Refine(Refine::Move { dx_px, dy_px })] = out[..] else {
            panic!("expected one move, got {out:?}");
        };

        assert!((dx_px - 100.0).abs() < 1.0, "dx {dx_px}");
        assert!(dy_px.abs() < 1.0, "dy {dy_px}");

        out.clear();
        mapper.step(touch_report(t0 + step + Duration::from_millis(32), Some((0.45, 0.60))), &mut out);

        let [Control::Refine(Refine::Move { dx_px, dy_px })] = out[..] else {
            panic!("expected one move, got {out:?}");
        };

        assert!(dx_px.abs() < 1.0, "dx {dx_px}");
        assert!((dy_px - 100.0).abs() < 1.0, "dy {dy_px}");

        out.clear();
        mapper.step(touch_report(t0 + step + Duration::from_millis(48), None), &mut out);
        assert_eq!(out, vec![Control::Refine(Refine::End), Control::Arm { down: false }]);
    }

    /// A thumb that rests on the pad arms the look and never starts a refine: the
    /// gaze keeps driving, and lifting only disarms.
    #[test]
    fn a_resting_thumb_arms_without_refining() {
        let config = DaydreamConfig {
            mode              : RefineMode::Touch,
            gyro_gain_px_rad  : DEFAULT_GYRO_GAIN_PX_PER_RAD,
            touch_gain_px     : DEFAULT_TOUCH_GAIN_PX,
            axes              : parse_axes(DEFAULT_AXES).unwrap(),
        };

        let mut mapper = Mapper::new(config);
        let mut out    = Vec::new();
        let t0         = Instant::now();
        let step       = Duration::from_millis(16);

        mapper.step(touch_report(t0, Some((0.50, 0.50))), &mut out);
        mapper.step(touch_report(t0 + step, Some((0.54, 0.50))), &mut out);
        mapper.step(touch_report(t0 + 30 * step, Some((0.50, 0.55))), &mut out);
        mapper.step(touch_report(t0 + 60 * step, Some((0.46, 0.47))), &mut out);

        assert_eq!(out, vec![Control::Arm { down: true }]);

        out.clear();
        mapper.step(touch_report(t0 + 90 * step, None), &mut out);

        assert_eq!(out, vec![Control::Arm { down: false }]);
    }

    #[test]
    fn a_quick_still_touch_is_a_context_commit_and_a_drag_is_not() {
        let config = DaydreamConfig {
            mode              : RefineMode::Touch,
            gyro_gain_px_rad  : DEFAULT_GYRO_GAIN_PX_PER_RAD,
            touch_gain_px     : 250.0,
            axes              : parse_axes(DEFAULT_AXES).unwrap(),
        };

        let mut mapper = Mapper::new(config);
        let mut out    = Vec::new();
        let t0         = Instant::now();

        // Down, one jittery report, up within the tap window.
        mapper.step(touch_report(t0, Some((0.50, 0.50))), &mut out);
        mapper.step(touch_report(t0 + Duration::from_millis(60), Some((0.51, 0.50))), &mut out);
        mapper.step(touch_report(t0 + Duration::from_millis(120), None), &mut out);

        assert_eq!(out.iter().filter(|c| **c == Control::Context).count(), 1);
        assert!(out.iter().position(|c| *c == Control::Context)
                   < out.iter().position(|c| *c == Control::Arm { down: false }),
                "the context commit precedes the disarm: {out:?}");

        // A drag of the same duration is not a tap.
        out.clear();
        mapper.step(touch_report(t0 + Duration::from_millis(500), Some((0.20, 0.50))), &mut out);
        mapper.step(touch_report(t0 + Duration::from_millis(560), Some((0.40, 0.50))), &mut out);
        mapper.step(touch_report(t0 + Duration::from_millis(620), None), &mut out);

        assert!(!out.contains(&Control::Context), "{out:?}");
    }

    /// The touch mapping at its defaults.
    fn default_config() -> DaydreamConfig {
        DaydreamConfig {
            mode              : RefineMode::Touch,
            gyro_gain_px_rad  : DEFAULT_GYRO_GAIN_PX_PER_RAD,
            touch_gain_px     : DEFAULT_TOUCH_GAIN_PX,
            axes              : parse_axes(DEFAULT_AXES).unwrap(),
        }
    }

    /// A report with `buttons` down, nothing else happening.
    fn button_report(at: Instant, buttons: Buttons) -> Report {
        let mut report = touch_report(at, None);

        report.packet.buttons = buttons;

        report
    }

    #[test]
    fn silence_releases_a_held_push_to_talk_and_a_repeating_wheel_key() {
        let config     = default_config();
        let mut mapper = Mapper::new(config);
        let t0         = Instant::now();
        let mut out    = Vec::new();

        let held = Buttons::from_bits(Button::App.mask() | Button::VolumeUp.mask());

        mapper.step(button_report(t0, held), &mut out);
        assert_eq!(out, vec![Control::PushToTalk { down: true }, Control::Wheel(1)]);
        out.clear();

        // Still streaming: nothing is released, and the wheel repeats.
        mapper.step(button_report(t0 + Duration::from_millis(16), held), &mut out);
        mapper.silence(t0 + Duration::from_millis(400), &mut out);
        assert!(out.is_empty());
        mapper.repeat(t0 + Duration::from_millis(400), &mut out);
        assert!(out.iter().all(|c| *c == Control::Wheel(1)) && !out.is_empty());
        out.clear();

        // The link drops: both come up, and nothing repeats afterwards.
        let later = t0 + Duration::from_millis(16) + SILENCE_MAX + Duration::from_millis(1);

        mapper.silence(later, &mut out);
        assert_eq!(out, vec![Control::PushToTalk { down: false }]);
        out.clear();

        mapper.repeat(later + Duration::from_secs(1), &mut out);
        assert!(out.is_empty());

        // It comes back with App still held: that is a fresh press.
        mapper.step(button_report(later + Duration::from_secs(1), Buttons::from_bits(Button::App.mask())), &mut out);
        assert_eq!(out, vec![Control::PushToTalk { down: true }]);
    }

    #[test]
    fn silence_with_nothing_held_is_not_an_event() {
        let config     = default_config();
        let mut mapper = Mapper::new(config);
        let t0         = Instant::now();
        let mut out    = Vec::new();

        mapper.step(touch_report(t0, None), &mut out);
        mapper.silence(t0 + Duration::from_secs(60), &mut out);
        assert!(out.is_empty());
    }

    #[test]
    fn the_pad_commits_and_home_exits() {
        assert_eq!(map_button(Button::Click), Some(Control::Commit));
        assert_eq!(map_button(Button::Home),  Some(Control::Exit));
        assert_eq!(map_button(Button::App),   Some(Control::PushToTalk { down: true }));
        assert_eq!(map_button(Button::VolumeUp),   Some(Control::Wheel(1)));
        assert_eq!(map_button(Button::VolumeDown), Some(Control::Wheel(-1)));
    }
}
