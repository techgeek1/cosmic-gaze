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
//!
//! # Consensus past the gate (PLAN-ET5 E2)
//!
//! The gate has a failure mode: a bias larger than it (glasses pushed up, a knocked
//! mount, a posture the anchors have never seen) makes *every* click a reject, and the
//! filter can never learn its way out. So rejects are not discarded. They go to a short
//! buffer keyed to the posture, and when [`CONSENSUS_CLICKS`] recent rejects near one
//! eye position agree with each other to within [`CONSENSUS_SPREAD_DEG`] *and* were
//! clicks on different places (at least [`CONSENSUS_TARGET_MM`] apart, so one misread
//! widget clicked four times is not a consensus), their median is taken to be the
//! truth and the gate to have been wrong: the anchor for that posture jumps to it in
//! one step, unclipped, and the event is logged as a bias jump. Scattered rejects stay
//! rejected. This is the RANSAC the plan asked for, sized for what it is: a 2D bias
//! over a handful of points needs a median and a spread check, not sampling.

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

/// Consistent rejects near one posture that make a consensus, and the least any one
/// adoption is founded on.
pub const CONSENSUS_CLICKS: usize = 4;

/// How closely the rejects must agree with their median to count as one bias, degrees.
/// A normal click scatters by about a degree; the same bias seen four times sits well
/// inside this, and four unrelated misclicks do not.
pub const CONSENSUS_SPREAD_DEG: f64 = 1.0;

/// Least extent the consensus clicks' targets must span along some axis, mm. One
/// element clicked repeatedly spans nothing; clicks across a window or a line of
/// controls span far more.
pub const CONSENSUS_TARGET_MM: f64 = 30.0;

/// How long a reject stays in the buffer, seconds. A bias jump is a thing that
/// happened at a moment; rejects from a different half hour say nothing about now.
pub const REJECT_TTL_S: f64 = 600.0;

/// Most rejects kept at once, oldest dropped.
const MAX_REJECTS: usize = 16;

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
    /// Bias jumps adopted from a consensus of rejects (see the module doc). More
    /// than a few in a day says the gate is fighting something a retrain should fix.
    #[serde(default)]
    pub jumps              : u64,
    /// Unix time of the last accepted click.
    pub updated_unix_s     : f64,
    /// Body hash of the on-device model the offset was measured under. The offset is
    /// what is left after the firmware and the residual model, so a retrained device
    /// orphans it exactly as it orphans the model.
    pub device_blob_sha256 : Option<String>,
}

/// One click as the filter sees it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Observation {
    /// Where the corrected ray still was relative to the clicked point, yaw and pitch
    /// degrees in the label frame, measured *after* the current offset: the innovation
    /// at this eye position.
    pub leftover_deg : [f64; 2],
    /// Mean eye position over the click's window, tracker millimetres.
    pub origin_mm    : [f64; 3],
    /// The clicked point in tracker millimetres, so a consensus can be checked for
    /// having been clicks on different things.
    pub target_mm    : [f64; 3],
}

/// A reject held for the consensus check.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Reject {
    observation : Observation,
    /// Unix time it was offered.
    at_s        : f64,
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
    /// Past the gate, but the last few rejects at this posture agreed with it, so the
    /// gate was what was wrong: the posture's bias jumped to their consensus. Carries
    /// the click's own leftover, the jump taken, the offset now predicted there, the
    /// anchor count, and how many clicks the consensus rested on.
    Adopted {
        leftover_deg : [f64; 2],
        jump_deg     : [f64; 2],
        offset_deg   : [f64; 2],
        anchors      : usize,
        clicks       : usize,
    },
}

