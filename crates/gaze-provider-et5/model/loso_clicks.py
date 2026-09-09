#!/usr/bin/env python3
"""Leave-one-session-out on click sessions, one row per click (Phase C4 on B4/B5 data).

    uv run python loso_clicks.py path/to/export.csv

The export is `gaze-et5-cli dataset export --csv`. Caret clicks are dropped (an
I-beam on a terminal says nothing about the eye: 10 degree median residual on the
2026-09-04 data), every click is reduced to the median of its frames, and each
estimator is fitted on every session but one and scored on that one. Numbers are the
per-click post-correction residual in degrees.

Estimators: the firmware alone; a global constant offset; the per-session offset
*oracle* (the held-out session's own median leftover, i.e. the ceiling of PLAN-ET5's
D5 online offset); the pooled quadratic screen field; and the Nystrom kernel model on
feature subsets (per-eye directions, plus head, plus pupils, everything). This is the
script behind DESIGN.md section 10c's 2026-09-04 entry and `train.rs`'s
`FitParams::position_field`.
"""

from __future__ import annotations

import sys
from pathlib import Path

import numpy as np
import pandas as pd

sys.path.insert(0, str(Path(__file__).parent))

import gaze_model.model as mm
from gaze_model.baselines import _quadratic_basis
from gaze_model.features import TARGET_COLS, add_target_angle_columns
from gaze_model.model import NystromResidualModel
from gaze_model import geometry as geo

ALL_FEATURES = list(mm.FEATURE_COLS)   # captured before `Kernel` swaps the module's list
DEFAULT_EXPLICIT = list(mm.EXPLICIT_COLS)

REPO_ROOT = Path(__file__).resolve().parents[3]
DESK_TOML = REPO_ROOT / "config" / "desk.toml"
CAP_DEG = 8.0

POS = ["dir_l_yaw_deg", "dir_l_pitch_deg", "dir_r_yaw_deg", "dir_r_pitch_deg", "angle_axis_deg"]
HEAD = ["origin_l_x_mm", "origin_l_y_mm", "origin_l_z_mm",
        "origin_r_x_mm", "origin_r_y_mm", "origin_r_z_mm",
        "inter_x_mm", "inter_y_mm", "inter_z_mm"]
PUPIL = ["pupil_l_mm", "pupil_r_mm"]


def load_clicks(csv: str) -> pd.DataFrame:
    df = pd.read_csv(csv)
    df = add_target_angle_columns(df, str(DESK_TOML))
    df = df[(df.is_mean == 0) & (df.source != "caret")].copy()
    df["click"] = df.session_id + "/" + df.hold_key
    num = list(df.select_dtypes(include="number").columns)
    cl = df.groupby("click")[num].median()
    cl["session_id"] = df.groupby("click").session_id.first()
    cl["source"] = df.groupby("click").source.first()
    cl["res"] = np.hypot(cl.residual_yaw_deg, cl.residual_pitch_deg)
    cl["t_order"] = df.groupby("click").t_s.median()
    cl["pupil_l_sq"] = cl.pupil_l_mm ** 2
    cl["pupil_r_sq"] = cl.pupil_r_mm ** 2
    add_per_eye_labels(cl)
    return cl


def _dir_from_angles(yaw_deg, pitch_deg, axis: np.ndarray) -> np.ndarray:
    """Inverse of `local_yaw_pitch_deg(d, axis)`: the unit vector with those angles."""
    ref = axis / np.linalg.norm(axis)
    right = np.cross(np.array([0.0, 1.0, 0.0]), ref); right /= np.linalg.norm(right)
    up = np.cross(ref, right)
    d = ref + right * np.tan(np.radians(yaw_deg)) + up * np.tan(np.radians(pitch_deg))
    return d / np.linalg.norm(d)


