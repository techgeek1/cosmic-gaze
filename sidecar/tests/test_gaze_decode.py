"""The L2CS bin decoding and the camera-frame gaze convention.

These run without the checkpoint: they pin the parts of the gaze stage that are
arithmetic rather than learned, which is where a sign error would otherwise hide.
"""

from __future__ import annotations

import math

import cv2
import numpy as np
import pytest

from gaze_ml.l2cs import BIN_DEG, NUM_BINS, GazeAngles, decode_bins


def one_hot(index: int) -> np.ndarray:
    """A degenerate softmax concentrated on one bin."""
    probs = np.zeros(NUM_BINS)
    probs[index] = 1.0
    return probs


def test_bin_grid_spans_the_full_circle() -> None:
    """Ninety four-degree bins cover [-180, 180)."""
    assert NUM_BINS * BIN_DEG == 360.0
    assert decode_bins(one_hot(0)) == pytest.approx(-180.0)
    assert decode_bins(one_hot(45)) == pytest.approx(0.0)
    assert decode_bins(one_hot(NUM_BINS - 1)) == pytest.approx(176.0)


def test_decode_is_the_expectation_not_the_argmax() -> None:
    """Mass split between adjacent bins decodes to the midpoint, which is the point."""
    probs = np.zeros(NUM_BINS)
    probs[45] = probs[46] = 0.5
    assert decode_bins(probs) == pytest.approx(2.0)


def test_looking_into_the_lens_is_minus_z() -> None:
    """Zero yaw and pitch must mean 'looking at the camera', i.e. `-z`."""
    vec = GazeAngles(yaw_deg=0.0, pitch_deg=0.0, sharpness=1.0).vector()
    assert vec == pytest.approx([0.0, 0.0, -1.0])


def test_vector_is_always_unit_length() -> None:
    """The protocol promises a unit vector for every angle pair."""
    for yaw in (-60.0, -20.0, 0.0, 25.0, 70.0):
        for pitch in (-40.0, 0.0, 30.0):
            vec = GazeAngles(yaw_deg=yaw, pitch_deg=pitch, sharpness=1.0).vector()
            assert np.linalg.norm(vec) == pytest.approx(1.0)


def test_yaw_and_pitch_signs() -> None:
    """Positive yaw points to `-x`, positive pitch to `-y`, in OpenCV camera axes."""
    right = GazeAngles(yaw_deg=30.0, pitch_deg=0.0, sharpness=1.0).vector()
    assert right[0] < 0.0 and right[1] == pytest.approx(0.0)
    up = GazeAngles(yaw_deg=0.0, pitch_deg=30.0, sharpness=1.0).vector()
    assert up[1] < 0.0 and up[0] == pytest.approx(0.0)


def test_angles_recover_from_the_vector() -> None:
    """The vector encoding is invertible, so downstream code can go either way."""
    yaw, pitch = -22.5, 13.0
    vec = GazeAngles(yaw_deg=yaw, pitch_deg=pitch, sharpness=1.0).vector()
    assert math.degrees(math.asin(-vec[1])) == pytest.approx(pitch)
    assert math.degrees(math.atan2(-vec[0], -vec[2])) == pytest.approx(yaw)


# --- cropping ---


def test_crop_is_padded_not_clamped() -> None:
    """A box hanging off the frame edge stays square, with the overlap in place."""
    from gaze_ml.l2cs import crop_padded

    frame = np.full((1080, 1920, 3), 200, dtype=np.uint8)
    crop  = crop_padded(frame, (1700, 900, 2100, 1300))
    assert crop is not None
    assert crop.shape == (400, 400, 3)
    assert (crop[:180, :220] == 200).all()   # the part that overlapped the frame
    assert (crop[180:, :] == 0).all()        # padding below the bottom edge
    assert (crop[:, 220:] == 0).all()        # padding past the right edge


def test_crop_keeps_the_face_centred() -> None:
    """The face's position inside the crop is preserved when the box is clipped.

    That is the whole point: the network reads off-centre framing as head pose.
    """
    from gaze_ml.l2cs import crop_padded

    frame = np.zeros((1080, 1920, 3), dtype=np.uint8)
    frame[1000:1080, 900:980] = 255
    crop = crop_padded(frame, (740, 840, 1140, 1240))
    assert crop is not None
    ys, xs = np.nonzero(crop[:, :, 0])
    assert xs.mean() == pytest.approx(199.5)
    assert ys.mean() == pytest.approx(199.5)


def test_degenerate_boxes_are_refused() -> None:
    """Tiny, non-square or fully off-frame boxes return None rather than garbage."""
    from gaze_ml.l2cs import crop_padded

    frame = np.zeros((100, 100, 3), dtype=np.uint8)
    assert crop_padded(frame, (0, 0, 4, 4)) is None
    assert crop_padded(frame, (0, 0, 40, 30)) is None
    assert crop_padded(frame, (500, 500, 600, 600)) is None


def test_detection_square_box_is_square_and_scaled() -> None:
    """`Detection.square` centres on the face and honours the expansion factor."""
    from gaze_ml.landmarks import Detection

    det = Detection(x=100.0, y=200.0, w=80.0, h=120.0, score=0.9)
    x0, y0, x1, y1 = det.square(1.5)
    assert (x1 - x0) == (y1 - y0) == 180
    assert (x0 + x1) / 2 == pytest.approx(140.0)
    assert (y0 + y1) / 2 == pytest.approx(260.0)


# --- preprocessing (regression guard) ---


def test_input_is_448_not_224() -> None:
    """The net input side is load-bearing: the same crop reads +9 deg at 224 and
    +41 deg at 448, and the adaptive pool means 224 runs without error."""
    from gaze_ml.l2cs import INPUT_PX

    assert INPUT_PX == 448


def test_preprocess_keeps_the_whole_box() -> None:
    """No centre crop. A marker in each corner of the box must survive into the tensor.

    This is the regression guard for the bug that made L2CS useless: a
    `CenterCrop(224)` copied from the archived demo threw away the outer half of
    the crop, leaving the network looking at a nose with the eyes cut off.
    """
    from gaze_ml.l2cs import INPUT_PX, MEAN, STD, crop_padded

    frame = np.zeros((400, 400, 3), dtype=np.uint8)
    box   = (100, 100, 300, 300)
    for cx, cy in ((110, 110), (290, 110), (110, 290), (290, 290)):
        frame[cy - 6 : cy + 6, cx - 6 : cx + 6] = 255

    crop = crop_padded(frame, box)
    assert crop is not None
    resized = cv2.resize(crop, (INPUT_PX, INPUT_PX), interpolation=cv2.INTER_LINEAR)
    rgb = (cv2.cvtColor(resized, cv2.COLOR_BGR2RGB).astype(np.float32) / 255.0 - MEAN) / STD
    assert rgb.shape == (INPUT_PX, INPUT_PX, 3)

    lit = rgb.max(axis=2) > 0.0
    edge = INPUT_PX // 8
    for ys, xs in (
        (slice(0, edge), slice(0, edge)),
        (slice(0, edge), slice(-edge, None)),
        (slice(-edge, None), slice(0, edge)),
        (slice(-edge, None), slice(-edge, None)),
    ):
        assert lit[ys, xs].any(), "a corner of the face box was cropped away"