/// The running offset with its parameters and its file.
#[derive(Debug)]
pub struct OnlineOffset {
    params  : OffsetParams,
    state   : OffsetState,
    /// Where accepted updates are written, `None` to keep the state in memory only.
    path    : Option<PathBuf>,
    /// Recent rejects, oldest first, for the consensus check. Not persisted: a jump
    /// is a thing that happens now.
    rejects : Vec<Reject>,
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
                jumps              : 0,
                updated_unix_s     : 0.0,
                device_blob_sha256 : blob_sha256,
            },
            path    : None,
            rejects : Vec::new(),
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

    /// Offers one click. The leftover is measured *after* the current offset, so it is
    /// the innovation at that eye position. Inside the gate it is folded into the
    /// posture's anchor; past it, it is held for the consensus check (module doc).
    pub fn observe(&mut self, obs: &Observation) -> ClickFeedback {
        let leftover_deg = obs.leftover_deg;
        let origin_mm    = obs.origin_mm;
        let [yaw, pitch] = leftover_deg;

        let finite = yaw.is_finite() && pitch.is_finite()
            && origin_mm.iter().all(|v| v.is_finite());

        if !finite {
            return ClickFeedback::Rejected { leftover_deg: leftover_deg };
        }

        if yaw.hypot(pitch) >= self.params.gate_deg {
            return self.reject(obs);
        }

        let clip  = self.params.clip_deg;
        let yaw   = yaw.clamp(-clip, clip);
        let pitch = pitch.clamp(-clip, clip);

        self.fold(origin_mm, [yaw, pitch], None);
        self.commit();

        ClickFeedback::Accepted {
            leftover_deg : leftover_deg,
            offset_deg   : self.offset_deg(origin_mm),
            anchors      : self.state.anchors.len(),
        }
    }

    /// Buffers a reject and adopts the consensus when there is one.
    fn reject(&mut self, obs: &Observation) -> ClickFeedback {
        let now = unix_now_s();

        self.rejects.retain(|r| now - r.at_s <= REJECT_TTL_S);
        self.rejects.push(Reject { observation: *obs, at_s: now });

        if self.rejects.len() > MAX_REJECTS {
            self.rejects.remove(0);
        }

        let Some((jump, members)) = self.consensus(obs.origin_mm) else {
            return ClickFeedback::Rejected { leftover_deg: obs.leftover_deg };
        };

        // The consensus is the innovation at this posture, taken whole: the gate
        // already established that a clipped nudge cannot get there.
        let clicks = members.len();

        self.fold(obs.origin_mm, jump, Some(clicks as u64));

        for i in members.into_iter().rev() {
            self.rejects.remove(i);
        }

        self.state.jumps += 1;
        self.commit();

        warn!(
            jump_yaw_deg   = format_args!("{:+.2}", jump[0]),
            jump_pitch_deg = format_args!("{:+.2}", jump[1]),
            clicks         = clicks,
            jumps          = self.state.jumps,
            "bias jump adopted from consistent rejects; a recurring jump wants a retrain",
        );

        ClickFeedback::Adopted {
            leftover_deg : obs.leftover_deg,
            jump_deg     : jump,
            offset_deg   : self.offset_deg(obs.origin_mm),
            anchors      : self.state.anchors.len(),
            clicks       : clicks,
        }
    }

    /// The consensus among the buffered rejects near `origin_mm`, if there is one:
    /// the median leftover of the agreeing rejects and their indices in the buffer.
    fn consensus(&self, origin_mm: [f64; 3]) -> Option<([f64; 2], Vec<usize>)> {
        let reach = self.params.reach_mm;

        let near: Vec<usize> = self.rejects.iter()
            .enumerate()
            .filter(|(_, r)| dist2(r.observation.origin_mm, origin_mm) <= reach * reach)
            .map(|(i, _)| i)
            .collect();

        if near.len() < CONSENSUS_CLICKS {
            return None;
        }

        let mut yaws:   Vec<f64> = near.iter().map(|&i| self.rejects[i].observation.leftover_deg[0]).collect();
        let mut pitchs: Vec<f64> = near.iter().map(|&i| self.rejects[i].observation.leftover_deg[1]).collect();
        let centre = [median(&mut yaws), median(&mut pitchs)];

        let agreeing: Vec<usize> = near.into_iter()
            .filter(|&i| {
                let [y, p] = self.rejects[i].observation.leftover_deg;

                (y - centre[0]).hypot(p - centre[1]) <= CONSENSUS_SPREAD_DEG
            })
            .collect();

        if agreeing.len() < CONSENSUS_CLICKS {
            return None;
        }

        // Clicks on different things: the targets must span something.
        let extent = (0..3)
            .map(|axis| {
                let values = agreeing.iter().map(|&i| self.rejects[i].observation.target_mm[axis]);
                let lo = values.clone().fold(f64::INFINITY, f64::min);
                let hi = values.fold(f64::NEG_INFINITY, f64::max);

                hi - lo
            })
            .fold(0.0, f64::max);

        if extent < CONSENSUS_TARGET_MM {
            return None;
        }

        let mut yaws:   Vec<f64> = agreeing.iter().map(|&i| self.rejects[i].observation.leftover_deg[0]).collect();
        let mut pitchs: Vec<f64> = agreeing.iter().map(|&i| self.rejects[i].observation.leftover_deg[1]).collect();

        Some(([median(&mut yaws), median(&mut pitchs)], agreeing))
    }

    /// Folds an innovation into the anchor nearest `origin_mm`, or founds one. With
    /// `whole` the innovation is taken in full, founded on that many clicks; without
    /// it, at the gain (or the prior count for a fresh anchor).
    fn fold(&mut self, origin_mm: [f64; 3], innovation: [f64; 2], whole: Option<u64>) {
        let [yaw, pitch] = innovation;
        let reach        = self.params.reach_mm;

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
                let alpha = {
                    match whole {
                        Some(_) => 1.0,
                        None    => self.params.alpha.max(1.0 / (a.updates as f64 + 1.0)),
                    }
                };

                a.yaw_deg   += alpha * yaw;
                a.pitch_deg += alpha * pitch;
                a.updates   += whole.unwrap_or(1);
            }

            None => {
                // A new posture: start where the blend already predicts, nudged by this
                // click as one of `PRIOR_CLICKS + 1` rather than the whole story.
                let [pred_yaw, pred_pitch] = self.offset_deg(origin_mm);
                let alpha                  = {
                    match whole {
                        Some(_) => 1.0,
                        None    => 1.0 / (PRIOR_CLICKS as f64 + 1.0),
                    }
                };

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
                    updates   : whole.unwrap_or(PRIOR_CLICKS + 1),
                });
            }
        }
    }

    /// Counts an update and writes the state, when there is somewhere to write it.
    fn commit(&mut self) {
        self.state.updates        += 1;
        self.state.updated_unix_s  = unix_now_s();

        if let Some(path) = &self.path
            && let Err(e) = self.state.save(path)
        {
            warn!(path = %path.display(), "online offset not saved ({e})");
        }
    }

    /// Forgets every posture. For an explicit user reset; nothing in the provider calls
    /// it on its own. The jump count stays: it is a diagnostic of the day, not a bias.
    pub fn reset(&mut self) {
        self.state.anchors.clear();
        self.state.updates = 0;
        self.rejects.clear();
    }

    /// Writes the state out where it persists, if anywhere. A reset wants this so the
    /// next run does not restore the anchors that were just forgotten.
    pub fn save(&self) {
        if let Some(path) = &self.path
            && let Err(e) = self.state.save(path)
        {
            warn!(path = %path.display(), "online offset not saved ({e})");
        }
    }

    /// Rejects currently held for the consensus check.
    pub fn pending_rejects(&self) -> usize {
        self.rejects.len()
    }
}

