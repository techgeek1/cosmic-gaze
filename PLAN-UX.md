# UX build plan: a quiet overlay, a daemon, an applet

**Status 2026-09-10: U1, U2 and U3 built; U4 next.** Everything before this was a prototype driven from a
terminal with a debug overlay that drew the raw gaze point, a box on whatever the snap
engine favoured and a caption. It worked and it was exhausting to look at: a ring on the
fovea over whatever was being read, boxes flickering across prose, text hard to read for
the movement around it. This plan turns it into something that can be left running all
day. DESIGN.md §3 principle 3 (hide the raw cursor, show the snapped target) is the
brief; the Vision Pro glow and Tobii's own keep-the-trace-off default are the precedent.

## U1. The pointer look (`gaze-overlay`) — built 2026-09-09

The session sends an intent, `Pointer { gaze, near, target, zone }`, and the overlay
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
- **Thumb down points, thumb up scrolls.** The eyes do one thing at a time. With a
  thumb on the pad or F14 latched they point: controls are marked and the edge scroller
  is shown nothing, so a scroll never fights a refine. With the thumb up they read, and
  may scroll: no control is marked, and the band of the scroll surface the eyes are in
  or approaching (from three quarters of a band height above its inner edge, closing at
  1.5x that) is drawn as a faint zone, 5% accent fill and a 1 px stroke at 30%, with the
  dot, deepening to 10% while the scroll runs. The zone opens on a fixation only, like
  the dot, and a running scroll keeps it up whatever the eyes do. This replaced the
  old rule that eyes on a small control held the scroller off, which after the near
  gate and the tree verdicts was gating on elements that no longer existed.
  The modes carry three more things. **The detector sleeps with the thumb up**: the
  perception thread neither captures nor detects until the look is armed (or the desk
  has no controller and no latch key, when it runs as before), and arming asks for the
  output under the gaze first; the tree verdict marks controls in the meantime, so the
  highlight is not waiting on the detector. **A scroll borrows the pointer**: Wayland
  delivers wheel motion to the surface under the pointer and nowhere else, so an edge
  scroll still has to put the pointer in the surface, but it now remembers where the
  pointer was and puts it back when the scroll stops, unless something else moved it
  meanwhile. **A commit lands on what is marked**: the highlight's centre, whatever the
  snap engine's own pick or the pointer's place; the refine point still outranks it,
  and with nothing marked the engine's pick is clicked as before. **Thumb down borrows
  the pointer**: it warps to the mark when one appears or changes and sits on it while
  the thumb rests, so the app's hover agrees with the highlight, the pad press clicks
  what is marked, and a refine nudges from there; the thumb lifting puts it back where
  it was, after the press has clicked, so the mouse hand finds it where it left it and
  mouse and gaze are fully decoupled. The controller's
  held/idle arbitration against the mouse (gyro thresholds, a six-second window) is
  gone: the thumb is the intent, and nothing gaze-side moves the pointer otherwise.
- **A hand on the mouse holds the scroller off.** The session reads the pointer every
  sample (which also keeps the cursor tracker's connection drained; the compositor hangs
  up on a client that leaves it unread, and did, for hours at a time, until 2026-09-12).
  Motion it did not cause is the mouse in use, and for a second after the last of it the
  scroller is shown nothing: no band, no start, and a running scroll stops. Reading a
  PR's sticky file header while ticking "Viewed" no longer scrolls it away. Likewise a
  gaze that leaves the tracker's display towards another configured display (the log
  panel below it) is a look at that display, not a look past the bezel, so it neither
  sustains a scroll at turbo nor, clamped to the edge above it, starts one.
- **The calibration mark (2026-09-10).** The ceremonies (`calibrate`, the sweeps, the
  recording sessions) drew the debug look: an amber square, a magenta cross and a
  caption in the 5x7 font. They now draw a mark in the pointer look's language: a
  20 px accent ring with the halo, the dot at its centre, and an interior that fills
  from 8% to 35% accent as the hold at that target progresses (the gate's hits, the
  dwell, the settle time), which is the only progress signal now that there is no
  text on screen. What the caption said (which round, which point, lean in) is one
  log line per segment instead. `gaze-overlay-cli --render DIR --mark X,Y[,P]
  --background black|white` shows it offline.
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

## U2. Daemon (`gazed`) — built 2026-09-10

The session loop in `gaze-proto` becomes a library (`gaze_proto::session::run`) run by a
daemon that owns the tracker, the overlay and the injector. What the prototype exposed as
forty flags sorts into three piles, decided 2026-09-10 after the modes session:

- **Dies with the prototype.** Provider selection and everything it drags in (synthetic,
  replay, webcam, the grabbed mouse, noise overrides, seed, gain), record, seconds,
  show-truth, the debug look, dry run versus click, freeze-offset, no-flywheel. Seconds,
  the debug look and dry run versus click stay in `gaze-proto`'s `Args` as
  the dev harness; the rest went with the non-ET5 paths and the model on 2026-09-10. Two were features and are cut outright:
  wheel routing to the window under gaze (`--scroll`) and focus-follows-gaze, both of
  which moved the pointer on their own initiative, which the borrow-and-return model
  abolished. The tier toggles go too: clicking, edge scrolling and the tree verdicts are
  always on, and the Daydream controller is used when one is paired (the first paired
  one), retried in the background while it is asleep. The gyro refine mode and its axes
  go; touch won.
