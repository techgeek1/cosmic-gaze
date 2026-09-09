# gaze-daydream

Google's Daydream View controller as the commit and fine channel for gaze.

DESIGN.md's fine channel: gaze puts the pointer near the target, a hand-held controller
does the last few pixels and the commit. The Daydream controller is that device for a few
dollars second hand: a touchpad, five buttons, a gyro, and a plain BLE GATT stream nobody
has to write a HID driver for.

## Transport

BlueZ over the system D-Bus, with `zbus`'s blocking API on a thread, the same stack
`gaze-a11y` uses for AT-SPI. The controller's reports are notifications on characteristic
`00000001-1000-1000-8000-00805f9b34fb` of service `0xfe55`; `StartNotify` on it makes
each report a `PropertiesChanged` on `Value`, and one match rule over the device's subtree
also carries the device's `Connected` property, so a controller going to sleep and coming
back is handled on the same iterator. No HCI socket, no async runtime, no `bluer`.

Pair once (the controller must be awake: hold Home until the light blinks):

```
bluetoothctl scan on
bluetoothctl pair  XX:XX:XX:XX:XX:XX
bluetoothctl trust XX:XX:XX:XX:XX:XX
```

After that `Controller::open` connects it itself, provided it is awake. It sleeps after a
few minutes untouched and drops the link; press Home to wake it and the reader reconnects.

## Report format

Twenty bytes at 62 Hz on the desk (`gaze-daydream-cli watch` reports the rate). The
layout is mrdoob's reverse engineering for `daydream-controller.js`, reproduced in
`packet.rs`: a 9-bit tick and 5-bit sequence, then orientation, acceleration and angular
rate as 13-bit two's complement triples (full scale one turn, 8 g, 2048 °/s), two 8-bit
touch coordinates where `(0, 0)` means no touch, and five button bits. Verified
2026-09-04 against the live stream: gravity reads +1 g on y lying flat, gyro noise at rest
is under 0.03 rad/s, and a captured packet is the unit test fixture.

## CLI

```
gaze-daydream-cli raw   --seconds 5    # every report, decoded
gaze-daydream-cli watch --seconds 30   # button edges, touch strokes, a status line a second
```

`watch` is the one to hold the controller for: press each button, drag a thumb across the
pad, turn the wrist, and read what came back. `gaze-proto --daydream` is where it drives
the session (pad click commits, a tap on the pad right-clicks, Home exits, App holds the voice
stack's push-to-talk as F13, volume scrolls, thumb-on-pad refines; a controller lying still
hands the pointer to the mouse).
