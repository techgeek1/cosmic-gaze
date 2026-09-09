//! The online offset (PLAN-ET5 D5): the day's bias, learnt from the clicks the user is
//! already making, and keyed to where the head is.
//!
//! The residual model is fitted across sessions and cannot know today's seating, glasses
//! position or mount creep. Leave-one-session-out on the 2026-09-04 data put that
//! per-session bias at 0.2 to 0.3 degrees of median error: 1.44 with the model alone,
//! 1.22 with the held-out session's own median offset added (the oracle). A causal
//! simulation of this filter over the same clicks, in time order, reached 1.20, so the
//! oracle is attainable from the clicks themselves within about twenty of them
//! (`model/loso_clicks.py`, `online`).
//!
//! A single bias was not enough on the desk (2026-09-05): it held in the calibration
//! posture and broke on a comfortable slouch, and relearning it on every posture change
//! costs the twenty clicks each time. So the bias is a function of the eye position: a
//! small set of *anchors* in tracker space, each holding the bias measured near it, blended
//! by a Gaussian in the distance from the eyes to each anchor (half of
//! [`OffsetParams::reach_mm`] as sigma).
//! A click near an anchor updates that anchor; a click farther than the reach from every
//! anchor starts a new one at the blended prediction, so a new posture inherits what is
//! known and then corrects it with its first few clicks (a fresh anchor takes its early
//! clicks at `1/(n+1)` over a small prior count: half the gap closed in four clicks, most
//! of it in ten, without any one label owning it). Far from every anchor the prediction
//! relaxes toward the global mean, which is the old single bias.
//!
//! Each update is deliberately dumb: an exponentially weighted mean of the leftover
//! residual at each accepted click, in the label's own yaw/pitch frame, with the
//! innovation clipped so one bad label cannot yank it and gated so a click the eye was
//! not on cannot enter at all. Ungated, the same simulation gave 1.36.
//!
//! The state persists to a small JSON file so a restart does not start cold; the bias
//! it tracks is a property of the seat and the day, not of the process.

use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

/// Where the offset state lives when no path is given.
pub const DEFAULT_OFFSET_PATH: &str = "config/offset-et5.json";

/// Fraction of each accepted innovation folded into an anchor once it is established.
/// At 0.1 the filter forgets over roughly ten clicks and converged within twenty in
/// simulation; 0.05 and 0.2 were within 0.04 degrees of it, so the choice is not sharp.
pub const DEFAULT_ALPHA: f64 = 0.1;

/// Magnitude an innovation is clipped to before it enters the filter, degrees per
/// axis. Two degrees is above the median leftover and below the gate, so a normal
/// click enters as is and a borderline one enters as a nudge.
pub const DEFAULT_CLIP_DEG: f64 = 2.0;

/// Leftover magnitude past which a click is not believed to be a gaze label at all,
/// degrees. Three degrees is the foveal tolerance PLAN-ET5's E2 starts at.
pub const DEFAULT_GATE_DEG: f64 = 3.0;

/// Distance past which a click starts a new anchor, millimetres of binocular midpoint;
/// the blend over anchors uses half of it as its sigma, so two anchors a reach apart
/// barely see each other. The head wobbles ten to twenty millimetres within a posture
/// and moves eighty or more between sitting up and slouching. Forty spawned a second
/// anchor without the user moving on the desk (2026-09-05), so sixty.
pub const DEFAULT_REACH_MM: f64 = 60.0;

/// Clicks a new anchor is treated as already holding when it is founded, so its first
/// real click enters at `1/(PRIOR_CLICKS + 2)` rather than in full: one label scatters
/// by about a degree, and an anchor that *is* one label misplaces the whole posture
/// until the next few clicks dilute it.
pub const PRIOR_CLICKS: u64 = 2;

/// Most anchors kept. Past this the least-updated one is dropped for a new posture.
pub const MAX_ANCHORS: usize = 24;

/// Weight the global mean carries in the blend, against anchor weights that peak at one.
/// It only matters far from every anchor, where it is what the prediction relaxes to;
/// on an anchor it moves the prediction by under a hundredth of the bias.
const FAR_WEIGHT: f64 = 0.01;

