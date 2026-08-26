"""The wire protocol: one JSON object per line over the Unix socket.

Exactly these keys, always all of them, in this order::

    {"t": float, "seq": int, "valid": bool,
     "eye_mm": [x, y, z] | null, "gaze": [dx, dy, dz] | null,
     "head_rot": [rx, ry, rz] | null, "conf": float | null, "lat_ms": float}

`--estimator both` adds two optional keys, `gaze_iris` and `gaze_l2cs`, carrying
the same unit vector in the same frame from each estimator so one run can be used
to compare them. They are additive: a consumer that does not know about them sees
an unchanged record, and `gaze` always mirrors the run's primary estimator.

Frames of reference: `eye_mm` and `gaze` are in OpenCV camera coordinates
(`+x` right, `+y` down, `+z` out of the lens into the scene), `eye_mm` in
millimetres, `gaze` a unit vector. `head_rot` is a Rodrigues vector taking the
generic face model into the camera frame. `t` is the capture instant on
`time.monotonic()`; `lat_ms` is capture-to-send for that same frame. When
`valid` is false the four nullable fields are `null` and only `t`, `seq` and
`lat_ms` carry meaning.
"""

from __future__ import annotations

import json
from typing import Any

import numpy as np

# --- constants ---

#: Field order on the wire. Consumers must not rely on order, but it is stable.
FIELDS: tuple[str, ...] = ("t", "seq", "valid", "eye_mm", "gaze", "head_rot", "conf", "lat_ms")

#: Optional additive fields. Absent unless the run produces them; a consumer that
#: ignores unknown keys is unaffected by their presence.
OPTIONAL_VECTORS: tuple[str, ...] = ("gaze_iris", "gaze_l2cs")

#: Fields that are `null` on an invalid frame and a value on a valid one.
NULLABLE: tuple[str, ...] = ("eye_mm", "gaze", "head_rot", "conf")

#: Fields that carry a three-element vector when non-null.
VECTORS: tuple[str, ...] = ("eye_mm", "gaze", "head_rot")


# --- building ---


def _vec3(value: Any) -> list[float]:
    """Coerce a length-3 sequence to a plain list of floats."""
    arr = np.asarray(value, dtype=np.float64).reshape(-1)
    if arr.size != 3:
        raise ValueError(f"expected 3 components, got {arr.size}")
    return [float(v) for v in arr]


def record(
    t:        float,
    seq:      int,
    lat_ms:   float,
    valid:    bool,
    eye_mm:   Any = None,
    gaze:     Any = None,
    head_rot: Any = None,
    conf:      float | None = None,
    gaze_iris: Any = None,
    gaze_l2cs: Any = None,
) -> dict[str, Any]:
    """Build one protocol record. Invalid frames null out the optional fields.

    `gaze_iris` / `gaze_l2cs` are emitted only when supplied, so the common case
    produces exactly the eight documented keys.
    """
    if not valid:
        return {
            "t":        float(t),
            "seq":      int(seq),
            "valid":    False,
            "eye_mm":   None,
            "gaze":     None,
            "head_rot": None,
            "conf":     None,
            "lat_ms":   float(lat_ms),
        }
    rec = {
        "t":        float(t),
        "seq":      int(seq),
        "valid":    True,
        "eye_mm":   _vec3(eye_mm),
        "gaze":     _vec3(gaze),
        "head_rot": _vec3(head_rot),
        "conf":     float(conf if conf is not None else 0.0),
        "lat_ms":   float(lat_ms),
    }
    for key, value in (("gaze_iris", gaze_iris), ("gaze_l2cs", gaze_l2cs)):
        if value is not None:
            rec[key] = _vec3(value)
    return rec


def encode(rec: dict[str, Any]) -> bytes:
    """Serialise one record to a single newline-terminated UTF-8 line."""
    return (json.dumps(rec, separators=(",", ":"), allow_nan=False) + "\n").encode()


def decode(line: bytes | str) -> dict[str, Any]:
    """Parse one line back into a record and validate it."""
    rec = json.loads(line)
    validate(rec)
    return rec


# --- validation ---


def validate(rec: dict[str, Any]) -> None:
    """Raise `ValueError` unless `rec` conforms exactly to the protocol."""
    if not isinstance(rec, dict):
        raise ValueError(f"record must be an object, got {type(rec).__name__}")
    known = set(FIELDS) | set(OPTIONAL_VECTORS)
    if not set(FIELDS) <= set(rec) or not set(rec) <= known:
        extra   = sorted(set(rec) - known)
        missing = sorted(set(FIELDS) - set(rec))
        raise ValueError(f"field mismatch: missing={missing} extra={extra}")

    if not isinstance(rec["valid"], bool):
        raise ValueError("`valid` must be a bool")
    if isinstance(rec["seq"], bool) or not isinstance(rec["seq"], int):
        raise ValueError("`seq` must be an int")
    for key in ("t", "lat_ms"):
        if isinstance(rec[key], bool) or not isinstance(rec[key], (int, float)):
            raise ValueError(f"`{key}` must be a number")

    if not rec["valid"]:
        for key in NULLABLE:
            if rec[key] is not None:
                raise ValueError(f"`{key}` must be null on an invalid frame")
        for key in OPTIONAL_VECTORS:
            if rec.get(key) is not None:
                raise ValueError(f"`{key}` must be absent or null on an invalid frame")
        return

    for key in VECTORS + tuple(k for k in OPTIONAL_VECTORS if k in rec):
        value = rec[key]
        if not isinstance(value, list) or len(value) != 3:
            raise ValueError(f"`{key}` must be a 3-element list on a valid frame")
        if any(isinstance(v, bool) or not isinstance(v, (int, float)) for v in value):
            raise ValueError(f"`{key}` components must be numbers")
    if isinstance(rec["conf"], bool) or not isinstance(rec["conf"], (int, float)):
        raise ValueError("`conf` must be a number on a valid frame")

    for key in ("gaze", *(k for k in OPTIONAL_VECTORS if k in rec)):
        norm = float(np.linalg.norm(rec[key]))
        if not 0.99 <= norm <= 1.01:
            raise ValueError(f"`{key}` must be a unit vector, got norm {norm:.4f}")
