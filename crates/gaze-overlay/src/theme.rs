//! The colours and radius the pointer look borrows from the desktop theme.
//!
//! COSMIC keeps its theme as one RON file per key under
//! `~/.config/cosmic/com.system76.CosmicTheme.{Dark,Light}/v1/`, with
//! `com.system76.CosmicTheme.Mode/v1/is_dark` choosing between the two, and system
//! defaults under the XDG data dirs. `cosmic-config` resolves that chain and watches it;
//! this module reads the two keys the overlay needs and turns them into 8-bit colours.
//! It deliberately deserialises only those keys with its own structs rather than pulling
//! `cosmic-theme`: that crate drags `palette` and iced's futures in for a colour and a
//! number.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cosmic_config::{Config, ConfigGet};
use serde::Deserialize;
use tracing::{debug, warn};

/// Config name holding `is_dark`.
const MODE_CONFIG: &str = "com.system76.CosmicTheme.Mode";

/// Config name of the dark theme.
const DARK_CONFIG: &str = "com.system76.CosmicTheme.Dark";

/// Config name of the light theme.
const LIGHT_CONFIG: &str = "com.system76.CosmicTheme.Light";

/// Theme config version, the `v1` in the path.
const THEME_VERSION: u64 = 1;

/// COSMIC's default dark accent, `accent_blue` in its dark palette. Used when the theme
/// cannot be read at all, so the overlay looks native on a stock install even then.
const DEFAULT_DARK_ACCENT: [u8; 4] = [0x63, 0xd0, 0xdf, 255];

/// COSMIC's default light accent.
const DEFAULT_LIGHT_ACCENT: [u8; 4] = [0x00, 0x52, 0x5a, 255];

/// COSMIC's `radius_s`, the radius of its buttons.
const DEFAULT_RADIUS_PX: f64 = 8.0;

/// Alpha of the halo colour. Opaque enough to separate the accent from a same-coloured
/// background, thin enough not to read as a second outline.
const HALO_ALPHA: u8 = 200;

/// What the pointer look is drawn in.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Theme {
    /// The accent colour, opaque. The dot, the trail and the highlight stroke.
    pub accent    : [u8; 4],
    /// Black or white, whichever the accent is further from. Outlines the dot and the
    /// highlight so a light accent survives a white page and a dark one a dark window.
    pub halo      : [u8; 4],
    /// Corner radius of the highlight, logical pixels: the theme's small radius, the
    /// one its buttons use, so the highlight looks like the widget it surrounds.
    pub radius_px : f64,
}

/// Keeps the theme's config watchers alive and records that something changed.
///
/// The watchers fire on their own thread; the overlay's event loop polls
/// [`ThemeWatch::take_changed`] and reloads when it is set, so no theme data crosses a
/// thread and a burst of writes (cosmic-settings writes several keys per change) costs
/// one reload.
pub struct ThemeWatch {
    /// Set by any watcher; cleared by the poll.
    changed  : Arc<AtomicBool>,
    /// Dropping a watcher stops it, so they live as long as the overlay does.
    watchers : Vec<notify::RecommendedWatcher>,
}

// --- Theme ---

impl Theme {
    /// The stock COSMIC dark look, for when there is no theme to read.
    pub fn fallback() -> Theme {
        Theme::from_parts(DEFAULT_DARK_ACCENT, DEFAULT_RADIUS_PX)
    }

    /// Reads the active theme. Any key that cannot be read falls back to the stock
    /// value for that key alone, with a warning, so a half-written theme still yields
    /// a usable look rather than an error.
    pub fn cosmic() -> Theme {
        let dark = match Config::new(MODE_CONFIG, THEME_VERSION) {
            Ok(mode) => mode.get::<bool>("is_dark").unwrap_or_else(|e| {
                debug!("theme mode unreadable, assuming dark: {e}");

                true
            }),
            Err(e)   => {
                warn!("cosmic theme mode config unavailable, assuming dark: {e}");

                true
            }
        };

        let default_accent = if dark { DEFAULT_DARK_ACCENT } else { DEFAULT_LIGHT_ACCENT };

        let theme = match Config::new(if dark { DARK_CONFIG } else { LIGHT_CONFIG }, THEME_VERSION) {
            Ok(theme) => theme,
            Err(e)    => {
                warn!("cosmic theme config unavailable, using the stock look: {e}");

                return Theme::from_parts(default_accent, DEFAULT_RADIUS_PX);
            }
        };

        let accent = match theme.get::<Accent>("accent") {
            Ok(accent) => accent.base.to_rgba8(),
            Err(e)     => {
                warn!("cosmic accent unreadable, using the stock colour: {e}");

                default_accent
            }
        };

        let radius = match theme.get::<CornerRadii>("corner_radii") {
            Ok(radii) => f64::from(radii.radius_s.0),
            Err(e)    => {
                warn!("cosmic corner radii unreadable, using the stock radius: {e}");

                DEFAULT_RADIUS_PX
            }
        };

        let theme = Theme::from_parts(accent, radius);

        debug!(
            accent    = ?theme.accent,
            radius_px = theme.radius_px,
            dark      = dark,
            "theme loaded",
        );

        theme
    }

    /// Builds a theme from an opaque accent and a radius, deriving the halo.
    pub fn from_parts(accent: [u8; 4], radius_px: f64) -> Theme {
        Theme {
            accent    : [accent[0], accent[1], accent[2], 255],
            halo      : halo_for(accent),
            radius_px : radius_px.max(0.0),
        }
    }