/// The filter's tunables.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct OffsetParams {
    /// Innovation gain, see [`DEFAULT_ALPHA`].
    pub alpha    : f64,
    /// Per-axis innovation clip, degrees, see [`DEFAULT_CLIP_DEG`].
    pub clip_deg : f64,
    /// Acceptance gate on the leftover magnitude, degrees, see [`DEFAULT_GATE_DEG`].
    pub gate_deg : f64,
    /// Anchor spawn distance (and twice the blend sigma), mm, see [`DEFAULT_REACH_MM`].
    pub reach_mm : f64,
}

impl Default for OffsetParams {
    fn default() -> Self {
        Self {
            alpha    : DEFAULT_ALPHA,
            clip_deg : DEFAULT_CLIP_DEG,
            gate_deg : DEFAULT_GATE_DEG,
            reach_mm : DEFAULT_REACH_MM,
        }
    }
}

/// The bias measured around one eye position.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct Anchor {
    /// Mean eye origin the anchor stands at, tracker space, mm.
    pub origin_mm : [f64; 3],
    /// Yaw of the bias, degrees, in the residual label's frame: added to the model's
    /// prediction before the ray is corrected.
    pub yaw_deg   : f64,
    /// Pitch of the bias, degrees, same frame.
    pub pitch_deg : f64,
    /// Accepted clicks folded into this anchor.
    pub updates   : u64,
}

/// The persisted state: the anchors and enough provenance to refuse a stale file.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct OffsetState {
    /// The postures seen so far and the bias at each.
    pub anchors            : Vec<Anchor>,
    /// Accepted clicks folded in over the life of the state.
    pub updates            : u64,
    /// Unix time of the last accepted click.
    pub updated_unix_s     : f64,
    /// Body hash of the on-device model the offset was measured under. The offset is
    /// what is left after the firmware and the residual model, so a retrained device
    /// orphans it exactly as it orphans the model.
    pub device_blob_sha256 : Option<String>,
}

/// What became of one click offered to the filter.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum ClickFeedback {
    /// Folded in. Carries the leftover the click showed, the offset now predicted at
    /// that eye position, and how many anchors there are (a new one means a new posture).
    Accepted {
        leftover_deg : [f64; 2],
        offset_deg   : [f64; 2],
        anchors      : usize,
    },
    /// Past the gate: the eye was somewhere else, or the label is wrong.
    Rejected {
        leftover_deg : [f64; 2],
    },
}

/// The running offset with its parameters and its file.
#[derive(Debug)]
pub struct OnlineOffset {
    params : OffsetParams,
    state  : OffsetState,
    /// Where accepted updates are written, `None` to keep the state in memory only.
    path   : Option<PathBuf>,
}

// --- OnlineOffset ---

impl OnlineOffset {
    /// A cold filter for the given device blob.
    pub fn new(params: OffsetParams, blob_sha256: Option<String>) -> Self {
        Self {
            params : params,
            state  : OffsetState {
                anchors            : Vec::new(),
                updates            : 0,
                updated_unix_s     : 0.0,
                device_blob_sha256 : blob_sha256,
            },
            path   : None,
        }
    }

    /// A filter that reads its state from `path` when the file exists and was written
    /// under `blob_sha256`, and writes every accepted update back to it. A missing,
    /// mismatched or older-format file starts cold, with a log line saying so.
    pub fn persisted(params: OffsetParams, path: impl Into<PathBuf>, blob_sha256: Option<String>)
        -> Self
    {
        let path = path.into();
        let mut offset = Self::new(params, blob_sha256.clone());

        match OffsetState::load(&path) {
            Ok(Some(state)) if state.device_blob_sha256 == blob_sha256 => {
                info!(anchors = state.anchors.len(), updates = state.updates,
                      path = %path.display(), "online offset restored");

                offset.state = state;
            }
            Ok(Some(state)) => {
                warn!(file = ?state.device_blob_sha256, device = ?blob_sha256,
                      "online offset file ignored: written under a different device blob");
            }
            Ok(None) => {
                info!(path = %path.display(), "online offset starts cold");
            }
            Err(e) => {
                warn!(path = %path.display(), "online offset file unreadable ({e}); starting cold");
            }
        }

        offset.path = Some(path);
        offset
    }

