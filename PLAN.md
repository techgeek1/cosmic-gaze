# Phase 0 build plan: does gaze + vision snapping work at all?

**Status 2026-08-26: complete. Verdict and numbers in DESIGN.md §10b. Next phase: fine channel
(Daydream controller daemon + refine modes in gaze-proto), then online recalibration.
ET5 provider rebuild (host-owned device state, the retrain ceremony): `PLAN-ET5.md`. The
Phase 0 apparatus this plan describes (the synthetic provider, the bench, the truth
scoring, the noise model) was removed 2026-09-10 once the ET5 was the only path; the
contracts below are kept as the record of what was built and why.**

Goal: a number. First-commit snap-correct rate on a real mixed desktop at sigma = 0.7
degrees (ET5-class) and 1.5 degrees, using detector boxes only (no a11y), driven by a
synthetic gaze provider. >= ~90% at 0.7 degrees means the idea is in the realm of
possibility. See DESIGN.md section 10.

Two experiments, sharing crates:

- **Offline (the number).** Screenshots of the real desktop -> detector boxes ->
  Monte Carlo: for each box as the intended target, sample where a human fixation would
  land (inside the box, biased to centre, plus provider noise in degrees through the desk
  geometry) -> snap -> did it pick the intended box? Report by sigma, by output, by box
  size, by element kind. Also detector recall by eye: overlay images for manual review.
- **Live (the feel).** Grabbed second mouse -> synthetic provider -> filters -> snap ->
  overlay highlight -> keypress commit -> uinput click. Exercises the precision-cone
  handoff, hysteresis, late-trigger attribution.

## Environment facts (verified 2026-08-25)

- cosmic-comp exposes `ext_image_copy_capture_manager_v1`,
  `ext_output_image_capture_source_manager_v1`, `zwlr_layer_shell_v1` v5,
  `zxdg_output_manager_v1`, `zcosmic_toplevel_info_v1`. No `zwlr_virtual_pointer`, no
  `zwlr_screencopy`. Portal has ScreenCast only, no RemoteDesktop (so no libei path).
- `/dev/uinput`: user has an ACL (rw). `/dev/input/event*`: group `input`, user is NOT a
  member yet (being fixed). Code must fail with a clear message, not panic.
- Mice present: Logitech G502 (the real mouse) and a PixArt Lenovo USB optical mouse (the
  one to grab as the gaze device). Keychron Q6 Max keyboard.
- Outputs: DP-1 LG 38GN950 3840x1600@1 at (2559,0); DP-2 VIOTEK GNV27DB 2560x1440@1 at
  (0,160); HDMI-A-1 1920x1200 physical, fractional scale ~1.15 (logical 1670x1043; `wl_output.scale`
  reports 2 and is wrong, use `zxdg_output_v1`) at (1506,1600), occasionally drops off
  the output list and comes back. Layout in `config/desk.toml`.
- GPU: AMD RX 7900 XTX, gfx1100, 24 GB (no CUDA; torch ROCm wheels work, no system ROCm needed). CPU: Ryzen 9 5950X. Inference is CPU via system
  onnxruntime 1.29 (`/usr/lib/libonnxruntime.so`), `ort` crate 2.0.0-rc.12 is cached.
- Rust 1.95, edition 2024. Python 3.14 with `uv` for model export scripts (no torch
  installed system-wide; use a `.venv`).

## Crate map and contracts

All crates depend on `gaze-core` for types. Boxes are global logical px. Angles in degrees.

### gaze-core (DONE; also has `off_axis_deg(&ray)`, `tracker()`, `OutputGeometry::normal_at`)
`DesktopGeometry`: cylindrical/flat surfaces, `intersect`, `px_per_deg` (numeric
Jacobian, central differences of ~2 px), `perturb_ray`. `SigmaProfile::sigma_at`.
Tests: round-trip `px -> world -> ray from eye -> intersect -> px` within 0.01 px on all
three outputs of `config/desk.toml`; flat panel as the large-radius limit of the cylinder;
`px_per_deg` at the seam roughly 55-65 px/deg for the LG at 650 mm; sigma profile
breakpoints.

