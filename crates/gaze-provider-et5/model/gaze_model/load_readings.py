"""Loader for the raw ET5 readings JSONL (`config/calibration-et5.readings.jsonl` and
whatever `record`/`collect` produce later): kind = "frame" | "traj" | "stop" lines,
one display per file (this loader assumes a single `display`, as session zero is —
multi-display readings would need grouping by `record["display"]` first).

Reproduces, in Python, the pipeline `sweep.rs::fit_display_direct` uses to decide which
samples are trustworthy and what target each one was looking at:

  1. Build the firmware's own gaze track from `gaze_2d_norm` (interior points only —
     the firmware clamps to the declared area, so an edge value is a wrong direction,
     not a corner fixation).
  2. Saccade-gate that track on pixel velocity (`SACCADE_DEG_S`).
  3. Estimate the tracker+pursuit lag by shifting the target trajectory until it best
     matches the gated, moving part of the track (`LAG_*`).
  4. For each stop: QC the whole window against the target (`ANCHOR_OUTLIER_DEG`, drop
     the window if it fails) — implemented locally since the task only asks to
     reproduce `LAG_*`, `SACCADE_DEG_S`, `SACCADE_PAD_S`, `OUTLIER_DEG` verbatim; the
     anchor threshold is carried over unchanged from `sweep.rs` for the same reason the
     glide gate is, but is not itself one of the named constants.
  5. For each glide sample: lag-shift its target and gate it (`OUTLIER_DEG`).

Departures from `fit_display_direct`, both deliberate (see the report for the full
list):

  - Parallax head-sweep holds are *included* here (phase "hold") instead of excluded —
    they are exactly the head-position-diverse data a residual model conditioned on
    head state needs; the Rust field fit excludes them only because a 2D polynomial
    field has no head-state input to give them to.
  - Stops contribute both a per-frame row per surviving sample and one `is_mean`
    aggregate row (Rust's field fit used only the aggregate); glides are not
    subsampled to `max_glide_rows` (that cap exists to bound a per-display fit's cost,
    not to shape training data).
"""

from __future__ import annotations

import json
from pathlib import Path

import numpy as np
import pandas as pd

from . import geometry as geo
from .schema import COLUMNS, HEAD_FEATURE_COLS, LAG_HEAD_FEATURE_COLS

# --- Constants, reproduced from sweep.rs verbatim ----------------------------------

SACCADE_DEG_S = 80.0
SACCADE_PAD_S = 0.06        # unused by the direct-mode gate itself (see module docs);
                             # kept for parity/documentation and available to callers.
OUTLIER_DEG = 5.0
ANCHOR_OUTLIER_DEG = 10.0
LAG_MAX_S = 0.35
LAG_STEP_S = 0.01
LAG_DEFAULT_S = 0.12
LAG_MIN_SAMPLES = 30
STOP_MIN_FRAMES = 5

# Not in sweep.rs's named-constant list, but literal plan text ("the same head
# features again lagged 300 ms") and the pairing gap borrowed from HeadGain's own
# HEAD_GAIN_PAIR_GAP_S so a lag pairing with no nearby frame becomes NaN rather than a
# wrong match.
HEAD_LAG_S = 0.30
HEAD_LAG_PAIR_GAP_S = 0.06


# --- Raw record parsing -------------------------------------------------------------


def _read_records(path: str) -> tuple[list[dict], list[dict], list[dict], str]:
    """Splits the JSONL into stop/traj/frame records for the one display present.
    Returns (stops, traj, frames, display_name)."""
    stops, traj, frames = [], [], []
    display = None

    with open(path) as f:
        for line in f:
            line = line.strip()
            if not line:
                continue
            rec = json.loads(line)
            display = display or rec.get("display")

            kind = rec.get("kind")
            if kind == "stop":
                stops.append(rec["stop"])
            elif kind == "traj":
                traj.append(rec["point"])
            elif kind == "frame":
                frames.append(rec["frame"])  # {"t_s": ..., "frame": {Et5Frame}}
            # "meta" (future B1 format) carries no per-record data this loader needs.

    stops.sort(key=lambda s: s["t_start"])
    traj.sort(key=lambda p: p["t_s"])
    frames.sort(key=lambda f: f["t_s"])

    return stops, traj, frames, (display or "unknown")


