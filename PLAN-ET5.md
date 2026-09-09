# ET5 provider build plan: host-owned device state, then a state-conditioned model

**Status 2026-09-04: A0–A3, A5, B1–B2, B4–B5, Phase C, D1–D3 and D5 built (DESIGN.md §10c
results log). Phase C ran on five click sessions: the per-eye direction kernel model takes
the held-out per-click median from 1.83° to 1.44° (21%, under the 25% gate) and one August
session gets worse, because the day-to-day bias is the next largest term. D5, a causal
click-fed offset, reaches 1.20° on the same clicks in simulation (the oracle is 1.22) and
is live in `gaze-proto`, fed by the real mouse. `gaze-et5-cli fit` writes
`config/model-et5.json`, `gaze-proto` picks it and `config/offset-et5.json` up.** Background and the research that led here:
DESIGN.md §10c. This plan replaces the compound sweep's client-side fitting (correction
field + head-gain regression) and, more importantly, the assumption that the tracker
remembers its own calibration.

Goal: a daily driver. Concretely, sit down on any day, in any posture, with or without a
recalibration screen, and have the gaze point land where you are looking to the sensor's
own limit (~0.7° inside the cone), with drift that corrects itself from ordinary use.

## What we learned (summary; details and sources in DESIGN.md §10c)

1. The Windows driver and Talon both keep the calibration blob on the host and upload it to
   the tracker on **every connect** (Windows: twice, before and after the display plane is
   declared; Talon: `display_setup` then `CALIBRATE_UPLOAD` on every attach). Neither trusts
   the device's flash. Our handshake never uploads and only compares the blob *size*. This is
   the leading explanation for "regressed after sitting off for a few hours".
2. Restore failures elsewhere were framing bugs (the blob needs 8 KB transfers with a
   per-transfer envelope, which our transport already does) or ordering. The references
   disagree on order: nottobii's captured Windows sequence uploads before the plane is
   declared and again after eye-enable; Talon declares the plane first, then uploads. We
   follow the captured Windows order. Killing a process mid-upload wedges the device until
   a physical unplug.
3. Talon calibrates in rounds (1, 4, 4 points) with `POINTS_APPLY` after each, over a
   600×340 mm area bottom-centred on the real screen, and only adds a point once the
   device's own gaze has sat on it. Third-party measurement: 0.69° from one such round.
   `CALIBRATE_GET_POINT_SUGGESTION` (0x442) exists.
4. Between-session variation that no model can remove from the device is real (Tobii's own
   advice: separate profiles for glasses, a second calibration for other lighting). The
   host-side answers in the patent literature are: implicit recalibration from interactions
   with RANSAC/error-buffer acceptance; pupil-radius offset terms; per-eye weighting by
   pupil-signal variance; head-jump episodes; reading as a drift oracle.

## Principles

- **The host owns device state.** The blob file is the model; the device is a cache that is
  refilled on every connect and verified afterwards. Nothing is ever "restored" ad hoc.
- **Retrain the firmware once.** Client-side data is keyed to the blob hash; a retrain
  orphans it. The firmware model is a feature extractor, not the thing that improves.
- **One model across sessions, conditioned on state.** Head position, interocular vector,
  pupil diameter, angle from the tracker axis are inputs. No per-session calibration.
- **Residual, in angle space, before intersection.** Prior = trust the firmware ray. A
  correction exists whether or not the ray hits a panel; the cone straddles the seam.
- **Honest evaluation only.** Leave-one-*session*-out is the number. Frames within a hold
  are correlated; per-sample CV lies.
- **Confidence is an output.** Away from training data the correction fades to zero and σ
  widens; the snap engine already consumes σ.
- Everything that changes the device or needs a seated user is marked **[user]**. Everything
  else is agent work.

## Phase A — Device state

### A0. Blob diagnostics (`gaze-et5-cli blob-info`, `blob-watch`)
- `blob-info [--file F]`: retrieve twice, print sizes and SHA-256 of both retrieves, whether
  they are byte-identical (is retrieve deterministic?), and against `--file` the size, hash,
  and first differing offset. Read-only on the device.
- `blob-watch --minutes N`: retrieve, stream for N minutes with a live gaze marker, retrieve
  again, report the diff. Answers "does the firmware mutate the model during use?"
