//! The tuning knobs: every number the feel still depends on, and where they are kept.
//!
//! These were forty command line flags on the prototype. What survives is what is still
//! being moved after a driven session: the filter stack, the snap and near radii, the
//! commit latency, the pointer look's timing, the edge scroller's bands and speeds, the
//! controller's refine gain and range, and the detector's redetect policy. They are
//! stored the way COSMIC stores settings, one RON file per field under
//! `~/.config/cosmic/dev.techgeek1.CosmicGaze/v1/`, so the applet can write one and the
//! daemon can pick it up without a restart.
//!
//! The defaults here are the prototype's, as tuned through 2026-09-09; each field's doc
//! says what it does and the [`KNOBS`] table says what range a slider should offer.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use cosmic_config::cosmic_config_derive::CosmicConfigEntry;
use cosmic_config::{Config, CosmicConfigEntry};
use serde::{Deserialize, Serialize};
use tracing::{debug, warn};

/// The cosmic-config name the tuning lives under.
pub const CONFIG_ID: &str = "dev.techgeek1.CosmicGaze";

/// The config version, the `v1` in the path. Bumped only on a breaking change to the
/// key set.
pub const CONFIG_VERSION: u64 = 1;

/// Every number the feel depends on. One cosmic-config key per field, named after it.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, CosmicConfigEntry)]
#[version = 1]
pub struct Tuning {
    // --- filter stack ---

    /// I-VT saccade velocity threshold, degrees per second. Above this a sample is in
    /// flight and the fixation ends.
    pub filter_velocity_deg_s : f64,
    /// Window the velocity is estimated over, seconds.
    pub filter_window_s       : f64,
    /// One-euro minimum cutoff, Hz. Lower is smoother and laggier during fixations.
    pub filter_min_cutoff_hz  : f64,
    /// One-euro speed coefficient: how much the cutoff opens up as the eyes move.
    pub filter_beta           : f64,

    // --- snap and commit ---

    /// Snap radius: how far the gaze may be from an element's nearest edge, degrees,
    /// for the engine to target it at all.
    pub snap_deg              : f64,
    /// How close the gaze must be to an element, degrees from its nearest edge, for the
    /// pointer look to show the dot and highlight it.
    pub near_deg              : f64,
    /// Commit channel latency, seconds: a commit is attributed to the target fixated
    /// this long ago, not to whatever is under the gaze now.
    pub commit_latency_s      : f64,
    /// Whether text elements (OCR words, labels) are marked as well as controls.
    pub highlight_text        : bool,

    // --- pointer look ---

    /// How long the dot takes to settle on a new gaze point, seconds.
    pub pointer_settle_s      : f64,
    /// How long the dot stays up after nothing clickable is near, seconds.
    pub pointer_linger_s      : f64,

    // --- edge scrolling ---

    /// Share of the surface's height forming the lower (read-on) band.
    pub edge_band             : f64,
    /// Share of the surface's height forming the upper (go-back) band.
    pub edge_top_band         : f64,
    /// Seconds the gaze must stay in the lower band before scrolling starts.
    pub edge_dwell_s          : f64,
    /// Seconds the gaze must stay in the upper band before scrolling starts.
    pub edge_top_dwell_s      : f64,
    /// Scroll speed at the surface's edge, wheel lines per second.
    pub edge_max_lines_s      : f64,
    /// Seconds for the speed to ramp up from zero when a scroll starts.
    pub edge_ramp_s           : f64,
    /// Exponent on band depth: 1 is linear, 2 slow near the inner edge, fast at the outer.
    pub edge_exponent         : f64,
    /// Seconds the eyes must hold the outer part of the band before the speed grows.
    pub edge_hold_s           : f64,
    /// Speed multiplier gained per second of hold past `edge_hold_s`.
    pub edge_hold_gain        : f64,
    /// Speed multiplier while the tracked eyes are past the edge of the screen itself.
    pub edge_turbo            : f64,

    // --- controller refine ---

    /// Touch refine gain, logical pixels per full pad width.
    pub refine_touch_gain_px  : f64,
    /// Side of the box, centred on where the refine began, that the refined point stays
    /// inside, logical pixels.
    pub refine_range_px       : f64,

    // --- detector ---

    /// Fraction of an output's frame that must change before detection re-runs on it.
    pub redetect_threshold    : f64,
    /// Seconds after which an output is re-detected regardless of how little changed.
    pub redetect_interval_s   : f64,
}

