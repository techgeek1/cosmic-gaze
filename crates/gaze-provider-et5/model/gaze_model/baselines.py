"""The three baselines PLAN-ET5.md C2 asks for, all sharing one interface —
`fit(df_train)` / `predict(df_test) -> (n, 2)` array of (residual_yaw_deg,
residual_pitch_deg) predictions — so `evaluate.py` can drive them identically to the
kernel model in `model.py`.

  1. `FirmwareOnly`      : the null model, predicts zero correction.
  2. `PerSessionQuadraticField`: `field.rs`'s old approach, ported to angle space: a
     ridge quadratic in the target's angular position, fit *per session* on that
     session's stop anchors (the `is_mean` rows), evaluated on its own held-out stops.
  3. `GlobalQuadraticHead`: one pooled ridge quadratic-in-target-angle plus linear
     head-origin and interocular terms, fit across every training session together.
"""

from __future__ import annotations

import numpy as np
import pandas as pd
from sklearn.linear_model import Ridge

from .features import TARGET_COLS

QUADRATIC_RIDGE_ALPHA = 1e-3  # mirrors field.rs's QUADRATIC_RIDGE in spirit, not value
                               # (that one regularises a [-1,1]^2 basis; this one a
                               # degrees-scale basis, so the raw constant does not
                               # transfer — picked by the same grouped-CV grid as
                               # everything else, see `evaluate.py`).


def _quadratic_basis(u: np.ndarray, v: np.ndarray) -> np.ndarray:
    """`[1, u, v, u^2, uv, v^2]`, the same six-term basis `field.rs::basis` truncates
    to at quadratic degree."""
    return np.stack([np.ones_like(u), u, v, u * u, u * v, v * v], axis=1)


class FirmwareOnly:
    """The prior: trust the firmware ray as-is. No parameters, no fit."""

    def fit(self, df_train: pd.DataFrame) -> "FirmwareOnly":
        return self

    def predict(self, df_test: pd.DataFrame) -> np.ndarray:
        return np.zeros((len(df_test), 2))


class PerSessionQuadraticField:
    """Fits one ridge-regularised quadratic-in-target-angle correction per session, on
    that session's stop anchors only. `evaluate.py` calls this per session directly
    (see its docstring) rather than through the pooled `fit`/`predict` pair every other
    estimator uses, because "per session" is the entire point of this baseline; the
    pooled-looking `fit` here still works (it just fits one field per session found in
    the training frame) for interface parity with the others.
    """

    def __init__(self, alpha: float = QUADRATIC_RIDGE_ALPHA):
        self.alpha = alpha
        self.models_: dict[str, tuple[Ridge, Ridge]] = {}

    def fit(self, df_train: pd.DataFrame) -> "PerSessionQuadraticField":
        anchors = df_train[df_train["is_mean"] & df_train["phase"].isin(["stop", "hold"])]
        self.models_ = {}
        for sess, g in anchors.groupby("session_id"):
            basis = _quadratic_basis(g["target_yaw_deg"].to_numpy(), g["target_pitch_deg"].to_numpy())
            ry = Ridge(alpha=self.alpha, fit_intercept=False).fit(basis, g["residual_yaw_deg"])
            rp = Ridge(alpha=self.alpha, fit_intercept=False).fit(basis, g["residual_pitch_deg"])
            self.models_[sess] = (ry, rp)
        return self

    def predict(self, df_test: pd.DataFrame) -> np.ndarray:
        basis = _quadratic_basis(df_test["target_yaw_deg"].to_numpy(), df_test["target_pitch_deg"].to_numpy())
        out = np.zeros((len(df_test), 2))
        for i, sess in enumerate(df_test["session_id"].to_numpy()):
            model = self.models_.get(sess)
            if model is None:
                continue  # a session with no anchors in training predicts zero
            ry, rp = model
            out[i, 0] = ry.predict(basis[i : i + 1])[0]
            out[i, 1] = rp.predict(basis[i : i + 1])[0]
        return out


class GlobalQuadraticHead:
    """One ridge regression, pooled across every training session: quadratic in the
    target's angular position plus linear head-origin (mean of L/R) and interocular
    terms. `alpha` is the ridge hyperparameter `evaluate.py`'s CV grid searches."""

    HEAD_COLS = [
        "origin_l_x_mm", "origin_l_y_mm", "origin_l_z_mm",
        "origin_r_x_mm", "origin_r_y_mm", "origin_r_z_mm",
        "inter_x_mm", "inter_y_mm", "inter_z_mm",
    ]

    def __init__(self, alpha: float = 1.0):
        self.alpha = alpha
        self._mean = None
        self._std = None
        self.ridge_ : Ridge | None = None

    def _design(self, df: pd.DataFrame) -> np.ndarray:
        basis = _quadratic_basis(df["target_yaw_deg"].to_numpy(), df["target_pitch_deg"].to_numpy())
        head = df[self.HEAD_COLS].to_numpy()
        return np.concatenate([basis, head], axis=1)

    def fit(self, df_train: pd.DataFrame) -> "GlobalQuadraticHead":
        X = self._design(df_train)
        self._mean = X.mean(axis=0)
        self._std = X.std(axis=0)
        self._std[self._std < 1e-9] = 1.0
        Xs = (X - self._mean) / self._std
        self.ridge_ = Ridge(alpha=self.alpha).fit(Xs, df_train[TARGET_COLS].to_numpy())
        return self

    def predict(self, df_test: pd.DataFrame) -> np.ndarray:
        X = self._design(df_test)
        Xs = (X - self._mean) / self._std
        return self.ridge_.predict(Xs)
