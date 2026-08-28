"""Loader for Rust's `dataset export --csv` (Phase B2), once it exists. It did not
exist when this harness was written (`load_readings.py` is the loader used for every
number in this pipeline's first run); this module is the seam B2 plugs into.

`schema.COLUMNS` is this harness's best-effort concrete naming for B2's documented
column groups (session id, group key, background, phase, timestamp, raw eye origins
L/R, per-eye direction yaw/pitch, interocular vector, pupil L/R, validity, angle from
tracker axis, lagged head features, target point, residual yaw/pitch, is_mean) — see
`schema.py`'s docstring for the exact mapping. If B2 ships different header names,
add the rename to `COLUMN_ALIASES` below rather than touching any other module; every
downstream file only imports `schema.COLUMNS` / `features.FEATURE_COLS`.
"""

from __future__ import annotations

import pandas as pd

from .schema import COLUMNS

# Map from an actual B2 CSV header to this harness's schema name, for any column B2
# names differently. Empty until B2 ships and the real headers are known.
COLUMN_ALIASES: dict[str, str] = {}


def load(csv_path: str) -> pd.DataFrame:
    df = pd.read_csv(csv_path)

    if COLUMN_ALIASES:
        df = df.rename(columns=COLUMN_ALIASES)

    missing = [c for c in COLUMNS if c not in df.columns]
    if missing:
        raise ValueError(
            f"{csv_path}: missing columns {missing}. If B2's export uses different "
            f"header names, add them to gaze_model.load_export.COLUMN_ALIASES."
        )

    extra = [c for c in df.columns if c not in COLUMNS]
    if extra:
        print(f"[load_export] {csv_path}: ignoring extra columns not in the shared "
              f"schema: {extra}")

    df["is_mean"] = df["is_mean"].astype(bool)
    return df[COLUMNS]
