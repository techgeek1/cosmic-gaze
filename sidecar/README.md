# `sidecar/` — `gaze-ml`

The GPU half of cosmic-gaze. The Rust side orchestrates; this process does nothing but
turn webcam frames into `{eye_mm, gaze, head_rot}` records on a Unix socket, one JSON
object per line, at camera rate.

It is a separate Python process for one reason: the only usable open gaze models ship as
PyTorch checkpoints, and the ROCm wheels bundle their own runtime. Nothing in here knows
about displays, snapping, or the desk geometry.

## Install

```sh
cd sidecar
uv sync --all-groups        # creates sidecar/.venv (Python 3.12) with torch+ROCm 7.0
uv run gaze-ml fetch-models # ~100 MB into sidecar/models/, gitignored
```

`uv sync` pulls `torch`/`torchvision`/`triton-rocm` from
`https://download.pytorch.org/whl/rocm7.0`, pinned in `pyproject.toml` via
`[[tool.uv.index]]`. **No system ROCm is required** — the wheels carry the runtime. The RX
7900 XTX is `gfx1100`, which those wheels support natively, so `HSA_OVERRIDE_GFX_VERSION`
must stay *unset*; setting it will break things, not fix them.

Verified on this box: `torch 2.10.0+rocm7.0`, HIP 7.0.51831, `torch.cuda.is_available()`
is `True`, device `AMD Radeon Graphics` / `gfx1100` / 24560 MB.

## Run

```sh
uv run gaze-ml serve --socket /run/user/1000/gaze-ml.sock --camera /dev/video0 \
                     [--width 1920 --height 1080 --fps 30] [--show]
uv run gaze-ml bench [--frames 200] [--input clip.mp4] [--show]
uv run gaze-ml dump-crops --out debug/crops [--sweep 1.0,1.4,2.0]
uv run gaze-ml fetch-models
```

Useful flags: `--intrinsics file.json`, `--hfov-deg`, `--device {auto,cuda,cpu}`,
`--no-fp16`, `--crop-scale`, `--min-score`, `--estimator {l2cs,iris,both}`,
`--iris-gain`, `--duration` (serve), `--json` (bench), `--no-fixed-framerate`,
`--no-threaded-capture`.

`dump-crops` writes the *exact* tensor the gaze network receives, denormalised back
to a PNG with the decoded angles in the filename, plus the full frame with the
detector box drawn. It is the first thing to reach for when the gaze output looks
wrong: an appearance model fed a bad crop produces stable, plausible nonsense, and
no amount of staring at the numbers will tell you that.

`--device auto` falls back to the CPU with a loud warning on stderr. `--device cuda`
refuses to start without a GPU rather than degrading silently.

## Wire protocol

One JSON object per line (`\n`-delimited), broadcast to every connected client:

```json
{"t": 81031.222515466, "seq": 68, "valid": true,
 "eye_mm": [122.46, 165.26, 613.55],
 "gaze": [0.4166, 0.1148, -0.9018],
 "head_rot": [0.2322, -0.2959, -0.0471],
 "conf": 0.9683, "lat_ms": 15.02}
```

| field | meaning |
|---|---|
| `t` | capture instant, `time.monotonic()` seconds (float) |
| `seq` | frame counter from the capture source, contiguous per run (int) |
| `valid` | a face was found and both PnP and the gaze net produced an answer |
| `eye_mm` | midpoint between the eye centres, camera frame, millimetres |
| `gaze` | unit gaze direction, camera frame |
| `head_rot` | Rodrigues vector, generic face model → camera frame |
| `conf` | face **detector** score in `[0, 1]` — see the note below |
| `lat_ms` | capture-to-send for this frame |

`--estimator both` adds two **additive** keys, `gaze_iris` and `gaze_l2cs`, carrying
the same unit vector in the same frame from each estimator so one run can be used to
compare them. `gaze` always mirrors the run's primary estimator, so a consumer that
ignores unknown keys sees an unchanged record. No other keys are ever added.

Camera frame is OpenCV's: `+x` right, `+y` down, `+z` out of the lens into the scene. A
subject looking straight into the lens therefore has `gaze ≈ (0, 0, -1)`. `head_rot` is
zero for a head facing the lens square-on.

When `valid` is `false`, `eye_mm`, `gaze`, `head_rot` and `conf` are all `null`; `t`,
`seq` and `lat_ms` still carry meaning, so a consumer can measure gaps.

