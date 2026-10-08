# PLAN-INTENT: which thing did they mean

> **Historical record.** This is a design and planning document kept as it was written
> while the project was built. Parts of what it describes were later removed (the
> residual model and its trainer, the webcam provider and its sidecar, the benchmark
> harness, the online offset), and some plans in it were never built. The code is the
> source of truth; where this disagrees with it, the code wins. Mentions of "Agentic
> Memory" refer to the author's private notes and are not part of this repo.

**Status 2026-09-10: planned, nothing built.** Research pass and sources: DESIGN.md §3 and
the Agentic Memory note `cosmic-gaze-intent-model-research-2026-09-10`.

## The problem

The two-stage input works: gaze puts the highlight close, the thumb finishes. What is
missing is the last step of precision in the *coarse* stage. The highlight sits on the
neighbour of the thing the user meant, or flickers between two rows, or jumps when the
eye only glanced somewhere on its way to the real target. Bench v3 put the number on it:
top-1 correct ~45% on real desktop density, top-3 75–94%, and every scoring knob trades
ambiguity against confident-wrong at a near constant product. The engine has run out of
things to learn from a single gaze sample and a fixed σ.

Three ideas from the literature, adopted 2026-09-10, each aimed at a different part of it:

1. **Target-centre forecasting from gaze and head history** (GazeFS, arXiv 2609.03868).
   The residual during the focus phase has a *persistent direction* the eye is converging
   from; a small model over the last second of gaze and head predicts where the target
   centre is rather than smoothing where the gaze is, and says whether the eye is still
   searching or has settled. This is the primary bet.
2. **Posterior accumulation over the fixation with a fitted endpoint distribution**
   (BayesGaze, GI 2021; Wei et al., CHI 2023). Score candidates by evidence accumulated
   across the focus phase under the user's own bivariate-Gaussian endpoint distribution,
   bias included, with a prior from element kind and history. Not the selection itself:
   the channel that decides which candidate the highlight goes to and whether it moves at
   all. The phase estimate from (1) is the gate: a glance is not evidence.