### gaze-provider-synthetic (removed 2026-09-10; the `GazeProvider` trait lives in `gaze-core`)
```rust
pub trait GazeProvider { fn next(&mut self) -> Option<GazeSample>; fn stop(&mut self); }
pub struct SyntheticProvider;   // SyntheticProvider::create().device(path).geometry(g).model(m).start()?
```
Opens an evdev device by path or by name substring (default `"Lenovo"`), `EVIOCGRAB`s it
so the compositor never sees it, integrates REL_X/REL_Y at a configurable px-per-count
gain into a virtual gaze point in global px, clamps to the union of enabled outputs (jump
across the seam is allowed: outputs are adjacent in logical space). Emits samples at
`model.rate_hz` on a background thread over a `crossbeam_channel`. Each sample: lift the
clean point to a ray from `geometry.eye()`, compute off-axis angle vs the tracker axis
(tracker -> eye direction), look up sigma, perturb the ray by N(0, sigma) in both axes
plus drift, re-intersect, fill `point`, `ray`, `sigma_deg`, `valid`. Also exposes the
clean point for ground truth (`fn truth(&self) -> GlobalPx`). A `ReplayProvider` that
reads a JSONL of samples is cheap and useful for tests; add it. Bin: `gaze-provider-cli`
prints samples; `--truth` prints clean vs noisy.

### gaze-capture (DONE; ~35 ms per ultrawide frame; also gaining `CursorTracker` for pointer position)
```rust
pub struct Capture;   // Capture::connect()? ; fn outputs(&self) -> Vec<OutputInfo>
pub struct Frame { pub output: String, pub logical: Rect, pub width: u32, pub height: u32, pub rgba: Vec<u8>, pub t_s: f64 }
fn capture_output(&mut self, name: &str) -> Result<Frame>;
fn capture_all(&mut self) -> Vec<Result<Frame>>;
```
`ext_image_copy_capture_v1` over `wayland-client` + `wayland-protocols` (the `ext`
protocols are in `wayland-protocols` 0.32+ under the `unstable`/`staging` features; check)
with `wl_shm` buffers. Use `zxdg_output_manager_v1` for logical position/size and names.
Outputs may appear/disappear; re-query on each capture. Bin: `gaze-capture-cli --out
screenshots/` writes `<output>-<n>.png` for each output, `--loop 2` captures every 2 s.
Also implement `fn changed_fraction(a: &Frame, b: &Frame) -> f32` (downsampled absolute
diff) as the frame-diff trigger.

