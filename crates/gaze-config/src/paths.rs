//! Where the desk's files live.
//!
//! The prototype took every path as a flag, defaulting to a checkout's `config/` and
//! `models/`. The daemon has no flags for them: they are XDG locations, and a checkout
//! is mapped onto them with [`Paths::home`] so a live run from the repo needs no copying.

use std::path::{Path, PathBuf};

/// The application's directory name under each XDG base.
const APP_DIR: &str = "cosmic-gaze";

/// The three roots everything hangs off.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Paths {
    /// The desk file, the calibration and its device blob, the residual model. What a
    /// user sets up once per desk.
    pub config_dir : PathBuf,
    /// The ONNX models. Large, downloaded, never edited.
    pub models_dir : PathBuf,
    /// The online offset and the flywheel: what the daemon writes as it runs.
    pub state_dir  : PathBuf,
}

// --- Paths ---

impl Paths {
    /// The XDG layout: `$XDG_CONFIG_HOME/cosmic-gaze`, `$XDG_DATA_HOME/cosmic-gaze/models`
    /// and `$XDG_STATE_HOME/cosmic-gaze`. Falls back to `~/.config`, `~/.local/share`
    /// and `~/.local/state` when the variables are unset, as `dirs` does.
    pub fn xdg() -> Paths {
        let config = dirs::config_dir().unwrap_or_else(|| PathBuf::from(".config"));
        let data   = dirs::data_dir().unwrap_or_else(|| PathBuf::from(".local/share"));
        let state  = dirs::state_dir().unwrap_or_else(|| PathBuf::from(".local/state"));

        Paths {
            config_dir : config.join(APP_DIR),
            models_dir : data.join(APP_DIR).join("models"),
            state_dir  : state.join(APP_DIR),
        }
    }

    /// A checkout's layout: `DIR/config` for config and state both (the prototype kept
    /// the offset and the flywheel next to the calibration) and `DIR/models`.
    pub fn home(dir: &Path) -> Paths {
        Paths {
            config_dir : dir.join("config"),
            models_dir : dir.join("models"),
            state_dir  : dir.join("config"),
        }
    }

    /// The desk geometry and noise file.
    pub fn desk(&self) -> PathBuf {
        self.config_dir.join("desk.toml")
    }

    /// The ET5 calibration (`gaze-et5-cli calibrate`).
    pub fn calibration(&self) -> PathBuf {
        self.config_dir.join("calibration-et5.toml")
    }

    /// The on-device calibration blob the provider re-declares on every connect.
    pub fn device_blob(&self) -> PathBuf {
        self.config_dir.join("calibration-et5.bin")
    }

    /// The residual model (`gaze-et5-cli fit`).
    pub fn model(&self) -> PathBuf {
        self.config_dir.join("model-et5.json")
    }

    /// Where the online offset persists across runs.
    pub fn offset(&self) -> PathBuf {
        self.state_dir.join("offset-et5.json")
    }

    /// Where the flywheel writes attributed clicks, one file per UTC day.
    pub fn flywheel(&self) -> PathBuf {
        self.state_dir.join("flywheel")
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// The checkout layout is the one the prototype's flag defaults spelled out, so a
    /// `--home .` run reads exactly the files a flagless prototype run did.
    #[test]
    fn a_checkout_maps_onto_the_prototype_defaults() {
        let paths = Paths::home(Path::new("."));

        assert_eq!(paths.desk(),        Path::new("./config/desk.toml"));
        assert_eq!(paths.calibration(), Path::new("./config/calibration-et5.toml"));
        assert_eq!(paths.device_blob(), Path::new("./config/calibration-et5.bin"));
        assert_eq!(paths.model(),       Path::new("./config/model-et5.json"));
        assert_eq!(paths.offset(),      Path::new("./config/offset-et5.json"));
        assert_eq!(paths.flywheel(),    Path::new("./config/flywheel"));
        assert_eq!(paths.models_dir,    Path::new("./models"));
    }

    #[test]
    fn the_xdg_layout_keeps_state_out_of_config() {
        let paths = Paths::xdg();

        assert!(paths.config_dir.ends_with(APP_DIR));
        assert!(paths.models_dir.ends_with("cosmic-gaze/models"));
        assert_ne!(paths.state_dir, paths.config_dir);
    }
}
