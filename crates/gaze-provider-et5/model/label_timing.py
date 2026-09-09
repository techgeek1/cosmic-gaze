#!/usr/bin/env python3
"""Where, in time, does the eye agree best with the click?

    uv run python label_timing.py path/to/*-clicks.jsonl

The collector labels a click with the median firmware ray over the stop window
(0.6 s before the press to 0.1 s after). PACE (Huang et al., CHI 2016) found on 1915
real clicks that the gaze-to-target distance is smallest around one second *before*
the press for a third of users, with the press itself often landing at the start of
the next saccade. This script walks every non-caret click's recorded frames (about
1.2 s before the press to a little after), computes the firmware residual per frame,
and reports (a) the median residual per 100 ms bin relative to the press, and (b) the
per-click residual under several label choices: the current window, shifted windows,
and the last stable fixation before the press.

Everything here is the raw firmware ray against the click target, no model, so a
lower number means a *cleaner label*, which is the noise floor every model trains
against.
"""

from __future__ import annotations

import json
import sys
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).parent))

from gaze_model import geometry as geo

REPO_ROOT = Path(__file__).resolve().parents[3]
DESK_TOML = REPO_ROOT / "config" / "desk.toml"

# The collector's stop window, from `gaze-clicks/src/collect.rs`.
STOP_BEFORE_S = 0.6
STOP_AFTER_S = 0.1

BIN_S = 0.1
CAP_DEG = 8.0
STABLE_MIN_S = 0.10
STABLE_SPREAD_DEG = 1.0


def area_point(area: dict, nx: float, ny: float) -> np.ndarray:
    tl = np.array(area["tl_mm"])
    tr = np.array(area["tr_mm"])
    bl = np.array(area["bl_mm"])
    return tl + (tr - tl) * nx + (bl - tl) * ny


def firmware_ray(frame: dict, area: dict):
    """Mirrors `dataset.rs::firmware_ray`: the firmware's 2D gaze lifted onto the
    declared plane, from the midpoint of the tracked eye origins."""
    g = frame.get("gaze_2d_norm")
    if g is None:
        return None
    nx, ny = g
    if not (0.0 <= nx < 1.0 and 0.0 <= ny < 1.0):
        return None
    left = frame.get("eye_origin_l_mm") if frame.get("validity_l") == geo.VALIDITY_OK else None
    right = frame.get("eye_origin_r_mm") if frame.get("validity_r") == geo.VALIDITY_OK else None
    if left is not None and right is not None:
        origin = (np.array(left) + np.array(right)) * 0.5
    elif left is not None:
        origin = np.array(left)
    elif right is not None:
        origin = np.array(right)
    else:
        return None
    delta = area_point(area, nx, ny) - origin
    if delta @ delta < 1.0:
        return None
    return origin, delta / np.linalg.norm(delta)


def load_session(path: Path, desk: geo.DesktopGeometry) -> list[dict]:
    """One dict per non-caret click: `dt` (frame time minus press), `res` (angular
    residual per frame, degrees), `yaw`/`pitch` (firmware ray in the tangent frame at
    the target), plus `source`."""
    meta = None
    frames: list[tuple[float, dict]] = []
    clicks: list[dict] = []
    with path.open() as fh:
        for line in fh:
            rec = json.loads(line)
            kind = rec.get("kind")
            if kind == "meta":
                meta = rec
            elif kind == "frame":
                frames.append((rec["frame"]["t_s"], rec["frame"]["frame"]))
            elif kind == "click":
                clicks.append(rec)
    assert meta is not None
    area = meta["display_area"]
    frames.sort(key=lambda f: f[0])
    t_all = np.array([f[0] for f in frames])

    out = []
    for rec in clicks:
        c = rec["click"]
        if c.get("source") == "caret":
            continue
        display = rec.get("display") or meta["display"]
        if display not in desk.outputs:
            continue
        px = np.array([c["px"]["x"], c["px"]["y"]])
        target = desk.px_to_world_sensor(display, px)
        press = c["t_press"]
        lo = np.searchsorted(t_all, press - 1.5)
        hi = np.searchsorted(t_all, press + 0.5)
        dt, res, yaw, pitch = [], [], [], []
        for t, fr in frames[lo:hi]:
            ray = firmware_ray(fr, area)
            if ray is None:
                continue
            origin, d = ray
            want = target - origin
            y, p = geo.local_yaw_pitch_deg(d, want)
            dt.append(t - press)
            yaw.append(y)
            pitch.append(p)
            res.append(np.hypot(y, p))
        if not dt:
            continue
        out.append({
            "session": meta["session_id"], "source": c.get("source") or "",
            "dt": np.array(dt), "res": np.array(res),
            "yaw": np.array(yaw), "pitch": np.array(pitch),
        })
    return out