### gaze-detect (DONE; TargetFinder yolo26n-640 + PP-OCRv5 det, ~380 ms per ultrawide frame at 8 threads; class 5 -> ElementKind::Slider)
```rust
pub struct Detector;  // Detector::load(models_dir)?  ; fn detect(&self, frame_rgba: &[u8], w: u32, h: u32, origin: GlobalPx, scale: f64) -> Vec<Element>
```
Two ONNX models run with `ort` against the system onnxruntime: **TargetFinder**
(arXiv 2607.19907, YOLO26n fine-tuned desktop widget detector, on PyPI, weights on HF)
for widget boxes with kinds mapped into `ElementKind`; **PP-OCR detection stage** (v5 or
v6 det model, e.g. from RapidOCR's ONNX distributions) for text boxes as `ElementKind::Text`
/ `ElementSource::Ocr`. Ultrawide frames must be tiled (e.g. 640 px tiles with overlap,
NMS across tiles) because a 3840x1600 frame downscaled to 640 loses every small target.
Export/download via a `uv` script in `crates/gaze-detect/scripts/`, models into
`models/` (gitignored), README documents licenses and the exact commands. Bin:
`gaze-detect-cli image.png --json out.json --overlay out.png` and `--bench` printing ms
per frame. Target: well under 500 ms per ultrawide frame on the 5950X.

### gaze-snap (DONE; API as built)
```rust
pub trait PxScale { fn px_per_deg(&self, p: GlobalPx) -> (f64, f64); }   // impl for DesktopGeometry (falls back to 60 px/deg off-screen) and ConstPxScale
pub enum FixationState { Fixating { since_s: f64 }, Saccade, Lost }
pub struct Filtered { pub sample: GazeSample, pub state: FixationState }
FilterStack::create().scale(Box<dyn PxScale>).velocity_threshold_deg_s(30.0).window_s(0.02).one_euro(min_cutoff_hz, beta).build();
fn push(&mut self, sample: GazeSample) -> Filtered;
SnapEngine::create().scale(Box<dyn PxScale>).radius_deg(2.0).hysteresis_margin(0.15).ring_window_s(1.5).build();
fn update(&mut self, f: &Filtered, elements: &[Element]) -> Option<SnapTarget>;
fn commit(&mut self, t_s: f64, latency_s: f64) -> Option<SnapTarget>;   // late-trigger: attributes to the target fixated at t_s - latency_s
fn current(&self) -> Option<&SnapTarget>;  fn candidates(&self) -> &[Candidate];
pub struct SnapTarget { pub element: Element, pub point: GlobalPx, pub score: f64 }  // score is a COST in degrees, lower wins
```
Scoring: `cost = 0.6*kind_penalty + 0.15*ln(1+area_deg2) + 1.0*distance_deg` (controls 0, Text 1, Unknown 2).
Bails to `None` when `sigma_deg > radius_deg` or the sample is invalid.

### gaze-overlay (DONE; `Overlay::spawn() -> (OverlayHandle, JoinHandle)`, `OverlayHandle::set(OverlayState)` / `stop()`, `OverlayState { gaze, highlight, label, background, pointer, mark }`; the pointer look is PLAN-UX.md U1, built 2026-09-09; the truth cross went 2026-09-10)
```rust
pub struct Overlay;  // Overlay::connect()? ; fn set(&mut self, state: OverlayState); OverlayState { gaze: Option<GlobalPx>, highlight: Option<Rect>, truth: Option<GlobalPx> }
```
One `zwlr_layer_shell_v1` overlay-layer surface per output, transparent, no keyboard
interactivity, input region empty so clicks pass through. Draw a gaze marker, the
highlighted candidate box, and (debug) the truth marker. `wl_shm` + `tiny-skia` is
enough; no GPU. Redraw only when state changes. Must survive outputs appearing and
disappearing. Bin: `gaze-overlay-cli` animates a marker across all outputs.

### gaze-inject (DONE; closed-loop relative backend on `gaze_capture::CursorTracker`, lands within 1 px on all outputs; cosmic-comp maps absolute devices to one output only)
```rust
pub struct Injector;  // Injector::create()? ; fn click_at(&mut self, p: GlobalPx, button: Button) -> Result<()>; fn move_to(&mut self, p: GlobalPx) -> Result<()>; fn scroll(&mut self, p: GlobalPx, dy: i32)
```
uinput. First test whether an absolute-axis device (`ABS_X`/`ABS_Y` with `INPUT_PROP_DIRECT`
or pointer-style) maps to the whole layout in cosmic-comp or to a single output; if the
latter, fall back to relative motion with a known start (move far negative to corner-home,
then relative delta) and document the choice. Bin: `gaze-inject-cli --move x y`,
`--click x y`, and `--probe` that walks the corners of each output and reports (by asking
the user to confirm) where the cursor landed.

### gaze-clicks (the collector removed 2026-09-10; the mouse reader and the tree thread remain)
```rust
gaze-clicks-cli devices | presses
```
Was the passive click collector (PLAN-ET5 B4): the real mouse read-only, the output
under the pointer captured on the press, the element recognised, the frames written as
a session file for the residual model's fit. It went with the model. What the session
still uses stays: `mouse::MouseReader` (every mouse-shaped evdev node, never grabbed,
rescanned, now only behind its CLI since the online offset went on 2026-09-12), and `tree::TreeService`, the
watched accessibility-tree thread the verifier and the edge scroller ask. See
`crates/gaze-clicks/README.md`.

### gaze-bench (removed 2026-09-10)
The Phase 0 offline Monte Carlo over screenshots and synthetic sigmas. Its report was
never regenerated after the snap rules changed. The reports it produced are in the
history (`crates/gaze-bench/report*.md` before the removal).

### gaze-proto (DONE; now the dev harness over the session library `gazed` runs; ~0.45 core idle, ~8 cores while detecting)

Had `--provider synthetic|webcam|replay`, the Lenovo's buttons as the commit device and
the scroll tier (2026-08-26). All of that is gone: the webcam provider and its sidecar
on 2026-09-10 (the ET5 having replaced them), the scroll tier with the borrow-and-return
model of 2026-09-09, and the synthetic and replay providers with the grabbed mouse, the
recording and the truth scoring on 2026-09-10 (the ET5 is the only source; the
controller commits). What follows is the record of those modes:

- **What commits.** The same three buttons on the Lenovo in every mode, and the device is
  `EVIOCGRAB`ed in every mode: `synthetic` grabs it as its gaze device, `webcam` and
  `replay` grab it through a buttons-and-wheel-only reader that discards motion. `--no-grab`
  opts out for those two, at the cost of every commit press also being a real click. Only
  `synthetic` has a truth point, so only its commits are graded; the others are counted and
  timed. A `webcam` run also logs a `sidecar health` line (connected, lines, bad lines,
  frames, mean conf, mean latency) so a sidecar that is up but not tracking is visible.
- **`--scroll`** routes the real wheel to the window under the gaze point: warp the pointer
  when gaze is more than `--scroll-warp-deg` (3) away, at most once per 300 ms unless gaze
  moved that far again, then scroll. On a grabbed device the scroll is re-injected (the
  compositor never saw it); under `--no-grab` only the warp happens, because the compositor
  is already delivering the user's own wheel.
- **`--focus-follows-gaze`** warps (no click) after `--focus-dwell-s` (0.4) of fixation on
  an output the pointer is not on. `--dry-run` gates clicks, scrolls and warps alike.
- **`--edge-scroll`** (2026-09-04) scrolls the surface under the gaze while the eyes dwell
  in its lower `--edge-band` (0.12 of its height) or upper `--edge-top-band` (0.12), after
  `--edge-dwell-s` (0.25) / `--edge-top-dwell-s` (0.5), at `--edge-max-lines-s` (8) times
  the depth into the band (`--edge-exponent` 1), ramped over `--edge-ramp-s` (0.15). Holding
  the outer band past `--edge-hold-s` (0.3) multiplies the speed by `--edge-hold-gain` (2) per
  second, tracked eyes past the screen edge multiply it by `--edge-turbo` (3), all capped at
  40 lines/s. The
  surface is the real clipping node from the accessibility tree (`gaze_a11y::clip_surface`,
  asked on `gaze-clicks`'s watched tree thread), never the window; no tree, no scroll. The
  pointer is warped into the surface once at the start if it is outside; the motion goes
  out as `REL_WHEEL_HI_RES` at the sample rate. A band with no content left in its
  direction is not a band, so the end of a page can be looked at and clicked. When a
  scroll stops the output under the gaze is re-detected first and the highlight is hidden
  until that detection is published. `--dry-run` gates it like everything else.
- **`--daydream`** (2026-09-04) reads the Daydream controller (`gaze-daydream`, BlueZ over
  D-Bus) beside the mouse: the pad's click commits, a tap on the pad (under 250 ms, no
  travel) commits with the right button, Home exits, App redetects, the volume keys are a
  wheel (repeating while held). The controller's gyro says whether it is in a hand (over
  0.035 rad/s within 6 s, or any touch or button), and the real mouse moving takes the pointer
  on the spot until the controller is touched, clicked or swung past 0.25 rad/s (a lift off the
  desk); put down or ceded to the mouse, gaze moves nothing (no edge
  scroll, no focus warp) and the mouse has the pointer. A thumb resting on the pad is the
  fine channel:
  it captures the snap point (the gaze point when nothing snapped), warps the pointer
  there, locks the gaze out, and `--refine touch` (default; `--refine-touch-gain` 250 px
  per pad width) or `--refine gyro` (`--refine-gyro-gain` 1500 px/rad, `--refine-axes`
  `-y,-x`) moves it as plain relative mouse motion inside a `--refine-range` 100 px box
  around the anchor; the next commit clicks where the pointer is. Lifting the thumb keeps
  the point until a commit or a retarget, and a thumb landing again resumes the drag; a
  lift that moved under 2 px is forgotten.
Live loop wiring provider -> filters -> snap -> overlay, capture+detect on a background
thread gated by `changed_fraction`, commit on a keypress read from the grabbed mouse's
buttons (simplest: left button on the Lenovo = commit), click via injector. Logs hits vs
misses using `truth()`.

## Agent working rules

- Own only your crate directory (plus `Cargo.lock`). Do not edit `gaze-core` except to fill
  its `todo!()`s if that is your task; if you need a change there, describe it in your
  report instead.
- Add crate-local dependencies to your own `Cargo.toml`; prefer versions already in
  `~/.cargo/registry/cache`. Do not touch the workspace `Cargo.toml`.
- Other crates are being built concurrently: `cargo build -p <yours>` only, and expect the
  target-dir lock to serialise builds occasionally.
- Anything needing a live compositor or device: build it, try it once if you can, and
  report exactly what happened. Never run anything that moves the real pointer or clicks
  without the user present, except `gaze-inject-cli --probe` which is designed for it.
