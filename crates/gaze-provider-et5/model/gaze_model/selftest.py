"""Lightweight geometry self-tests, plain asserts (no pytest dependency). Run via
`uv run python -m gaze_model.selftest` or automatically at the top of `run_all.py`.

The load-bearing check the task calls out explicitly: a frame whose `gaze_3d` points
exactly at the target must yield a residual of (0, 0) degrees. Everything else here is
a supporting sanity check on the same geometry code.
"""

from __future__ import annotations

import numpy as np

from . import geometry as geo


def test_residual_zero_when_gaze_3d_hits_target():
    """Monocular case: `combined_ray` passes a single tracked eye's origin/direction
    through unchanged (see `gaze.rs::combined_ray`), so a `gaze_3d` that lands exactly
    on the target must give an exactly zero residual."""
    origin_l = [-32.0, 100.0, 600.0]
    target = np.array([15.0, 250.0, -50.0])  # some point out on a panel, sensor frame

    frame = {
        "validity_l": 0, "validity_r": 4,  # right eye not detected
        "eye_origin_l_mm": origin_l, "eye_origin_r_mm": None,
        "gaze_3d_l_mm": target.tolist(), "gaze_3d_r_mm": None,
    }

    combined = geo.combined_ray(frame)
    assert combined is not None
    origin, direction, binocular = combined
    assert not binocular

    target_dir = target - origin
    yaw, pitch = geo.local_yaw_pitch_deg(direction, target_dir)
    assert abs(yaw) < 1e-9, f"yaw {yaw}"
    assert abs(pitch) < 1e-9, f"pitch {pitch}"


def test_binocular_convergence_is_a_small_angle_effect_only():
    """Both eyes' rays pointed exactly at the same target from different origins fuse
    to a combined ray that is *close* to the target direction (convergence geometry),
    not exactly on it — the gap should be well under a degree for realistic interocular
    separation and viewing distance, and exactly the same effect `gaze.rs::combined_ray`
    has, not a bug in this port."""
    origin_l = [-32.0, 100.0, 600.0]
    origin_r = [32.0, 100.0, 600.0]
    target = np.array([15.0, 250.0, -50.0])

    frame = {
        "validity_l": 0, "validity_r": 0,
        "eye_origin_l_mm": origin_l, "eye_origin_r_mm": origin_r,
        "gaze_3d_l_mm": target.tolist(), "gaze_3d_r_mm": target.tolist(),
    }

    origin, direction, binocular = geo.combined_ray(frame)
    assert binocular

    target_dir = target - origin
    yaw, pitch = geo.local_yaw_pitch_deg(direction, target_dir)
    assert abs(yaw) < 0.1, f"yaw {yaw}"
    assert abs(pitch) < 0.1, f"pitch {pitch}"


def test_residual_matches_a_known_offset():
    """A ray one degree of local yaw off the target should read back ~1 degree, not
    some other axis or a wildly different magnitude."""
    origin = np.array([0.0, 0.0, 0.0])
    target_dir = np.array([0.0, 0.0, -600.0])  # straight ahead (sensor -Z historically)

    # One degree of yaw about the local "up" (which local_yaw_pitch_deg derives from
    # world +Y and target_dir): rotate target_dir by 1 degree about +Y.
    theta = np.radians(1.0)
    c, s = np.cos(theta), np.sin(theta)
    ry = np.array([[c, 0, s], [0, 1, 0], [-s, 0, c]])
    measured_dir = ry @ target_dir

    yaw, pitch = geo.local_yaw_pitch_deg(measured_dir, target_dir)
    assert abs(yaw - 1.0) < 1e-6, f"yaw {yaw}"
    assert abs(pitch) < 1e-6, f"pitch {pitch}"


def test_desk_to_sensor_matches_desk_toml_derivation():
    """Cross-check against the worked example in desk.toml's own comment: the tracked
    origin (-38, 38, 714) is the nominal eye (-38, 200, 687) rotated into sensor frame
    by ~13 degrees."""
    eye_mm = np.array([-38.0, 200.0, 687.0])
    sensor = geo.desk_to_sensor(eye_mm, 13.0)
    assert abs(sensor[0] - (-38.0)) < 1e-9
    assert abs(sensor[1] - 38.0) < 3.0   # desk.toml's own numbers are an estimate
    assert abs(sensor[2] - 714.0) < 3.0


def test_flat_vs_curved_panel_agree_at_large_radius():
    flat = geo.OutputGeometry(
        name="flat", logical_x=0, logical_y=0, logical_w=1000, logical_h=1000,
        physical_w_mm=1000, physical_h_mm=1000, radius_mm=0.0,
        position_mm=np.array([0.0, 0.0, -500.0]), yaw_deg=0, pitch_deg=0, roll_deg=0,
    )
    curved = geo.OutputGeometry(**{**flat.__dict__, "name": "curved", "radius_mm": 1.0e7})

    for u in (0.0, 0.25, 0.5, 0.75, 1.0):
        for v in (0.0, 0.5, 1.0):
            a = flat.uv_to_world(np.array([u, v]))
            b = curved.uv_to_world(np.array([u, v]))
            assert np.linalg.norm(a - b) < 0.05, f"uv({u},{v}): {a} vs {b}"


def run_all():
    tests = [v for k, v in globals().items() if k.startswith("test_") and callable(v)]
    for t in tests:
        t()
        print(f"  ok  {t.__name__}")
    print(f"[selftest] {len(tests)} geometry self-tests passed")


if __name__ == "__main__":
    run_all()
