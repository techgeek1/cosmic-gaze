"""Dump the exact tensors fed to the gaze network, denormalised for viewing.

When an appearance model produces noise, the first question is always what it is
actually looking at. This writes the network's real input back out as a PNG, so
the crop can be inspected rather than reasoned about.
"""

from __future__ import annotations

from pathlib import Path

import cv2
import numpy as np
import torch

from gaze_ml.l2cs import MEAN, STD

# --- denormalisation ---


def tensor_to_bgr(batch: torch.Tensor) -> np.ndarray:
    """Undo the ImageNet normalisation and channel order of a `(1, 3, H, W)` batch."""
    chw = batch[0].detach().float().cpu().numpy()
    rgb = chw.transpose(1, 2, 0) * STD + MEAN
    rgb = np.clip(rgb * 255.0, 0, 255).astype(np.uint8)
    return cv2.cvtColor(rgb, cv2.COLOR_RGB2BGR)


def annotate(bgr: np.ndarray, lines: list[str]) -> np.ndarray:
    """Stamp a few short lines onto a crop so the PNG is self-describing."""
    out = bgr.copy()
    for i, line in enumerate(lines):
        y = 16 + 16 * i
        cv2.putText(out, line, (6, y), cv2.FONT_HERSHEY_SIMPLEX, 0.42, (0, 0, 0), 3, cv2.LINE_AA)
        cv2.putText(out, line, (6, y), cv2.FONT_HERSHEY_SIMPLEX, 0.42, (0, 255, 255), 1, cv2.LINE_AA)
    return out


def write_frame(path: Path, bgr: np.ndarray, box: tuple[int, int, int, int], det_box, score: float) -> None:
    """Save a full frame with the raw detector box and the expanded square crop box."""
    out = bgr.copy()
    x, y, w, h = det_box
    cv2.rectangle(out, (int(x), int(y)), (int(x + w), int(y + h)), (0, 200, 0), 2, cv2.LINE_AA)
    cv2.rectangle(out, (box[0], box[1]), (box[2], box[3]), (0, 128, 255), 2, cv2.LINE_AA)
    cv2.putText(
        out, f"green=blazeface {score:.2f}  orange=crop box", (12, 32),
        cv2.FONT_HERSHEY_SIMPLEX, 0.8, (0, 0, 0), 4, cv2.LINE_AA,
    )
    cv2.putText(
        out, f"green=blazeface {score:.2f}  orange=crop box", (12, 32),
        cv2.FONT_HERSHEY_SIMPLEX, 0.8, (255, 255, 255), 2, cv2.LINE_AA,
    )
    cv2.imwrite(str(path), out)
