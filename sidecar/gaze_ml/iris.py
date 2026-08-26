"""A purely geometric gaze estimator: iris offset within the eye, plus head pose.

No learned gaze model. The 478-point mesh gives the iris centre and the eye
corners; the iris displacement from the corner midpoint, normalised by the eye
width, is the eye-in-head angle up to a gain. Rotating that by the PnP head pose
gives a camera-frame gaze vector.

Why this is worth having next to L2CS: it fails *visibly*. Its input is two
numbers you can print and check against a face, so when it is wrong you can see
why, whereas an appearance model that has been fed a bad crop produces stable
plausible nonsense. It is also the estimator whose single free parameter -- the
gain -- is exactly what the Rust side's calibration is set up to fit.

What it cannot do: it inherits every error in the head pose, it assumes the
subject's resting iris position matches the canonical face (a per-user bias, not
noise), and it is blind whenever the iris is occluded by a lid or glasses glare.
"""

from __future__ import annotations

import math
from dataclasses import dataclass

import numpy as np

from gaze_ml.pnp import HeadPose

# --- landmark groups ---

#: MediaPipe iris ring for the subject's right eye; 468 is the centre.
IRIS_RIGHT: tuple[int, ...] = (468, 469, 470, 471, 472)

#: MediaPipe iris ring for the subject's left eye; 473 is the centre.
IRIS_LEFT: tuple[int, ...] = (473, 474, 475, 476, 477)

#: Outer and inner canthus of the right eye.
CORNERS_RIGHT: tuple[int, int] = (33, 133)

#: Inner and outer canthus of the left eye.
CORNERS_LEFT: tuple[int, int] = (362, 263)

#: Smallest usable eye width in pixels. Below this the iris centre is a rounding
#: error and the normalised offset is meaningless.
MIN_EYE_PX: float = 12.0

# --- gain ---

#: Degrees of eye rotation per unit of normalised iris offset.
#:
#: Derived, not tuned: the canonical face puts the eye corners 25.9 mm apart, so
#: one unit of offset is 12.95 mm of iris travel; the eyeball radius is about
#: 12 mm, so that travel is `asin(12.95 / 12)` -- past 90 degrees, which is the
#: honest statement that the normalisation saturates before the eye does. In the
#: small-angle regime that actually occurs, `d = R sin(theta)` gives
#: `theta ~= asin(1.079 u)`, i.e. about 62 deg per unit near zero. 60 is the
#: round number; the Rust side fits the real one per user.
DEFAULT_GAIN_DEG: float = 60.0


# --- types ---


@dataclass(frozen=True)
class EyeObs:
    """One eye's normalised iris offset, in eye-widths from the corner midpoint."""

    u:        float  #: Horizontal, positive toward image `+x` (the subject's left).
    v:        float  #: Vertical, positive downward.
    width_px: float  #: Corner-to-corner distance, the length scale used.


@dataclass(frozen=True)
class IrisGaze:
    """A geometric gaze estimate, in the same camera frame as everything else."""

    yaw_deg:   float          #: Eye-in-head yaw, positive toward image `+x`.
    pitch_deg: float          #: Eye-in-head pitch, positive downward.
    right:     EyeObs | None
    left:      EyeObs | None
    #: Agreement between the two eyes in eye-widths; large means one iris is
    #: occluded or mis-tracked. `None` when only one eye was usable.
    disparity: float | None

    def vector(self, pose: HeadPose) -> np.ndarray:
        """The unit gaze direction in camera coordinates.

        The eye-in-head direction is built in the model frame -- where straight
        ahead is `-z` -- and then rotated by the head pose, which is exactly what
        `HeadPose.rotation()` is.
        """
        yaw   = math.radians(self.yaw_deg)
        pitch = math.radians(self.pitch_deg)
        in_head = np.array(
            [
                math.cos(pitch) * math.sin(yaw),
                math.sin(pitch),
                -math.cos(pitch) * math.cos(yaw),
            ],
            dtype = np.float64,
        )
        vec = pose.rotation() @ in_head
        return vec / np.linalg.norm(vec)


# --- estimation ---


def _eye(points_px: np.ndarray, iris: tuple[int, ...], corners: tuple[int, int]) -> EyeObs | None:
    """Normalised iris offset for one eye, or `None` if the mesh lacks the points."""
    needed = max(max(iris), *corners)
    if len(points_px) <= needed:
        return None
    a, b   = points_px[corners[0]], points_px[corners[1]]
    width  = float(np.linalg.norm(b - a))
    if width < MIN_EYE_PX:
        return None
    centre = 0.5 * (a + b)
    # The whole ring, not just the centre landmark: averaging five points is a
    # cheap win when a lid clips the top of the iris.
    offset = points_px[list(iris)].mean(axis=0) - centre
    half   = width / 2.0
    return EyeObs(u=float(offset[0] / half), v=float(offset[1] / half), width_px=width)


def estimate_iris_gaze(
    points_px: np.ndarray,
    pose:      HeadPose,
    gain_deg:  float = DEFAULT_GAIN_DEG,
) -> IrisGaze | None:
    """Eye-in-head gaze from the mesh, corrected for head-pose foreshortening.

    Both corrections are first order and both are needed. Under head yaw the iris
    displacement and the eye width foreshorten together, so `u` is already
    invariant -- but `v`'s numerator does not foreshorten while its denominator
    does, so `v` must be multiplied by `cos(yaw)`. Under head pitch the vertical
    displacement foreshortens while the horizontal eye width does not, so `v`
    must be divided by `cos(pitch)`.
    """
    right = _eye(points_px, IRIS_RIGHT, CORNERS_RIGHT)
    left  = _eye(points_px, IRIS_LEFT, CORNERS_LEFT)
    eyes  = [e for e in (right, left) if e is not None]
    if not eyes:
        return None

    u = float(np.mean([e.u for e in eyes]))
    v = float(np.mean([e.v for e in eyes]))

    forward     = pose.rotation() @ np.array([0.0, 0.0, -1.0])
    head_pitch  = math.asin(max(-1.0, min(1.0, float(forward[1]))))
    head_yaw    = math.atan2(float(forward[0]), -float(forward[2]))
    v_corrected = v * math.cos(head_yaw) / max(math.cos(head_pitch), 0.2)

    disparity = (
        float(math.hypot(right.u - left.u, right.v - left.v))
        if right is not None and left is not None
        else None
    )
    return IrisGaze(
        yaw_deg   = gain_deg * u,
        pitch_deg = gain_deg * v_corrected,
        right     = right,
        left      = left,
        disparity = disparity,
    )