/// What a UI needs to draw one numeric knob: its key, a label, a unit, and the range
/// a slider should offer. The key is the field name and the cosmic-config key.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Knob {
    pub key   : &'static str,
    pub label : &'static str,
    pub unit  : &'static str,
    pub min   : f64,
    pub max   : f64,
    pub step  : f64,
}

/// The numeric knobs, in the order a settings page should list them. `highlight_text`
/// is the one boolean and is not here.
pub const KNOBS: &[Knob] = &[
    Knob { key: "filter_velocity_deg_s" , label: "Saccade velocity"      , unit: "°/s"  , min: 10.0  , max: 150.0 , step: 5.0   },
    Knob { key: "filter_window_s"       , label: "Velocity window"       , unit: "s"    , min: 0.01  , max: 0.2   , step: 0.01  },
    Knob { key: "filter_min_cutoff_hz"  , label: "Smoothing cutoff"      , unit: "Hz"   , min: 0.1   , max: 5.0   , step: 0.1   },
    Knob { key: "filter_beta"           , label: "Smoothing speed gain"  , unit: ""     , min: 0.0   , max: 1.0   , step: 0.01  },
    Knob { key: "snap_deg"              , label: "Snap radius"           , unit: "°"    , min: 0.5   , max: 5.0   , step: 0.1   },
    Knob { key: "near_deg"              , label: "Highlight radius"      , unit: "°"    , min: 0.2   , max: 3.0   , step: 0.1   },
    Knob { key: "commit_latency_s"      , label: "Commit latency"        , unit: "s"    , min: 0.0   , max: 0.5   , step: 0.01  },
    Knob { key: "pointer_settle_s"      , label: "Dot settle"            , unit: "s"    , min: 0.05  , max: 0.6   , step: 0.01  },
    Knob { key: "pointer_linger_s"      , label: "Dot linger"            , unit: "s"    , min: 0.0   , max: 1.0   , step: 0.05  },
    Knob { key: "edge_band"             , label: "Lower band"            , unit: ""     , min: 0.05  , max: 0.4   , step: 0.01  },
    Knob { key: "edge_top_band"         , label: "Upper band"            , unit: ""     , min: 0.05  , max: 0.4   , step: 0.01  },
    Knob { key: "edge_dwell_s"          , label: "Lower dwell"           , unit: "s"    , min: 0.05  , max: 1.0   , step: 0.05  },
    Knob { key: "edge_top_dwell_s"      , label: "Upper dwell"           , unit: "s"    , min: 0.05  , max: 1.5   , step: 0.05  },
    Knob { key: "edge_max_lines_s"      , label: "Scroll speed"          , unit: "l/s"  , min: 1.0   , max: 30.0  , step: 0.5   },
    Knob { key: "edge_ramp_s"           , label: "Scroll ramp"           , unit: "s"    , min: 0.0   , max: 1.0   , step: 0.05  },
    Knob { key: "edge_exponent"         , label: "Band curve"            , unit: ""     , min: 0.5   , max: 3.0   , step: 0.1   },
    Knob { key: "edge_hold_s"           , label: "Hold before speed-up"  , unit: "s"    , min: 0.0   , max: 2.0   , step: 0.1   },
    Knob { key: "edge_hold_gain"        , label: "Speed-up per second"   , unit: "x"    , min: 1.0   , max: 4.0   , step: 0.1   },
    Knob { key: "edge_turbo"            , label: "Off-screen turbo"      , unit: "x"    , min: 1.0   , max: 6.0   , step: 0.5   },
    Knob { key: "refine_touch_gain_px"  , label: "Refine gain"           , unit: "px"   , min: 50.0  , max: 800.0 , step: 10.0  },
    Knob { key: "refine_range_px"       , label: "Refine range"          , unit: "px"   , min: 20.0  , max: 400.0 , step: 10.0  },
    Knob { key: "redetect_threshold"    , label: "Redetect on change"    , unit: ""     , min: 0.005 , max: 0.2   , step: 0.005 },
    Knob { key: "redetect_interval_s"   , label: "Redetect interval"     , unit: "s"    , min: 1.0   , max: 60.0  , step: 1.0   },
];

/// The tuning as stored, watched for changes.
///
/// The watcher fires on its own thread and only sets a flag; the owner polls
/// [`TuningStore::take_changed`] and reloads with [`TuningStore::load`], so a burst of
/// writes (a slider being dragged) costs one reload per poll rather than one per key.
pub struct TuningStore {
    config  : Config,
    changed : Arc<AtomicBool>,
    /// Dropping the watcher stops it, so it lives as long as the store.
    watcher : Option<notify::RecommendedWatcher>,
}

