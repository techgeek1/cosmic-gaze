# Installs the daemon and the applet for this user. `just install` puts the binaries
# under ~/.local/bin, the applet's desktop entry where the panel looks for applets,
# and the daemon's autostart entry where cosmic-session looks; `just uninstall`
# removes them. The desk's files are not installed: run `gazed --home .` from the
# checkout, or copy config/ and models/ to the XDG locations gazed prints at start.

prefix    := env_var_or_default("PREFIX", env_var("HOME") + "/.local")
daemon    := "gazed"
applet    := "cosmic-ext-applet-gaze"
applet_id := "dev.techgeek1.CosmicGazeApplet"
daemon_id := "dev.techgeek1.CosmicGaze"

build:
    cargo build --release -p gaze-daemon -p gaze-applet

install: build
    install -Dm0755 target/release/{{daemon}} {{prefix}}/bin/{{daemon}}
    install -Dm0755 target/release/{{applet}} {{prefix}}/bin/{{applet}}
    install -Dm0644 crates/gaze-applet/data/{{applet_id}}.desktop {{prefix}}/share/applications/{{applet_id}}.desktop
    install -Dm0644 data/{{daemon_id}}.desktop {{env_var("HOME")}}/.config/autostart/{{daemon_id}}.desktop

uninstall:
    rm -f {{prefix}}/bin/{{daemon}} {{prefix}}/bin/{{applet}}
    rm -f {{prefix}}/share/applications/{{applet_id}}.desktop
    rm -f {{env_var("HOME")}}/.config/autostart/{{daemon_id}}.desktop