- **Becomes a fixed location.** `gaze_config::Paths`: the desk file, calibration and
  device blob under `$XDG_CONFIG_HOME/cosmic-gaze/`, the ONNX models under
  `$XDG_DATA_HOME/cosmic-gaze/models/`, the offset under `$XDG_STATE_HOME/cosmic-gaze/`. `gazed --home DIR` (and `gaze-proto --home DIR`) maps
  all three onto a checkout's `config/` and `models/` so a live run needs no copying.
- **Becomes tuning.** Every number still being moved: the filter quartet, snap and near
  radii, commit latency, pointer settle and linger, the ten edge-scroll knobs, the refine
  gain and range, the redetect pair, and whether text is highlighted. One
  `gaze_config::Tuning` struct, one key per field under cosmic-config
  `dev.techgeek1.CosmicGaze` v1, a `KNOBS` table (key, label, unit, range, step) so the
  applet can draw them without knowing any of them. The daemon watches the config and
  applies a change at the next sample without restarting anything: the filter stack and
  snap engine are rebuilt, the overlay is restyled, the scroller, the controller mapper
  and the perception thread take new parameters in place.

**What covers the windows.** The overlay's layer surfaces subscribe to
`zcosmic_overlap_notify_v1` (2026-09-12), so the session knows where the panel, the dock
and any other top or overlay layer surface is, including an auto-hidden panel the moment
it shows. A gaze there starts no edge scroll and shows no band; a scroll already running
keeps going, since its parked pointer at the screen edge is what reveals the panel.
Popups of layer surfaces (an applet's) are not reported, so the applet tells the daemon
over the bus (`SetPopupOpen`) while its own popup is up and the same rule holds
everywhere; other applets' popups are still unseen.

**Live state and commands** over the session D-Bus (`zbus`, as `gaze-a11y` already
uses), name and path `dev.techgeek1.CosmicGaze` / `/dev/techgeek1/CosmicGaze`:
properties `Tracker`, `Calibrated`, `Controller`, `Paused` (booleans), `Mode`
(`no-tracker`, `paused`, `reading`, `pointing`, `scrolling`, `calibrating`); methods
`Pause`, `Resume`, `SetPopupOpen`, `Calibrate`, `Quit`. The offset properties and `ResetOffset` went
with the online offset on 2026-09-12. `gaze_config::Status` is the Rust side of
those properties, filled by the session loop through `gaze_proto::Live`, which also
carries the pause and calibrate flags and the tuning in.

The daemon retries the tracker every few seconds when it is not on the bus, and runs
the session again if it ends. The overlay stays in-process for latency. Not started with
the session: the applet's Start button runs it and its Stop button calls `Quit`
(2026-09-10), so the tracker only runs while it is wanted.

## U3. Applet — built 2026-09-10

`gaze-applet` (`cosmic-ext-applet-gaze`), a libcosmic panel applet on the same pin as
the overlay's cosmic-config. Status icon; a popup in two fixed groups so nothing
reflows as the daemon comes and goes (2026-09-12): Status, with the mode beside the
switch that runs the installed `gazed` and calls `Quit`, and the calibrated dot beside
the Calibrate button (U4); Gaze, with the tracker and controller dots; then an
"Advanced" section holding the pause toggle and every
`KNOBS` entry as a slider writing straight to cosmic-config, so the feel can be tuned
on the running system without a rebuild or a restart. It polls the daemon's properties
over D-Bus (one `GetAll` a second, faster while open), which survives the daemon
restarting. Nothing gaze-specific in it beyond `gaze-config`.

## U4. Self-contained: calibrate in the daemon, a generated desk file, downloaded models

Ruled 2026-09-10: setting up a new machine and recalibrating must not need the CLI, and
there is no fit step (the residual model went). Three pieces:

- **Calibrate in the daemon — built 2026-09-12 as the quick ceremony.** The daemon
  holds the device, so calibration runs in the daemon between two sessions, behind a
  `Calibrate` method and a `calibrating` mode, and the applet has a Calibrate button
  (reads "Calibrating" while it runs; Stop aborts it, nothing written). What runs is
  `retrain::plan_quick` (PLAN-ET5 A6): the current blob seeds the device, one round of
  five targets (centre and the training rectangle's corners) is shown over the dimmed
  desktop, accepted on the gate's vote with the dwell fallback, applied once; the
  device is reopened to prove it kept the model, and the blob and calibration are
  written to the XDG config dir (previous files kept as `*.prev-<unix>`), the session
  restarting on them. The CLI's 4x4 health check follows the round (ruled 2026-09-12:
  once per session is not long), so the log carries rms and median per run and the
  correction field is refitted every time. The full two-background ceremony and the
  Daydream commit stay in the CLI; the online offset, which was to start over here, is
  gone.
- **A generated desk file.** What the ET5 path reads from `desk.toml` is the tracked
  output's logical rect and physical size (the compositor knows both), the panel pose
  relative to the camera (a mount constant for the clip bar, centred under the panel),
  the nominal eye, and the tracker pitch (derived at calibration from the reported eye
  origin against the assumed eye height, the derivation the desk file's comments do by
  hand). The daemon writes the file when there is none, from one choice in the applet:
  which output the bar is under.
- **The ONNX models as a download.** The export tooling is AGPL Ultralytics under torch,
  so `just install` cannot build them; the exported files are hosted as a release asset
  the justfile fetches into `$XDG_DATA_HOME/cosmic-gaze/models/`.

The most work in the split, last on purpose.

## Order

U1 first because it is visible immediately and verifiable with the CLI alone; U2 and U3
together, since the applet is the daemon's first client; U4 last. A perf pass over the
whole loop comes after, when the daemon is the thing to profile.
