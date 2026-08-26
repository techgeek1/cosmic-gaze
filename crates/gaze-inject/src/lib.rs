//! gaze-inject: pointer warp, click, and scroll injection through `/dev/uinput`. See
//! `PLAN.md`'s `gaze-inject` contract for the shape this crate fills; the portal on this
//! desk has no RemoteDesktop (ScreenCast only, see "Environment facts" in `PLAN.md`), so
//! libei is not an option and uinput is the only path in.
//!
//! Two backends implement the same [`Injector`] API. The choice is explicit, never
//! guessed: `gaze-inject-cli --backend abs|rel`, or [`Injector::create_with`].
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

// The style guide (`(private notes)`) requires explicit field syntax
// (`Foo { x: x }`) everywhere, which clippy's default lints read as redundant. Every
// other crate in this workspace carries the same allow for the same reason.
#![allow(clippy::redundant_field_names)]

use std::path::Path;

use gaze_core::GlobalPx;

mod abs;
mod codes;
pub mod layout;
mod rel;

use abs::AbsoluteInjector;
pub use layout::{DeskLayout, OutputLayout};
use rel::RelativeInjector;

/// Default desk layout path, resolved relative to the current working directory. Matches
/// every other crate's bin: run from the workspace root.
const DEFAULT_DESK_CONFIG: &str = "config/desk.toml";

/// Which mouse button an injected click reports.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
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

    /// Presses and releases `button` at the pointer's current position.
    fn click(&mut self, button: Button) -> Result<()>;

    /// Scrolls by `dy` wheel clicks at the pointer's current position.
    fn scroll(&mut self, dy: i32) -> Result<()>;

    /// Last known measured cursor position, if this backend can observe one. The default
    /// `None` covers the absolute backend and the relative backend's open-loop fallback,
    /// neither of which has a cursor tracker to ask; the closed-loop relative backend
    /// overrides this.
    fn last_known_position(&mut self) -> Result<Option<GlobalPx>> {
        Ok(None)
    }
}

/// Pointer warp, click, and scroll injection through a uinput virtual device. Construct
/// with [`Injector::create`] (default backend, default desk layout path) or
/// [`Injector::create_with`] (explicit backend and layout - what `gaze-inject-cli` always
/// uses).
pub struct Injector {
    backend : Box<dyn InjectBackend>,
}

// --- Injector ---

impl Injector {
    /// Creates an injector with the default backend ([`Backend::Relative`]) against
    /// `config/desk.toml` resolved relative to the current directory. Prefer
    /// [`Injector::create_with`] whenever the backend or layout path matters.
    pub fn create() -> Result<Injector> {
        let layout = DeskLayout::load(Path::new(DEFAULT_DESK_CONFIG))?;

        Injector::create_with(Backend::default(), &layout)
    }

    /// Creates an injector with an explicit backend and desk layout.
    pub fn create_with(backend: Backend, layout: &DeskLayout) -> Result<Injector> {
        let inner: Box<dyn InjectBackend> = match backend {
            Backend::Absolute => Box::new(AbsoluteInjector::create(layout)?),
            Backend::Relative => Box::new(RelativeInjector::create(layout)?),
        };

        Ok(Injector { backend: inner })
    }

    /// Moves the pointer to `p` (global logical px) without clicking. For the closed-loop
    /// relative backend this converges iteratively against a measured position and can
    /// return `InjectError::Unreachable` if it doesn't settle in time; every other backend
    /// always succeeds (or fails only on an I/O error).
    pub fn move_to(&mut self, p: GlobalPx) -> Result<()> {
        self.backend.move_to(p)
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

    /// Last known measured cursor position, if the active backend can observe one (the
    /// closed-loop relative backend can; the open-loop fallback and the absolute backend
    /// cannot and return `Ok(None)`). Useful for confirming where a `move_to` actually
    /// landed without eyeballing it, e.g. `gaze-inject-cli --probe`.
    pub fn last_known_position(&mut self) -> Result<Option<GlobalPx>> {
        self.backend.last_known_position()
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
