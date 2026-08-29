# gaze-clicks

Passive gaze labels from the mouse you are already using.

People look at what they click. So every deliberate click on a recognisable control is a
labelled gaze sample: the element's box is the target, the gaze frames around the press
are the observation, and neither cost a calibration screen. `gaze-clicks` collects them
in the background all day while you work.

The output is `gaze-provider-et5`'s session format, unchanged, so

```
gaze-et5-cli dataset export --sessions config/sessions --csv rows.csv
```

reads a day of clicks exactly as it reads a five-minute `record` session. Click rows
carry `session_phase = "click"`, `hold_key = click_<n>`, `background = "screen"`, and
four extra columns (`element_kind`, `element_w_px`, `element_h_px`, `crop_luma`). The
Python harness drops what it does not know, so nothing downstream needs changing.

## Running it all day

```sh
cargo run --release --bin gaze-clicks-cli -- run
```

Then use the machine normally. Ctrl-C stops it and writes the session's end line.

**It owns the tracker.** The ET5 takes one USB claimant at a time, so while this is
running `gaze-proto`, `gaze-et5-cli view`, `record` and `calibrate` cannot open the
device. Stop the collector before any of them.

It reads the mouse **read-only and never grabs it**, so every press still reaches the
compositor and the mouse behaves exactly as it would otherwise. The default node is the
one named `input-remapper mouse`, because that is the clone cosmic-comp reads on this
desk; reading the G502's hardware node instead would see presses the compositor never
acts on. `--mouse PATH` and `--mouse-name SUBSTR` override it, and the fallback for
another desk is the first device with a left button that is not a keyboard.

The ONNX models come from `gaze-detect`; fetch them with
`crates/gaze-detect/scripts/fetch_models.py` as that crate's README describes. They land
in `models/` (gitignored), which is `--models`' default.

Before leaving it running, two checks:

```sh
cargo run --bin gaze-clicks-cli -- devices     # is it reading the right node?
cargo run --bin gaze-clicks-cli -- probe       # does it see what you see? (runs until Ctrl-C)
```

`probe` runs until Ctrl-C (`--seconds N` to stop early). Four times a second (`--hz`) it
prints a line and draws the same thing on screen: the chosen box outlined with its kind,
size, score, text and the round trip's latency, and a cross where the pointer was read.
The cross should sit under the real cursor and the outline should be the thing you would
say you are pointing at.

```
DP-1 (2789, 96): Button 184x31 s=0.95 "Save" 96 ms [44 boxes, luma 0.21, sd 0.184, detect 78 ms, Press { age_s: 0.03 }]
DP-1 (3011, 402): BLANK 91 ms [12 boxes, luma 0.19, sd 0.004, detect 74 ms, ...]
DP-1 (120, 1580): NOTHING 88 ms [7 boxes, ... nearest is Button at (96, 1544) 48x48 score 0.91]
```

The latency is capture-start to overlay-draw, the whole round trip and not just the
detector's own milliseconds; the bracketed part is stdout only. `BLANK` and `NOTHING` are
the two rejections a click would get, decided by the same `element::pick` the collector
calls, so what the probe draws is what a click there would have been written against. On
`NOTHING` it still outlines the nearest *accepted* box, because a systematic coordinate
offset shows as every box sitting a fixed distance away. The overlay blanks for 60 ms
before each capture so its own outline is never what gets recognised; the flicker is that.

The blank has to actually reach the screen, and until 2026-08-28 it did not: the overlay
double-buffers, and a blank frame drawn into the buffer that was already blank repainted
nothing, so it was committed with no damage and the compositor kept showing the other
buffer's box and cross. The probe then captured its own marks: with the mouse idle the
outline grew every tick (`NOTHING` → `Text 14x15` → `Button 18x19` → `Text 25x23` →
`Button 31x28`), a 25x25 "button" appeared around the cross on empty background, and picks
cycled between button, text and nothing on a static screen. `gaze-overlay` now damages
what is on screen rather than what the target buffer held. If the probe ever looks
unstable on a still screen again, `RUST_LOG=gaze_overlay=debug` prints every commit with
its damage box; a blank commit with an empty box after a drawn one is this bug back.

`--no-tracker` runs everything except the device: clicks, recognition, tallies and click
records, with no gaze frames or stop windows. It is the way to exercise the rules with
the tracker in use elsewhere.

## What it captures, and when

