"""The one DataFrame schema every loader produces and everything downstream consumes.

Column groups, per PLAN-ET5.md Phase B2's spec (session id, group key, background,
phase, timestamp, raw eye origins L/R (6), per-eye direction yaw/pitch (4), interocular
vector (3), pupil L/R (2), validity (2), angle from tracker axis, the same head
features lagged 300 ms, target point (3), residual yaw/pitch (deg), is_mean):

  session_id, group_key, background, phase, t_s                          (5)
  origin_l_x_mm, origin_l_y_mm, origin_l_z_mm,
  origin_r_x_mm, origin_r_y_mm, origin_r_z_mm                            (6)
  dir_l_yaw_deg, dir_l_pitch_deg, dir_r_yaw_deg, dir_r_pitch_deg          (4)
  inter_x_mm, inter_y_mm, inter_z_mm                                     (3)
  pupil_l_mm, pupil_r_mm                                                 (2)
  valid_l, valid_r                                                       (2)
  angle_axis_deg                                                         (1)
  origin_l_x_mm_lag300 .. inter_z_mm_lag300                              (9)
  target_x_mm, target_y_mm, target_z_mm                                  (3)
  residual_yaw_deg, residual_pitch_deg                                   (2)
  is_mean                                                                (1)

38 columns total. B2's Rust `dataset export --csv` does not exist yet; this is this
harness's concrete choice for the exact names, documented so a later `load_export.py`
rename shim is a one-line fix rather than a redesign. All angles in degrees, all
lengths in millimetres, all coordinates in sensor (tracker) frame.
"""

from __future__ import annotations

HEAD_FEATURE_COLS = [
    "origin_l_x_mm", "origin_l_y_mm", "origin_l_z_mm",
    "origin_r_x_mm", "origin_r_y_mm", "origin_r_z_mm",
    "inter_x_mm", "inter_y_mm", "inter_z_mm",
]

LAG_HEAD_FEATURE_COLS = [f"{c}_lag300" for c in HEAD_FEATURE_COLS]

COLUMNS = [
    "session_id", "group_key", "background", "phase", "t_s",
    "origin_l_x_mm", "origin_l_y_mm", "origin_l_z_mm",
    "origin_r_x_mm", "origin_r_y_mm", "origin_r_z_mm",
    "dir_l_yaw_deg", "dir_l_pitch_deg", "dir_r_yaw_deg", "dir_r_pitch_deg",
    "inter_x_mm", "inter_y_mm", "inter_z_mm",
    "pupil_l_mm", "pupil_r_mm",
    "valid_l", "valid_r",
    "angle_axis_deg",
    *LAG_HEAD_FEATURE_COLS,
    "target_x_mm", "target_y_mm", "target_z_mm",
    "residual_yaw_deg", "residual_pitch_deg",
    "is_mean",
]

assert len(COLUMNS) == 38, f"schema drifted: {len(COLUMNS)} columns"


def empty_row() -> dict:
    return dict.fromkeys(COLUMNS)
