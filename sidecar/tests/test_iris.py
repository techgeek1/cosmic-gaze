"""The geometric iris estimator: normalisation, gain, and frame composition."""

from __future__ import annotations

import math

import numpy as np
import pytest

from gaze_ml.iris import (
    CORNERS_LEFT,
    CORNERS_RIGHT,
    DEFAULT_GAIN_DEG,
    IRIS_LEFT,
    IRIS_RIGHT,
    estimate_iris_gaze,
)
from gaze_ml.pnp import HeadPose

EYE_PX = 60.0


def mesh(
    right_offset: tuple[float, float] = (0.0, 0.0),
    left_offset:  tuple[float, float] = (0.0, 0.0),
    width_px:     float = EYE_PX,
    n_points:     int = 478,
) -> np.ndarray:
    """A synthetic mesh with both eyes level and the irises at given offsets.

    Offsets are in eye-widths from the corner midpoint, i.e. the units the
    estimator normalises to, so a test can state the answer it expects.
    """
    pts = np.zeros((n_points, 2), dtype=np.float64)
    for corners, centre, offset in (
        (CORNERS_RIGHT, np.array([400.0, 300.0]), right_offset),
        (CORNERS_LEFT,  np.array([600.0, 300.0]), left_offset),
    ):
        half = width_px / 2.0
        pts[corners[0]] = centre + np.array([-half, 0.0])
        pts[corners[1]] = centre + np.array([half, 0.0])
        iris = IRIS_RIGHT if corners is CORNERS_RIGHT else IRIS_LEFT
        if max(iris) < n_points:
            pts[list(iris)] = centre + np.array(offset) * half
    return pts


def pose(rvec: np.ndarray | None = None) -> HeadPose:
    """A head pose at 600 mm with the given Rodrigues rotation."""
    return HeadPose(
        rvec      = np.zeros(3) if rvec is None else np.asarray(rvec, dtype=np.float64),
        tvec      = np.array([0.0, 0.0, 600.0]),
        reproj_px = 1.0,
        n_points  = 16,
    )


def test_centred_iris_is_straight_ahead() -> None:
    """An iris at the corner midpoint means zero eye-in-head angle."""
    gaze = estimate_iris_gaze(mesh(), pose())
    assert gaze is not None
    assert gaze.yaw_deg == pytest.approx(0.0)
    assert gaze.pitch_deg == pytest.approx(0.0)
    assert gaze.vector(pose()) == pytest.approx([0.0, 0.0, -1.0])


def test_gain_maps_offset_to_degrees() -> None:
    """Half an eye-width of offset is half the gain, by definition of the gain."""
    gaze = estimate_iris_gaze(mesh((0.5, 0.0), (0.5, 0.0)), pose())
    assert gaze is not None
    assert gaze.yaw_deg == pytest.approx(DEFAULT_GAIN_DEG * 0.5)
    assert gaze.vector(pose())[0] > 0.0          # toward image +x


def test_gain_is_configurable() -> None:
    """The gain is the one free parameter, and the Rust side will fit it."""
    gaze = estimate_iris_gaze(mesh((1.0, 0.0), (1.0, 0.0)), pose(), gain_deg=30.0)
    assert gaze is not None
    assert gaze.yaw_deg == pytest.approx(30.0)


def test_downward_iris_gives_downward_gaze() -> None:
    """Positive `v` is downward in image coordinates, which is `+y` in the camera."""
    gaze = estimate_iris_gaze(mesh((0.0, 0.4), (0.0, 0.4)), pose())
    assert gaze is not None
    assert gaze.pitch_deg > 0.0
    assert gaze.vector(pose())[1] > 0.0


def test_eyes_are_averaged() -> None:
    """Disagreeing eyes average, and the disagreement is reported, not hidden."""
    gaze = estimate_iris_gaze(mesh((0.2, 0.0), (0.6, 0.0)), pose())
    assert gaze is not None
    assert gaze.yaw_deg == pytest.approx(DEFAULT_GAIN_DEG * 0.4)
    assert gaze.disparity == pytest.approx(0.4)


def test_head_rotation_carries_the_eye_direction() -> None:
    """A centred iris under a yawed head looks where the head looks."""
    rvec = np.array([0.0, 0.5, 0.0])
    gaze = estimate_iris_gaze(mesh(), pose(rvec))
    assert gaze is not None
    forward = np.array([0.0, 0.0, -1.0])
    expected = np.array(
        [[math.cos(0.5), 0, math.sin(0.5)], [0, 1, 0], [-math.sin(0.5), 0, math.cos(0.5)]]
    ) @ forward
    assert gaze.vector(pose(rvec)) == pytest.approx(expected, abs=1e-9)


def test_vector_is_always_unit_length() -> None:
    """The protocol promises a unit vector whatever the offsets and pose."""
    for u in (-0.8, 0.0, 0.7):
        for v in (-0.5, 0.3):
            p = pose(np.array([0.2, -0.4, 0.1]))
            gaze = estimate_iris_gaze(mesh((u, v), (u, v)), p)
            assert gaze is not None
            assert np.linalg.norm(gaze.vector(p)) == pytest.approx(1.0)


def test_pitch_correction_uses_head_pose() -> None:
    """A pitched head foreshortens vertical iris travel, so `v` is scaled back up."""
    level   = estimate_iris_gaze(mesh((0.0, 0.3), (0.0, 0.3)), pose())
    pitched = estimate_iris_gaze(mesh((0.0, 0.3), (0.0, 0.3)), pose([0.5, 0.0, 0.0]))
    assert level is not None and pitched is not None
    assert abs(pitched.pitch_deg) > abs(level.pitch_deg)


def test_mesh_without_irises_is_refused() -> None:
    """A 468-point mesh has no iris landmarks, so there is nothing to measure."""
    assert estimate_iris_gaze(mesh(n_points=468), pose()) is None


def test_collapsed_eye_is_refused() -> None:
    """A few pixels of eye width makes the normalised offset meaningless."""
    assert estimate_iris_gaze(mesh(width_px=4.0), pose()) is None


def test_one_usable_eye_still_estimates() -> None:
    """Losing one eye halves the evidence but must not lose the frame."""
    pts = mesh((0.5, 0.0), (0.5, 0.0))
    pts[list(CORNERS_LEFT)] = 0.0            # collapse the left eye
    gaze = estimate_iris_gaze(pts, pose())
    assert gaze is not None
    assert gaze.left is None
    assert gaze.disparity is None
    assert gaze.yaw_deg == pytest.approx(DEFAULT_GAIN_DEG * 0.5)
