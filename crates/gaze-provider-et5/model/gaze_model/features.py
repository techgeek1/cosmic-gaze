"""Feature matrix and residual target, built from the shared schema DataFrame.

Everything downstream (baselines, the kernel model) consumes `X`, `Y`, `groups` from
`build_feature_matrix`; nothing reads the raw JSONL or CSV again. Standardisation is
never fit here — `model.py` and `baselines.py` wrap the estimator in a
`sklearn.pipeline.Pipeline` with `StandardScaler` so every fold fits it on that fold's
train split only.
"""

from __future__ import annotations

import numpy as np
import pandas as pd

from .geometry import load_desk_toml, local_yaw_pitch_deg
from .schema import LAG_HEAD_FEATURE_COLS

# The model's input feature columns (27): raw eye origins L/R (6), per-eye direction
# yaw/pitch (4), interocular vector (3), pupil L/R (2), validity (2), angle from
# tracker axis (1), the same head features (origins + interocular, 9) lagged 300 ms.
FEATURE_COLS = [
    "origin_l_x_mm", "origin_l_y_mm", "origin_l_z_mm",
    "origin_r_x_mm", "origin_r_y_mm", "origin_r_z_mm",
    "dir_l_yaw_deg", "dir_l_pitch_deg", "dir_r_yaw_deg", "dir_r_pitch_deg",
    "inter_x_mm", "inter_y_mm", "inter_z_mm",
    "pupil_l_mm", "pupil_r_mm",
    "valid_l", "valid_r",
    "angle_axis_deg",
    *LAG_HEAD_FEATURE_COLS,
]

TARGET_COLS = ["residual_yaw_deg", "residual_pitch_deg"]

assert len(FEATURE_COLS) == 27, f"feature set drifted: {len(FEATURE_COLS)}"


def select_rows(df: pd.DataFrame, include_mean: bool = False) -> pd.DataFrame:
    """Per-frame rows only by default. `is_mean` rows are the per-stop aggregate the
    loader also emits (useful for the per-session bias diagnostic in `evaluate.py`),
    not for training — including both would double-count each stop's information."""
    if include_mean:
        return df
    return df[~df["is_mean"]].reset_index(drop=True)


def build_feature_matrix(df: pd.DataFrame) -> tuple[pd.DataFrame, pd.DataFrame, pd.Series]:
    """Returns (X, Y, groups): the 27-column feature frame, the 2-column residual
    target in degrees, and the grouping key for `GroupKFold` (`group_key`, not
    `session_id` — the smoke test's stop-index split lives in `group_key` and real
    multi-session data has `group_key == session_id`)."""
    X = df[FEATURE_COLS].copy()
    Y = df[TARGET_COLS].copy()
    groups = df["group_key"].copy()
    return X, Y, groups


def add_target_angle_columns(df: pd.DataFrame, desk_toml_path: str) -> pd.DataFrame:
    """Adds `target_yaw_deg` / `target_pitch_deg`: the fixation target's angular
    position relative to the tracker axis, computed from `target_{x,y,z}_mm` and the
    row's (approximate) combined origin (`(origin_l + origin_r) / 2`).

    This is the closest equivalent this dataset has to the old field-fit's panel `uv`:
    a 2-D angular "where on screen" coordinate, used only by `baselines.py`'s quadratic
    field baselines (a direct successor to `field.rs`'s `[1, x, y, x^2, xy, y^2]`
    basis) and not part of the model's own feature set (that keeps `angle_axis_deg`,
    already a schema column, as its radial term instead — the two are related but not
    identical: this one is the *target's* angle, that one the *ray's*).
    """
    desk = load_desk_toml(desk_toml_path)
    axis = desk.axis_sensor_mm()

    origin = df[["origin_l_x_mm", "origin_l_y_mm", "origin_l_z_mm"]].to_numpy() * 0.5 \
        + df[["origin_r_x_mm", "origin_r_y_mm", "origin_r_z_mm"]].to_numpy() * 0.5
    target = df[["target_x_mm", "target_y_mm", "target_z_mm"]].to_numpy()
    target_dir = target - origin

    yaw = np.empty(len(df))
    pitch = np.empty(len(df))
    for i in range(len(df)):
        yaw[i], pitch[i] = local_yaw_pitch_deg(-target_dir[i], axis)

    out = df.copy()
    out["target_yaw_deg"] = yaw
    out["target_pitch_deg"] = pitch
    return out