- Also run `blob-info` before and after a power cycle **[user]**. Record all three answers
  in DESIGN.md §10c; A1's verification mode depends on them.

### A1. Connect-time upload, Windows order
- `Device::connect_with(ConnectOptions)` where options carry the blob bytes, the display area,
  and a `double_upload` flag. Sequence: hello → realm → `cal_apply(blob)` → set display area
  → enabled eyes = 3 → unpause → (second `cal_apply` if flagged) → subscribe. Keep the old
  `connect()` for blob-less use (`info`, `dump`, a fresh device).
- Verify after upload: `cal_retrieve` and compare byte-exact (A0 result 2026-08-28: retrieve
  is deterministic on this unit). Mismatch is an error, not a warning.
- `Et5Calibration` gains `device_blob_sha256`; `device_blob_bytes` is dropped (format bump).
  The provider builder gains `.device_blob(path)` defaulting to `config/calibration-et5.bin`
  and logs the live blob hash on every connect.
- Verification: unit tests for the sequence against a scripted transport (ordering is the
  contract); `gaze-proto --provider et5` starts and logs the hash **[user]**.

### A2. `cal-restore` becomes `blob-push`
- Same path as A1, with the plane re-declared and eyes/pause re-sent after the apply.
- Mask SIGINT for the duration of the apply; print the unplug recovery note if anything goes
  wrong mid-transfer.

### A3. Re-enumeration
- On transport error the provider reconnects with A1 (re-upload, re-verify) and logs it.
  Today's behaviour on the observed one-second USB drop is to be checked first; whatever it
  is, the sample stream must carry an explicit `invalid` during the gap, never a stale hold.

### A4. The decisive test **[user]**
- Push the 16:24 blob with A1, run `view`. If the 16:24 behaviour is back, the regressions
  were device state. Record the verdict in DESIGN.md §10c either way.

### A5. Retrain ceremony, Talon-shaped (`calibrate`; confirms before opening the device)
- Rounds with `POINTS_APPLY` after each: centre; four mid-radius; four corners of the
  training area. Training area: the firmware's comfortable envelope, not the whole panel —
  start with Talon's 600×340 mm bottom-centred on the tracker axis and record what was used.
- Gaze-gated point acceptance: add a point only after the device's reported gaze has been
  the nearest to that target for ≥ N frames (Talon uses 60 of the last 120).
- A second pass on a white overlay background so the firmware model sees both pupil
  extremes — corners only, thirteen points in all. **The device's point store is a FIFO
  of 14 points (or ~640 KiB; the 2026-08-28 11:56 run fed 18 and kept the newest 14 at
  ~46.6 KB each, 862 bytes under 640 KiB).** The ceremony reads the trailer back and
  reports what the device kept.
- `--suggest`: query 0x442 and log what the device asks for (exploratory; do not depend on it).
- Save the blob and its hash; start a fresh history keyed to the hash. The plane stays the
  measured one from `desk.toml`.
- The old data pass, field fit, head-gain fit, and triangulation diagnostics are removed
  from `calibrate`; recording moves to Phase B. Keep the pose solver and the lag/saccade
  helpers as library code (B2 uses them).

## Phase B — Recording

### B1. `record` command
- A five-minute session that only records: stop grid on black (4×3 over the device display — `--cone-deg` narrows it to the
  tracker's cone, but the default covers the panel because the model needs rows where the
  user actually looks and `angle_axis_deg` carries the cone per row; stops
  as today), a low-discrepancy wander on white with posture prompts (as `collect` does
  today), then the same grid on white. Ends without fitting.
- Output `config/sessions/<unix>-<blobhash8>.jsonl`. First line `kind: "meta"`: session id,
  blob hash (retrieved at start), display area, `desk.toml` hash, glasses flag (prompted),
  free-text note. Every `stop`/`traj` record gains `background`. Frames as today
  (`Et5Frame` verbatim). Last line: blob hash retrieved at the end.
- `collect` and the old readings archive are retired; the 16:24 readings file is converted
  by a one-off `sessions import` so it can serve as session zero.

