# gaze-daemon (`gazed`)

The gaze session as a daemon: owns the ET5, the overlay and the injector for the life
of the desktop session, keeps the session running whatever happens to the tracker, and
serves a small control interface on the session bus. `PLAN-UX.md` U2 is the spec.

## Running

From a checkout, with the desk's files where the prototype kept them:

```
cargo run --release --bin gazed -- --home .
```

Without `--home` the files are read from the XDG locations, printed at startup:

| what                          | where                                          |
|-------------------------------|------------------------------------------------|
| `desk.toml`, calibration, model | `~/.config/cosmic-gaze/`                     |
| ONNX models                   | `~/.local/share/cosmic-gaze/models/`           |
| offset, flywheel              | `~/.local/state/cosmic-gaze/`                  |

`--dry-run` logs clicks, warps and scrolls instead of injecting them;
`--overlay-debug` draws the debug look. Everything else is tuning, stored by
cosmic-config under `~/.config/cosmic/dev.techgeek1.CosmicGaze/v1/` (one file per knob,
written with the defaults on first run) and applied at the next sample when edited,
whether by the applet's Advanced section or by hand.

`just install` from the workspace root puts `gazed` and the applet under `~/.local/bin`
and installs the autostart entry (`data/dev.techgeek1.CosmicGaze.desktop`) that
cosmic-session honours; `just install-desk` copies the desk's files from the checkout
to the XDG locations above. After both, `gazed` from a terminal is the installed daemon
on the installed files, and the next login starts it by itself.

## Control interface

```
busctl --user introspect dev.techgeek1.CosmicGaze /dev/techgeek1/CosmicGaze
busctl --user call dev.techgeek1.CosmicGaze /dev/techgeek1/CosmicGaze dev.techgeek1.CosmicGaze Pause
busctl --user get-property dev.techgeek1.CosmicGaze /dev/techgeek1/CosmicGaze dev.techgeek1.CosmicGaze Mode
```

Properties: `Tracker`, `Calibrated`, `Model`, `Controller`, `Paused`, `Mode`
(`no-tracker`, `paused`, `reading`, `pointing`, `scrolling`), `OffsetUpdates`,
`OffsetJumps`, `OffsetYawDeg`, `OffsetPitchDeg`. Methods: `Pause`, `Resume`,
`ResetOffset`. The Rust side of this is `gaze_config::bus`.

## What it needs

A running COSMIC session (the overlay and screen capture), the ET5 on USB (the session
retries every five seconds while it is not), the user in the `input` group (the
injector, the F14 latch), and optionally a paired Daydream controller, picked up when
it wakes.
