"""The Phase C3 model: a sparse (Nystrom) kernel-ridge regression with an ARD RBF
kernel on the residual, plus two explicit terms summed alongside the kernel rather than
left for it to discover implicitly:

  - a fixed parametric radial term `c * angle_axis_deg` (the DESIGN.md §10c "radial
    gain with angle from the axis" finding, Tobii [patent reference removed]) — fitted linearly;
  - a per-eye linear pupil term `k_l * pupil_l_mm + k_r * pupil_r_mm`.

Both are drawn straight from `angle_axis_deg` / `pupil_{l,r}_mm`, already columns of
`FEATURE_COLS`; the kernel still sees them too (so it can express anything the linear
terms miss), the explicit terms just guarantee the model can't fail to represent that
shape even if the ARD lengthscale search undershoots.

Sparsity: M inducing points chosen by k-means over the (imputed, standardised) training
features, in the standard Nystrom / "subset of regressors" form:

  K_mm = k(centers, centers), K_nm = k(X, centers)
  Phi  = K_nm @ K_mm^{-1/2}                     (n x M "kernel features")
  beta = (Phi^T Phi + ridge I)^{-1} Phi^T y      (ridge regression in feature space)
  predict(x) = phi(x) @ beta

which is exactly what D1/D2 need to port to Rust: M centers, a weight matrix, and the
ARD lengthscales are the whole serialised model. Predictive variance is the matching
"subset of regressors" GP posterior, `ridge * phi(x) @ A^{-1} @ phi(x).T` with
`A = Phi^T Phi + ridge I`, shared across both output dimensions since a single kernel
and ridge are fit jointly on both residual axes.
"""

from __future__ import annotations

import numpy as np
import pandas as pd
from sklearn.cluster import KMeans
from sklearn.model_selection import GroupKFold

from .features import FEATURE_COLS, TARGET_COLS

EXPLICIT_COLS = ["angle_axis_deg", "pupil_l_mm", "pupil_r_mm"]

JITTER = 1e-6        # relative to K_mm's own scale, for the eigendecomposition
LINEAR_RIDGE = 1e-6  # near-unregularised: the explicit terms are a fixed physical
                      # shape, not something meant to shrink like the kernel weights


def _ard_kernel(a: np.ndarray, b: np.ndarray, length_scale: np.ndarray) -> np.ndarray:
    """`exp(-0.5 * sum_i ((a_i - b_i) / length_scale_i)^2)`, unit diagonal (a=b)."""
    a = a / length_scale
    b = b / length_scale
    sq = (a * a).sum(axis=1)[:, None] + (b * b).sum(axis=1)[None, :] - 2.0 * a @ b.T
    return np.exp(-0.5 * np.clip(sq, 0.0, None))


