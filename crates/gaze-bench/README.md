# gaze-bench

The offline Monte Carlo behind Phase 0's headline number: screenshots of the real desktop
-> detector boxes -> simulated fixations on every box -> the snap engine -> did it come
back with the box we aimed at?

## Running

```sh
cargo run --release -p gaze-bench -- \
    --shots screenshots/ --desk config/desk.toml --models models/ \
    --sigma-fixed 0.5,0.7,1.0,1.5 --sigma-profile \
    --trials-per-element 20 --out crates/gaze-bench/report.md --overlays overlays/
```

Inputs are the `<output>-<n>.png` frames written by `gaze-capture-cli` (physical pixels,
one output per file; both are gitignored, as are the ONNX models). The output's origin
comes from the desk config and its compositor scale from the ratio of the PNG's width to
the output's logical width, so a fractional-scale panel needs no extra flag.

Detection is cached beside each PNG as `<output>-<n>.elements.json` and reused unless the
frame changed or `--redetect` is passed. The first pass costs about 400 ms per ultrawide
frame; later runs start instantly.

`--max-size-px 800` drops whole-pane boxes, `--min-size-px` drops slivers. Filtered
elements are removed entirely, so they are neither targets nor distractors.
`--snap-radius-deg`, `--hysteresis` and `--weights` pass straight through to the snap
engine for sweeping. `--weights` takes one to five values in `ScoreWeights::from_list`
order (`kind,area,distance,center,center_deg`); omit it entirely and the bench measures the
engine's own defaults, which is what the canonical report must do. `--ambiguity-margin` and `--ambiguity-rivals` set the two-tier gate,
`--jitter-only-legacy` restores the old independent-per-sample noise, and
`--no-edge-clamp` goes back to scoring an off-desk sample as lost gaze.

## What it measures

Every detected element is a trial target. Each trial draws a clean landing point uniformly
inside the box shrunk by 20% per side (centre for anything under 4 px after shrinking),
then applies the provider's angular error the same way `gaze-provider-synthetic` does at
runtime: lift to a ray from the nominal eye, rotate by the per-axis error in degrees,
re-intersect the desk. A perturbed ray that misses every panel is clamped to the panel
edge it left through (bisecting the perturbation scale to find the crossing), because a
real tracker still reports a point when the user glances past the bezel; `--no-edge-clamp`
scores it as `no_gaze` instead, which charges edge targets like title bars and taskbars for
something the hardware does not do.

The error is split the way a real tracker splits it: a **bias** drawn once per fixation
from `N(0, NoiseModel::bias_sigma(sigma))` plus **jitter** drawn per sample from
`N(0, jitter_deg)`, so the two add in quadrature to the requested sigma. That matters
because a tracker's quoted accuracy is mostly bias while its sample-to-sample precision is
0.1 to 0.3 deg, and drawing the full sigma independently every sample (the old behaviour,
still available as `--jitter-only-legacy`) both starves the I-VT fixation classifier and
lets a 24-sample average shrink the error by sqrt(24), which no real tracker does.

Targets are classified from their box, not from the detector's label: `line` is wider than
6:1 and under 40 px tall, `widget` is any remaining control class, `text-other` is the
rest. Every table is broken down by class, and a second candidate-set axis runs the same
widget targets with and without lines and text in the candidate set.

Two feeding modes, both reported:

- **single-sample**: one `Filtered { state: Fixating }` straight into `SnapEngine::update`.
- **fixation-sequence**: 24 samples at 120 Hz around the landing point, each with fresh
  noise, through `FilterStack` and then `update`, resolved by `commit(t_last, 0.0)`.

Runs are deterministic: every trial seeds its own RNG from `(seed, frame, element, trial,
sigma)`, so the numbers do not depend on how rayon split the work, and the two feeding
modes share a landing point and first noise draw for a paired comparison.

## Reading the report

`correct %` is strict element-id equality. A trial is **ambiguous** when a rival
candidate's cost came within `--ambiguity-margin` of the winner's: right or wrong, it is a
trial the two-tier design would hand to refinement rather than click. That splits the
answers into `confident-correct` (a click that lands), `confident-wrong` (a click the user
has to undo, the number that has to be near zero) and `ambiguous` (the refinement load).
`--ambiguity-rivals` decides which rivals count: `all`, `distinct` (the default: ignore a
rival that is the same box as the winner) or `separate` (also ignore boxes nested with it).

**Top-k accuracy** is what decides how the refinement tier has to look. It reports how
often the intended target is among the best 1, 2, 3 or 5 candidates by cost, alongside the
mean and 90th-percentile number of candidates inside the ambiguity margin, which is the
size of the hint that tier would show. Ranking comes from `SnapEngine::ranked()`; the
outcome columns come from what `update()` returned, which can differ because hysteresis
may hold a previous target. A candidate the rival policy calls the same thing on screen as
the target counts *as* the target, so detector duplication cannot push a target down its
own ranking.

**Nudge distance** sizes the fine channel. The handheld controller warps the pointer with
gaze and corrects it with the thumb, so what matters is how far that correction has to
travel: the distance from the point the system would warp to (the snapped target's clamped
point, or the raw gaze when nothing snapped) to the nearest point of the intended target's
box, zero when the warp already landed on it. Reported as the fraction needing no nudge at
all plus median/p90/p99 in degrees and pixels, from a histogram rather than a stored
sample.

**Flick coverage** asks whether a direction alone would do instead of a nudge: the share of
ranked trials where the intended target is inside the best three candidates *and* its
bearing from the warp point falls in a different 45 degree sector from every other one.

The slip anatomy section splits slips into `duplicate` (the same thing on screen detected
twice, once as a widget and once as OCR text), `nested` (a label inside its button, a
terminal line inside its pane) and `separate`. The overlays colour each box by what usually
happened to it: green confidently found, amber too close to call, red confidently wrong,
grey unresolved, magenta gaze lost.

Detection sidecars are invalidated by the PNG's mtime as well as its dimensions, because
the capture CLI writes frames by index and a fresh capture silently replaces the frame a
sidecar describes.