/// Squared distance between two eye positions, mm².
fn dist2(a: [f64; 3], b: [f64; 3]) -> f64 {
    (a[0] - b[0]).powi(2) + (a[1] - b[1]).powi(2) + (a[2] - b[2]).powi(2)
}

/// Median of a non-empty slice, sorting it in place.
fn median(values: &mut [f64]) -> f64 {
    values.sort_by(f64::total_cmp);

    let n = values.len();

    if n % 2 == 1 { values[n / 2] } else { 0.5 * (values[n / 2 - 1] + values[n / 2]) }
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

    /// One click with a leftover at an origin, on a target nothing checks.
    fn obs(leftover: [f64; 2], origin: [f64; 3]) -> Observation {
        Observation { leftover_deg: leftover, origin_mm: origin, target_mm: [0.0, 0.0, 0.0] }
    }

    /// Offers a click whose true bias is `bias` at `origin`, as the provider would: the
    /// leftover is the bias minus the current prediction there.
    fn click(offset: &mut OnlineOffset, origin: [f64; 3], bias: [f64; 2]) -> ClickFeedback {
        click_on(offset, origin, bias, [0.0, 0.0, 0.0])
    }

    /// `click` with the clicked point given, for the consensus tests.
    fn click_on(offset: &mut OnlineOffset, origin: [f64; 3], bias: [f64; 2], target: [f64; 3])
        -> ClickFeedback
    {
        let [y, p] = offset.offset_deg(origin);

        offset.observe(&Observation {
            leftover_deg : [bias[0] - y, bias[1] - p],
            origin_mm    : origin,
            target_mm    : target,
        })
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

        assert!(matches!(offset.observe(&obs([3.0, 0.0], SEAT)), ClickFeedback::Rejected { .. }));
        assert!(matches!(offset.observe(&obs([f64::NAN, 0.0], SEAT)), ClickFeedback::Rejected { .. }));
        assert!(matches!(offset.observe(&obs([1.0, 0.0], [f64::NAN, 0.0, 0.0])), ClickFeedback::Rejected { .. }));
        assert_eq!(offset.offset_deg(SEAT), [0.0, 0.0]);

        // Inside the gate but past the clip on one axis: a first click enters as the
        // clip over the prior count.
        let fed = offset.observe(&obs([2.9, 0.0], SEAT));

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
    fn consistent_rejects_on_different_targets_become_a_jump() {
        let mut offset = OnlineOffset::new(OffsetParams::default(), None);

        // A 5 degree bias: every click is past the 3 degree gate.
        let bias = [4.0, -3.0];
        let targets = [[0.0, 0.0, 0.0], [80.0, 0.0, 0.0], [0.0, 60.0, 0.0]];

        for (i, target) in targets.iter().enumerate() {
            let fed = click_on(&mut offset, SEAT, bias, *target);

            assert!(matches!(fed, ClickFeedback::Rejected { .. }), "click {i}: {fed:?}");
            assert_eq!(offset.offset_deg(SEAT), [0.0, 0.0]);
        }

        assert_eq!(offset.pending_rejects(), 3);

        // The fourth agreeing click on yet another place makes the consensus.
        let fed = click_on(&mut offset, SEAT, bias, [120.0, 90.0, 0.0]);

        let ClickFeedback::Adopted { jump_deg, offset_deg, anchors, clicks, .. } = fed else {
            panic!("{fed:?}");
        };

        assert_eq!((anchors, clicks), (1, 4));
        assert!((jump_deg[0] - 4.0).abs() < 1e-9 && (jump_deg[1] + 3.0).abs() < 1e-9, "{jump_deg:?}");
        assert!((offset_deg[0] - 4.0).abs() < 1e-9 && (offset_deg[1] + 3.0).abs() < 1e-9, "{offset_deg:?}");
        assert_eq!(offset.state().jumps, 1);
        assert_eq!(offset.pending_rejects(), 0);

        // From here the same bias is inside the gate and folds in normally.
        assert!(matches!(click(&mut offset, SEAT, bias), ClickFeedback::Accepted { .. }));
    }

    #[test]
    fn scattered_rejects_and_one_target_clicked_repeatedly_are_not_a_consensus() {
        let mut offset = OnlineOffset::new(OffsetParams::default(), None);

        // Four large leftovers that disagree: misclicks, not a bias.
        for (i, leftover) in [[4.0, 0.0], [-4.0, 0.0], [0.0, 4.0], [0.0, -4.0]].iter().enumerate() {
            let target = [40.0 * i as f64, 0.0, 0.0];
            let fed    = offset.observe(&Observation {
                leftover_deg : *leftover,
                origin_mm    : SEAT,
                target_mm    : target,
            });

            assert!(matches!(fed, ClickFeedback::Rejected { .. }), "{fed:?}");
        }

        assert_eq!(offset.offset_deg(SEAT), [0.0, 0.0]);

        // Four agreeing leftovers on the same spot: one misread widget.
        let mut fresh = OnlineOffset::new(OffsetParams::default(), None);

        for _ in 0..6 {
            let fed = click_on(&mut fresh, SEAT, [4.0, 0.0], [10.0, 10.0, 0.0]);

            assert!(matches!(fed, ClickFeedback::Rejected { .. }), "{fed:?}");
        }

        assert_eq!(fresh.offset_deg(SEAT), [0.0, 0.0]);
        assert_eq!(fresh.state().jumps, 0);

        // Rejects at a different posture do not vote for this one.
        let mut apart = OnlineOffset::new(OffsetParams::default(), None);

        for i in 0..3 {
            click_on(&mut apart, [0.0, -200.0, 750.0], [4.0, 0.0], [50.0 * i as f64, 0.0, 0.0]);
        }

        let fed = click_on(&mut apart, SEAT, [4.0, 0.0], [200.0, 0.0, 0.0]);

        assert!(matches!(fed, ClickFeedback::Rejected { .. }), "{fed:?}");
    }

    #[test]
    fn the_state_round_trips_and_a_foreign_blob_is_refused() {
        let dir  = std::env::temp_dir().join(format!("gaze-offset-{}", std::process::id()));
        let path = dir.join("offset.json");

        let _ = std::fs::remove_dir_all(&dir);

        let mut offset = OnlineOffset::persisted(OffsetParams::default(), &path, Some("abc".into()));

        offset.observe(&obs([1.0, 1.0], SEAT));

        let same  = OnlineOffset::persisted(OffsetParams::default(), &path, Some("abc".into()));
        let other = OnlineOffset::persisted(OffsetParams::default(), &path, Some("def".into()));

        assert_eq!(same.state(), offset.state());
        assert_eq!(other.offset_deg(SEAT), [0.0, 0.0]);

        let _ = std::fs::remove_dir_all(&dir);
    }
}