    /// The bias to add to the model's prediction for eyes at `origin_mm`, yaw and pitch
    /// degrees: the anchors blended by distance, relaxing to their mean far from all.
    pub fn offset_deg(&self, origin_mm: [f64; 3]) -> [f64; 2] {
        if self.state.anchors.is_empty() {
            return [0.0, 0.0];
        }

        let global = self.global_deg();
        let sigma  = 0.5 * self.params.reach_mm;
        let two_s2 = 2.0 * sigma * sigma;

        let mut sum_w     = FAR_WEIGHT;
        let mut sum_yaw   = FAR_WEIGHT * global[0];
        let mut sum_pitch = FAR_WEIGHT * global[1];

        for a in &self.state.anchors {
            let w = (-dist2(a.origin_mm, origin_mm) / two_s2).exp();

            sum_w     += w;
            sum_yaw   += w * a.yaw_deg;
            sum_pitch += w * a.pitch_deg;
        }

        [sum_yaw / sum_w, sum_pitch / sum_w]
    }

    /// The single bias the anchors average to, weighted by how much each has seen.
    pub fn global_deg(&self) -> [f64; 2] {
        let mut n     = 0.0;
        let mut yaw   = 0.0;
        let mut pitch = 0.0;

        for a in &self.state.anchors {
            let w = a.updates.max(1) as f64;

            n     += w;
            yaw   += w * a.yaw_deg;
            pitch += w * a.pitch_deg;
        }

        if n == 0.0 { [0.0, 0.0] } else { [yaw / n, pitch / n] }
    }

    /// The state as it would be written.
    pub fn state(&self) -> &OffsetState {
        &self.state
    }

    /// The parameters in force.
    pub fn params(&self) -> OffsetParams {
        self.params
    }

    /// Offers the leftover residual of one click: where the corrected ray still was
    /// relative to the clicked point, yaw and pitch degrees in the label frame, with
    /// the eyes at `origin_mm`. The leftover is measured *after* the current offset, so
    /// it is the innovation at that eye position.
    pub fn observe(&mut self, leftover_deg: [f64; 2], origin_mm: [f64; 3]) -> ClickFeedback {
        let [yaw, pitch] = leftover_deg;

        let finite = yaw.is_finite() && pitch.is_finite() && origin_mm.iter().all(|v| v.is_finite());

        if !finite || yaw.hypot(pitch) >= self.params.gate_deg {
            return ClickFeedback::Rejected { leftover_deg: leftover_deg };
        }

        let clip  = self.params.clip_deg;
        let yaw   = yaw.clamp(-clip, clip);
        let pitch = pitch.clamp(-clip, clip);
        let reach = self.params.reach_mm;

        let nearest = self.state.anchors.iter()
            .enumerate()
            .map(|(i, a)| (i, dist2(a.origin_mm, origin_mm)))
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .filter(|(_, d2)| *d2 <= reach * reach)
            .map(|(i, _)| i);

        match nearest {
            Some(i) => {
                // The innovation is against the blend, not this anchor alone, so it is
                // exactly what moves the blend onto the click.
                let a     = &mut self.state.anchors[i];
                let alpha = self.params.alpha.max(1.0 / (a.updates as f64 + 1.0));

                a.yaw_deg   += alpha * yaw;
                a.pitch_deg += alpha * pitch;
                a.updates   += 1;
            }

            None => {
                // A new posture: start where the blend already predicts, nudged by this
                // click as one of `PRIOR_CLICKS + 1` rather than the whole story.
                let [pred_yaw, pred_pitch] = self.offset_deg(origin_mm);
                let alpha                  = 1.0 / (PRIOR_CLICKS as f64 + 1.0);

                if self.state.anchors.len() >= MAX_ANCHORS
                    && let Some(least) = self.state.anchors.iter()
                        .enumerate()
                        .min_by_key(|(_, a)| a.updates)
                        .map(|(i, _)| i)
                {
                    self.state.anchors.swap_remove(least);
                }

                self.state.anchors.push(Anchor {
                    origin_mm : origin_mm,
                    yaw_deg   : pred_yaw + alpha * yaw,
                    pitch_deg : pred_pitch + alpha * pitch,
                    updates   : PRIOR_CLICKS + 1,
                });
            }
        }

        self.state.updates        += 1;
        self.state.updated_unix_s  = unix_now_s();

        if let Some(path) = &self.path
            && let Err(e) = self.state.save(path)
        {
            warn!(path = %path.display(), "online offset not saved ({e})");
        }

        ClickFeedback::Accepted {
            leftover_deg : leftover_deg,
            offset_deg   : self.offset_deg(origin_mm),
            anchors      : self.state.anchors.len(),
        }
    }