# --- Target trajectory helpers, mirroring sweep.rs ---------------------------------


def _target_at(traj_t: np.ndarray, traj_px: np.ndarray, t_s: float) -> np.ndarray | None:
    """Linearly interpolated target position at `t_s`. `None` outside the trajectory's
    span, matching `sweep.rs::target_at`."""
    i = np.searchsorted(traj_t, t_s)

    if i < len(traj_t) and traj_t[i] == t_s:
        return traj_px[i]
    if i == 0 or i >= len(traj_t):
        return None

    a_t, b_t = traj_t[i - 1], traj_t[i]
    a_px, b_px = traj_px[i - 1], traj_px[i]
    dt = b_t - a_t

    if dt <= 0.0:
        return a_px

    frac = (t_s - a_t) / dt
    return a_px + (b_px - a_px) * frac


def _was_moving(traj_t: np.ndarray, traj_moving: np.ndarray, t_s: float) -> bool:
    """Mirrors `sweep.rs::was_moving`: moving only if the trajectory samples on both
    sides of `t_s` were moving (a boundary frame belongs to neither the settle nor the
    glide unambiguously, so it is excluded from both)."""
    i = np.searchsorted(traj_t, t_s)

    if i < len(traj_t) and traj_t[i] == t_s:
        return bool(traj_moving[i])

    before = bool(traj_moving[i - 1]) if i > 0 else False
    after = bool(traj_moving[i]) if i < len(traj_t) else False
    return before and after


def _estimate_lag(traj_t: np.ndarray, traj_px: np.ndarray,
                   moving_t: np.ndarray, moving_px: np.ndarray, px_per_deg: float) -> float:
    """Mirrors `sweep.rs::estimate_lag`: the lag whose shifted target best matches the
    moving track, by mean angular error."""
    if len(moving_t) < LAG_MIN_SAMPLES:
        return LAG_DEFAULT_S

    best_lag, best_cost = LAG_DEFAULT_S, float("inf")
    steps = round(LAG_MAX_S / LAG_STEP_S)

    for step in range(steps + 1):
        lag = step * LAG_STEP_S
        total, n = 0.0, 0

        for t, px in zip(moving_t, moving_px):
            want = _target_at(traj_t, traj_px, t - lag)
            if want is None:
                continue
            d = px - want
            total += float(np.hypot(d[0], d[1])) / px_per_deg
            n += 1

        if n == 0:
            continue

        cost = total / n
        if cost < best_cost:
            best_cost, best_lag = cost, lag

    return best_lag


# --- Main entry point ----------------------------------------------------------------


