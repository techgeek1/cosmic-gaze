# Installs the daemon and the applet for this user. `just install` puts the binaries
# under ~/.local/bin, the applet's desktop entry where the panel looks for applets and its
# panel icon under the hicolor theme;
# `just uninstall` removes them. The daemon is started and stopped from the applet, not
# with the session. `just install-desk` copies the desk's files (desk.toml, the ET5
# calibration and its device blob, the ONNX models) from the
# checkout to the XDG locations the installed gazed reads; without it, run
# `gazed --home .` from the checkout instead.
#
# This repo distributes no models. `just fetch-ocr-model` downloads the PP-OCRv5 text
# detector (Apache-2.0) from its upstream Hugging Face repo, and install-desk runs it.
# The TargetFinder widget detector has to be exported on your machine with
# `just export-widget-model`, which runs Ultralytics (AGPL-3.0); see the README.

prefix    := env_var_or_default("PREFIX", env_var("HOME") + "/.local")
daemon    := "gazed"
applet    := "cosmic-ext-applet-gaze"
applet_id := "dev.techgeek1.CosmicGazeApplet"
xdg_conf  := env_var_or_default("XDG_CONFIG_HOME", env_var("HOME") + "/.config") + "/cosmic-gaze"
xdg_data  := env_var_or_default("XDG_DATA_HOME", env_var("HOME") + "/.local/share") + "/cosmic-gaze/models"
icons     := env_var_or_default("XDG_DATA_HOME", env_var("HOME") + "/.local/share") + "/icons/hicolor/scalable/apps"
ppocr_url := "https://huggingface.co/webnn/PP-OCRv5-ONNX/resolve/main/ch_PP-OCRv5_det.onnx"
ppocr     := "models/ch_PP-OCRv5_det.onnx"
widget    := "models/yolo26n-640.onnx"

build:
    cargo build --release -p gaze-daemon -p gaze-applet

install: build
    install -Dm0755 target/release/{{daemon}} {{prefix}}/bin/{{daemon}}
    install -Dm0755 target/release/{{applet}} {{prefix}}/bin/{{applet}}
    install -Dm0644 crates/gaze-applet/data/{{applet_id}}.desktop {{prefix}}/share/applications/{{applet_id}}.desktop
    install -Dm0644 crates/gaze-applet/data/{{applet_id}}-symbolic.svg {{icons}}/{{applet_id}}-symbolic.svg

# Downloads the PP-OCRv5 text detector from Hugging Face into models/ (skips it if already there)
fetch-ocr-model:
    mkdir -p models
    [ -f {{ppocr}} ] || { curl -fL --retry 3 -o {{ppocr}}.part {{ppocr_url}} && mv {{ppocr}}.part {{ppocr}}; }

# Exports the TargetFinder widget detector into models/ on this machine. Runs Ultralytics (AGPL-3.0) under uv; the result is yours, not something this repo ships
export-widget-model:
    UV_TORCH_BACKEND=cpu uv run --python 3.12 crates/gaze-detect/scripts/fetch_models.py

# Copies the desk's files from the checkout to where an installed gazed looks (needs the widget model exported first)
install-desk: fetch-ocr-model
    @[ -f {{widget}} ] || { echo "{{widget}} is missing: run 'just export-widget-model' first (it runs Ultralytics, AGPL-3.0; see the README)" >&2; exit 1; }
    install -Dm0644 config/desk.toml            {{xdg_conf}}/desk.toml
    install -Dm0644 config/calibration-et5.toml {{xdg_conf}}/calibration-et5.toml
    install -Dm0644 config/calibration-et5.bin  {{xdg_conf}}/calibration-et5.bin
    install -Dm0644 -t {{xdg_data}} models/*.onnx

uninstall:
    rm -f {{prefix}}/bin/{{daemon}} {{prefix}}/bin/{{applet}}
    rm -f {{prefix}}/share/applications/{{applet_id}}.desktop
    rm -f {{icons}}/{{applet_id}}-symbolic.svg
