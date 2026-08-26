//! Desk output layout: reads the `[[outputs]]` table from a `desk.toml`-shaped file for
//! the logical rects gaze-inject needs - the absolute backend's `ABS_X`/`ABS_Y` scaling
//! range, the relative backend's homing corner, and the per-output centres
//! `gaze-inject-cli --probe` walks. Deliberately reads only this slice of
//! `config/desk.toml`; the rest (`eye_mm`, `yaw_deg`, `[noise]`, ...) belongs to
//! `gaze_core::DesktopGeometry` and gaze-inject has no use for it.

use std::fs;
use std::path::Path;

use gaze_core::{GlobalPx, Rect};
use serde::Deserialize;

use crate::{InjectError, Result};

/// One output's logical rectangle, the slice of `config/desk.toml`'s `[[outputs]]`
/// entries gaze-inject reads. Other per-output fields (`physical_w_mm`, `radius_mm`,
/// `position_mm`, `yaw_deg`, ...) are present in the file but ignored here; `toml`
/// doesn't error on the extra fields since this struct isn't `deny_unknown_fields`.
#[derive(Clone, Debug, Deserialize)]
pub struct OutputLayout {
    /// Connector name as reported by `wl_output` (`"DP-1"`, `"HDMI-A-1"`).
    pub name      : String,
    pub logical_x : f64,
    pub logical_y : f64,
    pub logical_w : f64,
    pub logical_h : f64,
}

/// Shape of the file `DeskLayout::load` reads; only the `[[outputs]]` table matters here.
#[derive(Deserialize)]
struct DeskConfig {
    outputs : Vec<OutputLayout>,
}

/// Every configured output's logical rect, loaded from a `desk.toml`-shaped file.
#[derive(Clone, Debug)]
pub struct DeskLayout {
    pub outputs : Vec<OutputLayout>,
}

// --- OutputLayout ---

impl OutputLayout {
    /// This output's logical rect as a `gaze_core::Rect`, in global px.
    pub fn rect(&self) -> Rect {
        Rect { x: self.logical_x, y: self.logical_y, w: self.logical_w, h: self.logical_h }
    }

    /// Centre of this output's logical rect, in global px.
    pub fn center(&self) -> GlobalPx {
        self.rect().center()
    }
}

// --- DeskLayout ---

impl DeskLayout {
    /// Loads the layout from a `desk.toml`-shaped file at `path`. Fails clearly (no
    /// panic) if the file is missing, isn't valid TOML, or has no `[[outputs]]` entries.
    pub fn load(path: &Path) -> Result<DeskLayout> {
        let text = fs::read_to_string(path).map_err(|source| {
            InjectError::DeskConfigRead { path: path.to_path_buf(), source: source }
        })?;

        let config: DeskConfig = toml::from_str(&text).map_err(|source| {
            InjectError::DeskConfigParse { path: path.to_path_buf(), source: source }
        })?;

        if config.outputs.is_empty() {
            return Err(InjectError::EmptyLayout { path: path.to_path_buf() });
        }

        Ok(DeskLayout { outputs: config.outputs })
    }

    /// Union bounding box of every output's logical rect, in global px:
    /// `(min_x, min_y, max_x, max_y)`. This is the coordinate space the absolute
    /// backend's `ABS_X`/`ABS_Y` axes are scaled onto, and the space the relative
    /// backend's homing move targets the corner of. Outputs are not required to be
    /// contiguous; the box is just the min/max extent, gaps included.
    pub fn bounds(&self) -> (f64, f64, f64, f64) {
        let min_x = self.outputs.iter().map(|o| o.logical_x).fold(f64::INFINITY, f64::min);
        let min_y = self.outputs.iter().map(|o| o.logical_y).fold(f64::INFINITY, f64::min);
        let max_x = self.outputs.iter()
            .map(|o| o.logical_x + o.logical_w)
            .fold(f64::NEG_INFINITY, f64::max);
        let max_y = self.outputs.iter()
            .map(|o| o.logical_y + o.logical_h)
            .fold(f64::NEG_INFINITY, f64::max);

        (min_x, min_y, max_x, max_y)
    }

    /// Looks up an output by connector name (`"DP-2"`, `"HDMI-A-1"`, ...), erroring
    /// clearly rather than panicking if the layout doesn't have it.
    pub fn get(&self, name: &str) -> Result<&OutputLayout> {
        self.outputs.iter().find(|o| o.name == name)
            .ok_or_else(|| InjectError::UnknownOutput { name: name.to_string() })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Path to the real desk layout every other crate's bin also defaults to. Reading it
    /// in a test means a `config/desk.toml` edit that changes the output layout (as
    /// happened once already: HDMI-A-1's logical size moved from 960x600 to 1670x1043
    /// under fractional scaling) breaks this test instead of silently drifting from the
    /// scaling assumptions baked into `abs.rs`'s tests.
    fn desk_toml_path() -> &'static Path {
        Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/../../config/desk.toml"))
    }

    #[test]
    fn loads_and_bounds_desk_toml() {
        let layout = DeskLayout::load(desk_toml_path()).expect("config/desk.toml should parse");

        assert_eq!(layout.outputs.len(), 3);
        assert_eq!(layout.bounds(), (0.0, 0.0, 6399.0, 2643.0));
    }

    #[test]
    fn get_finds_configured_output_by_name() {
        let layout = DeskLayout::load(desk_toml_path()).expect("config/desk.toml should parse");

        let dp2 = layout.get("DP-2").expect("DP-2 should be configured");
        assert_eq!(dp2.center(), GlobalPx { x: 1280.0, y: 880.0 });
    }

    #[test]
    fn get_errors_clearly_for_unknown_output() {
        let layout = DeskLayout::load(desk_toml_path()).expect("config/desk.toml should parse");

        assert!(matches!(layout.get("DP-99"), Err(InjectError::UnknownOutput { .. })));
    }
}
