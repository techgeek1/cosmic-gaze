# gaze-a11y

What the application says is under a point on the desk, via the session's AT-SPI bus.

The screen recogniser (`gaze-detect`) reads pixels, and pixels cannot say that a
thumbnail, a title and a view count are one link, that a grey rounded rectangle is an
input, or that the thing under the pointer is a picture. The application's accessibility
tree can, and on a COSMIC session the bus is already running: `at-spi-bus-launcher` under
the session, cosmic-comp implementing `org.freedesktop.a11y.Manager`, Firefox and GTK
apps registered on it.

## The one question

`A11y::at(point, window)` asks the window's frame `Component.GetAccessibleAtPoint`, reads
the role, name and extents of what came back, and climbs `Parent` (at most twelve
levels) to the nearest ancestor with an actionable role: button, link, entry, check box,
list item, page tab, image and so on (`is_actionable`). There is no tree walk, ever;
DESIGN.md's finding that on-demand AT-SPI walks are non-viable stands, and this crate
makes about eight round trips per query. Firefox answers in 2 to 9 ms on the desk.

## Coordinates, and why this needs cosmic-comp

A Wayland client does not know where its window is (at-spi2-core#14), so a tree reports
window coordinates. `gaze_capture::ToplevelTracker` reads every window's rectangle from
`zcosmic_toplevel_info_v1`, which is what turns a window point into a desk point. Even
then toolkits disagree about what "window" and "screen" mean, and not in the way you
would guess: Firefox reports its frame at `(20, 20)` in *both* coordinate types, because
its space is its surface including the 20 px client-side shadow, and every node is offset
by the same amount. The first version added the toplevel origin to window coordinates
directly, and every YouTube button sat 20 px below its pixels (the probe drew boxes under
the controls; GitHub looked fine only because a 24 px table row shifted by 20 px still
contains the pointer).

The rule that works for any toolkit is **frame-relative**: read the frame node's own
extents in the coordinate type in use and treat them as the origin, so a desk point is
`p - toplevel.origin + frame.origin` and a node's extents are
`extents - frame.origin + toplevel.origin`. Whatever space the toolkit answers in, its
frame is at the toplevel's rectangle, and the offset cancels. `at` tries window
coordinates first and screen coordinates second, and accepts an answer only when the
node's own extents (asked the same way) contain the query point. Verified with a 40 px
grid over the YouTube window drawn onto a capture: every rectangle on its control.

## Coverage, measured 2026-08-29

| application | on the bus | answers |
| --- | --- | --- |
| Firefox | yes | yes, roles and extents correct |
| Discord (Electron, Flatpak) | with the override below | yes, 2–10 ms; `link` rows with names and extents |
| steamwebhelper (Chromium) | yes, as "Chromium" | thirteen unnamed frames, null at every point: accessibility off |
| COSMIC Terminal, other iced apps | no | — |

Chromium and Electron start their ATK bridge only when `ShouldEnableAccessibility`
says so, which on Linux means the `GNOME_ACCESSIBILITY` environment variable or the
`org.gnome.desktop.interface toolkit-accessibility` gsetting. A Flatpak does get the
a11y bus (`AT_SPI_BUS_ADDRESS=unix:path=/run/flatpak/at-spi-bus`) but not the host's
dconf, so inside the sandbox that gsetting reads `false` whatever the desk says and
the app never appears. The fix is per application and needs no screen reader:

```
flatpak override --user --env=GNOME_ACCESSIBILITY=1 com.discordapp.Discord
printf -- '--force-renderer-accessibility\n' \
    > ~/.var/app/com.discordapp.Discord/config/discord-flags.conf   # the wrapper reads it
```

then restart Discord. The flag keeps the renderer's tree built whether or not an
assistive technology is asking; the variable is what puts the app on the bus.

Do **not** reach for `org.a11y.Status.ScreenReaderEnabled` on COSMIC: at-spi-bus-launcher
mirrors it into `org.gnome.desktop.a11y.applications screen-reader-enabled`,
cosmic-session watches that key and starts Orca (auto-restarting when killed), Orca
grabs the keyboard, and Chromium ignores the property anyway. Tried 2026-08-29,
reverted within the minute.

`A11y::at` returns `Ok(None)` for anything not on the bus, and the caller falls back to
the pixels. That is the division of labour the design set out: the tree where there is
one, the recogniser where there is not.

## CLI

```
gaze-a11y-cli apps                 # applications and their windows on the bus
gaze-a11y-cli at 4919,698          # the node under a desk point, with timing
gaze-a11y-cli follow --seconds 30  # the node under the pointer whenever it moves
```

`follow` is the one to run beside `gaze-clicks-cli probe`: put the pointer on a card, an
input, an avatar, and read what the tree calls it and how long it took.
