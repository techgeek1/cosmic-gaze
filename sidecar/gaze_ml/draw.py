"""Debug overlay for `--show`: landmarks, face box, head axes, gaze ray."""

from __future__ import annotations

import cv2
import numpy as np

from gaze_ml.intrinsics import Intrinsics
from gaze_ml.pipeline import FrameResult
from gaze_ml.pnp import project

# --- constants ---

#: How far along the gaze direction the drawn ray extends, in mm.
RAY_MM: float = 300.0

#: Length of the drawn head-pose axes, in mm.
AXIS_MM: float = 60.0


# --- drawing ---


def overlay(bgr: np.ndarray, result: FrameResult, intr: Intrinsics) -> np.ndarray:
    """Return a copy of `bgr` annotated with whatever the pipeline recovered."""
    out = bgr.copy()
    if result.face is not None:
        _draw_mesh(out, result)
    if result.pose is not None:
        _draw_axes(out, result, intr)
    if result.valid:
        _draw_gaze(out, result, intr)
    _draw_hud(out, result)
    return out


def _draw_mesh(out: np.ndarray, result: FrameResult) -> None:
    """Dot every mesh landmark, ring the PnP subset, box the detection."""
    face = result.face
    assert face is not None
    for x, y in face.points_px[::4]:
        cv2.circle(out, (int(x), int(y)), 1, (90, 90, 90), -1, cv2.LINE_AA)
    for x, y in face.named_px:
        cv2.circle(out, (int(x), int(y)), 4, (0, 220, 255), 1, cv2.LINE_AA)
    for x, y in face.iris_px.values():
        cv2.circle(out, (int(x), int(y)), 3, (255, 120, 0), -1, cv2.LINE_AA)

    d = face.detection
    cv2.rectangle(
        out,
        (int(d.x), int(d.y)),
        (int(d.x + d.w), int(d.y + d.h)),
        (0, 180, 0),
        1,
        cv2.LINE_AA,
    )


def _draw_axes(out: np.ndarray, result: FrameResult, intr: Intrinsics) -> None:
    """Draw the model's x/y/z axes at the eye midpoint, in BGR order R/G/B."""
    pose = result.pose
    assert pose is not None
    origin = pose.eye_mm.reshape(1, 3)
    tips   = pose.model_to_camera(np.eye(3) * AXIS_MM)
    pts    = project(np.vstack([origin, tips]), intr).astype(int)
    for tip, colour in zip(pts[1:], ((0, 0, 255), (0, 255, 0), (255, 0, 0)), strict=True):
        cv2.line(out, tuple(pts[0]), tuple(tip), colour, 2, cv2.LINE_AA)


def _draw_gaze(out: np.ndarray, result: FrameResult, intr: Intrinsics) -> None:
    """Draw the gaze ray from the eye midpoint.

    A ray pointing back toward the lens has `z < 0` and cannot be projected, so
    it is clipped to just in front of the image plane -- which is exactly the
    common case of a subject looking at the camera, and it must not blow up.
    """
    eye  = np.asarray(result.eye_mm, dtype=np.float64)
    tip  = eye + np.asarray(result.gaze, dtype=np.float64) * RAY_MM
    if tip[2] < 50.0:
        scale = (eye[2] - 50.0) / max(eye[2] - tip[2], 1e-6)
        tip   = eye + (tip - eye) * max(min(scale, 1.0), 0.0)
    pts = project(np.vstack([eye, tip]), intr).astype(int)
    cv2.arrowedLine(out, tuple(pts[0]), tuple(pts[1]), (0, 0, 255), 2, cv2.LINE_AA, tipLength=0.15)


def _draw_hud(out: np.ndarray, result: FrameResult) -> None:
    """Two lines of text: validity/pose summary and stage timings."""
    if result.valid:
        eye  = result.eye_mm
        gaze = result.gaze
        head = f"eye {eye[0]:+6.0f} {eye[1]:+6.0f} {eye[2]:6.0f} mm"
        ray  = f"gaze {gaze[0]:+.3f} {gaze[1]:+.3f} {gaze[2]:+.3f}"
        conf = f"conf {result.conf:.2f}"
        if result.pose is not None:
            conf += f"  reproj {result.pose.reproj_px:.1f}px"
        text = f"{head}   {ray}   {conf}"
    else:
        text = "no face"
    timings = "  ".join(f"{k.removesuffix('_ms')} {v:.1f}" for k, v in result.stages.items())
    for i, line in enumerate((text, timings)):
        cv2.putText(
            out, line, (12, 28 + 26 * i), cv2.FONT_HERSHEY_SIMPLEX, 0.6,
            (0, 0, 0), 3, cv2.LINE_AA,
        )
        cv2.putText(
            out, line, (12, 28 + 26 * i), cv2.FONT_HERSHEY_SIMPLEX, 0.6,
            (255, 255, 255), 1, cv2.LINE_AA,
        )
