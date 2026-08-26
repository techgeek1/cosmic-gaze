"""Head pose from 2D landmarks by `solvePnP` against the generic 3D face model."""

from __future__ import annotations

from dataclasses import dataclass

import cv2
import numpy as np

from gaze_ml.face_model import MODEL_POINTS_MM, object_points
from gaze_ml.intrinsics import Intrinsics

# --- constants ---

#: `solvePnP` needs at least four non-coplanar correspondences to be determined.
MIN_POINTS: int = 4


# --- types ---


@dataclass(frozen=True)
class HeadPose:
    """A rigid transform from the generic face model into camera coordinates."""

    rvec:      np.ndarray  #: `(3,)` Rodrigues rotation, model -> camera.
    tvec:      np.ndarray  #: `(3,)` translation in mm; equals the eye midpoint.
    reproj_px: float       #: RMS reprojection error over the points used.
    n_points:  int         #: How many correspondences were solved against.

    @property
    def eye_mm(self) -> np.ndarray:
        """The midpoint between the eye centres in camera coordinates, in mm.

        Identical to `tvec` by construction: the model's origin is that midpoint.
        """
        return self.tvec

    def rotation(self) -> np.ndarray:
        """The `(3, 3)` rotation matrix corresponding to `rvec`."""
        return cv2.Rodrigues(self.rvec)[0]

    def model_to_camera(self, points_mm: np.ndarray) -> np.ndarray:
        """Map `(n, 3)` model-frame points into camera coordinates, in mm."""
        pts = np.asarray(points_mm, dtype=np.float64).reshape(-1, 3)
        return pts @ self.rotation().T + self.tvec.reshape(1, 3)

    def named_point_mm(self, name: str) -> np.ndarray:
        """A named model point (see `face_model`) in camera coordinates, in mm."""
        return self.model_to_camera(np.array([MODEL_POINTS_MM[name]]))[0]


# --- solving ---


def solve_head_pose(
    names:     tuple[str, ...],
    points_px: np.ndarray,
    intr:      Intrinsics,
) -> HeadPose | None:
    """Solve for head pose from named 2D landmarks.

    `names` are keys of `face_model.MODEL_POINTS_MM`; `points_px` is the matching
    `(n, 2)` array of pixel coordinates. Returns `None` when there are too few
    points or OpenCV fails to converge.

    EPnP gives the initial estimate (no initialisation needed, tolerant of a
    generic model) and virtual visual servoing refines it, which is what keeps the
    reprojection error meaningful as a quality signal.
    """
    pts_2d = np.asarray(points_px, dtype=np.float64).reshape(-1, 2)
    if len(names) != len(pts_2d):
        raise ValueError(f"{len(names)} names but {len(pts_2d)} points")
    if len(pts_2d) < MIN_POINTS:
        return None

    pts_3d = object_points(tuple(names))
    k      = intr.matrix()
    d      = intr.dist_coeffs()

    ok, rvec, tvec = cv2.solvePnP(pts_3d, pts_2d, k, d, flags=cv2.SOLVEPNP_EPNP)
    if not ok:
        return None
    rvec, tvec = cv2.solvePnPRefineVVS(pts_3d, pts_2d, k, d, rvec, tvec)

    proj, _ = cv2.projectPoints(pts_3d, rvec, tvec, k, d)
    resid   = proj.reshape(-1, 2) - pts_2d
    return HeadPose(
        rvec      = rvec.reshape(3).astype(np.float64),
        tvec      = tvec.reshape(3).astype(np.float64),
        reproj_px = float(np.sqrt((resid**2).sum(axis=1).mean())),
        n_points  = len(pts_2d),
    )


def project(points_mm: np.ndarray, intr: Intrinsics) -> np.ndarray:
    """Project `(n, 3)` *camera-frame* points in mm to `(n, 2)` pixel coordinates."""
    pts = np.asarray(points_mm, dtype=np.float64).reshape(-1, 3)
    out, _ = cv2.projectPoints(
        pts,
        np.zeros(3, dtype=np.float64),
        np.zeros(3, dtype=np.float64),
        intr.matrix(),
        intr.dist_coeffs(),
    )
    return out.reshape(-1, 2)