**On the press, not from a buffer.** Everything that destroys a click target fires on
the *release*: the menu item activates, the link navigates, the popup dismisses. Between
press and release the target is still there and still under the pointer, so a
pressed-state highlight or a popup opening underneath changes nothing the recogniser
cares about. A rolling frame a second old, by contrast, is exactly wrong for a menu item
clicked shortly after the menu opened, because it predates the menu.

So the evdev reader fires a `capture_output` the moment a press arrives, and that frame
is used when it lands within 150 ms (it measures 20 to 40 ms in practice). A slow rolling
capture, one frame per second by default (`--capture-hz`), is the fallback for a press
capture that stalled, and it is never used if it is more than 500 ms old or was taken
after the release.

Recognition then runs **around the pointer**, through `Detector::detect_near`. The
smallest accepted box containing the pointer wins, because boxes nest and the innermost
one is what the user aimed at. "Containing" allows 6 logical px of slop: boxes are drawn
around what is visible, and a control's padding is invisible to both models. On the desk
an emoji-picker button came back as its 31x25 glyph with the pointer 4 px to the right of
it, and a search field's placeholder line ended 3 px short of the caret; both were
`NOTHING` without the slop.

Before any of that, the collector asks the **application** (`gaze-a11y`, on its own
thread, in parallel with the capture): what is at this point in your accessibility
tree? Where the application is on the AT-SPI bus (Firefox today; Chromium and Electron
only with their accessibility switched on; COSMIC's own apps not yet) the answer comes
back in a few milliseconds with a role, a name and a rectangle on the desk, and it wins
over the pixels: a tree knows a card is one link and a grey rectangle is an input, and
pixels do not. The nearest actionable ancestor of the object at the point is the target
(`element_kind` folds its role into the same vocabulary as recognised boxes: `button`,
`link`, `input`, `checkbox`, `slider`, `image`, else the hyphenated role), it has to
contain the pointer, and the flat check below applies to it exactly as to a recognised
box. Every click records its `source`: `tree`, `vision` or `caret`. Where the tree has no
answer, nothing changes: the recogniser is the fallback, which is what it was built to
be.

The collector also reads the **pointer's shape**. The cursor session that reports the
position also reports the cursor image's size and hotspot, and those two numbers name
the shape without copying a pixel: an arrow's hotspot is in the top-left corner, a hand's
at the top edge, an I-beam's dead centre (`cursor.rs` has the bands and the 24 px
Adwaita and Pop hotspots they came from). That is the application's own word on what is
under the pointer, and it covers exactly what the widget model misses: input fields
drawn as a slightly different grey, borderless clickable regions, text in a terminal.

The **I-beam is trusted**: a click on nothing recognisable under an I-beam is accepted
as a `caret`, a nominal 24 px box around the pointer, because an input, a terminal or a
document is a place the eye was. A large flat box under an I-beam is still `blank`: that
is the empty body of an editor, and the eye could be anywhere in it. The **pointing hand
is recorded, not trusted**: it vouches for links and cards, but a card's padding is a
click the eye may have made from the title 100 px away. A **grab** is neither, since it
announces a drag. Every click carries `cursor: "arrow" | "hand" | "grab" | "text" |
"centred" | "other"`, and the status line counts the refusals made under a hand or an
I-beam, so a day's run says how many clicks the rules are still throwing away that the
application would have vouched for.

## Why local, and why not a crop

`detect_near` runs the widget tiles of the *full* tile plan that contain the pointer, one
to four instead of ten on the ultrawide, plus one 640 px widget tile centred on the
pointer, and runs the text model at native resolution over a 640 px square window centred
there. The two halves are opposite trades and both matter.

The pointer tile is the model's own input size, so it sees the pixels there unscaled. The
plan's 1024 px tiles shrink everything 1.6x, and that loses small icon buttons to the
labels beside them: on YouTube's action column (50 px circles at scale 1) the plan tiles
scored the circles 0.23 to 0.42 and the counts under them 0.55 to 0.58, so the 0.5 gate
outlined "4.2万" and the thumb glyph's OCR fragments instead of the button. The pointer
tile scores the same circles 0.79 to 0.95, and finds the Discord server-icon column the
plan tiles miss. Cost is one more inference, about 23 ms on this desk; over 40 sampled
points on the reference captures it changed the pick at five, all refinements, nested
sub-controls or gains (`gaze-detect/README.md`).

The widget half loses nothing, because a box is only ever emitted by a tile that holds it
whole, so any box containing the point lies in a tile containing the point. Checked on
identical pixels over 26 points across DP-1 and DP-2 with no gates on either side (`--conf
0.25 --max-widget 0,0`): the set of non-text boxes containing the point was **identical**
to the full pass at every one, including the points where four tiles were run.

