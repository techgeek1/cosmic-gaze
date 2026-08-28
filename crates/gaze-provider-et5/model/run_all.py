#!/usr/bin/env python3
"""Runs the whole Phase C pipeline: load -> features -> baselines + model -> report.

Usage (from `crates/gaze-provider-et5/model/`, via `uv run`):

    uv run python run_all.py
    uv run python run_all.py --readings ../../../config/calibration-et5.readings.jsonl \\
        --split-by-stop-index
    uv run python run_all.py --csv path/to/export.csv          # once B2 exists

Defaults to session zero (`config/calibration-et5.readings.jsonl`) with
`--split-by-stop-index` on, since that is the only data this harness has until B3
collects real multi-session data (PLAN-ET5.md). See `model/README.md` for what every
number in the report means and which ones are the real gate numbers versus this
smoke test.
"""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

import pandas as pd

sys.path.insert(0, str(Path(__file__).parent))

from gaze_model import evaluate, load_export, load_readings, selftest
from gaze_model.baselines import FirmwareOnly, GlobalQuadraticHead, PerSessionQuadraticField
from gaze_model.features import add_target_angle_columns, select_rows
from gaze_model.model import NystromResidualModel, grouped_cv_search

REPO_ROOT = Path(__file__).resolve().parents[3]
DEFAULT_READINGS = REPO_ROOT / "config" / "calibration-et5.readings.jsonl"
DEFAULT_DESK_TOML = REPO_ROOT / "config" / "desk.toml"
OUT_DIR = Path(__file__).parent / "out"


def parse_args() -> argparse.Namespace:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("--readings", nargs="*", default=None,
                    help="Raw readings JSONL file(s). Default: session zero.")
    p.add_argument("--csv", nargs="*", default=None,
                    help="B2 `dataset export --csv` file(s), once it exists.")
    p.add_argument("--desk-toml", default=str(DEFAULT_DESK_TOML))
    p.add_argument("--split-by-stop-index", action="store_true", default=None,
                    help="Smoke-test only: simulate two sessions from one readings "
                         "file's stops. Default: on when exactly one readings file "
                         "and no CSV is given, off otherwise.")
    p.add_argument("--out-dir", default=str(OUT_DIR))
    return p.parse_args()


def load_all(args: argparse.Namespace) -> pd.DataFrame:
    frames = []

    readings = args.readings if args.readings is not None else (
        [str(DEFAULT_READINGS)] if not args.csv else []
    )
    split = args.split_by_stop_index
    if split is None:
        split = len(readings) == 1 and not args.csv

    for path in readings:
        session_id = Path(path).stem
        frames.append(load_readings.load(path, args.desk_toml, session_id=session_id,
                                          split_by_stop_index=split))

    for path in args.csv or []:
        frames.append(load_export.load(path))

    if not frames:
        raise SystemExit("no --readings and no --csv given, and no default applies")

    return pd.concat(frames, ignore_index=True)


def format_table(df: pd.DataFrame) -> str:
    return df.to_markdown(index=False, floatfmt=".3f")


