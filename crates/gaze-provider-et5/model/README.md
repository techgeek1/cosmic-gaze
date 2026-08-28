# ET5 model prototype (PLAN-ET5.md Phase C)

Python/`uv` harness that prototypes the Phase D model: kernel-ridge regression on the
firmware's angular residual, conditioned on head state, evaluated leave-one-*session*-out.
Nothing here runs at runtime — Phase D (`crates/gaze-provider-et5/src/model.rs`,
`train.rs`) reimplements whatever this proves out, in Rust, over `nalgebra`.

## Running it

```sh
cd crates/gaze-provider-et5/model
uv run python run_all.py
```

Defaults to session zero (`config/calibration-et5.readings.jsonl`) with
`--split-by-stop-index` on. Output goes to `out/report.md` (and is printed).

Other entry points:

```sh
uv run python -m gaze_model.selftest        # geometry self-tests only
uv run python run_all.py --readings a.jsonl b.jsonl     # multiple raw sessions
uv run python run_all.py --csv export.csv               # once B2's CSV exists
uv run python run_all.py --readings a.jsonl --csv b.csv # mix both loaders
```

## What's here

- `gaze_model/geometry.py` — desk/tracker geometry ported from `gaze_core::geometry`,
  `gaze.rs`, `calibration.rs`, `sweep.rs`: rotations, the desk-frame/sensor-frame
  conversion (`desk_to_sensor`), the curved-panel `uv_to_world`, and `combined_ray`
  (the same eye-origin/`gaze_3d` fusion `gaze.rs::combined_ray` does).
- `gaze_model/schema.py` — the one DataFrame schema (38 columns) every loader produces
  and everything downstream consumes. Documents the exact mapping from PLAN-ET5.md's
  column-group description to concrete names.
- `gaze_model/load_readings.py` — loads the raw `kind: frame|traj|stop` JSONL
  (session zero's format, pre-Phase-B1) into the schema: firmware-track lag
  estimation, saccade gating, per-stop QC, target-to-tracker-space mapping, residual
  and per-eye/head features, all reproduced from `sweep.rs::fit_display_direct`.
- `gaze_model/load_export.py` — loads Rust's future `dataset export --csv` (Phase B2).
  Does not exist yet; this is the seam it plugs into (see the module docstring for the
  rename shim if B2's headers differ from `schema.py`'s names).
- `gaze_model/features.py` — the 27-column feature matrix and 2-column residual
  target; `add_target_angle_columns` (the "old uv" equivalent, used only by the
  quadratic-field baselines).
- `gaze_model/baselines.py` — firmware-only, per-session quadratic field (the old
  approach), and a pooled global quadratic + linear head/interocular ridge.
- `gaze_model/model.py` — the Nystrom (sparse) kernel-ridge model: ARD RBF kernel,
  k-means inducing points, an explicit radial `c * angle_axis_deg` term and a per-eye
  linear pupil term, subset-of-regressors predictive variance, grouped-CV
  hyperparameter search.
- `gaze_model/evaluate.py` — grouped (leave-one-session-out) and in-sample scoring,
  per-session bias, the pupil regression, the variance/error correlation.
- `run_all.py` — wires it all together and writes `out/report.md`.

## What every number means

- **RMS / p50 / p90**: the post-correction angular error, degrees — `|actual_residual
  - predicted_correction|`. For `firmware_only` (which always predicts zero
  correction) this is just the firmware's raw residual, i.e. "how wrong is the ray
  today".
- **Held-out (grouped) evaluation**: each estimator is fit on every group except one
  and scored on the held-out group (`sklearn.model_selection.LeaveOneGroupOut` on
  `group_key`). With real multi-session data this *is* leave-one-session-out, the
  number PLAN-ET5.md's gate is defined on.
- **In-sample evaluation**: fit and scored on the same rows. Never the gate number —
  it is reported because with one real session it is the only number this harness can
  produce beyond the smoke test, and because a model that can't even fit its own
  training data well is not worth cross-validating further.
- **`PerSessionQuadraticField`, within-session leave-one-stop-out**: the *old*
  approach's own evaluation scheme (what `field.rs` did): fit on one session's other
  stops, predict its held-out stop. Not comparable to the grouped table — it never
  crosses a session boundary by construction.
- **Per-session bias**: mean signed residual (yaw, pitch, magnitude) per session,
  before the model's correction and after. A model that only removes a per-session
  constant is not learning anything the online offset (PLAN-ET5.md D5) couldn't do
  alone — watch whether "after" collapses toward zero *within* a session too, not just
  across sessions.
- **Pupil regression**: slope and R² of pre-correction residual magnitude against
  each eye's pupil diameter — motivates (or doesn't) `model.py`'s explicit per-eye
  pupil term.