def load(
    readings_path: str,
    desk_toml_path: str,
    session_id: str | None = None,
    split_by_stop_index: bool = False,
) -> pd.DataFrame:
    """Loads one readings JSONL into the shared schema (`schema.COLUMNS`).

    `split_by_stop_index` simulates two sessions out of one file's stops (even/odd
    index), purely so the grouped-CV code has more than one group to run against for a
    smoke test. It is NOT a real second session — see the report and `evaluate.py`'s
    smoke-test labelling.
    """
    readings_path = str(readings_path)
    session_id = session_id or Path(readings_path).stem

    stops, traj, frames, display = _read_records(readings_path)
    desk = geo.load_desk_toml(desk_toml_path)
    out = desk.outputs[display]
    axis_sensor = desk.axis_sensor_mm()

    traj_t = np.array([p["t_s"] for p in traj])
    traj_px = np.array([[p["px"]["x"], p["px"]["y"]] for p in traj])
    traj_moving = np.array([p["moving"] for p in traj])

    # --- Firmware track from gaze_2d_norm, interior points only. ---
    track_idx, track_t, track_px = [], [], []
    for i, f in enumerate(frames):
        g = f["frame"].get("gaze_2d_norm")
        if g is None:
            continue
        nx, ny = g
        if not (0.001 < nx < 0.999 and 0.001 < ny < 0.999):
            continue
        track_idx.append(i)
        track_t.append(f["t_s"])
        track_px.append(out.uv_to_px(np.array([nx, ny])))

    track_idx = np.array(track_idx)
    track_t = np.array(track_t)
    track_px = np.array(track_px)

    # Saccade gate on pixel velocity, exactly as `fit_display_direct` does it: a fast
    # step marks both its endpoints, no pad-window expansion (that expansion is
    # `ray_samples`/`saccade_mask`'s legacy-mode behaviour, not direct mode's).
    scale = geo.DesktopGeometry.px_per_deg(out, desk.eye_mm, out.uv_to_px(np.array([0.5, 0.5])))
    if not np.isfinite(scale) or scale <= 0:
        raise RuntimeError(f"{display}: px_per_deg came back non-finite; check desk.toml")

    keep_track = np.ones(len(track_t), dtype=bool)
    for i in range(1, len(track_t)):
        dt = track_t[i] - track_t[i - 1]
        if dt <= 0:
            continue
        d = track_px[i] - track_px[i - 1]
        deg_s = float(np.hypot(d[0], d[1])) / scale / dt
        if deg_s > SACCADE_DEG_S:
            keep_track[i] = False
            keep_track[i - 1] = False

    saccade_dropped = int((~keep_track).sum())

    moving_mask = np.array([
        keep_track[k] and _was_moving(traj_t, traj_moving, t)
        for k, t in enumerate(track_t)
    ])
    moving_t = track_t[moving_mask]
    moving_px = track_px[moving_mask]

    lag_s = _estimate_lag(traj_t, traj_px, moving_t, moving_px, scale)

    # --- Build dataset rows. ---
    rows: list[dict] = []
    dropped_stops = 0

    def feature_row(frame_idx: int, target_px: np.ndarray, phase: str) -> dict | None:
        f = frames[frame_idx]["frame"]
        combined = geo.combined_ray(f)
        if combined is None:
            return None

        origin, direction, _binocular = combined
        target_sensor = desk.px_to_world_sensor(display, target_px)
        target_dir = target_sensor - origin
        if float(np.dot(target_dir, target_dir)) < 1.0:
            return None

        residual_yaw, residual_pitch = geo.local_yaw_pitch_deg(direction, target_dir)
        angle_axis = geo.angle_from_axis_deg(direction, axis_sensor)

        ol = f.get("eye_origin_l_mm")
        orr = f.get("eye_origin_r_mm")
        gl = f.get("gaze_3d_l_mm")
        gr = f.get("gaze_3d_r_mm")
        vl = f.get("validity_l") == geo.VALIDITY_OK
        vr = f.get("validity_r") == geo.VALIDITY_OK

        # `axis_sensor` points tracker -> eye (roughly +Z, "toward the user"); a gaze
        # ray points eye -> target (roughly -Z, "toward the screen"). They are
        # opposite-facing by construction, so the direction is negated before
        # decomposing against the axis — the same negation `off_axis_deg` and
        # `angle_from_axis_deg` apply, kept consistent here so small deviations from
        # "looking at the tracker" come out as small yaw/pitch, not a ~180 degree
        # wraparound.
        dir_l = (np.nan, np.nan)
        if vl and ol is not None and gl is not None:
            d = np.array(gl) - np.array(ol)
            if float(np.dot(d, d)) >= 1.0:
                dir_l = geo.local_yaw_pitch_deg(-d, axis_sensor)

        dir_r = (np.nan, np.nan)
        if vr and orr is not None and gr is not None:
            d = np.array(gr) - np.array(orr)
            if float(np.dot(d, d)) >= 1.0:
                dir_r = geo.local_yaw_pitch_deg(-d, axis_sensor)

        if ol is not None and orr is not None:
            inter = np.array(orr) - np.array(ol)
        else:
            inter = np.array([np.nan, np.nan, np.nan])

        row = dict.fromkeys(COLUMNS)
        row.update(
            session_id=session_id,
            group_key=session_id,
            background="unknown",  # not carried by this (pre-B1) readings format
            phase=phase,
            t_s=frames[frame_idx]["t_s"],
            origin_l_x_mm=ol[0] if ol else np.nan,
            origin_l_y_mm=ol[1] if ol else np.nan,
            origin_l_z_mm=ol[2] if ol else np.nan,
            origin_r_x_mm=orr[0] if orr else np.nan,
            origin_r_y_mm=orr[1] if orr else np.nan,
            origin_r_z_mm=orr[2] if orr else np.nan,
            dir_l_yaw_deg=dir_l[0], dir_l_pitch_deg=dir_l[1],
            dir_r_yaw_deg=dir_r[0], dir_r_pitch_deg=dir_r[1],
            inter_x_mm=inter[0], inter_y_mm=inter[1], inter_z_mm=inter[2],
            pupil_l_mm=f.get("pupil_l_mm", np.nan),
            pupil_r_mm=f.get("pupil_r_mm", np.nan),
            valid_l=float(vl), valid_r=float(vr),
            angle_axis_deg=angle_axis,
            target_x_mm=target_sensor[0], target_y_mm=target_sensor[1], target_z_mm=target_sensor[2],
            residual_yaw_deg=residual_yaw, residual_pitch_deg=residual_pitch,
            is_mean=False,
            _frame_idx=frame_idx,  # dropped before returning; used for lag300 pairing
        )
        return row

    # Stops (grid fixations and parallax holds alike — see module docs).
    for stop_i, stop in enumerate(stops):
        t0, t1 = stop["t_start"], stop["t_end"]
        sel = (track_t >= t0) & (track_t <= t1) & keep_track[: len(track_t)]
        sel_idx = track_idx[sel]

        if len(sel_idx) < STOP_MIN_FRAMES:
            dropped_stops += 1
            continue

        stop_px = np.array([stop["px"]["x"], stop["px"]["y"]])
        med_px = np.median(track_px[sel], axis=0)
        err_deg = float(np.hypot(*(med_px - stop_px))) / scale

        if err_deg > ANCHOR_OUTLIER_DEG:
            dropped_stops += 1
            continue

        phase = "hold" if stop.get("parallax") else "stop"
        stop_rows = []
        for idx in sel_idx:
            r = feature_row(int(idx), stop_px, phase)
            if r is not None:
                r["_stop_i"] = stop_i
                stop_rows.append(r)
                rows.append(r)

        if stop_rows:
            mean_row = dict.fromkeys(COLUMNS)
            # Lag-300 columns are filled in bulk after the row loop (below); they are
            # not present on `r` yet, so they are excluded from this average.
            numeric_cols = [c for c in COLUMNS if c not in
                             ("session_id", "group_key", "background", "phase", "is_mean")
                             and c not in LAG_HEAD_FEATURE_COLS]
            for c in numeric_cols:
                mean_row[c] = float(np.nanmean([r[c] for r in stop_rows]))
            mean_row.update(session_id=session_id, group_key=session_id,
                             background="unknown", phase=phase, is_mean=True)
            mean_row["_stop_i"] = stop_i
            mean_row["_frame_idx"] = stop_rows[-1]["_frame_idx"]
            rows.append(mean_row)

    # Glides: lag-shifted, outlier-gated, one row per surviving frame.
    for k in range(len(moving_t)):
        t = moving_t[k]
        want = _target_at(traj_t, traj_px, t - lag_s)
        if want is None:
            continue

        d = moving_px[k] - want
        err_deg = float(np.hypot(d[0], d[1])) / scale
        if err_deg > OUTLIER_DEG:
            continue

        # Recover the frame index for this moving-track sample.
        frame_idx = int(track_idx[moving_mask][k])
        r = feature_row(frame_idx, want, "glide")
        if r is not None:
            r["_stop_i"] = -1
            rows.append(r)

    if not rows:
        raise RuntimeError(f"{display}: no rows survived gating; check the readings file")

    df = pd.DataFrame(rows)

    # --- Head features lagged 300 ms: nearest binocular frame near t - 0.3s. ---
    head_t, head_vals = [], []
    for f in frames:
        fr = f["frame"]
        if fr.get("validity_l") == geo.VALIDITY_OK and fr.get("validity_r") == geo.VALIDITY_OK \
                and fr.get("eye_origin_l_mm") is not None and fr.get("eye_origin_r_mm") is not None:
            ol = np.array(fr["eye_origin_l_mm"])
            orr = np.array(fr["eye_origin_r_mm"])
            inter = orr - ol
            head_t.append(f["t_s"])
            head_vals.append(np.concatenate([ol, orr, inter]))

    head_t = np.array(head_t)
    head_vals = np.array(head_vals) if head_vals else np.zeros((0, 9))

    lag_cols = {c: [] for c in LAG_HEAD_FEATURE_COLS}
    for idx in df["_frame_idx"]:
        t = frames[int(idx)]["t_s"] - HEAD_LAG_S
        if len(head_t) == 0:
            vals = [np.nan] * 9
        else:
            j = int(np.argmin(np.abs(head_t - t)))
            vals = head_vals[j] if abs(head_t[j] - t) <= HEAD_LAG_PAIR_GAP_S else [np.nan] * 9
        for c, v in zip(LAG_HEAD_FEATURE_COLS, vals):
            lag_cols[c].append(v)

    for c in LAG_HEAD_FEATURE_COLS:
        df[c] = lag_cols[c]

    if split_by_stop_index:
        df["group_key"] = np.where(
            df["_stop_i"] < 0,
            session_id + "-glide",  # glides carry no stop of their own; see below
            np.where(df["_stop_i"] % 2 == 0, f"{session_id}-A", f"{session_id}-B"),
        )
        # Assign each glide to the group of the stop it is heading toward (the next
        # stop in file order), so no group is glide-only.
        stop_group = {
            i: (f"{session_id}-A" if i % 2 == 0 else f"{session_id}-B")
            for i in range(len(stops))
        }
        glide_target_group = _nearest_next_stop_group(df, stops, stop_group)
        df.loc[df["_stop_i"] < 0, "group_key"] = glide_target_group

    df = df.drop(columns=["_frame_idx", "_stop_i"])
    df = df[COLUMNS]

    n_dropped_saccade = saccade_dropped
    print(f"[load_readings] {readings_path}: display={display} lag_s={lag_s:.3f} "
          f"scale_px_per_deg={scale:.1f} saccade_dropped={n_dropped_saccade} "
          f"stops_dropped={dropped_stops}/{len(stops)} rows={len(df)}")

    return df


def _nearest_next_stop_group(df: pd.DataFrame, stops: list[dict], stop_group: dict) -> pd.Series:
    """For the smoke-test split: each glide row's group is the group of the next stop
    in time (glides happen between two stops; "next" is an arbitrary but consistent
    tie-break)."""
    stop_starts = np.array([s["t_start"] for s in stops])
    out = []
    for t in df.loc[df["_stop_i"] < 0, "t_s"]:
        j = int(np.searchsorted(stop_starts, t))
        j = min(j, len(stops) - 1)
        out.append(stop_group[j])
    return pd.Series(out, index=df.index[df["_stop_i"] < 0])
