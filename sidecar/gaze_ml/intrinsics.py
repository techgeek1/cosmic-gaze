"""Pinhole camera intrinsics: JSON loading, horizontal-FOV derivation, rescaling."""

from __future__ import annotations

import json
import math
from dataclasses import dataclass, replace
from pathlib import Path

import numpy as np

# --- constants ---

#: Default horizontal field of view, degrees. The Logitech C920 on this desk is
#: specified at 78 deg *diagonal*, which works out to ~70.4 deg horizontal at 16:9.
DEFAULT_HFOV_DEG: float = 70.4


# --- types ---


@dataclass(frozen=True)
class Intrinsics:
    """Pinhole intrinsics for one image size, in pixels.

    The origin convention is OpenCV's: `+x` right, `+y` down, `+z` out of the lens
    into the scene, principal point at the geometric image centre unless calibrated.
    """

    fx:     float
    fy:     float
    cx:     float
    cy:     float
    width:  int
    height: int
    dist:   tuple[float, ...] = (0.0, 0.0, 0.0, 0.0, 0.0)

    # --- constructors ---

    @classmethod
    def from_hfov(
        cls,
        width:    int,
        height:   int,
        hfov_deg: float = DEFAULT_HFOV_DEG,
    ) -> Intrinsics:
        """Derive intrinsics from the frame size and a horizontal field of view.

        Assumes square pixels (`fy == fx`), no distortion, and a centred principal
        point. `fx = (width / 2) / tan(hfov / 2)`.
        """
        if width <= 0 or height <= 0:
            raise ValueError(f"frame size must be positive, got {width}x{height}")
        if not 0.0 < hfov_deg < 180.0:
            raise ValueError(f"hfov_deg must be in (0, 180), got {hfov_deg}")
        fx = (width / 2.0) / math.tan(math.radians(hfov_deg) / 2.0)
        return cls(
            fx     = fx,
            fy     = fx,
            cx     = width / 2.0,
            cy     = height / 2.0,
            width  = int(width),
            height = int(height),
        )

    @classmethod
    def load(cls, path: str | Path) -> Intrinsics:
        """Load intrinsics from JSON.

        Two shapes are accepted: a flat `{fx, fy, cx, cy, width, height, dist}`
        object, or an OpenCV-style `{camera_matrix: [[..]], dist_coeffs: [..],
        image_size: [w, h]}` object.
        """
        obj = json.loads(Path(path).read_text())
        return cls.from_dict(obj)

    @classmethod
    def from_dict(cls, obj: dict) -> Intrinsics:
        """Build intrinsics from an already-parsed JSON object. See `load`."""
        if "camera_matrix" in obj:
            k = np.asarray(obj["camera_matrix"], dtype=np.float64).reshape(3, 3)
            w, h = obj.get("image_size", (int(k[0, 2] * 2), int(k[1, 2] * 2)))
            dist = tuple(float(v) for v in np.asarray(obj.get("dist_coeffs", [])).ravel())
            return cls(
                fx     = float(k[0, 0]),
                fy     = float(k[1, 1]),
                cx     = float(k[0, 2]),
                cy     = float(k[1, 2]),
                width  = int(w),
                height = int(h),
                dist   = dist or (0.0,) * 5,
            )
        missing = {"fx", "fy", "cx", "cy", "width", "height"} - set(obj)
        if missing:
            raise ValueError(f"intrinsics JSON is missing keys: {sorted(missing)}")
        dist = tuple(float(v) for v in obj.get("dist", ()))
        return cls(
            fx     = float(obj["fx"]),
            fy     = float(obj["fy"]),
            cx     = float(obj["cx"]),
            cy     = float(obj["cy"]),
            width  = int(obj["width"]),
            height = int(obj["height"]),
            dist   = dist or (0.0,) * 5,
        )

    # --- derived views ---

    def matrix(self) -> np.ndarray:
        """The 3x3 camera matrix `K` as float64, ready for OpenCV."""
        return np.array(
            [[self.fx, 0.0, self.cx], [0.0, self.fy, self.cy], [0.0, 0.0, 1.0]],
            dtype = np.float64,
        )

    def dist_coeffs(self) -> np.ndarray:
        """Distortion coefficients as an OpenCV-shaped `(n, 1)` float64 array."""
        return np.asarray(self.dist, dtype=np.float64).reshape(-1, 1)

    def scaled_to(self, width: int, height: int) -> Intrinsics:
        """Rescale to a different frame size, assuming the same field of view.

        Valid for a resize, not for a crop or a change of sensor mode.
        """
        if width <= 0 or height <= 0:
            raise ValueError(f"frame size must be positive, got {width}x{height}")
        sx = width / self.width
        sy = height / self.height
        return replace(
            self,
            fx     = self.fx * sx,
            fy     = self.fy * sy,
            cx     = self.cx * sx,
            cy     = self.cy * sy,
            width  = int(width),
            height = int(height),
        )

    @property
    def hfov_deg(self) -> float:
        """Horizontal field of view in degrees, implied by `fx` and `width`."""
        return 2.0 * math.degrees(math.atan((self.width / 2.0) / self.fx))

    @property
    def vfov_deg(self) -> float:
        """Vertical field of view in degrees, implied by `fy` and `height`."""
        return 2.0 * math.degrees(math.atan((self.height / 2.0) / self.fy))

    def to_dict(self) -> dict:
        """A flat JSON-serialisable form, the same one `load` accepts."""
        return {
            "fx":     self.fx,
            "fy":     self.fy,
            "cx":     self.cx,
            "cy":     self.cy,
            "width":  self.width,
            "height": self.height,
            "dist":   list(self.dist),
        }


# --- helpers ---


def resolve(
    path:     str | Path | None,
    width:    int,
    height:   int,
    hfov_deg: float = DEFAULT_HFOV_DEG,
) -> Intrinsics:
    """Load intrinsics from `path` (rescaled to the frame size) or derive from `hfov_deg`."""
    if path is None:
        return Intrinsics.from_hfov(width, height, hfov_deg)
    intr = Intrinsics.load(path)
    if (intr.width, intr.height) != (width, height):
        intr = intr.scaled_to(width, height)
    return intr