def window_label(click: dict, lo: float, hi: float) -> float | None:
    """Median-of-frames label over `[lo, hi]` seconds relative to the press, returned
    as the residual magnitude of the median yaw/pitch (what `mean_row` produces)."""
    m = (click["dt"] >= lo) & (click["dt"] <= hi)
    if m.sum() < 3:
        return None
    return float(np.hypot(np.median(click["yaw"][m]), np.median(click["pitch"][m])))


def last_stable_label(click: dict, before: float, until: float) -> float | None:
    """The last run of frames, ending no later than `until`, at least `STABLE_MIN_S`
    long whose yaw/pitch spread stays under `STABLE_SPREAD_DEG`: the final fixation
    before the press, PACE's behaviour-informed pick."""
    dt, yaw, pitch = click["dt"], click["yaw"], click["pitch"]
    idx = np.where((dt >= before) & (dt <= until))[0]
    if len(idx) < 3:
        return None
    best = None
    end = len(idx)
    # Grow runs backwards from each candidate end so the latest qualifying run wins.
    for e in range(len(idx), 0, -1):
        s = e - 1
        while s > 0:
            seg = idx[s - 1:e]
            spread = max(np.ptp(yaw[seg]), np.ptp(pitch[seg]))
            if spread > STABLE_SPREAD_DEG:
                break
            s -= 1
        seg = idx[s:e]
        if dt[seg[-1]] - dt[seg[0]] >= STABLE_MIN_S and len(seg) >= 3:
            best = seg
            break
    if best is None:
        return None
    return float(np.hypot(np.median(yaw[best]), np.median(pitch[best])))


def report(name: str, labels: list[float | None], sources: list[str]) -> None:
    arr = np.array([np.nan if v is None else v for v in labels])
    ok = np.isfinite(arr)
    src = {}
    for s in sorted(set(sources)):
        m = ok & (np.array(sources) == s)
        if m.any():
            src[s] = np.median(arr[m])
    srcs = " ".join(f"{k}={v:.2f}" for k, v in src.items())
    print(f"{name:34s} n={ok.sum():4d} p50 {np.percentile(arr[ok], 50):5.2f} "
          f"p75 {np.percentile(arr[ok], 75):5.2f} p90 {np.percentile(arr[ok], 90):5.2f} | {srcs}")


def main() -> None:
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    desk = geo.load_desk_toml(str(DESK_TOML))
    clicks = []
    for p in sys.argv[1:]:
        clicks.extend(load_session(Path(p), desk))
    print(f"clicks={len(clicks)} sessions={len({c['session'] for c in clicks})}")

    # (a) Residual against time before the press, pooled over clicks. Each click
    # contributes its own per-bin median so long fixations do not dominate.
    edges = np.arange(-1.3, 0.4 + 1e-9, BIN_S)
    print("\nmedian |residual| per bin (s relative to press), per-click medians pooled:")
    for lo, hi in zip(edges[:-1], edges[1:]):
        vals = []
        for c in clicks:
            m = (c["dt"] >= lo) & (c["dt"] < hi)
            if m.sum() >= 2:
                vals.append(np.median(c["res"][m]))
        vals = np.array(vals)
        if len(vals):
            print(f"  [{lo:+.1f}, {hi:+.1f})  n={len(vals):4d}  p50 {np.median(vals):5.2f}  "
                  f"p75 {np.percentile(vals, 75):5.2f}")

    # (b) Label choices.
    sources = [c["source"] for c in clicks]
    print("\nper-click label residual under each label choice:")
    report("current window [-0.6, +0.1]", [window_label(c, -STOP_BEFORE_S, STOP_AFTER_S) for c in clicks], sources)
    for lo, hi in [(-1.2, -0.6), (-0.9, -0.3), (-0.6, -0.2), (-0.4, 0.0), (-0.3, 0.1),
                   (-0.2, 0.2), (0.0, 0.3), (-1.0, 0.1)]:
        report(f"window [{lo:+.1f}, {hi:+.1f}]", [window_label(c, lo, hi) for c in clicks], sources)
    report("last stable fixation before press", [last_stable_label(c, -1.2, 0.05) for c in clicks], sources)
    report("last stable fixation, until -0.1", [last_stable_label(c, -1.2, -0.1) for c in clicks], sources)

    # How often does the stable pick disagree with the window by more than a degree?
    cur = np.array([np.nan if v is None else v for v in
                    (window_label(c, -STOP_BEFORE_S, STOP_AFTER_S) for c in clicks)])
    stab = np.array([np.nan if v is None else v for v in
                     (last_stable_label(c, -1.2, 0.05) for c in clicks)])
    both = np.isfinite(cur) & np.isfinite(stab)
    print(f"\nstable vs current: both defined for {both.sum()} clicks; "
          f"stable better by >0.5 deg on {(cur[both] - stab[both] > 0.5).sum()}, "
          f"worse by >0.5 deg on {(stab[both] - cur[both] > 0.5).sum()}")


if __name__ == "__main__":
    main()
