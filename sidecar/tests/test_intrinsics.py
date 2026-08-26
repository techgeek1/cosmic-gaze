"""Intrinsics derivation, rescaling, and JSON round trips."""

from __future__ import annotations

import json
import math

import pytest

from gaze_ml.intrinsics import DEFAULT_HFOV_DEG, Intrinsics, resolve


def test_hfov_90_gives_half_width_focal() -> None:
    """At a 90 degree horizontal FOV the focal length is exactly half the width."""
    intr = Intrinsics.from_hfov(1920, 1080, 90.0)
    assert intr.fx == pytest.approx(960.0)
    assert intr.fy == pytest.approx(960.0)
    assert (intr.cx, intr.cy) == (960.0, 540.0)


def test_hfov_round_trips() -> None:
    """`hfov_deg` recovers the angle that produced the focal length."""
    for hfov in (40.0, 70.4, 78.0, 120.0):
        intr = Intrinsics.from_hfov(1920, 1080, hfov)
        assert intr.hfov_deg == pytest.approx(hfov)


def test_c920_vfov_matches_datasheet() -> None:
    """70.4 deg horizontal on 16:9 implies ~43.3 deg vertical, as specified."""
    intr = Intrinsics.from_hfov(1920, 1080, DEFAULT_HFOV_DEG)
    assert intr.vfov_deg == pytest.approx(43.3, abs=0.3)


def test_diagonal_of_default_is_78_deg() -> None:
    """The C920's quoted 78 deg is the diagonal, which the default must reproduce."""
    intr  = Intrinsics.from_hfov(1920, 1080, DEFAULT_HFOV_DEG)
    diag  = math.hypot(intr.width, intr.height)
    dfov  = 2.0 * math.degrees(math.atan((diag / 2.0) / intr.fx))
    assert dfov == pytest.approx(78.0, abs=0.6)


def test_scaled_to_preserves_fov() -> None:
    """Rescaling to a different frame size keeps both fields of view."""
    full = Intrinsics.from_hfov(1920, 1080, 70.4)
    half = full.scaled_to(960, 540)
    assert half.fx == pytest.approx(full.fx / 2.0)
    assert half.cx == pytest.approx(full.cx / 2.0)
    assert half.hfov_deg == pytest.approx(full.hfov_deg)
    assert half.vfov_deg == pytest.approx(full.vfov_deg)


def test_matrix_layout() -> None:
    """`matrix()` is the standard OpenCV `K` layout."""
    intr = Intrinsics(fx=100.0, fy=200.0, cx=10.0, cy=20.0, width=64, height=48)
    k = intr.matrix()
    assert k.shape == (3, 3)
    assert (k[0, 0], k[1, 1], k[0, 2], k[1, 2], k[2, 2]) == (100.0, 200.0, 10.0, 20.0, 1.0)
    assert k[0, 1] == 0.0 and k[1, 0] == 0.0 and k[2, 0] == 0.0 and k[2, 1] == 0.0


def test_flat_json_round_trip(tmp_path) -> None:
    """A flat JSON file loads back to an identical object."""
    intr = Intrinsics.from_hfov(1280, 720, 70.4)
    path = tmp_path / "intr.json"
    path.write_text(json.dumps(intr.to_dict()))
    assert Intrinsics.load(path) == intr


def test_opencv_style_json(tmp_path) -> None:
    """The OpenCV `camera_matrix` / `dist_coeffs` shape is accepted too."""
    path = tmp_path / "cv.json"
    path.write_text(
        json.dumps(
            {
                "camera_matrix": [[900.0, 0.0, 640.0], [0.0, 905.0, 360.0], [0.0, 0.0, 1.0]],
                "dist_coeffs":   [0.1, -0.2, 0.0, 0.0, 0.05],
                "image_size":    [1280, 720],
            }
        )
    )
    intr = Intrinsics.load(path)
    assert (intr.fx, intr.fy, intr.cx, intr.cy) == (900.0, 905.0, 640.0, 360.0)
    assert intr.dist_coeffs().shape == (5, 1)


def test_resolve_prefers_file_and_rescales(tmp_path) -> None:
    """`resolve` rescales a calibration recorded at another resolution."""
    path = tmp_path / "intr.json"
    path.write_text(json.dumps(Intrinsics.from_hfov(1280, 720, 60.0).to_dict()))
    intr = resolve(path, 1920, 1080)
    assert (intr.width, intr.height) == (1920, 1080)
    assert intr.hfov_deg == pytest.approx(60.0)


def test_resolve_without_file_uses_hfov() -> None:
    """With no file, `resolve` derives from the frame size and the given FOV."""
    intr = resolve(None, 640, 480, 90.0)
    assert intr.fx == pytest.approx(320.0)


@pytest.mark.parametrize("bad", [0.0, -1.0, 180.0, 200.0])
def test_bad_hfov_rejected(bad: float) -> None:
    """Field of view outside (0, 180) is a hard error, not a silent clamp."""
    with pytest.raises(ValueError):
        Intrinsics.from_hfov(640, 480, bad)


def test_missing_keys_rejected() -> None:
    """An incomplete JSON object names what is missing."""
    with pytest.raises(ValueError, match="missing keys"):
        Intrinsics.from_dict({"fx": 1.0, "fy": 1.0})
