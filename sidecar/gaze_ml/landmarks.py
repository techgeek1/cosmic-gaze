"""Face detection and 2D landmarks, both from MediaPipe Tasks (Apache-2.0).

Two models run per frame:

- `blaze_face_short_range` gives the face box *and a detection score*, which is
  what the protocol's `conf` field carries. The landmarker on its own reports no
  score at all, so without this there would be nothing honest to put there.
- `face_landmarker` gives 478 mesh points, of which eleven are used for PnP and
  two (the iris centres) for drawing and for the eye-crop fallback.
"""

from __future__ import annotations

from dataclasses import dataclass
from pathlib import Path

import numpy as np

from gaze_ml.face_model import (
    MEDIAPIPE_INDEX,
    MEDIAPIPE_IRIS_INDEX,
    PNP_POINTS,
    named_from_indices,
)

# --- types ---


@dataclass(frozen=True)
class Detection:
    """A face box in pixels with the detector's own confidence."""

    x:     float
    y:     float
    w:     float
    h:     float
    score: float

    def square(self, scale: float = 1.0) -> tuple[int, int, int, int]:
        """A square crop box `(x0, y0, x1, y1)` around the face.

        Square because the gaze network's own preprocessing resizes without
        preserving aspect; feeding it a rectangle would shear the face. The box
        is deliberately *not* clamped to the frame: on this desk the head sits at
        the bottom edge, and clamping there would silently un-square the crop and
        shift the face inside it. `l2cs.crop_padded` pads instead.
        """
        cx, cy = self.x + self.w / 2.0, self.y + self.h / 2.0
        half   = max(self.w, self.h) * scale / 2.0
        return (
            int(round(cx - half)),
            int(round(cy - half)),
            int(round(cx + half)),
            int(round(cy + half)),
        )


@dataclass(frozen=True)
class FaceObs:
    """Everything the vision front end knows about one face in one frame."""

    detection: Detection
    points_px: np.ndarray            #: `(n, 2)` dense mesh landmarks in pixels.
    names:     tuple[str, ...]       #: Names of the PnP correspondence subset.
    named_px:  np.ndarray            #: `(len(names), 2)` pixels, aligned with `names`.
    iris_px:   dict[str, np.ndarray] #: Iris centres in pixels, when available.


# --- the front end ---


class FaceFrontEnd:
    """MediaPipe face detector plus mesh landmarker, held open across frames."""

    def __init__(
        self,
        detector_path:   str | Path,
        landmarker_path: str | Path,
        video_mode:      bool  = True,
        min_score:       float = 0.5,
    ) -> None:
        """Load both models. `video_mode` enables MediaPipe's inter-frame tracking.

        Video mode needs strictly increasing timestamps; `detect` enforces that.
        """
        from mediapipe.tasks.python import BaseOptions
        from mediapipe.tasks.python import vision as mp_vision

        for path in (detector_path, landmarker_path):
            if not Path(path).exists():
                raise FileNotFoundError(f"model not found: {path} (run `gaze-ml fetch-models`)")

        mode = (
            mp_vision.RunningMode.VIDEO if video_mode else mp_vision.RunningMode.IMAGE
        )
        self._video_mode = video_mode
        self._min_score  = min_score
        self._last_ms    = -1

        self._detector = mp_vision.FaceDetector.create_from_options(
            mp_vision.FaceDetectorOptions(
                base_options            = BaseOptions(model_asset_path=str(detector_path)),
                running_mode            = mode,
                min_detection_confidence = min_score,
            )
        )
        self._landmarker = mp_vision.FaceLandmarker.create_from_options(
            mp_vision.FaceLandmarkerOptions(
                base_options                  = BaseOptions(model_asset_path=str(landmarker_path)),
                running_mode                  = mode,
                num_faces                     = 1,
                min_face_detection_confidence = min_score,
            )
        )

    # --- inference ---

    def detect(self, bgr: np.ndarray, timestamp_ms: int) -> FaceObs | None:
        """Run both models on one BGR frame. Returns `None` when no face is found."""
        import cv2
        import mediapipe as mp

        height, width = bgr.shape[:2]
        # cvtColor, not `bgr[:, :, ::-1].copy()`: the strided NumPy reversal costs
        # 6.8 ms on a 1080p frame, as much as the landmarker itself.
        image = mp.Image(
            image_format = mp.ImageFormat.SRGB,
            data         = cv2.cvtColor(bgr, cv2.COLOR_BGR2RGB),
        )

        if self._video_mode:
            timestamp_ms  = max(int(timestamp_ms), self._last_ms + 1)
            self._last_ms = timestamp_ms
            det_result = self._detector.detect_for_video(image, timestamp_ms)
            lm_result  = self._landmarker.detect_for_video(image, timestamp_ms)
        else:
            det_result = self._detector.detect(image)
            lm_result  = self._landmarker.detect(image)

        if not det_result.detections or not lm_result.face_landmarks:
            return None

        best = max(det_result.detections, key=lambda d: d.categories[0].score)
        box  = best.bounding_box
        detection = Detection(
            x     = float(box.origin_x),
            y     = float(box.origin_y),
            w     = float(box.width),
            h     = float(box.height),
            score = float(best.categories[0].score),
        )

        landmarks = lm_result.face_landmarks[0]
        points_px = np.array(
            [[lm.x * width, lm.y * height] for lm in landmarks], dtype=np.float64
        )
        names, named_px = named_from_indices(points_px, MEDIAPIPE_INDEX, PNP_POINTS)
        iris_px = {
            name: points_px[idx]
            for name, idx in MEDIAPIPE_IRIS_INDEX.items()
            if idx < len(points_px)
        }
        return FaceObs(
            detection = detection,
            points_px = points_px,
            names     = names,
            named_px  = named_px,
            iris_px   = iris_px,
        )

    def close(self) -> None:
        """Release both MediaPipe graphs."""
        self._detector.close()
        self._landmarker.close()

    def __enter__(self) -> FaceFrontEnd:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()
