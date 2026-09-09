# UX build plan: a quiet overlay, a daemon, an applet

**Status 2026-09-09: U1 built.** Everything before this was a prototype driven from a
terminal with a debug overlay that drew the raw gaze point, a box on whatever the snap
engine favoured and a caption. It worked and it was exhausting to look at: a ring on the
fovea over whatever was being read, boxes flickering across prose, text hard to read for
the movement around it. This plan turns it into something that can be left running all
day. DESIGN.md §3 principle 3 (hide the raw cursor, show the snapped target) is the
brief; the Vision Pro glow and Tobii's own keep-the-trace-off default are the precedent.

## U1. The pointer look (`gaze-overlay`) — built 2026-09-09

The session sends an intent, `Pointer { gaze, motion, near, target }`, and the overlay
thread presents it on its own clock (`present.rs`). Rules:

- **Nothing is drawn over what is being read.** The dot appears only while an
  interactive element is within snapping reach (`near`), so a page of prose stays
  unmarked. Text elements (OCR words, labels) do not count unless `--highlight-text`.
- **The dot** is 8 px in the desktop accent with a one-pixel halo in black or white,
  whichever the accent is further from, so it survives a white page and a dark window.
  It fades in and out (60 ms in, 140 ms out) rather than popping.
- **Weight follows motion.** In flight and just after landing the dot is at full
  alpha; once a fixation has held 150 ms on a target it thins to a ghost, because the
  highlight already says where the commit will land and the dot is on the fovea.
- **The trail** is a few faint samples (150 ms, tapering) behind the dot while the eyes
  are moving, for the feel of motion the raw point lacked. A settled dot has none.
- **The highlight** is a rounded rectangle 3 px outside the element, corner radius from
  the theme's small radius, accent stroke with halo, 12% accent fill. It crossfades from
  element to element instead of jumping, which also hides the flicker between two
  adjacent candidates.
- **Theme** from cosmic-config (`com.system76.CosmicTheme.{Mode,Dark,Light}`), watched,
  so an accent change in cosmic-settings is picked up live. Stock COSMIC colours when
  there is no theme to read.

The debug look is still there behind `gaze-proto --overlay-debug` and every ceremony
(calibrate, record, collect) still draws its targets with it. `gaze-overlay-cli
--pointer` animates the look between three fake controls; `--render DIR --at T` writes
the frame the screen would show.

Not done here, on purpose: easing the dot's position (it would add lag to a channel that
already feels heavy; the trail shows the lag instead), and any caption in the pointer
look.

## U2. Daemon (`gazed`)

The session loop in `gaze-proto` becomes a library run by a daemon that owns the device,
the overlay and the injector. Two channels, the COSMIC-native split:

- **Settings** through cosmic-config: the applet writes, the daemon watches. Provider,
  the tier toggles (click, scroll, edge scroll, focus follows gaze, daydream), the
  overlay knobs above, model and offset paths.
- **Live state and commands** over the session D-Bus (`zbus`, as `gaze-a11y` already
  uses): properties for tracker present, calibrated, offset clicks and jumps, current
  owner; methods for pause, resume, reset offset, recalibrate.

The overlay stays in-process for latency. `gaze-proto` becomes a thin front-end for
one-off runs with flags. Autostart via the XDG autostart entry COSMIC honours.

## U3. Applet

A libcosmic panel applet (the trainer already builds against the same pin): status icon
with a popup of the toggles and the live numbers, and the recalibrate button. Nothing
gaze-specific in it beyond the D-Bus proxy and the config struct.

## U4. Calibrate moves into the daemon

The daemon holds the device, so the retrain ceremony leaves `gaze-et5-cli calibrate`
and runs in the daemon, drawing its targets on the daemon's overlay; the applet only
triggers it. The most work in the split, last on purpose.

## Order

U1 first because it is visible immediately and verifiable with the CLI alone; U2 and U3
together, since the applet is the daemon's first client; U4 last. A perf pass over the
whole loop comes after, when the daemon is the thing to profile.