// --- Tuning ---

impl Default for Tuning {
    fn default() -> Self {
        Tuning {
            filter_velocity_deg_s : 30.0,
            filter_window_s       : 0.02,
            filter_min_cutoff_hz  : 1.0,
            filter_beta           : 0.3,
            snap_deg              : 2.0,
            near_deg              : 0.8,
            commit_latency_s      : 0.15,
            highlight_text        : false,
            pointer_settle_s      : 0.2,
            pointer_linger_s      : 0.3,
            edge_band             : 0.12,
            edge_top_band         : 0.12,
            edge_dwell_s          : 0.25,
            edge_top_dwell_s      : 0.5,
            edge_max_lines_s      : 8.0,
            edge_ramp_s           : 0.15,
            edge_exponent         : 1.0,
            edge_hold_s           : 0.3,
            edge_hold_gain        : 2.0,
            edge_turbo            : 3.0,
            refine_touch_gain_px  : 250.0,
            refine_range_px       : 100.0,
            redetect_threshold    : 0.02,
            redetect_interval_s   : 15.0,
        }
    }
}

impl Tuning {
    /// The numeric knob named `key`, or `None` for a name that is not one.
    pub fn get(&self, key: &str) -> Option<f64> {
        let value = match key {
            "filter_velocity_deg_s" => self.filter_velocity_deg_s,
            "filter_window_s"       => self.filter_window_s,
            "filter_min_cutoff_hz"  => self.filter_min_cutoff_hz,
            "filter_beta"           => self.filter_beta,
            "snap_deg"              => self.snap_deg,
            "near_deg"              => self.near_deg,
            "commit_latency_s"      => self.commit_latency_s,
            "pointer_settle_s"      => self.pointer_settle_s,
            "pointer_linger_s"      => self.pointer_linger_s,
            "edge_band"             => self.edge_band,
            "edge_top_band"         => self.edge_top_band,
            "edge_dwell_s"          => self.edge_dwell_s,
            "edge_top_dwell_s"      => self.edge_top_dwell_s,
            "edge_max_lines_s"      => self.edge_max_lines_s,
            "edge_ramp_s"           => self.edge_ramp_s,
            "edge_exponent"         => self.edge_exponent,
            "edge_hold_s"           => self.edge_hold_s,
            "edge_hold_gain"        => self.edge_hold_gain,
            "edge_turbo"            => self.edge_turbo,
            "refine_touch_gain_px"  => self.refine_touch_gain_px,
            "refine_range_px"       => self.refine_range_px,
            "redetect_threshold"    => self.redetect_threshold,
            "redetect_interval_s"   => self.redetect_interval_s,
            _                       => return None,
        };

        Some(value)
    }

    /// Sets the numeric knob named `key`. Returns whether the name was one; the value
    /// is not range checked, since the range is advice for a slider, not a contract.
    pub fn set(&mut self, key: &str, value: f64) -> bool {
        let slot = match key {
            "filter_velocity_deg_s" => &mut self.filter_velocity_deg_s,
            "filter_window_s"       => &mut self.filter_window_s,
            "filter_min_cutoff_hz"  => &mut self.filter_min_cutoff_hz,
            "filter_beta"           => &mut self.filter_beta,
            "snap_deg"              => &mut self.snap_deg,
            "near_deg"              => &mut self.near_deg,
            "commit_latency_s"      => &mut self.commit_latency_s,
            "pointer_settle_s"      => &mut self.pointer_settle_s,
            "pointer_linger_s"      => &mut self.pointer_linger_s,
            "edge_band"             => &mut self.edge_band,
            "edge_top_band"         => &mut self.edge_top_band,
            "edge_dwell_s"          => &mut self.edge_dwell_s,
            "edge_top_dwell_s"      => &mut self.edge_top_dwell_s,
            "edge_max_lines_s"      => &mut self.edge_max_lines_s,
            "edge_ramp_s"           => &mut self.edge_ramp_s,
            "edge_exponent"         => &mut self.edge_exponent,
            "edge_hold_s"           => &mut self.edge_hold_s,
            "edge_hold_gain"        => &mut self.edge_hold_gain,
            "edge_turbo"            => &mut self.edge_turbo,
            "refine_touch_gain_px"  => &mut self.refine_touch_gain_px,
            "refine_range_px"       => &mut self.refine_range_px,
            "redetect_threshold"    => &mut self.redetect_threshold,
            "redetect_interval_s"   => &mut self.redetect_interval_s,
            _                       => return false,
        };

        *slot = value;

        true
    }