### B2. `dataset` module (Rust) + `dataset export`
- Session files → rows: features (below), target ray in tracker space, angular residual of
  the firmware's combined ray, and grouping keys (session, hold/stop index, background).
  Glides lag-shifted and saccade-gated with the existing helpers; stops use the per-stop mean
  plus the per-frame rows.
- `dataset export --csv|--parquet` for Python. Unit tests on synthetic sessions.
- Feature vector: raw eye origins L/R (6), per-eye direction yaw/pitch (4), interocular
  vector (3), pupil L/R (2), validity flags, angle of the combined ray from the tracker axis,
  and the head features again lagged 300 ms.

### B4. Passive click labels (`gaze-clicks`)

A background collector that runs all day while the user works the mouse normally. People
look at what they click, so every deliberate click on a recognised control is a labelled
gaze sample with no calibration screen and no dot: the element's box is the target, the
frames around the press are the observation. It feeds the same model B2 exports for, and
it is the data source E1/E2's flywheel acceptance will eventually run on.

- **Reads the real mouse read-only, never `EVIOCGRAB`.** The compositor keeps every
  event. Default node is the one named `input-remapper mouse` (the clone cosmic-comp
  reads on this desk); `--mouse`/`--mouse-name` override, fallback is the first device
  with `BTN_LEFT` that is not a keyboard. `BTN_LEFT` and `BTN_RIGHT` only; the wheel and
  the side buttons point at no element.
- **Capture on the press, not from a rolling buffer.** Everything that destroys a click
  target fires on the *release* (menu activation, navigation, popup dismissal), so
  between press and release the target is still under the pointer; pressed-state
  highlights and popups opening underneath are harmless. The evdev reader fires
  `capture_output` the moment the press arrives and that frame is used if it lands
  within 150 ms (measured: 20 to 40 ms). A 1 Hz rolling capture is the fallback only,
  never used past 500 ms old or after the release; using it tallies `late-capture`.
- **Recognition around the pointer, via `Detector::detect_near`.** Smallest accepted box
  containing the pointer wins (boxes nest; the innermost is what was aimed at). `Unknown`
  boxes are refused even as containers. `Icon` and `Slider` are accepted and recorded as
  themselves so the export can filter them.
  - Two opposite trades against the whole-frame pass. **Widgets**: the *same* tile plan,
    restricted to the one to four tiles that contain the pointer, so the model still sees
    a wide flat row inside its real 1024 px surroundings. Nothing is lost, because a box
    is only emitted by a tile that holds it whole; checked at 26 points across DP-1 and
    DP-2 with the gates off, the non-text boxes containing the point were identical to the
    full pass at every one. **Text**: native resolution over a 640 px window instead of the
    frame shrunk to 1600, which on a 3840 px panel was merging paragraphs into 800x500
    blobs; locally it gives line-level boxes (14 to 25 px tall) for about 62 ms. Measured
    on DP-1 under load, mean of eight: 42 ms widget + 66 ms OCR = 109 ms, against 384 ms
    whole-frame.
  - Three gates on top, because a collector wants a click target rather than a snap
    candidate: widget score at least **0.5** (the 0.25 snap default was tuned for recall
    and every box under 0.5 on the reference captures sat over styled prose), widget size
    at most **1200x240 frame pixels** applied *before* OCR fusion so a spurious panel box
    cannot swallow the text lines inside it, and a **flat check**: a box taller than 60
    logical px whose ±24 px pointer window has a luma standard deviation under 0.02 is
    tallied `blank`, because the model drew a control over empty pixels.
  - A ±256 px crop was tried first, for the ~10x saving over a full frame, and it loses
    every wide flat widget. Measured on identical pixels (one capture of DP-2, detected
    whole as the reference, then re-detected as crops around the twelve largest widget
    centres): the crop's pick agreed **0/12** at both 512 and 640 px, returning the
    widget's inner OCR text or nothing. Channel rows, member rows, the URL bar, links.
    Small square widgets survive cropping, which is what made it look like it worked.
    The widget model needs the surrounding layout; the failure is context, not scale.
    `detect_near` is not that crop: it feeds the model unchanged tiles of the unchanged
    plan. `crates/gaze-clicks/examples/recognition_check.rs` reruns the measurement.
  - The ±`--luma-px` window survives only as what `crop_luma` is averaged over.