The server accepts any number of clients. Each gets its own bounded queue (`--queue-depth`,
default 8) and its own writer thread; when a client falls behind, the **oldest** record in
its queue is dropped, because a stale gaze sample is worth nothing. Capture never blocks on
a consumer. Drops are counted and appear in the stats line.

Every 5 s a summary goes to stderr:

```
[gaze-ml] fps= 30.0 lat_mean=  13.4ms lat_p90=  17.4ms valid= 62% dev=AMD Radeon Graphics clients=2 dropped=0 skipped=17
```

## Models and licences

Weights live in `sidecar/models/` and are gitignored. `gaze-ml fetch-models` downloads all
three; the manifest lives in `gaze_ml/fetch.py`.

| model | role | licence | source |
|---|---|---|---|
| MediaPipe BlazeFace (short range) | face box + detection score | **Apache-2.0** (code and model) | `https://storage.googleapis.com/mediapipe-models/face_detector/blaze_face_short_range/float16/1/blaze_face_short_range.tflite` |
| MediaPipe Face Landmarker | 478 2D landmarks incl. irises | **Apache-2.0** (code and model) | `https://storage.googleapis.com/mediapipe-models/face_landmarker/face_landmarker/float16/1/face_landmarker.task` |
| L2CS-Net ResNet-50, Gaze360 | appearance-based gaze | code **MIT**; repackaged checkpoint published **MIT**; trained on **Gaze360, which is licensed for non-commercial research use** | `https://huggingface.co/py-feat/l2cs/resolve/main/l2cs_gaze360_resnet50.safetensors` |
| MediaPipe canonical face model | the 3D points PnP solves against | **Apache-2.0** | vertices baked into `gaze_ml/face_model.py`; upstream is `mediapipe/modules/face_geometry/data/canonical_face_model.obj` |

The Gaze360 term is the one that matters: **the gaze weights are research-use only.** The
architecture and this code are unencumbered, so a permissively-licensed replacement is a
retrain, not a rewrite — `GazeEstimator` takes any checkpoint with L2CS's key names.

Runtime dependencies are `numpy` (BSD-3), `opencv-python` (Apache-2.0), `mediapipe`
(Apache-2.0), `torch`/`torchvision` (BSD-3), `safetensors` (Apache-2.0). Nothing GPL or
AGPL. Note that `opencv-python` wheels are built without the non-free modules.

The test portrait used in development is
`https://upload.wikimedia.org/wikipedia/commons/8/8d/President_Barack_Obama.jpg` (public
domain, US federal government work). `tests/data/` is gitignored.

## Pipeline

1. **Capture.** OpenCV V4L2, MJPEG, 1920×1080@30. A drain thread reads continuously and
   hands the newest frame to the pipeline (see "Camera notes").
2. **Detect + landmark.** BlazeFace gives the box and the score; Face Landmarker gives 478
   mesh points. Both run in MediaPipe's VIDEO mode so it tracks between frames.
3. **Head pose.** `solvePnP` (EPnP + VVS refinement) of 16 named landmarks against the
   MediaPipe canonical face model, in millimetres. The model's origin is the midpoint
   between the eye centres, so the solved translation *is* `eye_mm` with no further
   transform. Mouth corners and the iris centres are deliberately excluded — see
   `gaze_ml/face_model.py`.
4. **Gaze.** Two interchangeable estimators, selected with `--estimator`:
   - `l2cs` (default) — L2CS-Net on a square, padded face crop resized to **448**, fp16
     on the GPU. Two 90-bin heads (4° per bin, spanning ±180°); the angle is the softmax
     expectation over bin centres.
   - `iris` — purely geometric, and 0.14 ms. The 478-point mesh gives the iris centres;
     the iris displacement from the eye-corner midpoint, normalised by eye width, is the
     eye-in-head angle up to a gain, and the PnP rotation carries it into the camera
     frame. One free parameter (`--iris-gain`, default 60°/unit, derived from a 12 mm
     eyeball and the canonical 25.9 mm corner separation) which is exactly what the Rust
     side's calibration is set up to fit.
   - `both` — runs both, streams both.
5. **Broadcast.** One line per frame to every client.

## Camera notes (Logitech C920 on this desk)

