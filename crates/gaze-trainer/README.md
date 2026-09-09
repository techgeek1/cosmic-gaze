# gaze-trainer

A working libcosmic application whose every control is labelled, for collecting gaze
labels faster than passive clicks and more honestly than a dot ceremony. See
`PLAN-ET5.md` B5 and the crate docs in `src/lib.rs` for the why.

## Running

Start the collector first, on the same desk; it listens for the trainer on
`$XDG_RUNTIME_DIR/gaze-clicks.sock`:

```sh
cargo run -p gaze-clicks --bin gaze-clicks-cli -- run
cargo run -p gaze-trainer --release
```

The window goes full screen on the output it opens on (open it on DP-1, the tracker's
display). `--output` names the output whose origin the coverage histogram is seeded
against; it defaults to DP-1. `--posture-every N` sets the number of labelled presses
between posture prompts (default 80). `--offline` silences the missing-collector
warning for looking at the application without recording anything.

The status line at the bottom of the window shows whether the collector is connected
and how many presses have been sent or lost. Without a collector nothing is recorded.

## What it does

There is no task and no target: it is an application to navigate. Every window is a
fresh generated application of one of six archetypes — Settings (a rail and a form),
Files (a wide list under a busy toolbar), Mail (a message list beside a reading pane),
Editor (file tabs over an article), Browser (an address bar and links), Store (a grid
of cards) — with a menu bar, an optional sidebar (left or right, random width), a
toolbar (top or bottom), tabs where the archetype has them, and dark or light theme.
Clicking around it does what an application does: menus open, tabs switch, rows
select, links follow, dialogs raise, searches return results. The switcher and the
shuffle button in the header bar generate the next window whenever the current one
stops being interesting; a posture prompt does the same every `--posture-every`
presses.

The layout is where coverage steers. An 8×4 histogram over the screen, seeded from
every click already in `config/sessions/`, decides how likely the next window is to
put its sidebar on the left and its toolbar on top, so the emptier half of the screen
gets the controls more often.

Every press inside the window goes to the collector with the widget's box, label,
kind, the window's generation number, whether a labelled control was under it, the
posture last asked for, and the theme. The collector treats a matched press as
authoritative (`source = "trainer"`), skips recognition for it, and writes it into the
ordinary session file. Presses on nothing labelled (padding, the header bar) are
reported as such and refused there as `no-element`.

## Not run here

The window needs a live compositor and the collector needs the mouse and the tracker.
`cargo test -p gaze-trainer` covers the scene generator, the six archetypes, the probe
registry and the coverage histogram; nothing in the tests opens a window.