- **Two threads, because recognition is slow.** A detection is tens to a couple of
  hundred milliseconds (250 to 400 ms before it went pointer-local) and a press capture
  has to happen within tens of milliseconds of the press, so
  perception is split: a **capture thread** owning `Capture` and `CursorTracker` (pointer
  polling, press and rolling captures, and the frame choice, since the capture times live
  there), and a **detect thread** owning the `Detector`, fed already-chosen `(request,
  frame)` pairs. Nothing on the capture thread may block for longer than one capture: the
  hand-off is a `try_send` onto a queue three deep and a full queue refuses the click
  (`overrun`) rather than waiting. So a double click's second press is captured while the
  first is still being recognised. The detector is built on the detect thread, which
  sidesteps moving an `ort` session between threads.
- **Rejections, in order:** `drag` (held > 400 ms or moved > 6 px), `off-desk` (an output
  `desk.toml` does not describe), `stale` (no usable frame), `overrun` (the detector was
  three frames behind), `no-element`, `blank` (a large box over flat pixels), `no-gaze`
  (under 20% of the frames in `[t_press - 0.6, t_press]` carrying a valid combined gaze).
  `no-element` and `blank` are the ones that matter: a focus click on empty space says nothing about gaze and is the most
  common press on a desktop.
- **Output** is B1's session format verbatim, one file per on-device eye model:
  `config/sessions/<unix>-<blobkey>-clicks.jsonl`, rotated if the tracker reconnects
  holding a different blob body hash. Per click: a `"stop"` (`phase: "click"`,
  `background: "screen"`, window `[t_press - 0.6, t_press + 0.1]`), a new `"click"`
  record (`n`, button, output, px, press/release times, `moved_px`, `multi`, the element,
  `crop_luma`, `frame_age_s`), and the `"frame"` records of
  `[t_press - 1.2, t_press + 0.4]`, deduplicated across overlapping clicks. Flushed per
  click; a session killed without its `meta_end` line still loads.
- **B2 changes.** Stops are now resolved against the record's own `display` rather than
  the meta line's, because a click lands on any of the three panels while the meta line
  names only the display the tracker's plane is declared on. Rows from a `click` stop get
  `hold_key = click_<n>`, `session_phase = "click"`, and four extra columns
  (`element_kind`, `element_w_px`, `element_h_px`, `crop_luma`), empty or NaN elsewhere.
  Unknown record kinds are skipped, so an older reader still loads a click session.
- **Provider change.** `Et5Provider::next_frame(timeout)` yields raw `Et5Frame`s with the
  same connect-time upload and reconnect behaviour as `next()`, reporting a gap as an
  absence of frames rather than as invalid samples; `connects()`, `device_blob_report()`
  and `device_display_area()` support the session meta and the rotation check.
  `next()` is unchanged.
- **Live feedback.** A line per accepted click, and a ten-second status line with the
  tallies, the running median firmware offset over the last twenty accepted clicks, and
  the median detector time over the same window.
  That offset is the angle between the firmware's filtered gaze point and where the user
  clicked, measured the way `retrain::run_health` measures its grid, which makes it a
  free daily drift number.
- `--no-tracker` runs the whole pipeline without the device (clicks, recognition,
  tallies, click records) and is the test mode. `devices` lists candidate nodes; `probe`
  prints and outlines the element under the pointer four times a second (`--hz`), with the
  capture-to-draw latency, through the same `element::pick` a click goes through.

### B5. Driven labels from a real application (`gaze-trainer`)

The five-minute dot ceremony does not produce accurate data (ruled 2026-09-03 after
eight attempts), and passive clicks are accurate but slow and biased toward wherever
the user's applications put their controls. The trainer is the third source: a
libcosmic application the user simply navigates, generated fresh in one of six
archetypes (settings, files, mail, editor, browser, store) so the layouts differ, and
that knows its own widget boxes. No tasks, no instructions, no targets (ruled
2026-09-03 after the first driven session): the clicks are whatever the user finds
worth clicking, which is what makes them ordinary clicks rather than target
acquisition.