def main() -> None:
    args = parse_args()
    out_dir = Path(args.out_dir)
    out_dir.mkdir(parents=True, exist_ok=True)

    print("=== geometry self-test ===")
    selftest.run_all()

    print("\n=== loading ===")
    df_all = load_all(args)
    df_all = add_target_angle_columns(df_all, args.desk_toml)
    df = select_rows(df_all, include_mean=False)

    n_sessions = df["session_id"].nunique()
    n_groups = df["group_key"].nunique()
    smoke = n_sessions <= 1
    print(f"sessions={n_sessions} groups={n_groups} rows(per-frame)={len(df)} "
          f"rows(with mean)={len(df_all)}")

    lines = []
    lines.append("# Phase C model prototype report\n")
    lines.append(f"Sessions: {n_sessions}. Groups for cross-validation: {n_groups} "
                  f"(`group_key`). Per-frame rows: {len(df)}. With `is_mean` rows: "
                  f"{len(df_all)}.\n")

    if smoke:
        lines.append(
            "**SMOKE TEST.** Only session zero is available (`config/"
            "calibration-et5.readings.jsonl`); real sessions wait on PLAN-ET5.md "
            "Phase B3. Every 'held-out' number below comes from "
            "`--split-by-stop-index`, which cuts session zero's own stops in half "
            "(even/odd index) and calls them two sessions purely to exercise the "
            "grouped-CV machinery end to end. **This is not the Phase C gate number** "
            "(session-out RMS >= 25% below the best baseline, no session worse) — "
            "it cannot be, from one real session. In-sample numbers (fit and "
            "evaluate on the same data) are reported alongside, clearly labelled, as "
            "the closest thing to a sanity check available right now.\n"
        )

    # --- Hyperparameter search (model only) ---
    print("\n=== CV hyperparameter search (model) ===")
    best_params, cv_table = grouped_cv_search(df, n_splits=min(3, n_groups))
    print(f"best: {best_params}")
    lines.append("## Model hyperparameter search\n")
    lines.append(f"Grouped CV (GroupKFold by `group_key`) picked: `{best_params}`.\n")
    lines.append(format_table(cv_table.head(10)) + "\n")

    def model_factory():
        return NystromResidualModel(length_scale=best_params["length_scale"],
                                     ridge=best_params["ridge"], M=int(best_params["M"]))

    estimators = {
        "firmware_only": lambda: FirmwareOnly(),
        "global_quadratic_head": lambda: GlobalQuadraticHead(alpha=1.0),
        "nystrom_kernel_model": model_factory,
    }

    # --- Held-out (group) evaluation ---
    print("\n=== held-out (grouped) evaluation ===")
    held_out_rows = []
    for name, factory in estimators.items():
        err = evaluate.leave_one_group_out(df, factory)
        stats = evaluate.rms_p50_p90(err[~pd_isna(err)])
        stats["model"] = name
        held_out_rows.append(stats)
        print(f"  {name:60s} rms={stats['rms_deg']:.3f} p50={stats['p50_deg']:.3f} "
              f"p90={stats['p90_deg']:.3f} n={stats['n']}")

    # `PerSessionQuadraticField` needs the `is_mean` anchor rows to fit at all, so it
    # gets `df_all` (not the per-frame-only `df`) via its own dedicated evaluator; see
    # `evaluate.leave_one_group_out_field`'s docstring for why it is not just another
    # entry in `estimators`.
    field_name = "per_session_quadratic_field (grouped LOSO — cannot pool across " \
                 "sessions, see note)"
    field_err = evaluate.leave_one_group_out_field(df_all, PerSessionQuadraticField)
    field_stats = evaluate.rms_p50_p90(field_err[~pd_isna(field_err)])
    field_stats["model"] = field_name
    held_out_rows.append(field_stats)
    print(f"  {field_name:60s} rms={field_stats['rms_deg']:.3f} "
          f"p50={field_stats['p50_deg']:.3f} p90={field_stats['p90_deg']:.3f} "
          f"n={field_stats['n']}")

    held_out_table = pd.DataFrame(held_out_rows)[["model", "rms_deg", "p50_deg", "p90_deg", "n"]]

    lines.append("## Held-out (grouped) evaluation" +
                  (" — SMOKE TEST, see note above" if smoke else " (leave-one-session-out)") + "\n")
    lines.append(format_table(held_out_table) + "\n")

    # --- In-sample evaluation (always reported, always labelled) ---
    print("\n=== in-sample evaluation ===")
    in_sample_rows = []
    for name, factory in estimators.items():
        err = evaluate.in_sample_error(df, factory)
        stats = evaluate.rms_p50_p90(err)
        stats["model"] = name
        in_sample_rows.append(stats)
        print(f"  {name:60s} rms={stats['rms_deg']:.3f} p50={stats['p50_deg']:.3f} "
              f"p90={stats['p90_deg']:.3f} n={stats['n']}")

    field_model = PerSessionQuadraticField().fit(df_all)
    field_pred = field_model.predict(df)
    field_in_sample = evaluate.rms_p50_p90(
        evaluate.angular_error(field_pred, df[["residual_yaw_deg", "residual_pitch_deg"]].to_numpy()))
    field_in_sample["model"] = field_name
    in_sample_rows.append(field_in_sample)
    print(f"  {field_name:60s} rms={field_in_sample['rms_deg']:.3f} "
          f"p50={field_in_sample['p50_deg']:.3f} p90={field_in_sample['p90_deg']:.3f} "
          f"n={field_in_sample['n']}")

    in_sample_table = pd.DataFrame(in_sample_rows)[["model", "rms_deg", "p50_deg", "p90_deg", "n"]]
    lines.append("## In-sample evaluation (fit and scored on the same data — "
                  "NOT a held-out number, reported for reference only)\n")
    lines.append(format_table(in_sample_table) + "\n")

    # --- The old approach's own within-session evaluation, for reference. ---
    print("\n=== per-session quadratic field, within-session leave-one-stop-out ===")
    within = evaluate.per_session_field_within_session(df_all)
    print(f"  rms={within['rms_deg']:.3f} p50={within['p50_deg']:.3f} "
          f"p90={within['p90_deg']:.3f} n={within['n']}")
    lines.append("## `PerSessionQuadraticField`, within-session leave-one-stop-out "
                  "(the old approach's own evaluation scheme — not comparable to the "
                  "grouped table above, which cross-validates *across* sessions)\n")
    lines.append(f"RMS {within['rms_deg']:.3f} deg, p50 {within['p50_deg']:.3f} deg, "
                  f"p90 {within['p90_deg']:.3f} deg, n={within['n']}.\n")

    # --- Diagnostics: fit the model once more on everything for these. ---
    model = model_factory().fit(df)
    pred = model.predict(df)
    var = model.predict_var(df)

    bias_table = evaluate.per_session_bias(df, pred)
    print("\n=== per-session bias (model, in-sample) ===")
    print(bias_table.to_string(index=False))
    lines.append("## Per-session mean bias, before and after the model's correction "
                  "(in-sample fit)\n")
    lines.append(format_table(bias_table) + "\n")

    pupil_table = evaluate.pupil_regression(df)
    print("\n=== pupil-diameter-vs-residual regression ===")
    print(pupil_table.to_string(index=False))
    lines.append("## Pupil diameter vs. (pre-correction) angular residual magnitude, "
                  "per eye\n")
    lines.append(format_table(pupil_table) + "\n")

    var_corr = evaluate.variance_error_correlation(df, pred, var)
    print("\n=== predicted variance vs. |error| (in-sample) ===")
    print(var_corr)
    lines.append("## Predicted variance vs. post-correction |error| (in-sample), "
                  "Spearman\n")
    lines.append(f"rho={var_corr['spearman_rho']:.3f}, p={var_corr['p_value']:.4g}, "
                  f"n={var_corr['n']}.\n")

    lines.append(
        "## Gate (PLAN-ET5.md Phase C)\n\n"
        "Session-out RMS at least 25% below the best baseline, and no session worse. "
        "Cannot be evaluated until PLAN-ET5.md Phase B3 collects real multi-session "
        "data; the smoke-test numbers above exercise the same code path but say "
        "nothing about whether the gate will be met.\n"
    )

    report = "\n".join(lines)
    report_path = out_dir / "report.md"
    report_path.write_text(report)
    print(f"\nWrote {report_path}")


def pd_isna(arr):
    import numpy as np
    return np.isnan(arr)


if __name__ == "__main__":
    main()
