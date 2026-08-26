"""Model weight downloads. Weights are gitignored; this is the only way in."""

from __future__ import annotations

import sys
import urllib.request
from dataclasses import dataclass
from pathlib import Path

# --- the manifest ---


@dataclass(frozen=True)
class Weight:
    """One downloadable model file and the licence it ships under."""

    filename: str
    url:      str
    licence:  str
    note:     str


#: Everything the pipeline needs, with provenance. Mirrored in `README.md`.
WEIGHTS: tuple[Weight, ...] = (
    Weight(
        filename = "blaze_face_short_range.tflite",
        url      = "https://storage.googleapis.com/mediapipe-models/face_detector/"
                   "blaze_face_short_range/float16/1/blaze_face_short_range.tflite",
        licence  = "Apache-2.0",
        note     = "MediaPipe BlazeFace short-range face detector.",
    ),
    Weight(
        filename = "face_landmarker.task",
        url      = "https://storage.googleapis.com/mediapipe-models/face_landmarker/"
                   "face_landmarker/float16/1/face_landmarker.task",
        licence  = "Apache-2.0",
        note     = "MediaPipe Face Landmarker bundle, 478 points including irises.",
    ),
    Weight(
        filename = "l2cs_gaze360_resnet50.safetensors",
        url      = "https://huggingface.co/py-feat/l2cs/resolve/main/"
                   "l2cs_gaze360_resnet50.safetensors",
        licence  = "MIT (repackaging); trained on Gaze360, which is research-use only",
        note     = "L2CS-Net ResNet-50, Gaze360 checkpoint, repackaged by py-feat.",
    ),
)


# --- fetching ---


def fetch(dest: Path, force: bool = False) -> list[Path]:
    """Download every manifest entry into `dest`, skipping what is already there."""
    dest.mkdir(parents=True, exist_ok=True)
    written: list[Path] = []
    for weight in WEIGHTS:
        path = dest / weight.filename
        if path.exists() and not force:
            print(f"have {path}", file=sys.stderr)
            written.append(path)
            continue
        print(f"get  {weight.url}", file=sys.stderr)
        tmp = path.with_suffix(path.suffix + ".part")
        with urllib.request.urlopen(weight.url) as response, tmp.open("wb") as out:
            while chunk := response.read(1 << 20):
                out.write(chunk)
        tmp.replace(path)
        print(f"     -> {path} ({path.stat().st_size / 1e6:.1f} MB, {weight.licence})", file=sys.stderr)
        written.append(path)
    return written


def missing(dest: Path) -> list[str]:
    """Names of manifest entries not present in `dest`."""
    return [w.filename for w in WEIGHTS if not (dest / w.filename).exists()]
