# gaze-provider-et5

A `GazeProvider` for the Tobii Eye Tracker 5, speaking the device's USB protocol
natively. No vendor SDK, no daemon, no Windows.

## Protocol provenance

The wire formats (TTP framing, TLV payloads, the HMAC-MD5 realm unlock, the 0x500
gaze stream columns, calibration ops) are a from-scratch Rust implementation of the
byte-level protocol documented by the tobiifree reverse-engineering project and
verified against this unit. No code was copied; `src/ttp.rs` ports the observed byte
layouts and test vectors only.

## Device setup

- The tracker must be in runtime mode (`lsusb -d 2104:0313`). A factory-fresh unit
  ships in bootloader mode (`2104:0102`) and needs a one-time firmware flash via the
  official installer in a Windows VM (see the project notes; the DFU container is
  encrypted, so it cannot be flashed directly from Linux).
- udev rule for user access: `/etc/udev/rules.d/70-tobii-et5.rules` granting rw on
  `2104:0102/0313/031e`.
- A flashed-but-never-calibrated device streams empty frames (validity 4) even though
  the handshake succeeds. Run `calibrate` (the retrain ceremony) once.
- **The host owns the eye model.** The device's flash is a cache, refilled on every
  connect. The provider uploads `config/calibration-et5.bin` (override with
  `.device_blob(path)`) and verifies it by reading it back; a mismatch fails the
  start. Without the file it warns and runs on whatever the flash holds, which is the
  state that drifts between sessions. This is the Windows driver's and Talon's
  behaviour, and the reason it matters is in `DESIGN.md` §10c.
- The firmware can crash and re-enumerate mid-session (observed 2026-08-27 as a
  one-second USB drop), and a reboot can reset the stored eye model to its ~1.5 KB
  factory default. The provider now handles that directly: on a transport error it
  drops its dropout hold, emits explicitly invalid samples for the length of the gap,
  and reconnects on a backoff with the same blob and plane, re-verifying the upload.
  Manual recovery is `blob-push <blob backup>`.

## The blob: a model body and a result trailer

A blob is two things end to end, and only the first is the model:

- The **body**, everything up to the trailer (604428 of 604948 bytes on this unit).
  This is the opaque firmware eye model. It round-trips an upload byte for byte, and
  **its SHA-256 is the blob's identity**: `blob::body_sha256_hex`, which is what
  `calibration-et5.toml`'s `device_blob_sha256`, a session file's `blob_sha256`, the
  `session_id` prefix and the history key all hold. The whole-blob hash names a
  *retrieval*, not a model, and is only printed as a diagnostic.
- The **trailer**, the firmware's own per-point calibration result table: 40 bytes per
  unique calibration target (13 of them here, 520 bytes), little-endian
  `target_u, target_v, left_u, left_v : f32`, `left_valid : u64`, then the same two
  fields for the right eye. `blob::decode_trailer` finds it by scanning backwards
  while the records validate, so the body/trailer split is measured rather than
  guessed at a size bound. Which per-eye slot is the left eye is a guess (Tobii's own
  result tables are left-then-right); nothing depends on it.

The trailer is a **view** of device state, not state. Retrieving a blob re-expresses
the table in whatever display area is declared *at read time*: the same blob read
under the trained 875 x 370 mm plane and under the oversized virtual plane differs in
every record by exactly the affine map between the two planes. That is the whole of
the "a blob does not round-trip but its body does" result in `DESIGN.md` §10c —
nothing is double-buffered and nothing mutates. So bodies get compared and trailers
get printed, and a table is only comparable number-for-number with another read under
the same plane.

The table is a free per-point health report on the model that was committed, which is
why the decoded form of the committed blob is stored in the calibration file as
`device_result` and `blob-info` prints it as rows.

## Connect sequence

`Device::connect_with(ConnectOptions)` performs, in this order (nottobii's
pcap-derived Windows order; `device::connect_sequence` is the same list as data, and a
unit test asserts it):

    hello -> realm unlock -> cal_apply(blob) -> set display area (corners)
          -> enabled eyes = 3 -> unpause -> [cal_apply again if double_upload]
          -> cal_retrieve and compare -> subscribe

