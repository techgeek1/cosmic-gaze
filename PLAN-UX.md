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

- **Nothing is drawn unless asked.** The look is armed while a thumb rests on the
  Daydream pad or F14 has latched it on (`keys.rs`, read from every keyboard, never
  grabbed); unarmed, the overlay draws nothing, so reading is never marked and the
  mouse is never fought. This is the pattern Tobii's desktop software and Talon settled
  on: the gaze trace shows on intent. Landing the thumb only arms; the refine begins
  once the thumb is 12% of the pad from where it landed and at least 120 ms have
  passed (a finger settling onto the pad shifts its centroid a good deal), so a resting
  thumb shows where the eyes are while the gaze keeps driving. `--overlay-always` is
  the old always-on behaviour.
- **Nothing is drawn over what is being read.** The dot appears only while an
  interactive element is within `--near-deg` (0.8°) of the gaze, so a page of prose
  stays unmarked. Text elements (OCR words, labels) do not count unless
  `--highlight-text`. The engine still snaps out to `--snap-deg` (2°); a commit past
  the near gate lands on an unmarked element, which is the price of not marking guesses.
- **The dot** is 8 px in the desktop accent with a one-pixel halo in black or white,
  whichever the accent is further from, so it survives a white page and a dark window.
  It fades in and out (60 ms in, 200 ms out) rather than popping, holds one weight while
  shown, and stays up for a linger (`--pointer-linger-s`, 0.3 s) after nothing is near,
  with the near gate itself closing at 1.5x the distance it opens at, so the edge of the
  gate does not blink it. A saccade never opens the gate: the return sweep to the next
  line of a paragraph crosses controls the eye is not aiming at, so only a fixation can
  bring the dot up, and in flight it can only stay or go.
- **The dot moves on a critically damped spring** stepped at the display's frame rate
  (`--pointer-settle-s`, 0.2 s to close 95% of a step), the VR laser-pointer treatment:
  continuous velocity between the tracker's 33 Hz samples, no overshoot, drift followed
  at the same pace as everything else. An invisible dot is placed on the gaze before it
  fades in, never swept in from where it was hidden. With the overlay doing the
  smoothing, the filter stack's one-euro cutoff for the tracker went from 0.3 Hz (a
  half-second lag on drift, which read as weight) to 1 Hz; its job is now only to keep
  the snap point from jittering.
- **The highlight** is a rounded rectangle 1 px outside the element, corner radius from
  the theme's small radius, accent stroke with halo, 12% accent fill. It crossfades from
  element to element instead of jumping, which also hides the flicker between two
  adjacent candidates. It is gated on the same near distance as the dot.
- **Tried and removed the same day, after a driven session:** a trail behind the dot
  in flight (at 33 Hz it read as a rendering bug), the dot thinning to a ghost once a
  fixation settled (a dot changing weight while the eyes hold still looks like it is
  doing something), a 3 px inflation of the highlight (the boxes read as bigger than
  their widgets, worst on Discord and Reddit), and showing the highlight anywhere in
  the snap radius (a box on an element the eyes are nowhere near reads as guessing).
- **The tree outranks the recogniser.** Once per place the eyes settle, the session asks
  the accessibility tree what is there (`verify.rs`, on `gaze-clicks`'s tree thread,
  polled, never blocking). A control it names (`link`, `push button`, `entry`, ...) is
  marked with the tree's own box; text, a heading or nothing actionable is never marked
  whatever the recogniser called it; a control taller than 56 px and wider than 240 is
  a row or a picture and is treated as text. No answer (an application off the bus)
  leaves the recogniser's kind in charge, but an accessible window answering nothing at
  the point (Firefox over a page it has a tree for) is text: the application would have
  named a control if it had one, and the recogniser guessing over an accessible page is
  what the tree is there to overrule. Found on Discord 2026-09-09: the recogniser
  called a message's last line a `Button` at 0.9 and the channel list `Text`; the tree
  says `list item` 880x71 and `link` 286x32. `--no-a11y` turns this off. The first few
  empty answers per session are logged at info with where; `gaze-a11y-cli at X,Y
  --window TITLE` probes a window even under another.
- **Theme** from cosmic-config (`com.system76.CosmicTheme.{Mode,Dark,Light}`), watched,
  so an accent change in cosmic-settings is picked up live. Stock COSMIC colours when
  there is no theme to read.

The debug look is still there behind `gaze-proto --overlay-debug` and every ceremony
(calibrate, record, collect) still draws its targets with it. `gaze-overlay-cli
--pointer` animates the look between three fake controls; `--render DIR --at T` writes
the frame the screen would show.

Not done here, on purpose: easing the dot's position (it would add lag to a channel that
already feels heavy), and any caption in the pointer look. Still open: the detector's
own boxes on web content are looser than the widgets they mark, which the 1 px inflation
does not fix and the tree fixes only where it answers. Firefox answered nothing at
three points on a Reddit tab on 2026-09-09 and everything on GitHub, a PR page and a
Reddit thread an hour later, probed the same way; not reproduced since, and the session
now logs the first few such answers so the next one says which page.

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