- **Field of view.** The C920's quoted 78° is *diagonal*. Horizontal is ~70.4° and vertical
  ~43.3° at 16:9, and it is the horizontal number that `Intrinsics.from_hfov` wants, so the
  default is **70.4**, not 78. Using 78 would shrink `fx` by 11 % and bias every recovered
  depth and pose. This is a derived-intrinsics stopgap: run a real checkerboard calibration
  and pass `--intrinsics` when the numbers start to matter.
- **`exposure_auto_priority` halves the frame rate.** In a dim room the driver stretches
  exposure to 66 ms and drops to 15 fps while still *reporting* 30. `Camera` clears the
  control by ioctl at open (`--no-fixed-framerate` to leave it alone). The cost is a darker
  image, which is the right trade for a gaze stream but is the first thing to check if
  detection starts failing.
- **Reading the camera inline caps you at 15 fps.** `cap.read()` returns the frame the
  driver already has; by the time ~17 ms of inference is done and the buffer is re-queued,
  the next frame has already been discarded, so the loop settles on every *other* frame.
  `AsyncCamera` drains the device on its own thread and hands over the newest frame.

## Verification

```sh
uv run pytest      # 63 tests, no camera and no weights required
uv run ruff check  # clean
```

`ruff format` is **not** part of the gate: the column alignment in this package is
deliberate and matches the Rust side's house style.

The tests cover intrinsics derivation and rescaling, the PnP wrapper against synthetic
projections of the model under known poses (including landmark noise and partial point
sets), the L2CS bin decoding and gaze-vector convention, face-crop padding, the JSON
schema, and the socket server's fan-out and drop behaviour. They do not touch the camera,
the GPU, or the model weights.

## Measured performance (2026-08-26, RX 7900 XTX / gfx1100, Ryzen 9 5950X)

Per-stage means over 150–200 frames, 1920×1080, fp16, `--estimator both`:

| stage | live camera | notes |
|---|---|---|
| detect + landmark | 10.6 ms | two MediaPipe graphs on the full frame |
| `solvePnP` | 0.31 ms | 16 points, EPnP + VVS |
| gaze — L2CS at 448 | 8.7 ms | was 6.7 ms at the wrong 224 input |
| gaze — iris | 0.14 ms | free; it is arithmetic on landmarks you already have |
| **inference total** | **19.7 ms** | 11.0 ms with `--estimator iris` |
| **throughput** | **28.4 fps** | camera-bound; the iris-only pipeline could run at ~90 |

Capture-to-send latency measured at a socket client: **mean 18.8 ms, p90 19.9 ms** with
both estimators, 30.0 records/s, contiguous `seq`, no drops. (`bench`'s `wall_ms` is larger
because it includes the wait for the next frame; `lat_ms` on the wire is the number.)

fp16 vs fp32 on the gaze net is 0.04° of yaw — numerically irrelevant, kept for the ~1 ms.
On the CPU the gaze stage is 5× slower, which is why `--device cuda` refuses to fall back.

## Known limits

- **The gaze model is nowhere near ET5 accuracy.** L2CS-Net/Gaze360 is a ~10–13° mean
  angular error model in the literature, and nothing here improves on that. Against
  DESIGN.md's 0.5–1° budget, this is a *coarse gaze direction* signal — useful for "which
  display / which region" and for head pose, not for snapping. Treat it as the Phase 0
  stand-in DESIGN §11 describes, not as a tracker.
- **Both eyes disagree systematically** (mean iris-offset disparity 0.23 eye-widths ≈ 14°
  at the nominal gain, sd 0.04). Vergence at 43 cm accounts for only ±4°, and it is
  opposite-signed per eye, so most of this is a per-subject offset between the canonical
  corner midpoint and the real resting iris position. It is a *bias*, not noise, and it is
  precisely what per-user calibration removes.
- **Vertical is the weak axis for both estimators.** Over a still-face run the two agree
  in yaw at r = +0.81 but at r = +0.02 in pitch. The iris has little vertical travel and
  the lid occludes what there is; L2CS's pitch head is its noisier one. Do not build
  anything that needs vertical precision on either without calibrating it first.
### The centre-crop bug (2026-08-26)

