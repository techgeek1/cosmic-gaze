# cosmic-gaze

Gaze pointing, scrolling and clicking for the COSMIC desktop with a Tobii Eye Tracker 5.
The tracker's own calibration puts a pointer where you look, the pointer settles onto the
control under it, a real click or the Daydream controller's pad commits, and looking at a
window's edge scrolls it.

I built it to keep working through a repetitive strain injury. Together with my voice
dictation tool, [cosmic-voice](https://github.com/techgeek1/cosmic-voice), it moved most
of my pointing and typing off my hands, and the RSI is now something I rarely notice.

This is a personal project. It works on my setup, and it is published in case it is
useful to someone with a similar one, not as a supported product. Expect to read code
and edit config files to get it running on yours.

## What it needs

- The COSMIC desktop on Wayland. It has been run against cosmic-comp 1.6.0.
- A Tobii Eye Tracker 5, driven over USB with no vendor software. A factory-fresh unit
  needs a one-time firmware flash from Tobii's Windows installer (a VM works), and the
  user needs read-write access to it through a udev rule; see
  `crates/gaze-provider-et5/README.md`.
- Write access to `/dev/uinput` for pointer, click and scroll injection.
- The system onnxruntime library (Arch's `onnxruntime-cpu` here) and two ONNX detector
  models, which you fetch or export yourself (below).
- Optional: a Google Daydream controller over Bluetooth, whose pad and buttons commit
  clicks and refine the pointer.

`DESIGN.md` is the why; `PLAN.md`, `PLAN-ET5.md`, `PLAN-UX.md` and `PLAN-INTENT.md` are
the build plans. All of them are historical records: parts of what they describe were
removed later, and the code is the source of truth.

Much of the code was written with Claude Code; `CLAUDE.md` is its project brief.

## Install

```
just export-widget-model   # once: exports the TargetFinder model into models/ (runs Ultralytics, AGPL-3.0)
just install               # gazed and the applet into ~/.local/bin, the applet's desktop entry
just install-desk          # desk.toml, the ET5 calibration and the detector models into the XDG dirs
```

Then add the "Gaze" applet to a panel (Settings, Desktop, Panel, Configure panel
applets) and flip the switch in its popup. Calibrate, Pause and the tuning sliders
are in the same popup; the daemon's log is `~/.local/state/cosmic-gaze/gazed.log`. The
daemon does not start with the session, so the tracker only runs while it is wanted.

`install-desk` downloads the PP-OCRv5 text detector from Hugging Face if `models/` does
not have it, and stops with a message if the TargetFinder export is missing. A new desk
needs `config/desk.toml` edited for its monitors and mount (the checked-in one is mine)
and a calibration: `cargo run --release --bin gaze-et5-cli -- calibrate` writes
`config/calibration-et5.toml` and the device blob that `install-desk` copies.

## Models and licences

The code is GPL-3.0-only (`LICENSE`). That is not a preference so much as a
consequence: the ET5 driver includes protocol payloads taken from, and tests translated
from, [tobiifree](https://github.com/Aetherall/tobiifree), which is GPL-3.0
(`crates/gaze-provider-et5/README.md` and `NOTICE` say exactly what).

This repo distributes no model files. The detector models under `models/` are not ours,
and you get them yourself:

- `ch_PP-OCRv5_det.onnx` is PaddlePaddle's PP-OCRv5 mobile text detector, Apache-2.0,
  downloaded by `just fetch-ocr-model` from
  [`huggingface.co/webnn/PP-OCRv5-ONNX`](https://huggingface.co/webnn/PP-OCRv5-ONNX).
- `yolo26n-640.onnx` is the TargetFinder widget detector (arXiv:2607.19907, repo MIT),
  an Ultralytics YOLO26n fine-tune whose checkpoint metadata claims AGPL-3.0, and
  Ultralytics hold that AGPL reaches derived weights. The upstream MIT grant and that
  string disagree; `crates/gaze-detect/README.md` has the full note and the MIT-clean
  alternatives. `just export-widget-model` runs
  `crates/gaze-detect/scripts/fetch_models.py`, which downloads the upstream checkpoint
  and exports it with Ultralytics (AGPL-3.0) on your machine. The result is yours; this
  repo does not ship it.

The export tooling is never linked into a binary here.

## Acknowledgements

The ET5 driver stands on tobiifree's protocol documentation, nottobii's capture of the
Windows driver's init order and Talon's `eye_mouse.py` calibration flow. `NOTICE` has the
full list, along with the papers behind the fixation filter and edge scrolling.