- **Labels come from the application, not from recognition.** Every control is wrapped
  in a `Probe` that reports its box on draw and publishes its label on a press,
  synchronously in the event pass. The press goes to the collector over
  `$XDG_RUNTIME_DIR/gaze-clicks.sock` as one JSON line (`gaze_core::trainer`), in
  window-local coordinates; the collector saw the same press in global coordinates
  and the difference is the window origin, which cancels the toolkit's coordinate
  space. Matched by wall-clock time within 150 ms. A matched press skips the tree and
  the recogniser (`source = "trainer"`, score 1); a press on nothing labelled is
  refused as `no-element`. Probes never nest, so one probe speaks per press.
- **Coverage steering.** An 8×4 histogram over the window, seeded from every `click`
  record in `config/sessions/` on the trainer's output, biases the *layout* rather than
  choosing a target: the share of labels in the left half and in the top half become
  the probabilities of putting the next window's sidebar on the right and its toolbar
  at the bottom (clamped to [0.2, 0.8]), so the emptier half fills first. The rest of
  the layout varies per window anyway: sidebar width, density, dialog anchor.
- **Posture prompts** every 80 labelled presses (`--posture-every`), cycling normal /
  back / in / left / normal / right / tall / slouch; the posture stays on the wire until
  the next prompt. **Theme** flips dark/light per window for the pupil; the theme
  background's luminance is the click's `crop_luma`.
- **Records.** `ClickRecord` gains `trainer: Option<TrainerTag>` (task, step, hit,
  posture, theme). `task` is now the generated window's number and `step` is reserved
  at 0; `hit` now means "a labelled control was under the press", so a press on
  padding or the header bar is the only `hit = false`. The export gains `source`,
  `posture`, `trainer_task` and `trainer_hit` columns, so a window can be held out
  whole and passive clicks can be the generalisation check for trainer clicks.
- **Open questions for Phase C.** Whether deliberate trainer clicks predict passive
  ones (train on trainer, test on passive sessions); whether unlabelled (`hit = false`)
  presses are worth keeping; whether the eye leads the click by the same margin in both.

### B3. Collection **[user]**
- ≥ 6 sessions over ≥ 3 days, morning and evening, glasses state noted. Nothing else moves
  (tracker, monitors, desk config) during the collection window.

## Phase C — Model prototype (Python, `uv`, `crates/gaze-provider-et5/model/`)

### C1. Loader and features from B2's export; grouped splits by session.
### C2. Baselines on identical splits: firmware only; a quadratic uv field per session
  (what we had); a global quadratic with head terms (what we had, pooled).
### C3. Model: kernel ridge / sparse GP (ARD RBF, 500–1000 inducing points) on the residual,
  with a fixed radial prior term in angle from the axis and a per-eye linear pupil term.
  Hyperparameters by grouped CV.
### C4. Report: leave-one-session-out RMS / p50 / p90 in degrees, per-session bias before and
  after, pupil-diameter-vs-error regression, and predicted-variance-vs-error correlation
  (does the confidence mean anything). Written into DESIGN.md §10c.
- **Gate to Phase D:** session-out RMS at least 25% below the best baseline and no session
  worse. If the residual is dominated by a per-session constant uncorrelated with any
  feature, Phase D shrinks to D5 (the online offset is the whole answer).
  **Outcome 2026-09-04:** 21% on the per-click median, one session worse; neither
  branch cleanly. The field is real (every trainer session −30%) and so is the day
  bias (the per-session offset oracle on top takes it to −33%). D1–D3 built for the
  feel; D5 next.

## Phase D — Rust runtime

### D1. `model.rs`: kernel model (inducing points, weights, lengthscales, variance solve),
  `predict(&Features) -> (dyaw_deg, dpitch_deg, var)`, serde in the calibration file.
  **Built 2026-09-04**, as its own file `config/model-et5.json` (800 KB of centres and
  the variance form is not TOML material), keyed to the blob hash and the mount pitch.
### D2. `train.rs`: fit from B2 rows with `nalgebra` Cholesky; grouped-CV grid for the
  hyperparameters; `gaze-et5-cli fit` writes the calibration. Must reproduce C3's numbers
  on the same export (test). **Built**: no `nalgebra` (a Jacobi eigensolve, a Cholesky
  and k-means++ in 300 lines); no grid at fit time, the hyperparameters are the harness's
  pick and are flags; `fit` reproduces `loso_clicks.py` (1.82 → 1.43 vs 1.83 → 1.44).
