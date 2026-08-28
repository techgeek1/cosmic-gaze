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

`probe` runs until Ctrl-C (`--seconds N` to stop early). Once a second it prints the
pointer, its output and the element under it, and draws the same thing on screen: the
chosen box outlined with its kind, size and text, and a cross where the pointer was
read, so the cross should sit under the real cursor and the outline should be the thing
you would say you are pointing at. When nothing contains the pointer it outlines the
nearest box labelled `NEAREST` (a systematic coordinate offset would show as every box
sitting a fixed distance away), and `NOTHING` when the frame had no boxes. The overlay
blanks for 60 ms before each capture so its own outline is never what gets recognised —
the once-a-second blink is that.

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

Recognition then runs on the **whole** captured output. The smallest accepted box
containing the pointer wins, because boxes nest and the innermost one is what the user
aimed at.

## Why the whole frame

The first version recognised a ±256 px crop around the pointer, on the reasoning that a
click only lands on something under the pointer and a crop costs tens of milliseconds
against a whole ultrawide frame's few hundred. It loses the widgets that matter.

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

Reproduce it with the example:

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

`--luma-px` (default 256) is now only the window the screen luminance is averaged over
for `crop_luma`, the pupil covariate. It has no effect on recognition.

## The rejection rules

In the order they apply:

| tally | rule |
| --- | --- |
| `drag` | held longer than 400 ms, or the pointer moved more than 6 logical px between press and release. The press and the release are about different places. |
| `off-desk` | the click landed on an output `desk.toml` does not describe, so there is no surface to put the target on. |
| `stale` | no press capture within 150 ms and no rolling frame from the last 500 ms. |
| `no-element` | a frame, but nothing recognisable under the pointer. **This is the important one**: a click on empty space to focus a window says nothing about where you were looking, and it is the most common press on a desktop. |
| `no-gaze` | fewer than 20% of the frames in the 600 ms before the press carried a valid combined gaze. A blink over the approach is not a label. |
| `overrun` | clicked faster than whole-frame recognition runs, so the detector already had three frames queued and this one was dropped rather than made to wait. |

`late-capture` is not a rejection: it counts accepted clicks that fell back to a rolling
frame, and a rising count means the compositor is struggling.

`overrun` exists because of how the threads are split. A whole-frame detection takes 250
to 400 ms and a press capture has to happen within tens of milliseconds of the press, so
the capture thread (pointer, `Capture`, frame choice) never waits on the detect thread
(the models): it hands a chosen frame over with a non-blocking send onto a queue three
deep and refuses the click if that queue is full. A double click's second press is
captured while the first is still being recognised. A steady `overrun` count means the
detector cannot keep up with how the machine is used; an occasional one is a burst.

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
status: 34 accepted / 7 drag / 12 no-element / 3 no-gaze / 1 stale (2 late-capture, 0 overrun, 0 off-desk, 0 error) — median offset 0.83 deg over the last 20
```

The **median offset** is the daily "is the model drifting" number. For every accepted
click it is the angle between where the firmware said you were looking (its filtered 2D
output, lifted onto the declared plane) and where you actually clicked, both seen from
the desk config's nominal seated eye. That is the same measurement
`gaze-et5-cli calibrate`'s health pass makes against a grid of dots, except it costs
nothing and accumulates all day. A figure that sits near the sensor's own limit (~0.7°)
is a healthy day; one that climbs over a week is the signal to look at the model.

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
`[t_press - 0.6, t_press + 0.1]`, a `"click"` record with the press and the element, and
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