def add_per_eye_labels(cl: pd.DataFrame) -> None:
    """`res_{l,r}_{yaw,pitch}`: each eye's own ray against the target, in the same
    tangent-frame convention as the firmware label. NaN when the eye was not tracked.
    `dir_{l,r}_*` are angles of the *reversed* gaze against the tracker axis
    (`model.rs::eye_angles`), so the ray is rebuilt and flipped before comparing."""
    desk = geo.load_desk_toml(str(DESK_TOML))
    axis = desk.axis_sensor_mm()
    target = cl[["target_x_mm", "target_y_mm", "target_z_mm"]].to_numpy()
    for eye in ("l", "r"):
        origin = cl[[f"origin_{eye}_x_mm", f"origin_{eye}_y_mm", f"origin_{eye}_z_mm"]].to_numpy()
        yaw = cl[f"dir_{eye}_yaw_deg"].to_numpy(); pitch = cl[f"dir_{eye}_pitch_deg"].to_numpy()
        ry = np.full(len(cl), np.nan); rp = np.full(len(cl), np.nan)
        for i in range(len(cl)):
            if not (np.isfinite(yaw[i]) and np.isfinite(pitch[i])):
                continue
            gaze = -_dir_from_angles(yaw[i], pitch[i], axis)
            ry[i], rp[i] = geo.local_yaw_pitch_deg(gaze, target[i] - origin[i])
        cl[f"res_{eye}_yaw"] = ry; cl[f"res_{eye}_pitch"] = rp


class Firmware:
    def fit(self, d): return self
    def predict(self, d): return np.zeros((len(d), 2))


class Offset:
    def fit(self, d): self.o = d[TARGET_COLS].median().to_numpy(); return self
    def predict(self, d): return np.tile(self.o, (len(d), 1))


class QuadField:
    """The old per-display field, pooled over sessions, on the target's angles."""
    def __init__(self, alpha=1.0): self.alpha = alpha
    def design(self, d): return _quadratic_basis(d.target_yaw_deg.to_numpy(), d.target_pitch_deg.to_numpy())
    def fit(self, d):
        X = self.design(d)
        self.imp = np.nan_to_num(np.nanmedian(X, 0)); X = np.where(np.isnan(X), self.imp, X)
        self.m = X.mean(0); self.sd = X.std(0); self.sd[self.sd < 1e-9] = 1
        Xs = np.c_[np.ones(len(X)), (X - self.m) / self.sd]
        reg = self.alpha * np.diag(np.r_[0, np.ones(Xs.shape[1] - 1)])
        self.w = np.linalg.solve(Xs.T @ Xs + reg, Xs.T @ d[TARGET_COLS].to_numpy()); return self
    def predict(self, d):
        X = self.design(d); X = np.where(np.isnan(X), self.imp, X)
        return np.c_[np.ones(len(X)), (X - self.m) / self.sd] @ self.w


class Kernel:
    """`NystromResidualModel` on a chosen feature subset, optionally with a different
    explicit-term list (the module default is angle_axis + linear pupils)."""
    def __init__(self, cols, explicit=None, **kw): self.cols = cols; self.explicit = explicit; self.kw = kw
    def _enter(self):
        mm.FEATURE_COLS = self.cols
        if self.explicit is not None: mm.EXPLICIT_COLS = self.explicit
    def fit(self, d):
        self._enter(); self.m = NystromResidualModel(**self.kw).fit(d); mm.EXPLICIT_COLS = DEFAULT_EXPLICIT; return self
    def predict(self, d):
        self._enter(); p = self.m.predict(d); mm.EXPLICIT_COLS = DEFAULT_EXPLICIT; return p
    def predict_var(self, d):
        self._enter(); v = self.m.predict_var(d); mm.EXPLICIT_COLS = DEFAULT_EXPLICIT; return v