The text half is the fix for the opposite problem. The full pass shrinks the frame to
`--ocr-max-side` (1600) for time, and on a 3840x1600 panel that merges the lines of a
paragraph into one 800x500 blob, which is a useless click target. Native resolution over a
640 px window costs about 62 ms and gives line-level boxes: measured over two points on
DP-1's prose, every text box came back between 14 and 25 px tall, against a full-pass
median of 28 and a maximum of 61 on the same capture.

Measured cost on this desk with the machine under heavy unrelated load, mean of eight: a
one-tile point is 42 ms of widget plus 66 ms of native OCR, 109 ms total on DP-1 and
110 ms on DP-2, against 384 ms and 375 ms for the respective full passes.

### Why a crop is not the same thing

The first version recognised a ±256 px crop around the pointer, on the reasoning that a
click only lands on something under the pointer and a crop costs tens of milliseconds
against a whole ultrawide frame's few hundred. It loses the widgets that matter, and
`detect_near` is not a repeat of it: a crop cuts new pixels and hands them to the model as
a whole image, while `detect_near` runs the unchanged 1024 px tiles of the unchanged plan.

Take one capture of DP-2 (Discord plus a browser), detect it whole as the reference,
then cut a crop out of **those same pixels** around each of the twelve largest widgets
and detect each crop: no drift, no timing, the same detector and settings on both sides.
The crop's pick agreed with the full frame **0 times out of 12**, at 512 px and again at
640 px. Every one came back as the widget's own inner OCR text, or as nothing.

What is lost is the wide flat widgets: Discord's channel and member rows (~290x45), the
browser's URL bar (`Input 636x35`), a link. Small square things (server icons, toolbar
buttons) survive cropping fine, which is why the crop version looked like it worked. The
failure is not resolution, it is **context** — the widget model needs the surrounding
layout to tell a list row from a run of text with padding. Symptomatically it is "it's
just locking onto the text and icons".

Reproduce the crop failure with the example:

```sh
cargo run --bin gaze-capture-cli -- --out shots
cargo run --bin gaze-detect-cli -- shots/DP-2-0.png --json shots/DP-2.json
cargo run --release --example recognition_check -- shots/DP-2.json DP-2 0 160
```

The origin (`0 160` here) is the output's logical top-left from `gaze-capture-cli`'s
listing. Produce the reference **without** `--origin`: `gaze-detect-cli` bakes whatever
origin it is given into the boxes, so passing it on both sides offsets every check point
by the output's position. Expect full agreement, or one or two misses where the screen
changed between the reference and the check — run it straight after the capture, and
judge the misses rather than the count. Two captures a second apart already disagree on
about one widget in thirty on a working desktop.

`--luma-px` (default 256) is only the window the screen luminance is averaged over for
`crop_luma`, the pupil covariate. It has no effect on recognition. A second, much smaller
window (±24 px) around the pointer is measured too, and its luma *standard deviation* is
what the `blank` rule reads.

## The rejection rules

In the order they apply:

| tally | rule |
| --- | --- |
| `drag` | held longer than 400 ms, or the pointer moved more than 6 logical px between press and release. The press and the release are about different places. |
| `off-desk` | the click landed on an output `desk.toml` does not describe, so there is no surface to put the target on. |
| `stale` | no press capture within 150 ms and no rolling frame from the last 500 ms. |
| `no-element` | a frame, no answer from the accessibility tree, nothing recognisable under the pointer, and the pointer is not an I-beam. **This is the important one**: a click on empty space to focus a window says nothing about where you were looking, and it is the most common press on a desktop. Under an I-beam the same click is accepted as a `caret` instead. |
| `blank` | a box did contain the pointer, but it is taller than 60 logical px and the luma standard deviation of a ±24 px window around the pointer is under 0.02. The model drew a control over flat pixels, so there is nothing there to have been looking at. Small boxes skip this check: a confident small button's body can be flat where the pointer landed and its label a few pixels away, which is a fine target either way. |
| `no-gaze` | fewer than 20% of the frames in the 600 ms before the press carried a valid combined gaze. A blink over the approach is not a label. |
| `overrun` | clicked faster than recognition runs, so the detector already had three frames queued and this one was dropped rather than made to wait. |

`late-capture` is not a rejection: it counts accepted clicks that fell back to a rolling
frame, and a rising count means the compositor is struggling.