The blob goes up *before* the plane is declared and eye-enable/unpause come after it,
which is what the references do and what earlier failed restores got wrong.
Verification happens before the subscribe so the ~600 KB inbound transfer reassembles
with no gaze notifications interleaved. Comparison is `BlobCheck::Body` by default:
the model bodies byte for byte, plus the same number of points in both result tables
(see below for why the trailer cannot be compared). `BlobCheck::Exact` and
`BlobCheck::SizeAndPrefix` remain for firmware that round-trips its blob unchanged.

`Device::connect()` is `connect_with(ConnectOptions::default())`: no upload, the
device's stored plane left alone. That is what the read-only diagnostics use.

Realm handling stays per-operation: `realm_unlock` is safe to repeat and `cal_apply`
unlocks and closes around itself, so no realm session spans the plane declaration.
nottobii keeps one session across its whole init; nothing observed here needs that.

## CLI

`gaze-et5-cli` (see `--help`):

- `info [--seconds S] [--no-blob]` — upload the saved eye model (a replugged tracker is
  back on its factory default and streams nothing), print the declared display area and
  the device's stream rates, then one line a second: rate, tracked share per eye, eye
  origin, and its azimuth/elevation off the sensor axis with gauges, plus distance. The
  mount-aiming loop; `--seconds 0` runs until Ctrl-C. The rate is always 33 Hz: the
  ET5's camera runs at 132 Hz and the firmware publishes every fourth frame (the device
  reports `(132, 33)` for op 0x672, and `frame_counter` advances four per notification).
  That is Tobii's specification for this unit, not a mode; the tracked share is the
  health signal.
- `op <opcode> [--u32 N | --hex "..."] [--seconds S]` — send one raw opcode and print
  the response as hex and a best-effort TLV walk, optionally watching the stream rate
  afterwards. The probe for opcodes the typed API does not cover; nottobii's and Talon's
  maps name many more than this crate sends. Read-only queries are safe to run
  unattended; anything else is an experiment.
- `dump --seconds 10 --jsonl out.jsonl` — decoded frames plus desk intersection.
- `set-display-area` — declare the display plane (defaults: the 237x148 mm small
  panel with the tracker centred on its top edge).
- `cal-backup FILE` — download the on-device calibration blob to a file.
- `blob-info [--file F] [--calibration F]` — retrieve the blob twice and report, for
  each, the length and hash of its model body and the decoded result table: target uv,
  each eye's measured uv and validity flag, and the per-eye error in degrees at a
  nominal 650 mm when the plane the numbers are normalised against is known (the
  device's currently declared area for the live retrieves, `--calibration`'s
  `device_area` for `--file`). Then whether the two reads agree, and whether the saved
  blob and the device hold the same *model*. Read-only, safe to run unattended.
- `blob-watch --minutes N` — retrieve, stream for N minutes with a status line every
  10 s, retrieve again, print the result table at both ends and report whether the
  **body** changed: does the firmware mutate its model during ordinary use? A trailer
  that moved with the body intact is reported separately, since that is not a changed
  model. Read-only.
- `blob-push FILE [--double] [--calibration F]` — upload a blob through the
  connect-time path above and verify it. `--double` sends it twice, as the Windows
  driver does. SIGINT is held off for the duration; **a process killed mid-upload
  wedges the tracker until it is physically unplugged and replugged**. Replaces the
  old `cal-restore`, which uploaded with no plane and no verification.
