//! gaze-inject: pointer warp, click, scroll and forwarded-key injection through
//! `/dev/uinput`. See
//! `PLAN.md`'s `gaze-inject` contract for the shape this crate fills; the portal on this
//! desk has no RemoteDesktop (ScreenCast only, see "Environment facts" in `PLAN.md`), so
//! libei is not an option and uinput is the only path in.
//!
//! Two backends implement the same [`Injector`] API. The choice is explicit, never
//! guessed: `gaze-inject-cli --backend abs|rel`, or [`Injector::create_with`]. Alongside
//! either pointer device sits a keyboard-shaped one (`keys.rs`) for the keys the session
//! forwards on behalf of something else: [`Key::F13`], the voice stack's push-to-talk,
//! held from the controller.
//!
//! - [`Backend::Relative`] (the default): a `REL_X`/`REL_Y` mouse. Primary mode is
//!   **closed-loop**: cosmic-comp 1.6.0 exposes cursor position through
//!   `ext_image_copy_capture_cursor_session_v1`, which `gaze_capture::CursorTracker` wraps,
//!   so `move_to` reads the real position, emits a capped relative step toward the target,
//!   reads the new real position, and repeats until it's within a pixel or gives up after a
//!   few iterations - see `rel.rs`. This makes libinput's pointer-acceleration curve a
//!   convergence-speed problem, not an accuracy problem, unlike a single blind relative
//!   delta. If `CursorTracker::connect()` fails (an older cosmic-comp, or the protocol
//!   missing for some other reason), this backend falls back to **open-loop**: home to the
//!   layout's top-left corner with a saturating negative move, then one uncorrected
//!   relative delta - the original, imprecise scheme, kept only as a last resort.
//! - [`Backend::Absolute`]: an `ABS_X`/`ABS_Y` uinput device (`INPUT_PROP_POINTER` +
//!   `BTN_LEFT`/`RIGHT`/`MIDDLE`, no tool buttons, no `INPUT_PROP_DIRECT`), scaled to the
//!   union bounding box of every configured output. This is **not** how cosmic-comp
//!   actually routes it, confirmed by reading `src/input/mod.rs`: a device with this
//!   capability set is classified by libinput as a plain pointer (no `BTN_TOOL_PEN` /
//!   `BTN_STYLUS`, no `INPUT_PROP_DIRECT`), which sends `PointerMotionAbsolute` events
//!   through a path that hardcodes `seat.active_output()` and ignores the compositor's
//!   per-device `map_to_output` config entirely (an open bug,
//!   pop-os/cosmic-comp#2103). Only the tablet/touch path
//!   (`mapped_output_for_device` in the same file) honours `map_to_output`, and even that
//!   path maps to exactly one named output, never the union of all outputs; cosmic-comp
//!   has no "whole layout" concept for an absolute device to begin with.
//!
//!   A per-output tablet device is a real second option, not implemented here: give the
//!   device `BTN_TOOL_PEN` (or `INPUT_PROP_DIRECT`) so libinput classifies it as a tablet
//!   or touchscreen instead of a plain pointer, scope its `ABS_X`/`ABS_Y` range to one
//!   output's local logical rect instead of the union bbox, and add an entry for its
//!   device name to `~/.config/cosmic/com.system76.CosmicComp/v1/input_devices` setting
//!   `map_to_output` to the target output's connector name, e.g. `"DP-1"`. The file and
//!   its location are confirmed, not guessed - it already exists on this desk, RON,
//!   keyed by device name - but the one entry present is a `Mouse`-shaped variant for the
//!   real Lenovo mouse (`state`, `left_handed`), not a tablet entry, so `map_to_output`'s
//!   exact field name and placement on a tablet/touch variant comes from reading
//!   cosmic-comp's source, not from an example seen in this file. Getting a per-output
//!   device working this way would mean one uinput device per output and a one-time
//!   manual compositor-config edit outside anything this crate can do for itself (writing
//!   to the user's live compositor config is out of this crate's scope). Given the
//!   closed-loop relative backend already reaches every output with sub-pixel accuracy
//!   (confirmed live: DP-2, DP-1, and HDMI-A-1 centres all landed within 1 px), this
//!   wasn't worth building for Phase 0.

