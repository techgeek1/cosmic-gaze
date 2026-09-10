# gaze-detect

Pixel-only UI element detection. One output's RGBA frame in, `Vec<gaze_core::Element>` in
global logical pixels out. Two ONNX models on the CPU through the system onnxruntime.

```rust
let detector = Detector::load("models")?;
let elements = detector.detect(&frame.rgba, frame.width, frame.height, origin, scale)?;
```

`Detector::create()` returns a builder if you want a non-default `DetectConfig`.
`detect_timed` returns the same elements plus per-stage milliseconds.

## `detect_near`: everything that could be under one point

`detect_near(rgba, w, h, origin, scale, at, near)` is the pass a pointer-local caller wants,
and it makes two opposite trades against the full pass. For **widgets** it builds the
*same* tile plan `detect_timed` builds and runs only the tiles whose rectangle contains
`at`, which is one to four instead of ten on the ultrawide. That loses nothing: a box is
only emitted by a tile that holds it whole, so any box containing the point lies in a tile
containing the point. Note what this is *not* — it is not a crop around the pointer. A
crop cuts new pixels and hands them to the model as a whole image, which changes the
context a wide flat list row is recognised by, and it measured 0 agreements out of 12
(the click collector's measurement, 2026-08-28, DESIGN.md §10c). Here the model sees the same 1024 px tile at the same scale
with the same surroundings. For **text** it goes the other way: the model runs at native
resolution over a `near.ocr_px` square window centred on the point, shrinking at the
frame edges rather than padding. The full pass caps the frame at `ocr_max_side` for time,
and on a 3840 px panel that merges lines into paragraph blobs; locally there is nothing to
save, and a 640 px window costs about 62 ms and returns line-level boxes around 20 px tall.

On top of the plan's tiles it runs **one more widget tile**, `near.tile_px` (default 640)
centred on the point (`tile_at`), through the same NMS. The plan's 1024 px tiles reach the
640 px model input at 0.625x, and that is where small icon buttons go: on a YouTube action
column at scale 1 the 50 px circles score 0.23 to 0.42 from the plan tiles while the text
labels under them score 0.55 to 0.58, so a 0.5 gate keeps the label and loses the button;
the same circles score 0.79 to 0.95 from the pointer tile, and a Discord server-icon
column the plan tiles miss entirely comes back at 0.51 to 0.90. Over 40 sampled points on
the two reference captures the pointer tile changed the smallest containing box at five:
two same-box refinements, two nested sub-controls (a status-dot pair inside a sidebar row,
a path inside a prompt line) and one gained icon. It cannot replace the plan, because a box
wider than the tile is whole only in a plan tile. `tile_px` of zero turns it off.

Widget boxes of kind `Text` do not claim OCR lines in `fuse_text`. The widget model has a
text class and emits paragraph-sized boxes with it; letting those swallow the lines under
them undid the native OCR at the first point tested (a 22 px line replaced by a 562x209
paragraph). Both survive; a smallest-box hit test prefers the line.

Nor does a widget with **more than one** OCR line inside it. One line inside a button is
its label; several are content, and the box is a row or a card the model called a
button. On Discord a hovered or mention-highlighted message row comes back as a `Button`
at 0.81–0.94 over avatar, name, timestamp and text, and fusing its lines away turned
every click on the text into a click on a 280x69 button (2026-08-28). The row still
stands; the lines stand beside it.

`max_widget_w` / `max_widget_h` (both default `0.0`, unlimited) drop widget boxes bigger
than the given frame pixels **before** NMS and fusion. The ordering is the point: an
oversized spurious "control" drawn over a paragraph would otherwise swallow every text
line inside it as its own label.

Measured here on a 5950X with the machine under heavy unrelated load (mean of eight,
best in brackets), before the pointer tile. `--near 700,500` on DP-1 3840x1600, one
tile: widget 42 ms (39), OCR 66 ms (58), total 109 ms (100). `--near 1300,180` on DP-2
2560x1440, one tile: widget 38 ms (33), OCR 72 ms (59), total 110 ms (93). Against 384 ms
and 375 ms for the respective full passes. A four-tile point costs roughly three more
widget inferences. On an idle machine the same DP-1 point is 72 ms with `--near-tile 0`
and 95 ms with the default pointer tile (widget 29 ms to 51 ms, OCR 43 ms either way).

## Getting the models

```bash
# from the repo root; models land in models/ (gitignored)
UV_TORCH_BACKEND=cpu uv run --python 3.12 crates/gaze-detect/scripts/fetch_models.py
```

That downloads the TargetFinder PyTorch checkpoint, exports it to ONNX with Ultralytics,
downloads the PP-OCRv5 detection model, and prints both models' tensor shapes. It takes
about two minutes on a cold cache, almost all of it pulling CPU torch.

`--no-export` skips the torch dependency and only downloads (useful if you already have
the `.onnx`). `--variant yolo26s-640` picks a bigger backbone; see the table below.

Python 3.12 rather than 3.14 because torch has no 3.14 wheels yet. `UV_TORCH_BACKEND=cpu`
keeps uv from resolving the CUDA wheels; there is no CUDA on this machine (RX 7900 XT).

## Models

### Widget detector: TargetFinder

- Paper: *TargetFinder: Detecting Widgets from Pixels on Desktop Interfaces*,
  [arXiv:2607.19907](https://arxiv.org/abs/2607.19907) (23 Jul 2026).
- Code: <https://github.com/ahmedbenakouche/target_finder_toolkit>, **MIT** (`LICENSE.txt`).
- PyPI: `target-finder-toolkit` 0.2.0, classifier `License :: OSI Approved :: MIT License`.
- Dataset: <https://osf.io/fr6y4/overview> (520 desktop screenshots, ~38k annotations,
  Windows / macOS / Ubuntu / web). OSF lists no explicit licence on the node.
- Weights: the repo ships nine checkpoints under `target_finder_toolkit/models/`, three
  backbones (`n`, `s`, `m`) at three training sizes (640, 1280, 1920). This crate defaults
  to `yolo26n-640`, the configuration the paper reports the best F1 for (0.885 mono-class,
  against 0.698 for OmniParser's YOLO11m-1280).

**Licence caveat, read this before shipping.** The repo is MIT, but the weights are an
Ultralytics YOLO26n fine-tune and the checkpoint's own metadata carries
`license: AGPL-3.0 (https://ultralytics.com/license)`, which the ONNX export copies into
the graph metadata. Ultralytics' position is that AGPL-3.0 reaches derived weights. The
upstream MIT grant and that string disagree, and this crate cannot resolve the conflict.
The export tooling (`ultralytics`, AGPL-3.0) is only ever run from
`scripts/fetch_models.py`, never linked into any binary here, so that part is clean. If
the AGPL question matters for distribution, the MIT alternatives from DESIGN.md section 8
are OmniParser `icon_detect_v3` (YOLOv9-E, MIT as of Jul 2026) and Salesforce
GPA-GUI-Detector; both give boxes only, no classes.

Checkpoint sizes, for picking a variant:

| variant       | `.pt`   | exported `.onnx` |
|---------------|---------|------------------|
| `yolo26n-640` | 5.5 MB  | 9.4 MB           |
| `yolo26s-640` | 20.4 MB | -                |
| `yolo26m-640` | 44.1 MB | -                |

### Text detector: PP-OCRv5 detection stage

- PP-OCRv5 mobile **det** only, DBNet. No recognition head is run, so `Element::text` is
  always `None`; these are text *targets*, not text content.
- Weights: <https://huggingface.co/webnn/PP-OCRv5-ONNX> file `ch_PP-OCRv5_det.onnx`,
  **Apache-2.0**, an ONNX conversion of PaddlePaddle's official
  [`PP-OCRv5_mobile_det`](https://huggingface.co/PaddlePaddle/PP-OCRv5_mobile_det).
- Equivalent files live in `bukuroo/PPOCRv5-ONNX` (`ppocrv5-mobile-det.onnx`, byte
  identical size) and `RapidAI`'s distributions. The `server` det variant is 88 MB and
  roughly 20x slower for no useful gain at these text sizes.

## Tensor shapes

```
yolo26n-640.onnx        (static, batch 1, end2end)
  input   images        [1, 3, 640, 640]   float32   RGB, /255, letterboxed with 114 grey
  output  output0       [1, 300, 6]        float32   x1, y1, x2, y2, score, class

ch_PP-OCRv5_det.onnx    (fully dynamic spatial dims)
  input   x             [N, 3, H, W]       float32   RGB, /255, ImageNet mean/std
  output  fetch_name_0  [N, 1, H, W]       float32   per-pixel text probability
```

The YOLO export has `end2end: True` in its ONNX metadata, so the graph performs its own
NMS and the 300 rows come back sorted by descending confidence with a zero-padded tail.
Per-tile NMS is therefore unnecessary; cross-tile NMS still is.

The PP-OCR input `H` and `W` must both be multiples of 32. `ResizePlan` scales the frame
to `ocr_max_side` on its long edge and rounds up to the alignment, padding right and
bottom with post-normalisation zeros.

## Class mapping

TargetFinder's six classes, read out of the checkpoint's `names` map, onto
`gaze_core::ElementKind`:

| idx | TargetFinder   | `ElementKind` | note |
|-----|----------------|---------------|------|
| 0   | `Button`       | `Button`      | |
| 1   | `ToggleButton` | `Checkbox`    | closest two-state control in the shared taxonomy |
| 2   | `Hyperlink`    | `Link`        | |
| 3   | `Text`         | `Text`        | source stays `Detector`, not `Ocr` |
| 4   | `TextInput`    | `Input`       | |
| 5   | `Slider`       | `Unknown`     | **no `ElementKind::Slider` exists** |

`Slider` maps to `Unknown` rather than being folded into `Button` so the snap benchmark can
tell a real button from a control this crate could not name. A `Slider` variant in
`gaze_core::ElementKind` would let the snap engine rank sliders properly; that is a
`gaze-core` change and is not made here.

OCR boxes are always `ElementKind::Text` / `ElementSource::Ocr`.

## Pipeline

1. **Tile** (`tile.rs`). A 3840x1600 frame squashed into a 640 px input turns a 20 px
   toolbar icon into 3 px. Instead the frame is cut into overlapping square tiles of
   `tile_px` frame pixels, each scaled into the 640 px input on its own. Tiles at the right
   and bottom edge are shifted back to sit flush rather than hanging off, so every tile is
   full size. Overlap defaults to 15%, enough that a widget on a seam is whole in at least
   one tile.
2. **Widget inference** (`widget.rs`). Preprocess is one gather pass per tile: for every
   destination pixel, sample the frame at the mapped position. Nearest when the tile is
   unscaled (an exact copy), bilinear otherwise. Rows go through rayon.
3. **Text inference** (`ocr.rs`). DBNet is fully convolutional, so it runs once over the
   whole frame rather than per tile: no seam can split a word.
4. **DB postprocess** (`ocr.rs`). Threshold the probability map at `ocr_map`, flood fill
   four-connected components, score each by the mean probability over its own pixels, drop
   anything under `ocr_box`, then unclip. The unclip is DB's Vatti offset specialised to a
   rectangle: every edge moves out by `area * ratio / perimeter`, which for an axis-aligned
   box is exactly what pyclipper produces minus the rounded corners an AABB cannot keep.
5. **NMS** (`detection.rs`). Greedy, class-agnostic, IoU 0.5, across tiles. Class-agnostic
   because the same widget cut differently by two tiles can come back with two different
   classes and we want one candidate per thing on screen.
6. **Fusion** (`detection.rs`). UFO2 style: a text box with more than `text_contain` of its
   *own area* inside a widget box is that widget's label and is dropped. The test is
   intersection over text area, not IoU, because a label is much smaller than its button
   and their IoU is low even when the label is fully enclosed. The test is per widget, not
   cumulative, so a caption spanning two adjacent buttons survives.
7. **To global px** (`detector.rs`). `origin + frame_px / scale`. Frame pixels are
   physical, which is how the 1920x1200 HDMI panel at scale 2 lands as a 960x600 logical
   rectangle at (1506, 1600). `id` is the index in the returned vector and is only stable
   within one call.

## Measured on this desk (Ryzen 9 5950X, onnxruntime 1.29 CPU)

Real capture of DP-1, 3840x1600, a terminal-heavy tiling layout. Mean of 5 iterations after
warmup, defaults (`tile_px` 1024, `ocr_max_side` 1600, 8 threads):

```
widget model   226 ms   (10 tiles)
ocr model      158 ms
fuse             0.1 ms
total          384 ms   ->  219 widget boxes + 131 text boxes = 350 elements
```

DP-2, 2560x1440: widget 136 ms (6 tiles), ocr 239 ms, total 375 ms, 222 elements. OCR costs
more there than on the wider DP-1 because 2560x1440 capped at 1600 gives a 1600x900 input
against DP-1's 1600x672.

### Where the defaults came from

Tile size, widget model only, 3840x1600:

| `tile_px` | tiles | boxes | ms  |
|-----------|-------|-------|-----|
| 640       | 21    | 240   | 607 |
| 1024      | 10    | 219   | 306 |
| 1280      | 8     | 146   | 235 |
| 1920      | 3     | 62    | 90  |

Recall falls off a cliff past 1024 (146 boxes at 1280, 62 at 1920) while 640 costs twice as
much for 10% more boxes. 1024 it is. Cost per inference is flat at ~29 ms regardless of
tile count, so this is purely a tile-count trade.

Text model input size, text model only, 3840x1600:

| `ocr_max_side` | boxes | ms   |
|----------------|-------|------|
| 960            | 132   | 49   |
| 1600           | 333   | 191  |
| 2048           | 312   | 333  |
| 2560           | 299   | 579  |
| 0 (native)     | 343   | 1513 |

1600 gets 97% of the boxes native resolution finds for an eighth of the time. Above 1600 the
count actually drops slightly: at higher resolution DB starts splitting runs that the
unclip then fails to rejoin.

onnxruntime threads, widget model, `tile_px` 1024:

| threads | ms  |
|---------|-----|
| 1       | 568 |
| 2       | 335 |
| 4       | 234 |
| 8       | 211 |
| 16      | 326 |
| 32      | 581 |

**onnxruntime's default of one thread per logical core is 2.8x slower than 8 threads here.**
These models are far too small to fill 32 threads and the pool thrashes. The text model
peaks at 8 too. `DetectConfig::threads` defaults to 8; zero restores onnxruntime's default.

The obvious remaining lever, not taken: run tiles concurrently across a pool of sessions at
4 threads each instead of one session at 8. That should roughly halve the widget stage, at
the cost of `WidgetModel` holding N sessions instead of one.

## CLI

```
gaze-detect-cli IMAGE [--models DIR] [--origin X,Y] [--scale S]
                      [--json out.json] [--overlay out.png] [--bench N]
                      [--tile PX] [--ocr-max-side PX] [--conf F] [--threads N]
                      [--max-widget W,H] [--near X,Y] [--ocr-px N] [--near-tile N]
                      [--no-widgets] [--no-ocr]
```

`--near X,Y` switches to `detect_near` at that point, given in the **image's own frame
pixels** whatever `--origin` and `--scale` say, and `--bench` then benches `detect_near`
rather than the full pass. `--ocr-px N` (default 640) is the side of the native-resolution
text window it reads, and `--near-tile N` (default 640, 0 to disable) the side of the
extra widget tile centred on the point. `--max-widget W,H` sets the two size limits in frame pixels, zero on
an axis for unlimited. The output format is unchanged, so runs diff against each other;
with `--near` a `near:` line is added under `image:`.

```bash
cargo run -p gaze-detect --release --bin gaze-detect-cli -- \
    shots/DP-1-0.png --near 700,500 --conf 0.5 --max-widget 1200,240 --bench 5
```

`--json` writes the `Vec<Element>` through serde. `--overlay` redraws the source image with
every box stroked in a colour per kind: green button, cyan checkbox, purple link, red input,
white detector text, grey unknown, amber OCR text. `--bench N` runs N more passes and prints
mean, best and worst per stage. `--no-widgets` / `--no-ocr` isolate one model's timing.

```bash
cargo run -p gaze-detect --release --bin gaze-detect-cli -- \
    screenshots/DP-1-0.png --origin 2559,0 --scale 1 \
    --json shots/DP-1-0.json --overlay shots/DP-1-0-overlay.png --bench 5
```

## onnxruntime linking

`ort` 2.0.0-rc.12 with `default-features = false` and `load-dynamic`. Nothing is linked at
build time and nothing is downloaded; the library is `dlopen`ed at first session build.
`runtime.rs` tries `/usr/lib/libonnxruntime.so`, then `/usr/local/lib`, then the loader
path. `ORT_DYLIB_PATH` overrides all of it.

`ort` rc.12 requests C API version 24 (its README says "ONNX Runtime 1.24"); the system
library here is 1.29.0 from Arch's `onnxruntime-cpu` and serves the older API fine. The CPU
execution provider is registered explicitly. No GPU provider: the card is an RX 7900 XT and
there is no CUDA.

## Known limits

- A terminal-heavy screenshot is close to a worst case. The model boxes whole terminal
  panes and individual output lines as `Button`, which is why fusion drops so much text on
  the DP-1 capture (343 raw OCR boxes, 131 surviving). On conventional GUI apps the widget
  boxes are much tighter and far less text is swallowed.
- Boxes are axis-aligned, so rotated text from DB's polygon output is widened to its AABB.
- No recognition stage, so `Element::text` is always `None`. Content-addressed clicking
  ("click the thing that says Save") would need the `rec` model added.