    /// Forgets every posture. For an explicit user reset; nothing in the provider calls it.
    pub fn reset(&mut self) {
        self.state.anchors.clear();
        self.state.updates = 0;
    }
}

/// Squared distance between two eye positions, mm².
fn dist2(a: [f64; 3], b: [f64; 3]) -> f64 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

// --- OffsetState ---

impl OffsetState {
    /// Reads a state file. `Ok(None)` when there is no file.
    pub fn load(path: &Path) -> Result<Option<Self>, OffsetError> {
        let text = {
            match std::fs::read_to_string(path) {
                Ok(text)                                            => text,
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                Err(e) => return Err(OffsetError::Io(path.display().to_string(), e.to_string())),
            }
        };

        serde_json::from_str(&text)
            .map(Some)
            .map_err(|e| OffsetError::Parse(path.display().to_string(), e.to_string()))
    }

    /// Writes the state, creating the parent directory if needed.
    pub fn save(&self, path: &Path) -> Result<(), OffsetError> {
        if let Some(dir) = path.parent()
            && !dir.as_os_str().is_empty()
        {
            std::fs::create_dir_all(dir)
                .map_err(|e| OffsetError::Io(dir.display().to_string(), e.to_string()))?;
        }

        let text = serde_json::to_string_pretty(self)
            .map_err(|e| OffsetError::Parse(path.display().to_string(), e.to_string()))?;

        std::fs::write(path, text)
            .map_err(|e| OffsetError::Io(path.display().to_string(), e.to_string()))
    }
}

/// Seconds since the Unix epoch at millisecond resolution, zero if the clock is before
/// it. Rounded because `serde_json` does not parse floats to the exact bits it printed
/// without its `float_roundtrip` feature, and a state that changes by reading it back
/// is a state whose equality nothing can rely on.
fn unix_now_s() -> f64 {
    SystemTime::now().duration_since(UNIX_EPOCH)
        .map(|d| (d.as_secs_f64() * 1000.0).round() / 1000.0)
        .unwrap_or(0.0)
}

// --- Error ---

/// Offset file failure.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum OffsetError {
    #[error("{0}: {1}")]
    Io(String, String),
    #[error("{0}: {1}")]
    Parse(String, String),
}

// --- Tests ---

#[cfg(test)]
mod tests {
    use super::*;

    const SEAT: [f64; 3] = [0.0, 0.0, 650.0];

    /// Offers a click whose true bias is `bias` at `origin`, as the provider would: the
    /// leftover is the bias minus the current prediction there.
    fn click(offset: &mut OnlineOffset, origin: [f64; 3], bias: [f64; 2]) -> ClickFeedback {
        let [y, p] = offset.offset_deg(origin);

        offset.observe([bias[0] - y, bias[1] - p], origin)
    }

    #[test]
    fn a_constant_bias_is_learnt_to_within_the_gain() {
        let mut offset = OnlineOffset::new(OffsetParams::default(), None);

        for _ in 0..60 {
            click(&mut offset, SEAT, [1.0, -0.5]);
        }

        let [y, p] = offset.offset_deg(SEAT);

        assert!((y - 1.0).abs() < 0.01, "yaw {y}");
        assert!((p + 0.5).abs() < 0.01, "pitch {p}");
        assert_eq!(offset.state().updates, 60);
        assert_eq!(offset.state().anchors.len(), 1);
    }