// The project's code style (CLAUDE.md) requires explicit field syntax
// (`Foo { x: x }`) everywhere, which clippy's default lints read as redundant. Every
// other crate in this workspace carries the same allow for the same reason.
#![allow(clippy::redundant_field_names)]


use gaze_core::GlobalPx;

mod abs;
mod codes;
mod keys;
pub mod layout;
mod rel;

use abs::AbsoluteInjector;
use codes::WHEEL_HI_RES_PER_CLICK;
use keys::KeyInjector;
pub use layout::{DeskLayout, OutputLayout};
use rel::RelativeInjector;

/// Which mouse button an injected click reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
}

/// A keyboard key the injector can hold and release on behalf of another program. Not a
/// general keyboard: only the keys something on this desk listens for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Key {
    /// Push-to-talk for the voice stack, which is bound to F13 outside cosmic-gaze.
    F13,
}

// --- Key ---

impl Key {
    /// Every key the device registers.
    pub const ALL: [Key; 1] = [Key::F13];
}

/// Which uinput device shape [`Injector`] creates. See the module docs for what each one
/// actually does once cosmic-comp gets hold of its events.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum Backend {
    /// `ABS_X`/`ABS_Y` device scaled to the union of all outputs. Only lands correctly
    /// when the target point falls inside cosmic-comp's currently active output; see the
    /// module docs for why.
    Absolute,
    /// `REL_X`/`REL_Y` mouse, closed-loop against a measured cursor position when
    /// `gaze-capture`'s cursor tracker is available, open-loop corner-homing otherwise.
    /// The default: the only backend that reliably reaches the whole desk layout given
    /// cosmic-comp's actual absolute-device handling (see the module docs).
    #[default]
    Relative,
}

/// Shared behaviour of a uinput backend, so [`Injector`] can hold either one behind a
/// single trait object instead of duplicating the move-then-act sequencing in
/// `click_at`/`scroll` per backend.
trait InjectBackend {
    /// Moves the pointer to `p` (global logical px) without clicking.
    fn move_to(&mut self, p: GlobalPx) -> Result<()>;

    /// Nudges the pointer by `(dx, dy)` counts as one mouse report, uncorrected: the
    /// caller is a human steering by eye, and the closed loop's measure-and-correct
    /// flurry would fight them.
    fn move_by(&mut self, dx: i32, dy: i32) -> Result<()>;

    /// Presses and releases `button` at the pointer's current position.
    fn click(&mut self, button: Button) -> Result<()>;

    /// Scrolls by `dy` wheel clicks at the pointer's current position.
    fn scroll(&mut self, dy: i32) -> Result<()>;

    /// Emits one wheel report at the pointer's current position: `clicks` on `REL_WHEEL`
    /// and `hi_res` on `REL_WHEEL_HI_RES`, either of which may be zero and is then left
    /// out. The two are the same motion at two resolutions, as a high-resolution mouse
    /// reports it; the caller keeps them consistent.
    fn wheel(&mut self, clicks: i32, hi_res: i32) -> Result<()>;

    /// Whether this backend can measure the pointer right now: a cursor tracker is
    /// connected. False for the absolute backend and while the relative one is open loop,
    /// when `last_known_position` is `None` for want of a tracker rather than a pointer.
    fn measures(&self) -> bool {
        false
    }

    /// Last known measured cursor position, if this backend can observe one. The default
    /// `None` covers the absolute backend and the relative backend's open-loop fallback,
    /// neither of which has a cursor tracker to ask; the closed-loop relative backend
    /// overrides this.
    fn last_known_position(&mut self) -> Result<Option<GlobalPx>> {
        Ok(None)
    }
}

