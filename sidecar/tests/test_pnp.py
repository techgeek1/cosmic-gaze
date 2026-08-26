"""PnP against synthetic projections of the generic face model."""

from __future__ import annotations

import cv2
import numpy as np
import pytest

from gaze_ml.face_model import MEDIAPIPE_INDEX, PNP_POINTS, named_from_indices, object_points
from gaze_ml.intrinsics import Intrinsics
from gaze_ml.pnp import MIN_POINTS, project, solve_head_pose

INTR = Intrinsics.from_hfov(1920, 1080, 70.4)


def synth(rvec: np.ndarray, tvec: np.ndarray, noise_px: float = 0.0, seed: int = 0) -> np.ndarray:
    """Project the PnP model points under a known pose, optionally with pixel noise."""
    pts, _ = cv2.projectPoints(
        object_points(PNP_POINTS), rvec, tvec, INTR.matrix(), INTR.dist_coeffs()
    )
    pts = pts.reshape(-1, 2)
    if noise_px:
        pts = pts + np.random.default_rng(seed).normal(0.0, noise_px, pts.shape)
    return pts


@pytest.mark.parametrize(
    "rvec",
    [
        np.zeros(3),
        np.array([0.0, 0.35, 0.0]),    # yaw
        np.array([0.25, 0.0, 0.0]),    # pitch
        np.array([0.0, 0.0, 0.20]),    # roll
        np.array([0.15, -0.40, 0.10]), # combined
    ],
)
def test_recovers_exact_pose(rvec: np.ndarray) -> None:
    """Noise-free projections invert to the pose that made them."""
    tvec = np.array([40.0, -60.0, 650.0])
    pose = solve_head_pose(PNP_POINTS, synth(rvec, tvec), INTR)
    assert pose is not None
    assert pose.tvec == pytest.approx(tvec, abs=0.5)
    assert pose.rvec == pytest.approx(rvec, abs=1e-3)
    assert pose.reproj_px < 0.05
    assert pose.n_points == len(PNP_POINTS)


def test_eye_mm_is_the_translation() -> None:
    """`eye_mm` is the model origin, i.e. the midpoint between the eye centres."""
    tvec = np.array([0.0, 0.0, 700.0])
    pose = solve_head_pose(PNP_POINTS, synth(np.zeros(3), tvec), INTR)
    assert pose is not None
    assert pose.eye_mm is pose.tvec
    midpoint = 0.5 * (
        pose.named_point_mm("eye_left_centre") + pose.named_point_mm("eye_right_centre")
    )
    assert midpoint == pytest.approx(pose.eye_mm, abs=1e-6)


def test_frontal_face_has_zero_rotation() -> None:
    """A head facing the lens square-on gives `rvec == 0`, which pins the convention."""
    pose = solve_head_pose(PNP_POINTS, synth(np.zeros(3), np.array([0.0, 0.0, 650.0])), INTR)
    assert pose is not None
    assert np.linalg.norm(pose.rvec) < 1e-3
    assert pose.rotation() == pytest.approx(np.eye(3), abs=1e-3)


def test_survives_realistic_landmark_noise() -> None:
    """One pixel of landmark jitter at 650 mm keeps depth inside a few percent."""
    rvec, tvec = np.array([0.1, 0.3, -0.05]), np.array([20.0, -40.0, 650.0])
    errors = []
    for seed in range(20):
        pose = solve_head_pose(PNP_POINTS, synth(rvec, tvec, noise_px=1.0, seed=seed), INTR)
        assert pose is not None
        errors.append(abs(pose.tvec[2] - tvec[2]) / tvec[2])
        assert np.degrees(np.linalg.norm(pose.rvec - rvec)) < 6.0
    assert np.mean(errors) < 0.05


def test_projection_inverts_the_pose() -> None:
    """Reprojecting the solved model points lands back on the input pixels."""
    rvec, tvec = np.array([0.05, -0.2, 0.0]), np.array([-30.0, 10.0, 600.0])
    pixels = synth(rvec, tvec)
    pose   = solve_head_pose(PNP_POINTS, pixels, INTR)
    assert pose is not None
    camera = pose.model_to_camera(object_points(PNP_POINTS))
    assert project(camera, INTR) == pytest.approx(pixels, abs=0.2)


def test_too_few_points_returns_none() -> None:
    """Fewer than four correspondences is unsolvable, and says so by returning None."""
    names = PNP_POINTS[: MIN_POINTS - 1]
    assert solve_head_pose(names, np.zeros((MIN_POINTS - 1, 2)), INTR) is None


def test_mismatched_lengths_raise() -> None:
    """Names and points must be the same length; a mismatch is a programming error."""
    with pytest.raises(ValueError):
        solve_head_pose(PNP_POINTS, np.zeros((3, 2)), INTR)


def test_partial_point_set_still_solves() -> None:
    """A landmarker missing the ear points still yields a pose from what is left."""
    subset = tuple(n for n in PNP_POINTS if "tragion" not in n)
    rvec, tvec = np.array([0.0, 0.2, 0.0]), np.array([0.0, 0.0, 650.0])
    pts, _ = cv2.projectPoints(
        object_points(subset), rvec, tvec, INTR.matrix(), INTR.dist_coeffs()
    )
    pose = solve_head_pose(subset, pts.reshape(-1, 2), INTR)
    assert pose is not None
    assert pose.n_points == len(subset)
    assert pose.tvec == pytest.approx(tvec, abs=2.0)


def test_mediapipe_index_map_is_complete() -> None:
    """Every PnP point has a MediaPipe index, and the extraction preserves order."""
    assert set(PNP_POINTS) <= set(MEDIAPIPE_INDEX)
    dense = np.arange(478 * 2, dtype=np.float64).reshape(478, 2)
    names, pts = named_from_indices(dense, MEDIAPIPE_INDEX, PNP_POINTS)
    assert names == PNP_POINTS
    assert pts[0] == pytest.approx(dense[MEDIAPIPE_INDEX[PNP_POINTS[0]]])
