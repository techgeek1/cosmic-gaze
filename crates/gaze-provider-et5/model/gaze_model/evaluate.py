"""Phase C4: leave-one-session-out (or, in smoke-test mode, leave-one-pseudo-session-
out) RMS / p50 / p90 for every baseline and the model, per-session bias before/after,
the pupil-vs-residual regression, and the predicted-variance-vs-error correlation.
Writes `out/report.md` and prints it.

The grouping is always `df["group_key"]`, never `session_id` directly, because that is
where the smoke-test split lives (`load_readings.load(..., split_by_stop_index=True)`);
with real multi-session data the two are identical.
"""

from __future__ import annotations

import numpy as np
import pandas as pd
from scipy import stats
from sklearn.model_selection import LeaveOneGroupOut

from .baselines import FirmwareOnly, GlobalQuadraticHead, PerSessionQuadraticField
from .features import TARGET_COLS, select_rows
from .model import NystromResidualModel


def angular_error(pred: np.ndarray, truth: np.ndarray) -> np.ndarray:
    """Per-row magnitude of the post-correction residual, degrees: `truth - pred` is
    what is left of the firmware's error after applying `pred` as the correction."""
    d = truth - pred
    return np.sqrt((d ** 2).sum(axis=1))


def rms_p50_p90(err: np.ndarray) -> dict:
    return {
        "rms_deg": float(np.sqrt(np.mean(err ** 2))),
        "p50_deg": float(np.percentile(err, 50)),
        "p90_deg": float(np.percentile(err, 90)),
        "n": int(len(err)),
    }


# --- Group cross-validation over the shared estimator interface --------------------


def leave_one_group_out(df: pd.DataFrame, estimator_factory, min_groups: int = 2) -> np.ndarray:
    """Runs `estimator_factory()` (a zero-arg callable returning a fresh, unfitted
    estimator with `fit(df_train)`/`predict(df_test)`) under `LeaveOneGroupOut` on
    `df["group_key"]`. Returns the per-row post-correction error, aligned to `df`'s
    index order via a returned array in `df`'s row order (NaN where no fold covered a
    row — should not happen with `LeaveOneGroupOut`, kept as a canary).
    """
    groups = df["group_key"].to_numpy()
    n_groups = len(np.unique(groups))

    err = np.full(len(df), np.nan)

    if n_groups < min_groups:
        return err  # caller decides how to report "not enough groups"

    logo = LeaveOneGroupOut()
    for train_idx, test_idx in logo.split(df, groups=groups):
        model = estimator_factory()
        model.fit(df.iloc[train_idx])
        pred = model.predict(df.iloc[test_idx])
        truth = df.iloc[test_idx][TARGET_COLS].to_numpy()
        err[test_idx] = angular_error(pred, truth)

    return err


def leave_one_group_out_field(df_all: pd.DataFrame, estimator_factory, min_groups: int = 2) -> np.ndarray:
    """Like `leave_one_group_out`, but for `PerSessionQuadraticField` specifically:
    it needs the `is_mean` stop-anchor rows to fit at all, so it is given the *full*
    frame (`df_all`, not the per-frame-only `df`) for each training fold, while still
    scored only on the held-out fold's per-frame rows (so its numbers are comparable
    to every other estimator's). Returns an array aligned to `df_all`'s rows, NaN for
    `is_mean` rows and any fold not covered (only relevant when `min_groups` is not
    met)."""
    groups = df_all["group_key"].to_numpy()
    n_groups = len(np.unique(groups))
    err = np.full(len(df_all), np.nan)

    if n_groups < min_groups:
        return err

    logo = LeaveOneGroupOut()
    for train_idx, test_idx in logo.split(df_all, groups=groups):
        model = estimator_factory()
        model.fit(df_all.iloc[train_idx])

        test_frame = df_all.iloc[test_idx]
        keep = ~test_frame["is_mean"].to_numpy()
        test_sub_idx = test_idx[keep]
        if len(test_sub_idx) == 0:
            continue

        pred = model.predict(df_all.iloc[test_sub_idx])
        truth = df_all.iloc[test_sub_idx][TARGET_COLS].to_numpy()
        err[test_sub_idx] = angular_error(pred, truth)

    return err


def in_sample_error(df: pd.DataFrame, estimator_factory) -> np.ndarray:
    """Fits and predicts on the same data — an in-sample number, reported only because
    with one session it is all that is available; never presented as a held-out
    number."""
    model = estimator_factory()
    model.fit(df)
    pred = model.predict(df)
    truth = df[TARGET_COLS].to_numpy()
    return angular_error(pred, truth)