class PerEye:
    """One kernel per eye on that eye's own residual, combined as the valid-weighted
    mean. The firmware's combined ray averages the two eye rays, so its residual is
    (to first order) the mean of the per-eye residuals when both are tracked."""
    def __init__(self, cols, **kw): self.cols = cols; self.kw = kw
    def fit(self, d):
        self.models = {}
        for eye in ("l", "r"):
            sub = d[np.isfinite(d[f"res_{eye}_yaw"])].copy()
            sub["residual_yaw_deg"] = sub[f"res_{eye}_yaw"]; sub["residual_pitch_deg"] = sub[f"res_{eye}_pitch"]
            self.models[eye] = Kernel(self.cols, **self.kw).fit(sub)
        return self
    def predict(self, d):
        pl = self.models["l"].predict(d); pr = self.models["r"].predict(d)
        vl = np.isfinite(d.res_l_yaw.to_numpy())[:, None]; vr = np.isfinite(d.res_r_yaw.to_numpy())[:, None]
        w = vl.astype(float) + vr.astype(float); w[w == 0] = 1
        return (np.where(vl, pl, 0) + np.where(vr, pr, 0)) / w


class PerEyeOracle:
    """Corrects by the *actual* mean per-eye residual: how well does the mean of the two
    eye residuals reproduce the firmware label at all? The floor for `PerEye`."""
    def fit(self, d): return self
    def predict(self, d):
        l = d[["res_l_yaw", "res_l_pitch"]].to_numpy(); r = d[["res_r_yaw", "res_r_pitch"]].to_numpy()
        vl = np.isfinite(l[:, :1]); vr = np.isfinite(r[:, :1])
        w = vl.astype(float) + vr.astype(float); w[w == 0] = 1
        return (np.where(vl, l, 0) + np.where(vr, r, 0)) / w


def loso(cl: pd.DataFrame, name: str, factory, offset: bool = False) -> None:
    sessions = sorted(cl.session_id.unique())
    P = np.zeros((len(cl), 2))
    for s in sessions:
        train = cl[(cl.session_id != s) & (cl.res < CAP_DEG)]
        test = (cl.session_id == s).to_numpy()
        p = factory().fit(train).predict(cl[test])
        if offset:
            p = p + np.median(cl[test][TARGET_COLS].to_numpy() - p, 0)
        P[test] = p
    d = cl[TARGET_COLS].to_numpy() - P
    e = np.hypot(d[:, 0], d[:, 1])
    per = " ".join(f"{np.median(e[(cl.session_id == s).to_numpy()]):.2f}" for s in sessions)
    src = " ".join(f"{k}={np.median(e[(cl.source == k).to_numpy()]):.2f}" for k in sorted(cl.source.unique()))
    print(f"{name:40s} p50 {np.percentile(e, 50):5.2f} p75 {np.percentile(e, 75):5.2f} "
          f"p90 {np.percentile(e, 90):5.2f} | per-session {per} | {src}", flush=True)


def online(cl: pd.DataFrame, name: str, factory, alpha: float, clip_deg: float = 3.0,
           gate_deg: float = 4.0, warm: int = 20) -> None:
    """The realistic D5: the held-out session's clicks in time order, each corrected by
    the frozen model plus a running offset, the offset nudged by `alpha` towards each
    click's leftover (clipped to `clip_deg`, ignored past `gate_deg`) *after* the click
    is scored. Reports everything and the steady state after `warm` clicks."""
    sessions = sorted(cl.session_id.unique())
    P = np.zeros((len(cl), 2)); late = np.zeros(len(cl), bool)
    for s in sessions:
        train = cl[(cl.session_id != s) & (cl.res < CAP_DEG)]
        test = (cl.session_id == s).to_numpy()
        idx = np.where(test)[0]; idx = idx[np.argsort(cl.t_order.to_numpy()[idx])]
        p = factory().fit(train).predict(cl.iloc[idx])
        y = cl.iloc[idx][TARGET_COLS].to_numpy()
        off = np.zeros(2)
        for k in range(len(idx)):
            P[idx[k]] = p[k] + off
            inn = y[k] - P[idx[k]]
            if np.hypot(*inn) < gate_deg:
                off += alpha * np.clip(inn, -clip_deg, clip_deg)
            late[idx[k]] = k >= warm
    d = cl[TARGET_COLS].to_numpy() - P
    e = np.hypot(d[:, 0], d[:, 1])
    per = " ".join(f"{np.median(e[(cl.session_id == s).to_numpy()]):.2f}" for s in sessions)
    print(f"{name:40s} p50 {np.percentile(e, 50):5.2f} p75 {np.percentile(e, 75):5.2f} "
          f"p90 {np.percentile(e, 90):5.2f} | per-session {per} | after {warm}: p50 {np.median(e[late]):.2f}", flush=True)


