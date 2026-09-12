# Installs the daemon and the applet for this user. `just install` puts the binaries
# under ~/.local/bin, the applet's desktop entry where the panel looks for applets and its
# panel icon under the hicolor theme;
# `just uninstall` removes them. The daemon is started and stopped from the applet, not
# with the session. `just install-desk` copies the desk's files (desk.toml, the ET5
# calibration and its device blob, the ONNX models) from the
# checkout to the XDG locations the installed gazed reads; without it, run
# `gazed --home .` from the checkout instead. `just fetch-models` pulls the two ONNX
# detector models from the `models-v1` release into models/ (needs `gh auth login`;
# the repo is private), which install-desk runs when models/ is empty.

prefix    := env_var_or_default("PREFIX", env_var("HOME") + "/.local")
daemon    := "gazed"
applet    := "cosmic-ext-applet-gaze"
applet_id := "dev.techgeek1.CosmicGazeApplet"
xdg_conf  := env_var_or_default("XDG_CONFIG_HOME", env_var("HOME") + "/.config") + "/cosmic-gaze"
xdg_data  := env_var_or_default("XDG_DATA_HOME", env_var("HOME") + "/.local/share") + "/cosmic-gaze/models"
icons     := env_var_or_default("XDG_DATA_HOME", env_var("HOME") + "/.local/share") + "/icons/hicolor/scalable/apps"
models    := "models-v1"

build:
    cargo build --release -p gaze-daemon -p gaze-applet

install: build
    install -Dm0755 target/release/{{daemon}} {{prefix}}/bin/{{daemon}}
    install -Dm0755 target/release/{{applet}} {{prefix}}/bin/{{applet}}
    install -Dm0644 crates/gaze-applet/data/{{applet_id}}.desktop {{prefix}}/share/applications/{{applet_id}}.desktop
    install -Dm0644 crates/gaze-applet/data/{{applet_id}}-symbolic.svg {{icons}}/{{applet_id}}-symbolic.svg

# Downloads the detector models from the release into models/ (skips files already there)
fetch-models:
    gh release download {{models}} --pattern '*.onnx' --dir models --skip-existing

# Copies the desk's files from the checkout to where an installed gazed looks (the offset is state gazed writes itself and is not copied)
install-desk: fetch-models
    install -Dm0644 config/desk.toml            {{xdg_conf}}/desk.toml
    install -Dm0644 config/calibration-et5.toml {{xdg_conf}}/calibration-et5.toml
    install -Dm0644 config/calibration-et5.bin  {{xdg_conf}}/calibration-et5.bin
    install -Dm0644 -t {{xdg_data}} models/*.onnx

uninstall:
    rm -f {{prefix}}/bin/{{daemon}} {{prefix}}/bin/{{applet}}
    rm -f {{prefix}}/share/applications/{{applet_id}}.desktop
    rm -f {{icons}}/{{applet_id}}-symbolic.svg