Two more gates sit inside the detector rather than in the tally table, because they change
what it returns rather than what happens to a click. Widget boxes must score at least
**0.5** (the snap default of 0.25 was tuned for recall; on both reference captures every
box under 0.5 sat over styled prose, inline-code chips, timestamps and headings, and every
real control scored above it), and must be at most **1200x240 frame pixels**, because a
control that big is a pane. The size limit is applied before OCR fusion, so a spurious
panel box cannot take the text lines inside it down with it.

`overrun` exists because of how the threads are split. A detection is tens to a couple of
hundred milliseconds and a press capture has to happen within tens of milliseconds of the
press, so the capture thread (pointer, `Capture`, frame choice) never waits on the detect
thread (the models): it hands a chosen frame over with a non-blocking send onto a queue
three deep and refuses the click if that queue is full. A double click's second press is
captured while the first is still being recognised. A steady `overrun` count means the
detector cannot keep up with how the machine is used; an occasional one is a burst. It was
already rare with whole-frame recognition and is rarer now.

`Icon` and `Slider` are accepted but recorded as themselves, so the export can filter
them out: an icon's centre is not always where the eye goes and a slider is dragged as
often as it is clicked. `Unknown` boxes are refused outright, including as containers, so
an unclassified panel box cannot swallow a click that landed on a real button.

Double and triple clicks are kept as separate presses and tagged with their position in
the run (`multi`). The eye may well have left the target by the second press, and that is
a fact about eyes worth having in the data rather than one to average away.

## What the status line means

Every ten seconds:

```
status: 34 accepted (20 tree, 5 caret) / 7 drag / 12 no-element / 2 blank (6 under a hand or I-beam) / 3 no-gaze / 1 stale (2 late-capture, 0 overrun, 0 off-desk, 0 error) — median offset 0.83 deg over the last 20, median detect 84 ms
```

`tree` is how many of the accepted clicks were labelled from the accessibility tree
rather than the pixels; it is also a running measure of how much of the desktop is
accessible. `caret` is how many were taken on the I-beam's word with no recognised box. The count after `blank` is how many `no-element` and `blank` refusals
happened under a pointing hand or an I-beam: the application said something was there
and the rules refused it anyway. It is the size of the gap between the rules and the
screen, and the number to look at before deciding whether the hand should be trusted
the way the I-beam is.

The **median offset** is the daily "is the model drifting" number. For every accepted
click it is the angle between where the firmware said you were looking (its filtered 2D
output, lifted onto the declared plane) and where you actually clicked, both seen from
the desk config's nominal seated eye. That is the same measurement
`gaze-et5-cli calibrate`'s health pass makes against a grid of dots, except it costs
nothing and accumulates all day. A figure that sits near the sensor's own limit (~0.7°)
is a healthy day; one that climbs over a week is the signal to look at the model.

The **median detect** is the detector's own cost over the same window of recent clicks,
which is what `overrun` and the collector's per-click latency both follow from. Each click
also logs its `detect_ms` at debug level (`--verbose`).

Each accepted click also logs a line:

```
click #12 DP-1 (2789, 96) button "Save" — firmware gaze 0.8 deg off, 71 frames
```

## The file

`config/sessions/<unix>-<blobkey>-clicks.jsonl` (`--out` overrides it), where `blobkey`
is the first eight hex digits of the on-device eye model's body hash, exactly as `record`
names its files. Every client-side row is keyed to that model, so if the tracker
reconnects holding a *different* one (which only happens if you retrain mid-run) the
collector closes the file and opens a new one rather than mixing two feature extractors.

Per accepted click: a `"stop"` record with `phase: "click"` whose window is
`[t_press - 0.6, t_press + 0.1]`, a `"click"` record with the press, the element, where
the element came from (`source`: `tree`, `vision` or `caret`) and the pointer's shape
(`cursor`; both absent in files written before 2026-08-28), and
the `"frame"` records of `[t_press - 1.2, t_press + 0.4]`, deduplicated so overlapping
clicks never write a frame twice. The file is flushed after every click, because this
process gets killed rather than stopped; a session missing its `meta_end` line still
loads.

Stop records are filed under the **click's own output**, which is why the exporter reads
each stop's `display` rather than the meta line's. The meta line names the display whose
plane the tracker has declared, which is where the firmware's 2D output lives, and that
is only one of the three panels a click can land on.

## Memory

The capture thread holds up to two rolling frames per visited output plus three press
captures, and the detect queue holds up to three more. On this desk (3840x1600 +
2560x1440 + 1920x1200) that is roughly 250 MB resident in the worst case. Frames are
dropped as soon as a click has been recognised from them.