def main() -> None:
    if len(sys.argv) < 2:
        raise SystemExit(__doc__)
    cl = load_clicks(sys.argv[1])
    print(f"clicks={len(cl)} sessions={sorted(cl.session_id.unique())}")
    loso(cl, "firmware", Firmware)
    loso(cl, "global offset", Offset)
    loso(cl, "session offset (oracle)", Offset, offset=True)
    loso(cl, "quadratic field (pooled)", QuadField)
    sweep = "--sweep" in sys.argv
    if sweep:
        for ls in (1.0, 2.0, 3.0):
            for ridge in (1.0, 3.0, 10.0):
                loso(cl, f"kernel per-eye dirs ls={ls} r={ridge}", lambda: Kernel(POS, length_scale=ls, ridge=ridge, M=200))
        loso(cl, "kernel per-eye dirs + head ls=2 r=3", lambda: Kernel(POS + HEAD, length_scale=2.0, ridge=3.0, M=200))
        loso(cl, "kernel all 27 ls=2 r=3", lambda: Kernel(ALL_FEATURES, length_scale=2.0, ridge=3.0, M=200))
    K = lambda: Kernel(POS, length_scale=2.0, ridge=3.0, M=200)
    loso(cl, "kernel per-eye dirs ls=2 r=3", K)
    loso(cl, "kernel per-eye dirs ls=2 r=3 + session offset (oracle)", K, offset=True)
    print("--- pupils")
    loso(cl, "kernel dirs + pupils in kernel", lambda: Kernel(POS + PUPIL, length_scale=2.0, ridge=3.0, M=200))
    loso(cl, "kernel dirs, explicit + pupil^2", lambda: Kernel(POS, explicit=DEFAULT_EXPLICIT + ["pupil_l_sq", "pupil_r_sq"], length_scale=2.0, ridge=3.0, M=200))
    loso(cl, "kernel dirs + pupils, explicit + pupil^2", lambda: Kernel(POS + PUPIL, explicit=DEFAULT_EXPLICIT + ["pupil_l_sq", "pupil_r_sq"], length_scale=2.0, ridge=3.0, M=200))
    loso(cl, "kernel dirs, no pupil terms", lambda: Kernel(POS, explicit=["angle_axis_deg"], length_scale=2.0, ridge=3.0, M=200))
    print("--- per eye")
    loso(cl, "per-eye mean of actual residuals (oracle)", PerEyeOracle)
    loso(cl, "per-eye kernels, valid-weighted mean", lambda: PerEye(POS, length_scale=2.0, ridge=3.0, M=200))
    print("--- causal online offset (D5 simulation)")
    for a in (0.05, 0.1, 0.2, 0.3):
        online(cl, f"firmware + online offset a={a}", Firmware, alpha=a)
    for a in (0.05, 0.1, 0.2, 0.3):
        online(cl, f"kernel + online offset a={a}", K, alpha=a)
    online(cl, "kernel + online offset a=0.1 gate 3 clip 2", K, alpha=0.1, clip_deg=2.0, gate_deg=3.0)
    online(cl, "kernel + online offset a=0.1 no gate", K, alpha=0.1, clip_deg=8.0, gate_deg=99.0)


if __name__ == "__main__":
    main()
