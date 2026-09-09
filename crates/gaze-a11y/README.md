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

Chromium hides the shadow somewhere else. Discord's frame answers `(0, 0) 1291x1448` for
a 1271x1428 toplevel, in both coordinate types: the origin is zero and the *size* carries
the 20 px, so with the origin-only rule every row and heading sat 10 px right of and below
its pixels. The shadow is symmetric (with a 10 px shift the sidebar's section lands on the
toplevel's left edge and bottom exactly), so the content origin is
`frame.origin + (frame.size - toplevel.size) / 2`, which reads as `(20, 20)` for Firefox
and `(10, 10)` for Discord. A frame no larger than its toplevel adds nothing.

## Coverage, measured 2026-08-29

| application | on the bus | answers |
| --- | --- | --- |
| Firefox | yes | yes, roles and extents correct |
| Discord (Electron, Flatpak) | with the override below | yes, 2–10 ms; `link` rows with names and extents |
| VS Code (Electron, native) | yes | with `--force-renderer-accessibility` and the punch-through below; buttons, menus, headings, list items at 3–5 ms |
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

A *native* Electron app skips the sandbox problem — the host gsetting is already true, so
it sits on the bus with a named frame — but still needs the flag for the renderer tree.
VS Code's wrapper (`/usr/bin/code`) reads `~/.config/code-flags.conf`, one flag per line,
same as Discord's.

VS Code adds one more trap: even with the tree fully populated, `GetAccessibleAtPoint` on
the frame answered a nameless, childless panel for every content point. Its workbench
keeps an invisible full-window overlay as a *later sibling* of the branch holding the
`document web`, and Chromium hit-tests topmost first, so the empty overlay wins. `at`
now treats a hit on such a *vacant* node (nameless, childless, generic container role) as
a non-answer and punches through: climb from it, hit-test each ancestor's other
point-containing children topmost first, take the first non-vacant answer. Verified with
a 40–80 px grid: menu items, title-bar buttons, welcome-page headings and buttons all
answer with names and extents. VS Code's frame extents equal its toplevel exactly (no
shadow insets, custom title bar), so the shadow rule is a no-op there.

Do **not** reach for `org.a11y.Status.ScreenReaderEnabled` on COSMIC: at-spi-bus-launcher
mirrors it into `org.gnome.desktop.a11y.applications screen-reader-enabled`,
cosmic-session watches that key and starts Orca (auto-restarting when killed), Orca
grabs the keyboard, and Chromium ignores the property anyway. Tried 2026-08-29,
reverted within the minute.

`A11y::at` returns `Ok(None)` for anything not on the bus, and the caller falls back to
the pixels. That is the division of labour the design set out: the tree where there is
one, the recogniser where there is not.

## The second question: what scrolls here

`A11y::scroll_surface` finds the scrollable region under a point, for `gaze-proto`'s edge
scroller. Toolkits do not label scroll surfaces (AT-SPI has a `scroll pane` role, but
Firefox scrolls its page as a `document web` and a Discord list is a `section`), so the
rule is geometric: walk the ancestors of the node under the point, and the first one whose
child's extents poke out above or below its own is the clip, its extents the viewport
(`clip_surface`). Measured 2026-09-04 with `gaze-a11y-cli chain`:

| point                    | overflowing child            | clip found                          |
|--------------------------|------------------------------|-------------------------------------|
| Discord message list     | `list` 882 x 4305            | `panel` 898 x 1296, above composer  |
| Discord channel sidebar  | `list` 294 x 2272            | `section` 302 x 1292                |
| Firefox page body        | `landmark` 1269 x 12901      | `document web` 1269 x 1343          |

Content that fits produces no surface, which is also the right answer. Horizontal
overflow (a carousel) is ignored, and so is a clip under 120 px tall (`MIN_CLIP_PX`): a
cell whose glyphs overhang it passes the geometric test but is not what a wheel moves, and
stopping there would hide the page scroll behind it. Cost is the hit plus one parent walk, 10 to 30 ms.

## CLI

```
gaze-a11y-cli apps                 # applications and their windows on the bus
gaze-a11y-cli at 4919,698          # the node under a desk point, with timing
gaze-a11y-cli chain 4919,698       # that node and every ancestor, role and extents each
gaze-a11y-cli follow --seconds 30  # the node under the pointer whenever it moves
```

`follow` is the one to run beside `gaze-clicks-cli probe`: put the pointer on a card, an
input, an avatar, and read what the tree calls it and how long it took.