# --- Diagnostics ---------------------------------------------------------------------


def per_session_bias(df: pd.DataFrame, pred: np.ndarray) -> pd.DataFrame:
    """Mean signed bias (yaw, pitch, magnitude) per session, before (raw residual) and
    after (residual minus the prediction)."""
    truth = df[TARGET_COLS].to_numpy()
    after = truth - pred
    rows = []
    for sess, idx in df.groupby("session_id").indices.items():
        idx = np.asarray(idx)
        before_yaw, before_pitch = truth[idx, 0].mean(), truth[idx, 1].mean()
        after_yaw, after_pitch = after[idx, 0].mean(), after[idx, 1].mean()
        rows.append({
            "session_id": sess,
            "n": len(idx),
            "bias_before_yaw_deg": before_yaw, "bias_before_pitch_deg": before_pitch,
            "bias_before_mag_deg": float(np.hypot(before_yaw, before_pitch)),
            "bias_after_yaw_deg": after_yaw, "bias_after_pitch_deg": after_pitch,
            "bias_after_mag_deg": float(np.hypot(after_yaw, after_pitch)),
        })
    return pd.DataFrame(rows)


def pupil_regression(df: pd.DataFrame) -> pd.DataFrame:
    """Slope and R^2 of (raw, pre-correction) angular residual magnitude against pupil
    diameter, per eye — does pupil size predict how wrong the firmware ray currently
    is (motivates `model.py`'s explicit per-eye pupil term)."""
    truth = df[TARGET_COLS].to_numpy()
    mag = np.sqrt((truth ** 2).sum(axis=1))
    rows = []
    for eye, col in (("left", "pupil_l_mm"), ("right", "pupil_r_mm")):
        x = df[col].to_numpy()
        ok = np.isfinite(x) & np.isfinite(mag)
        result = stats.linregress(x[ok], mag[ok])
        rows.append({
            "eye": eye, "n": int(ok.sum()),
            "slope_deg_per_mm": result.slope, "intercept_deg": result.intercept,
            "r2": result.rvalue ** 2, "p_value": result.pvalue,
        })
    return pd.DataFrame(rows)


def variance_error_correlation(df: pd.DataFrame, pred: np.ndarray, var: np.ndarray) -> dict:
    """Spearman correlation between the model's predicted variance and the actual
    post-correction error magnitude — "does the confidence mean anything"."""
    err = angular_error(pred, df[TARGET_COLS].to_numpy())
    ok = np.isfinite(var) & np.isfinite(err)
    rho, p = stats.spearmanr(var[ok], err[ok])
    return {"spearman_rho": float(rho), "p_value": float(p), "n": int(ok.sum())}


def per_session_field_within_session(df: pd.DataFrame) -> dict:
    """The old approach's own evaluation scheme, for reference: leave-one-stop-out
    *within* each session (never pooling across sessions — see `baselines.py`'s
    `PerSessionQuadraticField` docstring). Not comparable to the LOSO table; reported
    separately so the "what we had" number is not lost."""
    target_cols = ["target_x_mm", "target_y_mm", "target_z_mm"]
    errs = []
    for sess, g in df.groupby("session_id"):
        anchors = g[g["is_mean"] & g["phase"].isin(["stop", "hold"])]
        if len(anchors) < 3:
            continue

        per_frame = g[~g["is_mean"] & g["phase"].isin(["stop", "hold"])]

        for held_idx in anchors.index:
            train = g.drop(index=held_idx)
            held_target = anchors.loc[held_idx, target_cols].to_numpy()

            # Per-frame rows belonging to the held-out stop: same target point. Every
            # frame in one stop shares the exact same `target_px`, but the anchor's own
            # target is a `nanmean` over those frames (see `load_readings.py`), which
            # is not bit-exact with the constant it averages, hence `isclose`.
            same_stop = np.isclose(per_frame[target_cols].to_numpy(), held_target,
                                    rtol=0.0, atol=1e-6).all(axis=1)
            test = per_frame[same_stop]
            if test.empty:
                continue

            model = PerSessionQuadraticField().fit(train)
            pred = model.predict(test)
            errs.append(angular_error(pred, test[TARGET_COLS].to_numpy()))

    if not errs:
        return {"rms_deg": float("nan"), "p50_deg": float("nan"), "p90_deg": float("nan"), "n": 0}
    return rms_p50_p90(np.concatenate(errs))
