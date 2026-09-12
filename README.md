# cosmic-gaze

Gaze pointing, scrolling and clicking for COSMIC with a Tobii Eye Tracker 5. The
tracker's own calibration puts a pointer where you look, the pointer settles onto the
control under it, a real click or the Daydream controller's pad commits, and looking
at a window's edge scrolls it. `DESIGN.md` is the why; `PLAN.md`, `PLAN-ET5.md` and
`PLAN-UX.md` are the build plans and per-crate contracts.

## Install

```
just install        # gazed and the applet into ~/.local/bin, the applet's desktop entry
just install-desk   # desk.toml, the ET5 calibration and the detector models into the XDG dirs
```

Then add the "Gaze" applet to a panel (Settings, Desktop, Panel, Configure panel
applets) and flip the switch in its popup. Calibrate, Pause and the tuning sliders
are in the same popup; the daemon's log is `~/.local/state/cosmic-gaze/gazed.log`. The
daemon does not start with the session, so the tracker only runs while it is wanted.

`install-desk` fetches the two ONNX models from this repo's `models-v1` release, which
needs `gh auth login` since the repo is private. A new desk needs `config/desk.toml`
edited for its panel (PLAN-UX.md U4 is the plan to generate it) and a calibration:
`cargo run --release --bin gaze-et5-cli -- calibrate` until U4 moves that into the applet.

## Models and licences

The code is MIT OR Apache-2.0. The detector models under `models/` are not ours:

- `yolo26n-640.onnx` is the TargetFinder widget detector (arXiv:2607.19907, repo MIT),
  an Ultralytics YOLO26n fine-tune whose checkpoint metadata claims AGPL-3.0, and
  Ultralytics hold that AGPL reaches derived weights. The upstream MIT grant and that
  string disagree; `crates/gaze-detect/README.md` has the full note and the MIT-clean
  alternatives. This is a personal project and the export is hosted on a private
  release for our own installs, not distributed.
- `ch_PP-OCRv5_det.onnx` is PaddlePaddle's PP-OCRv5 mobile text detector via
  `huggingface.co/webnn/PP-OCRv5-ONNX`, Apache-2.0.

The export tooling (`crates/gaze-detect/scripts/fetch_models.py`, which runs
Ultralytics) is never linked into a binary here.