3. **Velocity-shaped touch refinement with a gaze scope** (GazeTune, UIST '26). The thumb
   nudge gets an exponential control-display gain and a re-anchor rule, so the fine channel
   spends less of its budget on the coarse stage's mistakes.

All three are fitted from the user's own clicks. There are no public weights or data for
(1) and the HoloLens 2 data would not transfer to a remote tracker at 33 Hz anyway. The
sessions the old collector recorded were deleted with it (untracked `config/sessions/`,
2026-09-10), so **the first deliverable is data**.

## Rulings

- **Training data comes from the trainer, free-form, at mouse speed.** The trainer is a
  generated libcosmic application the user clicks around with the mouse as fast as they
  can look and click (B5 in the pre-prune PLAN-ET5, restored below). No prompts, no
  targets, no dwell, no controller in the loop. Prompted target acquisition was rejected
  twice (2026-09-03, 2026-09-10): waiting for the prompt is the bottleneck.
- Real-use clicks are recorded by the same path and are the generalisation check, never
  the bootstrap.
- Every model lives behind the existing snap interface and is a toggle. Nothing here
  changes what the thumb or the commit do.
- Honest evaluation: hold out whole trainer windows (`task` id) and whole real sessions.
  Frames within a fixation are correlated; per-sample CV lies (PLAN-ET5 principle).

## Data volumes (estimates, to be replaced by the learning curve after the first fit)

| what                                   | needs (clicks)   | at trainer speed (~15/min) |
|----------------------------------------|------------------|----------------------------|
| endpoint distribution (bias, σ, ρ)     | 200–500          | 15–35 min                  |
| phase classifier (search / focus)      | 300–800          | 20–55 min                  |
| target-centre forecaster               | 1,000–2,000      | 70–140 min                 |

Real use produces ~3.5 clicks/min (213 in 72 min, DESIGN.md §10c).

## Episode format

One JSON line per accepted press, `$XDG_STATE_HOME/cosmic-gaze/episodes/<session>.jsonl`
(`Paths::episodes_dir`; `--home` puts it under `DIR/episodes`). A file per daemon run;
the first line is a `meta` record with the calibration hash, blob hash, desk file hash,
sample rate and outputs, so every episode is keyed to the geometry it was recorded under.

```
{"kind":"episode","n":17,"t_unix_s":…,"press":{"px":[x,y],"output":"DP-1","button":"left","via":"mouse"|"pad"},
 "label":{"source":"trainer"|"tree"|"vision"|"none","kind":"button","bbox":[x,y,w,h],"text":"Save","score":1.0,
          "trainer":{"task":7,"hit":true,"posture":"normal","theme":"dark","luma":0.12}},
 "candidates":[{"kind":"button","bbox":[…],"score":0.31,"source":"tree"},…],   // top 8 at commit, as the engine ranked them
 "snap":{"bbox":[…],"kind":"button"} | null,                                  // what the highlight was on
 "refine":{"dx_px":…,"dy_px":…} | null,                                       // thumb travel before this commit, pad presses only
 "window":{"t0_s":…,"hz":33,"frames":[[dt_s, yaw_deg, pitch_deg, px, py, hx_mm, hy_mm, hz_mm, pupil_mm, valid],…]}}
```

The window is *up to* 1.5 s before the press and 0.3 s after, at the provider's rate
(≤ 60 frames). It is a cap on history, not a wait: nothing gates the next press, consecutive
episodes overlap when presses come faster than the cap, and the after-window fills while
the user is already on the next target. A saccade and settle is 200–400 ms; the rest of
the window is the previous fixation, which is evidence for where the eye came from.
`yaw_deg`/`pitch_deg` are the corrected combined ray in tracker angle space (after
calibration and field — what the snap engine saw), `px`/`py` the desk
point, `h*` the posture origin, `valid` the both-eyes flag. Raw device fields are not
recorded; the calibration hash in the meta line is what makes the angles reproducible.

The format is the one thing every later stage reads, so it is versioned (`"format":1`)
and grown by adding fields, never by changing them.

## Work items

### I1. Episode recorder in `gazed` (`gaze-proto::episodes`)
- A ring of the last 2 s of samples with posture origin and pupil, filled at sample rate.
  `GazeSample` carries none of the head data, so the provider gains a `PostureSample`
  side channel (origin, pupil, validity) the session reads next to the sample. **This is
  the one `gaze-core` addition**: the posture struct is needed by the provider, the
  session and the recorder.
- Hooked where `offer_clicks` and the pad commit already are: every press the session
  hears becomes an episode 0.3 s later, when the after-window has filled. Drags and
  off-desk presses are dropped as the collector dropped them; `no-element` presses are
  *kept* with `label.source = "none"` (a search-phase negative is worth having).
- Label priority: trainer match (I2) > a11y tree hit under the press > snapped element
  when the press point is inside its box > none. The candidate list is the engine's
  `ranked()` at the commit instant, top 8, so the scorer (I4) can be replayed offline
  against exactly what it would have seen.
- Off by default; on with a `Recording` bus property the applet exposes as a toggle,
  and forced on while a trainer is connected. The status line in the applet shows the
  episode count.

### I2. Trainer, restored
- `git checkout 169862f~1 -- crates/gaze-trainer crates/gaze-core/src/trainer.rs`,
  then trim: the collector's listener (`gaze-clicks/src/trainer.rs` at the same commit,
  272 lines) moves into `gaze-proto` as the recorder's socket, matching presses by
  wall-clock time within 150 ms as before. The socket name stays `gaze-clicks.sock`;
  nothing else uses it.
- Coverage seeds from the episodes directory instead of `config/sessions/`.
- **Training mode.** While a trainer is connected the session stops warping and
  injecting and hides the pointer overlay, so the mouse is the user's and the gaze is
  only observed. The
  mode is the `Mode` property's `training` value. The trainer runs full screen on the
  tracker's display, as before.
- Posture prompts stay (every 80 presses, a few seconds each): the head channel of the
  forecaster needs the posture to vary. Theme flips per window stay for the pupil.