class NystromResidualModel:
    """`fit(df_train)` / `predict(df_test)` / `predict_var(df_test)`, matching the
    baselines' interface. Owns its own imputation and standardisation of the kernel
    features, fit on the training frame it is given — a caller doing grouped CV need
    only pass each fold's train/test frames."""

    def __init__(self, length_scale: float = 2.0, ridge: float = 1.0, M: int = 500,
                 random_state: int = 0):
        self.length_scale = length_scale
        self.ridge = ridge
        self.M = M
        self.random_state = random_state

    def fit(self, df_train: pd.DataFrame) -> "NystromResidualModel":
        X_raw = df_train[FEATURE_COLS].to_numpy(dtype=float)
        self._impute_ = np.nanmedian(X_raw, axis=0)
        self._impute_ = np.where(np.isnan(self._impute_), 0.0, self._impute_)
        X = np.where(np.isnan(X_raw), self._impute_, X_raw)

        self._mean_ = X.mean(axis=0)
        self._std_ = X.std(axis=0)
        self._std_[self._std_ < 1e-9] = 1.0
        Xs = (X - self._mean_) / self._std_

        n = len(Xs)
        m = min(self.M, n)
        km = KMeans(n_clusters=m, n_init=4, random_state=self.random_state).fit(Xs)
        self.centers_ = km.cluster_centers_

        ls = np.full(Xs.shape[1], float(self.length_scale))
        self._length_scale_ = ls

        Kmm = _ard_kernel(self.centers_, self.centers_, ls)
        Kmm += np.eye(m) * JITTER * np.trace(Kmm) / m
        eigval, eigvec = np.linalg.eigh(Kmm)
        eigval = np.clip(eigval, 1e-10, None)
        self._Kmm_inv_sqrt_ = (eigvec * (1.0 / np.sqrt(eigval))) @ eigvec.T

        Phi = _ard_kernel(Xs, self.centers_, ls) @ self._Kmm_inv_sqrt_

        Z = df_train[EXPLICIT_COLS].to_numpy(dtype=float)
        Z = np.nan_to_num(Z, nan=0.0)
        D = np.concatenate([Phi, Z], axis=1)

        reg = np.concatenate([
            np.full(m, self.ridge),
            np.full(Z.shape[1], LINEAR_RIDGE),
        ])
        A = D.T @ D + np.diag(reg)
        y = df_train[TARGET_COLS].to_numpy(dtype=float)
        self._A_ = A
        self._A_inv_ = np.linalg.inv(A)
        self.coef_ = self._A_inv_ @ (D.T @ y)  # (M + len(EXPLICIT_COLS), 2)
        self._design_dim_ = D.shape[1]
        return self

    def _design(self, df: pd.DataFrame) -> np.ndarray:
        X_raw = df[FEATURE_COLS].to_numpy(dtype=float)
        X = np.where(np.isnan(X_raw), self._impute_, X_raw)
        Xs = (X - self._mean_) / self._std_
        Phi = _ard_kernel(Xs, self.centers_, self._length_scale_) @ self._Kmm_inv_sqrt_
        Z = df[EXPLICIT_COLS].to_numpy(dtype=float)
        Z = np.nan_to_num(Z, nan=0.0)
        return np.concatenate([Phi, Z], axis=1)

    def predict(self, df_test: pd.DataFrame) -> np.ndarray:
        return self._design(df_test) @ self.coef_

    def predict_var(self, df_test: pd.DataFrame) -> np.ndarray:
        """Subset-of-regressors posterior variance, one value per row (shared across
        the yaw/pitch outputs — see module docs)."""
        D = self._design(df_test)
        return self.ridge * np.einsum("ij,jk,ik->i", D, self._A_inv_, D)


# --- Hyperparameter search ----------------------------------------------------------


def grouped_cv_search(
    df: pd.DataFrame,
    length_scales: list[float] = (1.0, 2.0, 3.0, 5.0),
    ridges: list[float] = (0.1, 1.0, 10.0),
    M: int = 500,
    n_splits: int = 3,
    random_state: int = 0,
) -> tuple[dict, pd.DataFrame]:
    """Grid search over (length_scale, ridge) by `GroupKFold` on `df["group_key"]`,
    scoring the mean RMS residual (degrees, both axes combined) on each held-out fold.
    `M` is not searched by default (500 is the plan's default); pass a list-like via
    the caller if it should be.

    Returns (best_params, results_table).
    """
    groups = df["group_key"].to_numpy()
    n_groups = len(np.unique(groups))
    splits = min(n_splits, n_groups)

    if splits < 2:
        # Not enough groups for CV (e.g. a single real session with no smoke split):
        # fall back to the first grid point and say so loudly in the report, not here.
        best = {"length_scale": length_scales[0], "ridge": ridges[0], "M": M}
        return best, pd.DataFrame([{**best, "rms_deg": float("nan"), "note": "no CV possible"}])

    gkf = GroupKFold(n_splits=splits)
    rows = []

    for ls in length_scales:
        for ridge in ridges:
            fold_rms = []
            for train_idx, test_idx in gkf.split(df, groups=groups):
                model = NystromResidualModel(length_scale=ls, ridge=ridge, M=M,
                                              random_state=random_state)
                model.fit(df.iloc[train_idx])
                pred = model.predict(df.iloc[test_idx])
                truth = df.iloc[test_idx][TARGET_COLS].to_numpy()
                err = np.linalg.norm(pred - truth, axis=1)
                fold_rms.append(float(np.sqrt(np.mean(err ** 2))))

            rows.append({"length_scale": ls, "ridge": ridge, "M": M,
                          "rms_deg": float(np.mean(fold_rms))})

    table = pd.DataFrame(rows).sort_values("rms_deg").reset_index(drop=True)
    best = table.iloc[0][["length_scale", "ridge", "M"]].to_dict()
    return best, table
