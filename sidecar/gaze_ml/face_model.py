"""A generic 3D face model in millimetres, plus landmark-index maps onto it.

The coordinates are the MediaPipe canonical face model
(`mediapipe/modules/face_geometry/data/canonical_face_model.obj`, Apache-2.0),
which is the geometry the landmarker was trained to reproduce. Using anything
else -- a hand-built anthropometric table, a 300-W mean shape -- means the
landmark indices and the 3D points disagree about what, say, "mouth corner"
means, and `solvePnP` absorbs that disagreement as a spurious head rotation.
That was measured, not assumed: a hand-built table gave 38 px RMS reprojection
and a phantom 19 degrees of pitch on a frontal portrait.

Frame convention, chosen so a head facing the camera square-on has `rvec == 0`:

- `+x` to the image right, which is the subject's *left*
- `+y` down, toward the chin
- `+z` away from the camera, into the back of the head

The canonical model is in centimetres with `+y` up and `+z` toward the camera,
so the conversion is a scale by ten and a 180-degree rotation about `x`. The
origin is moved to the midpoint between the two eye centres, taken at the
corneal plane 4 cm forward of the canonical eye-corner plane. That choice is
load-bearing: `solvePnP`'s translation is then *exactly* the protocol's
`eye_mm`, with no further transform.

A generic model buys angles cheaply and distances expensively. Head *rotation*
is good to a couple of degrees, but the recovered *depth* carries the subject's
deviation from the canonical face size as a proportional scale error, typically
5-10 %. Calibrate per user if absolute depth matters.
"""

from __future__ import annotations

import numpy as np

# --- the model ---

#: Named 3D points, millimetres, in the model frame above. The trailing comment
#: is the MediaPipe mesh index the coordinate was taken from.
MODEL_POINTS_MM: dict[str, tuple[float, float, float]] = {
    "eye_right_outer": ( -44.46,   -0.39,    8.27),  # 33
    "eye_right_inner": ( -18.56,    0.39,    2.42),  # 133
    "eye_left_inner":  (  18.56,    0.39,    2.42),  # 362
    "eye_left_outer":  (  44.46,   -0.39,    8.27),  # 263
    "nasion":          (   0.00,   -6.46,  -12.36),  # 168
    "nose_bridge":     (   0.00,    1.51,  -17.89),  # 6
    "nose_tip":        (   0.00,   37.51,  -34.76),  # 1
    "subnasale":       (   0.00,   47.14,  -20.58),  # 2
    "alare_right":     ( -17.86,   36.03,   -8.50),  # 129
    "alare_left":      (  17.86,   36.03,   -8.50),  # 358
    "cheek_right":     ( -38.33,   41.62,   -1.38),  # 205
    "cheek_left":      (  38.33,   41.62,   -1.38),  # 425
    "mouth_right":     ( -24.56,   69.67,   -2.84),  # 61
    "mouth_left":      (  24.56,   69.67,   -2.84),  # 291
    "chin":            (   0.00,  120.28,   -2.64),  # 152
    "tragion_right":   ( -76.64,   19.51,   64.36),  # 234
    "tragion_left":    (  76.64,   19.51,   64.36),  # 454
    "forehead":        (   0.00,  -56.37,   -4.82),  # 10
    # Constructed, not canonical: the canonical mesh has no eyeball centre. Placed
    # at the model origin plane on a 63 mm interpupillary distance.
    "eye_right_centre": (-31.50,    0.00,    0.00),
    "eye_left_centre":  ( 31.50,    0.00,    0.00),
}

#: Points fed to `solvePnP`. Two exclusions are deliberate. The eye centres are
#: out because the visible iris moves with gaze, so using it as a rigid-body
#: correspondence would leak gaze into head pose. The mouth corners are out
#: because they are the least rigid points on the face: a smile widens
#: chelion-chelion by a quarter, and on a smiling portrait they were the worst
#: two residuals by a factor of three.
PNP_POINTS: tuple[str, ...] = (
    "eye_right_outer",
    "eye_right_inner",
    "eye_left_inner",
    "eye_left_outer",
    "nasion",
    "nose_bridge",
    "nose_tip",
    "subnasale",
    "alare_right",
    "alare_left",
    "cheek_right",
    "cheek_left",
    "chin",
    "tragion_right",
    "tragion_left",
    "forehead",
)

# --- landmark index maps ---

#: MediaPipe Face Mesh / Face Landmarker indices (468 mesh points, 478 with irises).
#: "right" is the subject's right, which is the *left* half of an unmirrored image.
MEDIAPIPE_INDEX: dict[str, int] = {
    "eye_right_outer": 33,
    "eye_right_inner": 133,
    "eye_left_inner":  362,
    "eye_left_outer":  263,
    "nasion":          168,
    "nose_bridge":     6,
    "nose_tip":        1,
    "subnasale":       2,
    "alare_right":     129,
    "alare_left":      358,
    "cheek_right":     205,
    "cheek_left":      425,
    "mouth_right":     61,
    "mouth_left":      291,
    "chin":            152,
    "tragion_right":   234,
    "tragion_left":    454,
    "forehead":        10,
}

#: MediaPipe iris centres, present only in the 478-point output.
MEDIAPIPE_IRIS_INDEX: dict[str, int] = {
    "eye_right_centre": 468,
    "eye_left_centre":  473,
}

#: iBUG 300-W / dlib 68-point indices, for a drop-in swap of the landmark model.
#: Unused by the shipped pipeline; kept so the PnP wrapper stays landmarker-agnostic.
#: Only the points 68 landmarks actually carry are listed.
DLIB68_INDEX: dict[str, int] = {
    "eye_right_outer": 36,
    "eye_right_inner": 39,
    "eye_left_inner":  42,
    "eye_left_outer":  45,
    "nasion":          27,
    "nose_bridge":     28,
    "nose_tip":        30,
    "subnasale":       33,
    "alare_right":     31,
    "alare_left":      35,
    "mouth_right":     48,
    "mouth_left":      54,
    "chin":            8,
    "tragion_right":   0,
    "tragion_left":    16,
}


# --- helpers ---


def object_points(names: tuple[str, ...] = PNP_POINTS) -> np.ndarray:
    """The named model points as an `(n, 3)` float64 array, in the given order."""
    return np.array([MODEL_POINTS_MM[n] for n in names], dtype=np.float64)


def named_from_indices(
    points_px: np.ndarray,
    index_map: dict[str, int],
    names:     tuple[str, ...] = PNP_POINTS,
) -> tuple[tuple[str, ...], np.ndarray]:
    """Pull the named subset out of a dense landmark array.

    Returns the names that were actually available and their `(n, 2)` pixel
    coordinates, so a landmarker missing some of the points still yields a
    usable correspondence set.
    """
    have = tuple(n for n in names if 0 <= index_map.get(n, -1) < len(points_px))
    return have, np.array([points_px[index_map[n]] for n in have], dtype=np.float64)