/// Pointer warp, click, and scroll injection through a uinput virtual device. Construct
/// with [`Injector::create_with`]: an explicit backend and a layout, either loaded from a
/// file (`gaze-inject-cli`) or taken from the parsed desk geometry (the daemon).
pub struct Injector {
    backend : Box<dyn InjectBackend>,
    /// The keyboard-shaped device for forwarded keys.
    keys    : KeyInjector,
    /// High-resolution wheel units emitted since the last whole click, so a stream of
    /// fractional scrolls still produces the `REL_WHEEL` clicks a legacy consumer counts.
    wheel   : WheelAccumulator,
}

/// Turns a stream of high-resolution wheel units into the whole clicks that accompany
/// them. A real high-resolution mouse sends `REL_WHEEL_HI_RES` on every report and a
/// `REL_WHEEL` click each time 120 units have gone by; this keeps that bookkeeping so a
/// smooth gaze scroll looks like such a mouse to anything reading either axis.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct WheelAccumulator {
    /// Units since the last click, always within `(-120, 120)`.
    remainder : i32,
}

// --- Injector ---

impl Injector {
    /// Creates an injector with an explicit backend and desk layout.
    pub fn create_with(backend: Backend, layout: &DeskLayout) -> Result<Injector> {
        let inner: Box<dyn InjectBackend> = match backend {
            Backend::Absolute => Box::new(AbsoluteInjector::create(layout)?),
            Backend::Relative => Box::new(RelativeInjector::create(layout)?),
        };

        let keys = KeyInjector::create()?;

        Ok(Injector { backend: inner, keys: keys, wheel: WheelAccumulator::default() })
    }

    /// Moves the pointer to `p` (global logical px) without clicking. For the closed-loop
    /// relative backend this converges iteratively against a measured position and can
    /// return `InjectError::Unreachable` if it doesn't settle in time; every other backend
    /// always succeeds (or fails only on an I/O error).
    pub fn move_to(&mut self, p: GlobalPx) -> Result<()> {
        self.backend.move_to(p)
    }

    /// Nudges the pointer by `(dx, dy)` counts as a single relative report, no correction.
    /// The compositor's pointer acceleration applies, so the pointer moves about that far;
    /// read back where it landed with [`Injector::last_known_position`] when it matters.
    pub fn move_by(&mut self, dx: i32, dy: i32) -> Result<()> {
        if dx == 0 && dy == 0 {
            return Ok(());
        }

        self.backend.move_by(dx, dy)
    }

    /// Moves the pointer to `p` and clicks `button`.
    pub fn click_at(&mut self, p: GlobalPx, button: Button) -> Result<()> {
        self.backend.move_to(p)?;
        self.backend.click(button)
    }

    /// Moves the pointer to `p` and scrolls by `dy` wheel clicks (kernel `REL_WHEEL`
    /// convention: positive is up, away from the user).
    pub fn scroll(&mut self, p: GlobalPx, dy: i32) -> Result<()> {
        self.backend.move_to(p)?;
        self.backend.scroll(dy)
    }

    /// Scrolls by `units` high-resolution wheel units (120 per click, positive is up,
    /// away from the user) at the pointer's current position, without moving it. For
    /// continuous scrolling: the caller integrates a speed and hands over whatever whole
    /// units have accrued each tick, and a `REL_WHEEL` click rides along every 120.
    pub fn scroll_hi_res(&mut self, units: i32) -> Result<()> {
        if units == 0 {
            return Ok(());
        }

        let clicks = self.wheel.push(units);

        self.backend.wheel(clicks, units)
    }

    /// Presses (`down`) or releases `key` on the keyboard-shaped device. The pointer is
    /// untouched. Edges are the caller's to pair; a session that ends while a key is down
    /// should release it, though the kernel releases everything held when the device
    /// goes away.
    pub fn key(&mut self, key: Key, down: bool) -> Result<()> {
        self.keys.key(key, down)
    }

