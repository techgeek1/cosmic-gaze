# gaze-capture

Per-output screenshots of a running COSMIC session, as RGBA8 frames tagged with the
output's global logical rectangle.

## Protocols

cosmic-comp exposes no `wlr-screencopy`, so capture goes through the staging ext protocols:

| purpose               | interface                                        | version |
| --------------------- | ------------------------------------------------ | ------- |
| capture source        | `ext_output_image_capture_source_manager_v1`      | 1       |
| capture, cursor       | `ext_image_copy_capture_manager_v1`               | 1       |
| logical geometry      | `zxdg_output_manager_v1`                          | 3       |
| buffers               | `wl_shm`                                          | 2       |
| connector name, mode  | `wl_output`                                       | 4       |
| pointer object        | `wl_seat`                                         | 5       |

Crates: `wayland-client` 0.31, `wayland-protocols` 0.32 with the `client`, `staging` and
`unstable` features (the ext image-copy-capture protocols live under `staging`, xdg-output
under `unstable`).

## Design notes

**A fresh session per capture.** `ext_image_copy_capture_frame_v1.capture` is specified to
wait an indefinite amount of time for the source to change on every frame after the first
one in a session. A persistent per-output session would therefore hang on a static
desktop, which is exactly the state we want to screenshot. Creating the session
immediately before each capture always yields its first frame, so a call costs one extra
round trip and one shm allocation and never blocks. Measured cost on this desk is 14 to 40
ms per output end to end, which is dominated by the copy, not the setup.

**Outputs are re-enumerated on every call.** HDMI-A-1 on this desk drops off the output
list and comes back. `outputs()`, `capture_output()` and `capture_all()` each round-trip
the compositor first; a missing output is `Err(CaptureError::NoSuchOutput)`, never a panic.

**No cursor.** The session is created with an empty options bitfield, so `paint_cursors`
is off and a detector never sees a pointer-shaped element.

**Scale comes from the buffer, not `wl_output.scale`.** `wl_output` reports an integer
scale (2 for HDMI-A-1); the real scale is fractional. `OutputInfo::scale` is
`physical_w / logical.w` from the actual mode and the `zxdg_output_v1` logical size, which
is the ratio that maps detector boxes back into global logical pixels.

**Formats.** cosmic-comp offers `abgr8888`, `xbgr8888`, `abgr2101010` and `xbgr2101010`.
This crate handles the four packed 8-bit-per-channel layouts (`xrgb8888`, `argb8888`,
`xbgr8888`, `abgr8888`), prefers the opaque ones, and converts to RGBA8. The 10-bit
formats are not handled; if a compositor ever offers only those, `capture_output` fails
with `NoUsableFormat` rather than producing garbage.

## CursorTracker

`CursorTracker` reads the pointer's position from
`ext_image_copy_capture_cursor_session_v1`, which cosmic-comp does implement. It exists as
closed-loop feedback for uinput injection: cosmic-comp maps an absolute device onto a
single output, so injection has to be relative, and relative injection needs to measure
where the pointer actually ended up.

It opens its own `wl_display` connection, separate from `Capture`, so a frame copy cannot
delay a position reading and so a caller can follow the pointer without allocating a
capture buffer. One cursor session is opened per output, since the protocol reports the
pointer relative to a single capture source, and sessions are opened and closed as outputs
come and go.

**Units, verified against cosmic-comp 1.6.0 source.** `position` is in the capture
source's **buffer pixels**, which are physical, not logical. cosmic-comp computes it as the
pointer's output-local logical position multiplied by the output's fractional scale, then
rounds to an integer (`update_output_image_copy_cursor_position` in `src/input/mod.rs`, and
the same conversion in `new_cursor_session`). `OutputInfo::buffer_to_global` inverts that:
`logical_origin + buffer * logical_size / physical_size`, per axis.

Two caveats. The value is rounded to whole physical pixels, so on a scaled output it
quantises to under one logical pixel. And the scale recoverable from `zxdg_output_v1` is
the ratio of a *rounded* logical size to the mode size, so it differs from the
compositor's true fractional scale by up to ~0.03%: at the far edge of HDMI-A-1 that is
under half a logical pixel. Neither matters at gaze precision; both would matter if this
were used for pixel-exact placement.

`hotspot` is the cursor image's own offset and is not part of the position: cosmic-comp
sends the pointer location itself. It is kept, along with the `buffer_size` the derived
capture session announces, as `CursorReport::hotspot` and `CursorReport::image_px`: the
pair is the cursor's *shape*, re-sent whenever the client under the pointer changes it
(Adwaita at 24 px: the arrow is `24x24@3,1`, the I-beam `@11,12`, the hand `@7,5`). A
shape change without motion arrives as those two events alone, so the report is updated
in place rather than rebuilt from the next `position`. `gaze-capture-cli --cursor`
prints the pair as `cursor WxH@X,Y` on every line.

cosmic-comp emits `position` when a session is created (if the pointer is on that output)
and on every pointer motion over it, not on a timer. A still pointer therefore produces one
event and then silence, which is why `position()` returns the last known value rather than
requiring a fresh event.

## ToplevelTracker

Where every window is, in global logical pixels. Wayland clients cannot know this, so
anything they report about their own contents (an accessibility tree, say) is in window
coordinates; cosmic-comp can, and says so through `zcosmic_toplevel_info_v1` (bound at
version 2 or 3), whose `geometry` event gives each toplevel's rectangle relative to each
output it is on. Titles and app ids come from `ext_foreign_toplevel_list_v1`, which the
cosmic object is created from (`get_cosmic_toplevel`). Adding the output's logical origin
puts the window on the same desk as the pointer and the captures.

Two things learned on the desk. cosmic-comp answers `get_cosmic_toplevel` from its own
loop, *after* the `wl_display.sync` reply, so no number of round trips brings the
geometry; `connect` waits on the socket until every listed window has a rectangle (up
to 500 ms). And the protocol carries no stacking order, which matters as soon as two
maximised windows share an output: identical rectangles, one on screen. The tracker
watches activation over time and keeps a `focus_rank` per window (higher is more
recently activated), and `at(point)` takes the highest-ranked containing window, then
the currently activated one, then the smallest. A fresh tracker has no history, so
until the first focus change a tie between two unfocused windows falls to the smallest
and then list order; a tracker that runs all day does better than the probe's. The complete answer would be a capture of each candidate toplevel
(`ext_foreign_toplevel_image_capture_source_manager_v1`, which cosmic-comp advertises)
compared with the screen.

## CLI

```
gaze-capture-cli --list                       # print the current outputs
gaze-capture-cli --out screenshots/           # one PNG per output, timed
gaze-capture-cli --out screenshots/ --loop 1 --duration 5
gaze-capture-cli --out screenshots/ --output DP-1
gaze-capture-cli --out screenshots/ --all     # time the batch capture_all call
gaze-capture-cli --cursor --duration 5        # poll the pointer at 10 Hz for 5 s
gaze-capture-cli --toplevels                  # every window's desk rectangle, and the one under the pointer
gaze-capture-cli --toplevels --duration 30    # the same, reprinted on every change
```

`--cursor` prints the raw buffer coordinates next to the converted global position, plus
the cursor-session events seen, so the unit convention can be checked against the desk
layout by hand.

Files are written as `<connector>-<n>.png`, RGBA8. In loop mode each line also carries
`changed_fraction` against that output's previous frame.

## Screenshots

`screenshots/` at the workspace root is gitignored. It is populated by

```
cargo run -p gaze-capture --bin gaze-capture-cli -- --out screenshots/
```

against a live COSMIC session, and is the input `gaze-detect` consumes.