Worth recording, because the failure mode generalises. The first version of
`GazeEstimator.preprocess` reproduced the chain in L2CS-Net's archived `demo.py`:
`Resize(448)` then `CenterCrop(224)`. That discards the outer half of the crop. Combined
with a 1.4× face box it left the network looking at a nose and a mouth with the eyes cut
off above the frame — visible immediately in `dump-crops` output, and invisible in the
numbers, which were *stable* (yaw sd 1.5° over 12 consecutive frames) and plausible and
tracked nothing at all.

The maintained reference (`l2cs/utils.py` + `l2cs/pipeline.py`) has **no centre crop**: it
resizes the raw detector box to 224 with OpenCV and then straight back up to 448, so the
network input is 448 and the whole face goes in. Vendoring that reference and running it on
the same frames put the disagreement at 17.5° of pitch. After the fix this implementation
agrees with the reference to **0.6° yaw / 0.3° pitch**.

Three things that were checked and were *not* the problem: the checkpoint (the vendored
reference architecture loads it with zero missing and zero unexpected keys), fp16 (0.04°),
and the bin decode. Two things that were: input size (the same crop reads +9° yaw at 224
and +41° at 448 — the trunk's adaptive average pool means 224 runs silently) and crop scale
(1.8× costs 7° of pitch, 2.2× costs 20°, because the face shrinks inside the frame and the
network reads that as pose).

One upstream landmine: `l2cs/model.py` returns `(yaw, pitch)` but `l2cs/pipeline.py`
unpacks that call as `pitch, yaw = self.model(x)`. This implementation follows the *model*.
Do not "fix" it to agree with the reference pipeline.

- **Sign conventions are cross-checked, in yaw only.** Over 299 live frames the PnP head
  yaw and the L2CS gaze yaw correlate at **+0.75** with means agreeing to 1° (-24.3° vs
  -25.4°), which is what you expect if both really are in the same camera frame — people
  look roughly where their head points. That validates the yaw sign end to end. Head pitch
  barely varied (sd 4.2°) over the same run, so the **pitch sign is not independently
  confirmed**; check it before trusting the vertical axis. The gaze estimate is about twice
  as noisy as the head pose (sd 23° vs 11°) and its range ran to an impossible -115°.
  Independently, the two estimators — a ResNet-50 on pixels and a ruler on landmarks —
  agree in yaw at **r = +0.81** over a 299-frame still-face run, which is the strongest
  evidence available here that both are now tracking real eye direction rather than
  producing correlated artefacts of the head pose.
- **Depth is generic-model depth.** `eye_mm[2]` read 613–637 mm against a ~650 mm seating
  distance, which is the right order but carries the subject's deviation from the canonical
  face as a proportional scale error, typically 5–10 %. Per-user calibration would fix it.
- **`conf` is the detector's score, not a gaze confidence.** L2CS has no confidence head.
  The bin-softmax peak is available as `GazeAngles.sharpness` (it sat around 0.33) and
  would be a better basis for a real confidence, but it is not calibrated, so the protocol
  reports the honest number instead. Low detector scores did correlate with implausible
  gaze vectors in practice.
- **Framing on this desk is marginal.** The camera sits above the user and looks across the
  room; the head lands at the *bottom edge* of the frame with the chin often clipped, in a
  dim, warm-lit room with the face lit mostly by the monitors. With the user settled,
  detection is 100 %; while moving, leaning, or with a hand near the face it fell to 12–60 %
  over a 200-frame window. Aiming the camera down at the face and adding any fill light is
  the cheapest accuracy win available, and it will matter more for gaze than for detection.
- Single face only (`num_faces=1`), and the gaze model gives one combined direction, not
  per-eye.

## Estimator stability (10 s still-face capture, 299 frames)

| | yaw sd | pitch sd | angular sd about the mean | p90 deviation |
|---|---|---|---|---|
| L2CS (448, fixed) | 6.34° | 4.88° | 3.08° | 10.87° |
| iris (geometric) | 5.05° | 2.21° | 2.26° | 7.12° |
| head forward (PnP) | 2.03° | 1.50° | 1.37° | 4.18° |

The head row is the floor: the subject was not perfectly still, so ~2° of the gaze spread
is real head motion, not estimator noise. On this run the geometric estimator is the
*steadier* of the two, at 1/60th the cost — though steadiness is not accuracy, and its
per-eye bias (above) is larger than either sd.

For comparison, the same L2CS weights through the broken centre-crop preprocessing gave
9–27° of yaw sd per pose with means that did not track the target at all.