    #[test]
    fn the_gate_refuses_and_the_clip_bounds() {
        let mut offset = OnlineOffset::new(OffsetParams::default(), None);

        assert!(matches!(offset.observe([3.0, 0.0], SEAT), ClickFeedback::Rejected { .. }));
        assert!(matches!(offset.observe([f64::NAN, 0.0], SEAT), ClickFeedback::Rejected { .. }));
        assert!(matches!(offset.observe([1.0, 0.0], [f64::NAN, 0.0, 0.0]), ClickFeedback::Rejected { .. }));
        assert_eq!(offset.offset_deg(SEAT), [0.0, 0.0]);

        // Inside the gate but past the clip on one axis: a first click enters as the
        // clip over the prior count.
        let fed = offset.observe([2.9, 0.0], SEAT);

        assert!(matches!(fed, ClickFeedback::Accepted { anchors: 1, .. }));
        assert!((offset.offset_deg(SEAT)[0] - DEFAULT_CLIP_DEG / (PRIOR_CLICKS as f64 + 1.0)).abs() < 1e-9);
    }

    #[test]
    fn a_new_posture_gets_its_own_bias_and_the_old_one_keeps_its() {
        let mut offset = OnlineOffset::new(OffsetParams::default(), None);
        let slouch     = [0.0, -120.0, 750.0];

        for _ in 0..30 {
            click(&mut offset, SEAT, [1.0, 0.0]);
        }

        // The slouch inherits the seat's bias, then takes its own in a few clicks.
        let inherited = offset.offset_deg(slouch);

        assert!((inherited[0] - 1.0).abs() < 0.05, "inherited yaw {}", inherited[0]);

        // Four clicks close most of the gap, twenty close it.
        for _ in 0..4 {
            click(&mut offset, slouch, [-0.5, 1.0]);
        }

        assert_eq!(offset.state().anchors.len(), 2);

        let [sy, sp] = offset.offset_deg(slouch);

        assert!(sy < 0.25 && sp > 0.5, "after four: slouch {sy} {sp}");

        for _ in 0..16 {
            click(&mut offset, slouch, [-0.5, 1.0]);
        }

        let [sy, sp] = offset.offset_deg(slouch);
        let [uy, up] = offset.offset_deg(SEAT);

        assert!((sy + 0.5).abs() < 0.1 && (sp - 1.0).abs() < 0.1, "slouch {sy} {sp}");
        assert!((uy - 1.0).abs() < 0.05 && up.abs() < 0.05, "seat {uy} {up}");
    }

    #[test]
    fn far_from_every_anchor_the_blend_is_the_global_mean() {
        let mut offset = OnlineOffset::new(OffsetParams::default(), None);

        for _ in 0..20 {
            click(&mut offset, SEAT, [1.0, 0.0]);
            click(&mut offset, [0.0, -150.0, 750.0], [0.0, 1.0]);
        }

        let far    = offset.offset_deg([400.0, 400.0, 1200.0]);
        let global = offset.global_deg();

        assert!((far[0] - global[0]).abs() < 1e-6 && (far[1] - global[1]).abs() < 1e-6);
        assert!((global[0] - 0.5).abs() < 0.05 && (global[1] - 0.5).abs() < 0.05, "{global:?}");
    }

    #[test]
    fn the_state_round_trips_and_a_foreign_blob_is_refused() {
        let dir  = std::env::temp_dir().join(format!("gaze-offset-{}", std::process::id()));
        let path = dir.join("offset.json");

        let _ = std::fs::remove_dir_all(&dir);

        let mut offset = OnlineOffset::persisted(OffsetParams::default(), &path, Some("abc".into()));

        offset.observe([1.0, 1.0], SEAT);

        let same  = OnlineOffset::persisted(OffsetParams::default(), &path, Some("abc".into()));
        let other = OnlineOffset::persisted(OffsetParams::default(), &path, Some("def".into()));

        assert_eq!(same.state(), offset.state());
        assert_eq!(other.offset_deg(SEAT), [0.0, 0.0]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