    /// The accent at a given alpha, straight (not premultiplied).
    pub fn accent_at(&self, alpha: f32) -> [u8; 4] {
        with_alpha(self.accent, alpha)
    }

    /// The halo at a given alpha, scaled from its own base alpha.
    pub fn halo_at(&self, alpha: f32) -> [u8; 4] {
        with_alpha(self.halo, alpha * f32::from(HALO_ALPHA) / 255.0)
    }
}

// --- ThemeWatch ---

impl ThemeWatch {
    /// Watches the mode and both theme configs. Returns `None` when none of them can be
    /// watched, which is the no-COSMIC case; a subset failing is logged and the rest are
    /// kept.
    pub fn start() -> Option<ThemeWatch> {
        let changed      = Arc::new(AtomicBool::new(false));
        let mut watchers = Vec::new();

        for name in [MODE_CONFIG, DARK_CONFIG, LIGHT_CONFIG] {
            let config = match Config::new(name, THEME_VERSION) {
                Ok(config) => config,
                Err(e)     => {
                    debug!(config = name, "theme config not watchable: {e}");

                    continue;
                }
            };

            let flag = Arc::clone(&changed);

            match config.watch(move |_, keys| {
                debug!(keys = ?keys, "theme changed");
                flag.store(true, Ordering::Relaxed);
            }) {
                Ok(watcher) => watchers.push(watcher),
                Err(e)      => debug!(config = name, "theme watch failed: {e}"),
            }
        }

        if watchers.is_empty() {
            return None;
        }

        Some(ThemeWatch { changed: changed, watchers: watchers })
    }

    /// True once since the last change; the caller reloads the theme in response.
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }

    /// How many configs are being watched.
    pub fn watching(&self) -> usize {
        self.watchers.len()
    }
}

// --- Internals ---

/// The `accent` key. Only `base` is read; the rest of the accent's derived colours are
/// for widgets that have hover and pressed states.
#[derive(Debug, Deserialize)]
struct Accent {
    base : Srgba,
}

/// The `corner_radii` key. Only the small radius is read.
#[derive(Debug, Deserialize)]
struct CornerRadii {
    radius_s : (f32, f32, f32, f32),
}

/// A colour as cosmic-theme serialises `palette::Srgba`: unit floats. Negative zero
/// shows up in real files (`red: -0.0`), so the conversion clamps.
#[derive(Debug, Deserialize)]
struct Srgba {
    red   : f32,
    green : f32,
    blue  : f32,
    alpha : f32,
}

impl Srgba {
    /// Straight 8-bit RGBA.
    fn to_rgba8(&self) -> [u8; 4] {
        let q = |v: f32| (v.clamp(0.0, 1.0) * 255.0).round() as u8;

        [q(self.red), q(self.green), q(self.blue), q(self.alpha)]
    }
}

/// Black for a light accent, white for a dark one. Relative luminance, gamma ignored:
/// the decision is a coin flip near the middle either way and the ends are clear.
fn halo_for(accent: [u8; 4]) -> [u8; 4] {
    let luma = 0.2126 * f32::from(accent[0])
        + 0.7152 * f32::from(accent[1])
        + 0.0722 * f32::from(accent[2]);

    if luma > 127.5 {
        [0, 0, 0, HALO_ALPHA]
    }
    else {
        [255, 255, 255, HALO_ALPHA]
    }
}

/// Replaces a colour's alpha with `alpha` in [0, 1].
fn with_alpha(c: [u8; 4], alpha: f32) -> [u8; 4] {
    [c[0], c[1], c[2], (alpha.clamp(0.0, 1.0) * 255.0).round() as u8]
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The green on this desk is light and gets a dark halo; the stock light-mode teal
    /// is dark and gets a white one.
    #[test]
    fn halo_opposes_the_accent() {
        assert_eq!(halo_for([146, 207, 156, 255])[0], 0);
        assert_eq!(halo_for(DEFAULT_LIGHT_ACCENT)[0], 255);
    }

    /// The RON cosmic-settings writes, trimmed to the keys read here plus one it also
    /// writes, parses; the negative zero in real files does not trip the conversion.
    #[test]
    fn accent_file_parses() {
        let text = "(base: (red: 0.57254905, green: 0.8117647, blue: 0.6117647, alpha: 1.0), \
                    on: (red: -0.0, green: 0.0, blue: 0.0, alpha: 1.0))";

        let accent: Accent = ron::from_str(text).unwrap();

        assert_eq!(accent.base.to_rgba8(), [146, 207, 156, 255]);

        let radii: CornerRadii = ron::from_str(
            "(radius_0: (0.0, 0.0, 0.0, 0.0), radius_s: (8.0, 8.0, 8.0, 8.0))",
        )
        .unwrap();

        assert_eq!(radii.radius_s.0, 8.0);
    }

    /// Alpha scaling keeps the colour and clamps the alpha.
    #[test]
    fn alpha_is_replaced_not_multiplied() {
        let theme = Theme::from_parts([10, 20, 30, 255], 8.0);

        assert_eq!(theme.accent_at(0.5), [10, 20, 30, 128]);
        assert_eq!(theme.accent_at(2.0), [10, 20, 30, 255]);
        assert_eq!(theme.halo_at(1.0)[3], HALO_ALPHA);
    }
}