### D3. Provider: correction applied to the firmware ray before intersection; variance →
  σ profile; correction faded to zero above a variance threshold. Field and head gain
  removed. Calibration format v2 (older files refused). **Built**: field and head gain
  bypassed rather than removed when a model is loaded; the corrected ray is intersected
  with the configured desk rotated by the pitch the labels used, not the solved poses.
### D4. `EyeCombiner`: weights from rolling pupil-signal variance through a sigmoid with a
  moving average (Tobii [patent reference removed]), times a per-eye residual weight by region from the
  training data.
### D5. Online offset: yaw/pitch bias with exponential forgetting (minutes), reset when the
  head state jumps past a threshold, updated only by accepted flywheel pairs (E2).
  **Built 2026-09-04** as `offset.rs` plus the provider's `observe_click`: forgetting is
  per click (gain 0.1, so about ten clicks) rather than per minute, the innovation is
  clipped at 2° and gated at 3° (E2's foveal tolerance, no RANSAC yet), and there is no
  head-jump reset (the simulation tracked the trainer's posture changes without one).
  Persisted to `config/offset-et5.json`, keyed to the blob. Simulated 1.44 → 1.20°.
  **Reworked 2026-09-05** after the single bias broke on a slouch: the bias is now a
  function of head position, a set of anchors (one per posture, 60 mm reach on the
  binocular midpoint) each holding its own yaw/pitch and blended by distance, a new
  posture inheriting the blend and closing most of its gap in ten clicks. Daydream pad commits feed it alongside mouse presses.
  The file format changed; an old file starts cold.

## Phase E — Flywheel

### E1. Pair logging in `gaze-proto`: on every real click (read-only evdev on the real mouse,
  pointer from `CursorTracker`), attribute the fixation from the ring buffer with a
  stimulus-type window (gaze precedes the click) and log (features, click px, fixation
  stats) to `config/flywheel/<date>.jsonl`. Snap commits log the same record.
  **Partly built 2026-09-04** (`gaze-proto/src/feedback.rs`): the real mouse, the
  pointer and the attribution (0.4 s before the press, the window `label_timing.py`
  found flat) feed D5 directly, one log line per press. Not built: the JSONL record with
  features. Mouse-button snap commits are *not* offered (they are the gaze clicking, and
  the feed already sees the physical press); Daydream pad commits are, since 2026-09-05,
  refined or not.
### E2. Acceptance: foveal tolerance against the current prediction (start at 3°); rejects go
  to an error buffer whose overflow raises "recalibrate" in the log; RANSAC over the recent
  buffer before any parameter update; ≥ 4 observations and a spatial-spread check.
### E3. Retrain pooling sessions plus accepted pairs (down-weighted), on provider start or a
  timer; the previous calibration is kept as a fallback and the new one must not be worse on
  the held sessions.
### E4. Later: reading-line vertical drift from OCR boxes; typed-text read-back offset.

## Order and parallelism

A0, A1, A2, A3, A5 code, B1, B2, B4, B5 and the C1–C4 harness (against session zero) are all
agent work and mostly independent; A1 first because everything else connects through it. A4 and
B3 are the user's, and Phase C's real result waits on B3. D1–D2 can start against C3's
exported model before the gate if the harness is ready; D3–D5 and E wait for the gate.

## Agent working rules

- Own `crates/gaze-provider-et5` (plus `Cargo.lock`); `gaze-proto` only for E1. `gaze-core`
  changes are described in the report, not made, unless the task says so.
- Style and verification per CLAUDE.md: zero warnings, clippy clean, docs on every item.
- Anything that talks to the device: `blob-info`-class read-only commands may be run
  unattended; anything that uploads, retrains, or needs a fixated user is **[user]** — build
  it, state what was and was not run, and how to run it.
- Never send `cal_points_apply` or `cal_apply` from a test or a CLI default; the firmware
  model is write-once by policy.
- Every experiment result goes into DESIGN.md §10c with the date, the blob hash, and the
  numbers. A number without its split (session-out vs in-sample) is not a number.