- Speed is the design constraint. Anything in the trainer that makes the user wait
  (dialog animations, settle delays, the switcher) is measured against presses per
  minute and cut if it costs.

### I3. Offline tooling (`tools/intent/`, Python, uv, CPU is enough)
- `episodes.py`: loader, hold-out splits by trainer task and by session, the QC gates
  (fixation present in the window, both-eyes valid fraction, press inside the label box).
- `endpoint.py`: fits the bivariate Gaussian endpoint distribution of the focus-phase
  gaze relative to the label box centre, per output, optionally conditioned on box width
  and saccade amplitude (Wei); reports bias, σx, σy, ρ with bootstrap CIs. Writes
  `endpoint.json` for I4.
- `replay.py`: re-scores every episode's recorded candidate list with the current engine
  rule and with the posterior rule (I4), reports top-1 / top-3 / confident-wrong /
  highlight moves per episode, split by label source. **The gate for I4 going live:**
  top-1 up by ≥ 5 points on held-out trainer windows *and* not down on real sessions.
- `forecaster/`: the GazeFS-lite model (I5) — PyTorch, a few hundred lines, ONNX export
  with a fixed 50-frame window, and the same replay bench with the forecast in place of
  the last sample.

### I4. Posterior scorer in `gaze-snap`
- `SnapEngine` gains a second scoring rule behind a builder flag: per sample, likelihood
  of each candidate under the fitted endpoint distribution (a Gaussian integrated over
  the box, not a point evaluation, so a wide box near the gaze is not out-scored by a
  narrow one under it), times a kind prior, accumulated over the focus phase and decayed
  at its onset. The winner is the posterior argmax; hysteresis becomes "the held target
  keeps its mass", replacing the score margin.
- The phase gate: while the phase estimate says search, the accumulator holds and the
  highlight does not move. Until I5 exists the phase comes from the I-VT state the
  filter already produces (fixation = focus), so I4 can ship and be measured on its own.
- `endpoint.json` is read from the config dir next to the calibration; absent, the
  scorer falls back to the fixed σ and behaves as today.

### I5. Target-centre forecaster (GazeFS-lite)
- Contract: input `[50, 7]` (Δyaw, Δpitch, yaw rate, pitch rate, head x/y/z rates,
  causal, zero-padded — the model is trained on every prefix so it is useful 200 ms into
  a saccade, not only with a full window), output `[Δyaw, Δpitch]` to the target centre from the current
  gaze and `p_focus`. One small causal transformer or GRU; the paper's fine/coarse
  patching only if the plain model plateaus. Trained on trainer episodes, validated on
  held-out windows, generalisation-checked on real sessions.
- Runs in `gaze-proto` through `ort` (already a dependency for the detector), one
  inference per sample, well under a millisecond. Its point replaces the filtered gaze
  point fed to the scorer during focus; its `p_focus` replaces the I-VT gate in I4.
- Same replay gate as I4. If it does not beat I4 alone on held-out real sessions it stays
  off and the plan says so here.

### I6. Nudge, GazeTune shape (`gaze-proto::daydream`, `session`)
- Gain `g(s) = g_min + (g_max − g_min)(1 − e^(−s/v))` on pad velocity, replacing the
  flat `refine_touch_gain_px`; two knobs (`refine_gain_min_px`, `refine_gain_max_px`)
  and the velocity scale fixed.
- Gaze scope: a refine in progress is anchored; if gaze leaves a scope around the anchor
  (radius `snap_deg`) and settles (I-VT fixation, ≥ 400 ms), the anchor moves to the new
  fixation and the thumb offset resets. Replaces "refined point stands for the next
  commit" when the eye has clearly gone elsewhere.
- No data needed; ships first.

## Order

I6 → I1 → I2 → **[user] two sittings of trainer, ~90 min total** → I3 (endpoint fit,
replay) → I4 → **[user] a day of real use with recording on** → I3 (forecaster) → I5.

I1 and I2 are one PR: the recorder is not testable without presses to record. I4 lands
behind its flag with the replay numbers in this file's results log before it becomes the
default.

## Results log

(append per experiment: date, episodes, split, numbers)