    /// Last known measured cursor position, if the active backend can observe one (the
    /// closed-loop relative backend can; the open-loop fallback and the absolute backend
    /// cannot and return `Ok(None)`). Useful for confirming where a `move_to` actually
    /// landed without eyeballing it, e.g. `gaze-inject-cli --probe`.
    ///
    /// A long-running caller should call this every few milliseconds whether or not it
    /// wants the answer: it is also what drains the cursor tracker's connection, and a
    /// compositor hangs up on a client that leaves its socket unread (see `rel`).
    pub fn last_known_position(&mut self) -> Result<Option<GlobalPx>> {
        self.backend.last_known_position()
    }

    /// Whether [`Injector::last_known_position`] measures anything right now. False
    /// means its `None` is "cannot tell", not "on no output".
    pub fn measures(&self) -> bool {
        self.backend.measures()
    }
}

// --- WheelAccumulator ---

impl WheelAccumulator {
    /// Adds `units` and returns the whole clicks that crossed, carrying the rest.
    pub fn push(&mut self, units: i32) -> i32 {
        let total  = self.remainder + units;
        let clicks = total / WHEEL_HI_RES_PER_CLICK;

        self.remainder = total % WHEEL_HI_RES_PER_CLICK;

        clicks
    }
}

// --- Error ---

/// Everything that can go wrong creating or driving a uinput injector. No variant here
/// is reachable through a panic; every fallible step in `abs.rs`/`rel.rs`/`layout.rs`
/// maps its `io::Error`/`toml::de::Error`/`gaze_capture::CaptureError` into one of these
/// instead of unwrapping.
#[derive(Debug, thiserror::Error)]
pub enum InjectError {
    #[error("failed to read desk layout {path:?}: {source}")]
    DeskConfigRead {
        path   : std::path::PathBuf,
        #[source]
        source : std::io::Error,
    },

    #[error("failed to parse desk layout {path:?}: {source}")]
    DeskConfigParse {
        path   : std::path::PathBuf,
        #[source]
        source : toml::de::Error,
    },

    #[error("desk layout {path:?} has no [[outputs]] entries")]
    EmptyLayout {
        path : std::path::PathBuf,
    },

    #[error("no output named {name:?} in the desk layout")]
    UnknownOutput {
        name : String,
    },

    #[error("failed to open /dev/uinput: {0}")]
    UinputOpen(#[source] std::io::Error),

    #[error("failed to configure uinput device: {0}")]
    UinputSetup(#[source] std::io::Error),

    #[error("failed to create uinput device: {0}")]
    UinputCreate(#[source] std::io::Error),

    #[error("failed to write input event: {0}")]
    Emit(#[source] std::io::Error),

    #[error("gaze-capture cursor tracker error: {0}")]
    CursorTracker(#[source] gaze_capture::CaptureError),

    /// The closed-loop relative backend exhausted its correction budget without landing
    /// within a pixel of the target. `got` is the last measured position, `wanted` the
    /// target `move_to` was asked for.
    #[error("relative injector settled at {got:?} instead of {wanted:?} after the maximum number of correction steps")]
    Unreachable {
        got    : GlobalPx,
        wanted : GlobalPx,
    },
}

/// Crate-local result alias; every fallible gaze-inject function returns this.
pub type Result<T> = std::result::Result<T, InjectError>;

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_accumulator_clicks_once_per_120_units_and_carries_the_rest() {
        let mut acc = WheelAccumulator::default();

        // Reading down the page: negative units, a click every 120.
        assert_eq!(acc.push(-50), 0);
        assert_eq!(acc.push(-50), 0);
        assert_eq!(acc.push(-50), -1);
        assert_eq!(acc.remainder, -30);

        // Reversing direction eats the remainder before it clicks the other way.
        assert_eq!(acc.push(100), 0);
        assert_eq!(acc.remainder, 70);
        assert_eq!(acc.push(170), 2);
        assert_eq!(acc.remainder, 0);
    }
}