    /// Applies one `key=value` override, the form a command line passes. Errors name
    /// the key or the number that was wrong.
    pub fn apply(&mut self, assignment: &str) -> Result<(), String> {
        let Some((key, value)) = assignment.split_once('=') else {
            return Err(format!("expected KEY=VALUE, got {assignment:?}"));
        };

        let key   = key.trim();
        let value = value.trim();

        if key == "highlight_text" {
            self.highlight_text = match value {
                "true" | "1"  => true,
                "false" | "0" => false,
                _             => return Err(format!("{key} wants true or false, got {value:?}")),
            };

            return Ok(());
        }

        let number: f64 = value
            .parse()
            .map_err(|_| format!("{key} wants a number, got {value:?}"))?;

        if !self.set(key, number) {
            return Err(format!("no tuning knob named {key:?}"));
        }

        Ok(())
    }
}

// --- TuningStore ---

impl TuningStore {
    /// Opens the config, creating the directory if it is missing. Fails only when there
    /// is no config directory at all.
    pub fn open() -> Result<TuningStore, cosmic_config::Error> {
        let config  = Config::new(CONFIG_ID, CONFIG_VERSION)?;
        let changed = Arc::new(AtomicBool::new(false));

        let watcher = {
            let flag = Arc::clone(&changed);

            match config.watch(move |_, keys| {
                debug!(keys = ?keys, "tuning changed");
                flag.store(true, Ordering::Relaxed);
            }) {
                Ok(watcher) => Some(watcher),
                Err(e)      => {
                    warn!("tuning config not watchable, changes need a restart: {e}");

                    None
                }
            }
        };

        Ok(TuningStore { config: config, changed: changed, watcher: watcher })
    }

    /// Reads the tuning. A missing key takes its default; an unreadable one is logged
    /// and takes its default too, so a half-written config still yields a usable set.
    pub fn load(&self) -> Tuning {
        match Tuning::get_entry(&self.config) {
            Ok(tuning) => tuning,

            Err((errors, tuning)) => {
                for e in errors {
                    warn!("tuning key unreadable, using its default: {e}");
                }

                tuning
            }
        }
    }

    /// Writes every key. What the applet does after a change, and what a first run does
    /// so there is a file to edit.
    pub fn save(&self, tuning: &Tuning) -> Result<(), cosmic_config::Error> {
        tuning.write_entry(&self.config)
    }

    /// The underlying config, for a caller that writes single keys.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// True once since the last change; the caller reloads in response.
    pub fn take_changed(&self) -> bool {
        self.changed.swap(false, Ordering::Relaxed)
    }

    /// Whether changes are being watched at all.
    pub fn watching(&self) -> bool {
        self.watcher.is_some()
    }
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    /// Every knob in the table is a field `get` and `set` know, and the default sits
    /// inside the range the table advertises, so a slider drawn from it starts on the
    /// default rather than pinned to an end.
    #[test]
    fn every_knob_is_a_field_with_its_default_in_range() {
        let tuning = Tuning::default();

        for knob in KNOBS {
            let value = tuning.get(knob.key).unwrap_or_else(|| panic!("{} is not a field", knob.key));

            assert!(
                value >= knob.min && value <= knob.max,
                "{} default {value} outside {}..{}", knob.key, knob.min, knob.max,
            );

            let mut copy = tuning.clone();

            assert!(copy.set(knob.key, knob.max));
            assert_eq!(copy.get(knob.key), Some(knob.max));
        }

        assert_eq!(tuning.get("no_such_knob"), None);
    }

    /// The command line form: a number for a numeric knob, a boolean for the one
    /// boolean, and a clear error for anything else.
    #[test]
    fn overrides_parse_and_reject() {
        let mut tuning = Tuning::default();

        tuning.apply("edge_dwell_s = 0.4").unwrap();
        tuning.apply("highlight_text=true").unwrap();

        assert_eq!(tuning.edge_dwell_s, 0.4);
        assert!(tuning.highlight_text);

        assert!(tuning.apply("edge_dwell_s=fast").is_err());
        assert!(tuning.apply("nothing=1").is_err());
        assert!(tuning.apply("edge_dwell_s").is_err());
    }
}