- **Predicted variance vs. |error|, Spearman**: does the model's own confidence mean
  anything. Positive and significant is the bar; it does not have to be large to be
  useful (a σ profile that reliably widens as accuracy drops is enough for a fade-to-
  zero corrector).

## SMOKE TEST vs. the real number

Session zero (`config/calibration-et5.readings.jsonl`) is the only data that exists
right now. `--split-by-stop-index` cuts its 19 stops in half by index parity and
labels the two halves as two "sessions" purely so `GroupKFold`/`LeaveOneGroupOut` has
more than one group to run against — a smoke test of the *mechanism*, not a measurement
of anything. Every report produced this way says so explicitly (a top banner, and the
"grouped LOSO" model names calling out `PerSessionQuadraticField`'s halves-share-the-
same-real-session degeneracy).

**The Phase C gate** — session-out RMS at least 25% below the best baseline, no
session worse — requires PLAN-ET5.md Phase B3 (≥ 6 real sessions, ≥ 3 days, morning and
evening, glasses state noted). Once those exist as either more readings JSONL files or
B2's CSV export, `run_all.py --readings s1.jsonl s2.jsonl ... --csv ...` (with
`--split-by-stop-index` *off*, which is the default once more than one file is given)
produces the real number.

## Session-zero smoke-test numbers (2026-08-27 run)

In-sample (fit and scored on the same 1592 per-frame rows — not held out):

| model | RMS deg | p50 deg | p90 deg |
|---|---|---|---|
| firmware only | 4.005 | 2.125 | 5.913 |
| global quadratic + head (ridge) | 3.335 | 1.529 | 5.482 |
| Nystrom kernel model | **1.549** | 0.855 | 2.376 |
| per-session quadratic field (fit on all, incl. its own anchors) | 3.799 | 1.450 | 5.927 |

Held out, smoke-test grouping only (`--split-by-stop-index`, two pseudo-sessions from
session zero's own stops — **not the Phase C gate number**):

| model | RMS deg | p50 deg | p90 deg |
|---|---|---|---|
| firmware only | 4.005 | 2.125 | 5.913 |
| global quadratic + head (ridge) | 4.281 | 1.855 | 6.867 |
| Nystrom kernel model | **3.555** | 1.905 | 5.233 |
| per-session quadratic field (grouped LOSO) | 8.991 | 2.835 | 9.544 |

The per-session field baseline getting *worse* than doing nothing under the grouped
split is expected and informative, not a bug: its two "sessions" are interleaved stop
positions of one real sitting, so a quadratic fit on one half extrapolates badly to the
other half's different screen positions — exactly the generalisation failure mode
DESIGN.md §10c's decision to drop per-session calibration is about. The Nystrom model
beats firmware-only by ~11% under this same degenerate split; nowhere near the 25% gate,
and it cannot be, from a split that manufactures its "two sessions" out of one sitting's
worth of head posture and lighting.

Within-session leave-one-stop-out (the old approach's own scheme, `field.rs`-equivalent):
RMS 4.833°, p50 1.852°, p90 7.516°, n=996 (worse than firmware-only in-sample — a
single-session quadratic field with only ~16 training anchors per fold is data-starved,
consistent with DESIGN.md §10c's "constant regression, recalibration needed every other
sitting" account of the old approach).

Per-session bias (in-sample, one session): before yaw +0.208°, pitch −0.808°
(magnitude 0.834°) → after yaw +0.007°, pitch −0.027° (magnitude 0.028°). Pupil
regression: left eye slope −0.217°/mm (R²=0.011,
p<0.001, weakly significant — more pupil diameter, less residual, small effect);
right eye slope −0.015°/mm (R²=0.0001, p=0.76, not significant). Predicted-variance-vs-
error Spearman ρ=0.128 (p=3.2e-7, n=1592) — positive and significant in-sample, i.e. the
confidence signal is not noise, though the effect is modest; whether it holds up
held-out is exactly what real session-out data will show.

Re-run `uv run python run_all.py` to regenerate `out/report.md` with current numbers —
the ones above are a snapshot, not a promise the code will reproduce bit-for-bit
(`KMeans` inducing-point selection has a fixed `random_state` but sklearn version
drift can still move things slightly).
