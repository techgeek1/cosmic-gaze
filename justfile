# Installs the daemon and the applet for this user. `just install` puts the binaries
# under ~/.local/bin, the applet's desktop entry where the panel looks for applets,
# and the daemon's autostart entry where cosmic-session looks; `just uninstall`
# removes them. `just install-desk` copies the desk's files (desk.toml, the ET5
# calibration and its device blob, the residual model, the ONNX models) from the
# checkout to the XDG locations the installed gazed reads; without it, run
# `gazed --home .` from the checkout instead.

prefix    := env_var_or_default("PREFIX", env_var("HOME") + "/.local")
daemon    := "gazed"
applet    := "cosmic-ext-applet-gaze"
applet_id := "dev.techgeek1.CosmicGazeApplet"
daemon_id := "dev.techgeek1.CosmicGaze"
xdg_conf  := env_var_or_default("XDG_CONFIG_HOME", env_var("HOME") + "/.config") + "/cosmic-gaze"
xdg_data  := env_var_or_default("XDG_DATA_HOME", env_var("HOME") + "/.local/share") + "/cosmic-gaze/models"

build:
    cargo build --release -p gaze-daemon -p gaze-applet

install: build
    install -Dm0755 target/release/{{daemon}} {{prefix}}/bin/{{daemon}}
    install -Dm0755 target/release/{{applet}} {{prefix}}/bin/{{applet}}
    install -Dm0644 crates/gaze-applet/data/{{applet_id}}.desktop {{prefix}}/share/applications/{{applet_id}}.desktop
    install -Dm0644 data/{{daemon_id}}.desktop {{env_var("HOME")}}/.config/autostart/{{daemon_id}}.desktop

# Copies the desk's files from the checkout to where an installed gazed looks (the offset and flywheel are state gazed writes itself and are not copied)
install-desk:
    install -Dm0644 config/desk.toml            {{xdg_conf}}/desk.toml
    install -Dm0644 config/calibration-et5.toml {{xdg_conf}}/calibration-et5.toml
    install -Dm0644 config/calibration-et5.bin  {{xdg_conf}}/calibration-et5.bin
    install -Dm0644 config/model-et5.json       {{xdg_conf}}/model-et5.json
    install -Dm0644 -t {{xdg_data}} models/*.onnx

uninstall:
    rm -f {{prefix}}/bin/{{daemon}} {{prefix}}/bin/{{applet}}
    rm -f {{prefix}}/share/applications/{{applet_id}}.desktop
    rm -f {{env_var("HOME")}}/.config/autostart/{{daemon_id}}.desktop