- `calibrate [--daydream] [--manual]` — **the retrain ceremony** (`src/retrain.rs`):
  the one command that writes the tracker's own eye model. With `--daydream` the
  controller's pad click takes each point (the gate's vote only checks that the reported
  gaze names that target), App skips and Home aborts, and nothing is accepted on the vote
  alone; `--manual` is the same rule from the keyboard (`c` commits). It declares the panel's measured plane from
  `desk.toml` (corners rotated into the sensor frame by `tracker_pitch_deg` — the
  same plane the provider re-declares on every connect), lays a 600x340 mm training
  area bottom-aligned to the panel and centred on the tracker (`--area WxH`,
  `--area-full`), and runs six rounds of Talon's schedule — centre, four mid-edges,
  four corners — on a black background and then a white one, with a 4 s pupil
  adaptation wait at each flip and `cal_points_apply` after every round. After
  `cal_stop` + `cal_retrieve` it runs a 3x3 health check on neutral grey (1 s per
  stop, the firmware's gaze against the target in degrees), then reopens the device to
  check the model survived, and only then writes the blob and the numbers, the plane,
  the blob's body hash and the blob's decoded result table (`device_result`) into
  `config/calibration-et5.toml`. The previous blob and calibration are moved aside as
  `*.prev-<unix>`, never overwritten. `--suggest` additionally queries
  `CALIBRATE_GET_POINT_SUGGESTION` (0x442) after each round and logs the raw reply;
  nothing depends on it. `--dry-run` prints the plane, the area, the seed decision, the
  nine points and the round schedule without touching the device or the compositor.
  Needs a live compositor and a seated user.

  **The session is seeded.** `cal_start`, `cal_clear`, then `cal_apply` of the blob the
  run is about to replace (nottobii's captured Windows order), because every gate here
  reads the device's *own* gaze and a cleared model does not report one. The seed
  defaults to `--blob` when that file exists and decodes with a result trailer, is
  overridden by `--seed FILE` (a file that cannot be used is an error, never a silent
  skip), and is dropped by `--no-seed`. Which was used is printed by `--dry-run` and
  logged by the real run.

  **The gate is Talon's.** A point is fed to the device once the firmware's own gaze
  has named it as the nearest of the round's targets for 22 of the last 43 frames (Talon's 60 of 120 were at the 4C's 90 Hz; the same 0.67 s of 1.33 s at the ET5's 33 Hz)
  (capped at 2 s) — no accuracy test, because during a retrain the model reporting the
  gaze is the one being replaced. A round with one target therefore accepts on any 60
  frames that carry a gaze point at all. `--accept-deg` adds the old ellipse back on
  top of the vote for a deliberate experiment; it is off by default.

  **Nothing is skipped and nothing starves silently.** Each point reports frames seen,
  frames carrying a gaze point and frames with both eyes tracked, once a second. If no
  frame has carried a gaze point at all after `--gaze-timeout-s` (default 5), the point
  falls back to a **dwell**: both eyes tracked for 1.5 s of continuous frames adds it,
  logged as `dwell`. `--point-timeout-s` (default 15) is a nag interval, not a timeout:
  it says which point it is waiting on and keeps waiting. Enter forces one in, `s`
  skips it (the only way a point is not added), `q` aborts and commits nothing.
  `--apply-from-round N` (default 1) defers the first `cal_points_apply` past the
  one-point round without losing its points, for testing whether a one-point fit is
  what leaves the device reporting no live gaze.

  **It refuses to commit a bad ceremony.** Fewer than `--min-points` (default 9)
  accepted and the session is closed, nothing is written, and the previous blob and
  calibration stay exactly where they were. Same for a tracker that leaves the USB bus
  mid-ceremony (a firmware reboot, which resets the model to the factory blob) and for
  a model that does not survive the post-ceremony reconnect: the check drops the
  device, waits a second, reopens blob-less, `cal_retrieve`s and requires the body hash
  to match what was committed. The USB bus address is printed either side of that
  reconnect, so a re-enumeration is visible in the log even when the model survived.
  This is the 2026-08-28 01:24 failure: the ceremony accepted 2 of 18 points on a
  starved 3° gate, committed the two-point model over a good one, and the tracker
  re-enumerated as it finished, so the file it wrote described a model the device no
  longer had.

  **Retrain once.** The correction field and the online offset are keyed to the
  blob's hash, so a retrain starts both over. Run this after a remount, a fresh
  device, or a deliberate experiment — not when a session feels off; the offset
  absorbs a day's drift by itself.

- `health [--grid 4] [--daydream]` — the health check on its own against the committed
  model: uploads the blob, declares the trained plane, dwells on the grid, replaces the
  calibration's health table and refits the correction field from it. Sixteen seconds at
  the default grid, no retrain. `calibrate` does the same at the end of a ceremony.
- `fit-field [--dry-run]` — refit the correction field from the health stops already in
  the calibration file. The identity competes on held-out error, so a sparse or noisy
  health table leaves the field alone and says so.
- `view [--calibration F | --direct OUTPUT]` — live gaze marker on every display through
  `gaze-overlay`. `--direct DP-2` declares that display's configured plane to the firmware
  and draws its own 2D output on it with no client fit: the range check after a remount.

`gaze-proto` (and `gazed`) run the full snap/click session on this provider; they pick
up `config/calibration-et5.toml` from the desk's files. In direct mode the firmware's
trained 2D output is mapped to the panel, run through the head-gain correction and the
correction field the health check fitted, and then moved by the online offset.

The online offset (`offset.rs`, PLAN-ET5 D5) is the day's yaw/pitch bias, learnt from
the real mouse and the Daydream pad while the session runs. The ray from the eyes
through the calibrated point is rotated by the offset for the current posture and
intersected with the desk again (`correct_direction`, the inverse of the leftover a
click measures). Every physical press (and every pad commit) is attributed against the
corrected rays of the 0.4 s before it and the median leftover, if under 3°, enters the
offset clipped at 2°. The bias is keyed to where the eyes are: a set of anchors in tracker space, one per
posture (60 mm reach on the binocular midpoint, rebuilt from the last eye spacing when
one eye drops), each holding its own bias and blended by distance, so sitting up and
slouching each keep their own correction instead of relearning one over twenty clicks.
A click far from every anchor founds a new one at the blended prediction and its clicks
enter at `1/(n+1)` over a two-click prior; established anchors move by a tenth. It
persists to `config/offset-et5.json`, keyed to the calibration's blob; `gaze-proto
--freeze-offset` attributes and logs without moving it, and deleting the file starts
cold. Leave-one-session-out on the 2026-09-04 click sessions put a single bias at
0.2 to 0.3° of median error, the largest term after the firmware's own; the residual
model that sat under it (1.83° to 1.44° held out, Phases C and D) was removed on
2026-09-10 along with its trainer, recorder, exporter and flywheel, because the
offset alone carried the day and the model's gain sat under the snap radius.

The 3° gate has a way to be wrong: a bias larger than it (glasses moved, a knocked
mount, a posture the anchors have not seen) rejects every click and the filter can
never learn its way out. So rejects are held (PLAN-ET5 E2): when four recent rejects
near one eye position agree to within 1° *and* were clicks on places at least 30 mm
apart, their median is the truth, the anchor jumps to it in one unclipped step, and
the session log says so (`click adopted`). Scattered rejects, and one widget clicked
four times, stay rejected. A jump count lives in the offset file; a day with several
wants a retrain, not a filter.

Calibration files, blobs and the offset are gitignored (`/config/calibration*`,
`/config/offset*.json`).

## What was and was not run

Unit tests cover the wire protocol against captured reference vectors, the gaze
decoder, the correction field fits, the head-gain regression, the retrain's plan
(plane pitch, training rectangle, round schedule), its acceptance gate as a pure
function (the nearest-target vote with and without a radius, and a one-target round
accepting on frames alone) and its seed resolution against a real blob, the
point-suggestion decoder, the provider's edge-pinning and dropout-hold behaviour and
its sensor-to-desk rotation, the ray-correction inverse and the yaw/pitch
decomposition against reference values, the online offset's convergence, gate, clip
and file round trip, and the provider's click attribution (an empty offset leaves the
calibrated point alone, a click on the gaze point leaves nothing, a click beside it
moves the next sample towards it, a far click is refused), the connect sequence
ordering
(`device::connect_sequence` for every combination of blob, plane and double upload),
the blob hashing and diff helpers, the trailer decoder against the last 1 KB of two
real blobs (`tests/fixtures/blob-tail-*.bin`: the same model committed under the
trained plane and read back under the virtual one, identical bodies and 13 points
either side).
The `info`/`dump` paths and `blob-info` were exercised against the real device, and
`calibrate --dry-run` against the real desk config (with a seed, with `--no-seed`,
with `--accept-deg`/`--apply-from-round`, and against an unusable `--seed` file).
`blob-push`, `blob-watch`, `calibrate`, `health` and `view` write device state or need
the user seated and a compositor; run them manually as above. The retrain's device
sequence, its dwell fallback, the min-points refusal and the post-ceremony persistence
check have not been run against hardware.
