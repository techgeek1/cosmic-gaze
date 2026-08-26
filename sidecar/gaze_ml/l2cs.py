"""L2CS-Net: appearance-based gaze from a face crop, on the GPU.

L2CS-Net (Abdelrahman et al., "L2CS-Net: Fine-Grained Gaze Estimation in
Unconstrained Environments", 2022) is a ResNet-50 with two classification heads,
one for yaw and one for pitch, each over 90 four-degree bins spanning
[-180, 180). The predicted angle is the softmax expectation over bin centres,
which is what makes it fine-grained despite being a classifier.

The checkpoint is the Gaze360-trained one, so the predicted angles live in the
*camera* frame of the capture rig, not in a head-normalised frame: no
de-rotation is applied here.
"""

from __future__ import annotations

import math
from dataclasses import dataclass
from pathlib import Path

import cv2
import numpy as np
import torch
import torch.nn.functional as F
from torch import nn
from torchvision.models import resnet50

# --- constants ---

#: Number of angular bins per head in the Gaze360 checkpoint.
NUM_BINS: int = 90

#: Width of one bin, degrees. `NUM_BINS * BIN_DEG == 360`.
BIN_DEG: float = 4.0

#: Network input side. This is 448, not 224, and it matters: the same crop fed at
#: 224 reads +9 degrees of yaw where 448 reads +41. The trunk's adaptive average
#: pool means a 224 input runs without error, which is exactly why the mistake is
#: easy to make and silent. The maintained reference (`l2cs/utils.py`) resizes the
#: face crop to 224 with OpenCV and then straight back up to 448 with PIL; going
#: directly to 448 agrees with that round trip to within 1.3 degrees, so this
#: skips it.
INPUT_PX: int = 448

#: ImageNet normalisation, as used at training time.
MEAN = np.array([0.485, 0.456, 0.406], dtype=np.float32)
STD  = np.array([0.229, 0.224, 0.225], dtype=np.float32)


# --- types ---


@dataclass(frozen=True)
class GazeAngles:
    """A gaze prediction, in the network's own angular parameterisation."""

    yaw_deg:   float
    pitch_deg: float
    #: Peak softmax mass of the two heads, averaged. Not a calibrated confidence,
    #: but a usable sharpness signal; the protocol reports the detector score.
    sharpness: float

    def vector(self) -> np.ndarray:
        """The unit gaze direction in OpenCV camera coordinates.

        `+x` right, `+y` down, `+z` out of the lens into the scene. A subject
        looking straight into the lens therefore yields `(0, 0, -1)`.
        """
        yaw   = math.radians(self.yaw_deg)
        pitch = math.radians(self.pitch_deg)
        return np.array(
            [
                -math.cos(pitch) * math.sin(yaw),
                -math.sin(pitch),
                -math.cos(pitch) * math.cos(yaw),
            ],
            dtype = np.float64,
        )


# --- the network ---


class L2CS(nn.Module):
    """ResNet-50 trunk with separate yaw and pitch classification heads."""

    def __init__(self, num_bins: int = NUM_BINS) -> None:
        """Build the trunk with no pretrained weights; the checkpoint supplies them."""
        super().__init__()
        trunk         = resnet50(weights=None)
        self.features = nn.Sequential(*list(trunk.children())[:-1])
        self.fc_yaw_gaze   = nn.Linear(2048, num_bins)
        self.fc_pitch_gaze = nn.Linear(2048, num_bins)

    def forward(self, x: torch.Tensor) -> tuple[torch.Tensor, torch.Tensor]:
        """Return `(yaw_logits, pitch_logits)` for a `(n, 3, H, W)` batch.

        Yaw first, matching upstream `l2cs/model.py`. Note that upstream's own
        `l2cs/pipeline.py` then unpacks that call as `pitch, yaw = self.model(x)`,
        which swaps them; do not "fix" this to agree with the reference pipeline.
        """
        feat = torch.flatten(self.features(x), 1)
        return self.fc_yaw_gaze(feat), self.fc_pitch_gaze(feat)


def _load_state_dict(path: Path) -> dict[str, torch.Tensor]:
    """Read either a safetensors or a pickled torch checkpoint into a state dict."""
    if path.suffix == ".safetensors":
        from safetensors.torch import load_file

        return load_file(str(path))
    obj = torch.load(str(path), map_location="cpu", weights_only=True)
    return obj.get("state_dict", obj) if isinstance(obj, dict) else obj


def _remap_resnet_keys(src: dict[str, torch.Tensor]) -> dict[str, torch.Tensor]:
    """Rewrite upstream L2CS key names onto this module's `features.N.*` layout.

    The published checkpoint uses torchvision's flat ResNet names (`conv1.weight`,
    `layer3.0.bn2.bias`, ...). `nn.Sequential(*children)` renumbers those to
    positional indices, so the mapping is a fixed table.
    """
    order = {
        "conv1":  "0",
        "bn1":    "1",
        "layer1": "4",
        "layer2": "5",
        "layer3": "6",
        "layer4": "7",
    }
    out: dict[str, torch.Tensor] = {}
    for key, value in src.items():
        head, _, rest = key.partition(".")
        if head in order:
            out[f"features.{order[head]}.{rest}"] = value
        elif head in ("fc_yaw_gaze", "fc_pitch_gaze"):
            out[key] = value
        # `fc_finetune` is an unused 3-way auxiliary head; drop it.
    return out


# --- the estimator ---


class GazeEstimator:
    """Runs L2CS-Net on face crops, holding the model resident on the device."""

    def __init__(
        self,
        weights:    str | Path,
        device:     str | torch.device = "cuda",
        fp16:       bool  = True,
        crop_scale: float = 1.0,
    ) -> None:
        """Load the checkpoint onto `device`.

        `crop_scale` expands the detector box before the resize. The reference
        pipeline feeds the raw RetinaFace box with no expansion at all, and
        MediaPipe's BlazeFace box is framed comparably (eyebrows to chin), so the
        default is 1.0. Expanding hurts: 1.8 costs 7 degrees of pitch and 2.2
        costs 20, because the face shrinks inside the frame and the network reads
        that as distance and pose.
        """
        path = Path(weights)
        if not path.exists():
            raise FileNotFoundError(f"gaze weights not found: {path} (run `gaze-ml fetch-models`)")

        self.device     = torch.device(device)
        self.fp16       = bool(fp16) and self.device.type == "cuda"
        self.crop_scale = float(crop_scale)
        self.dtype      = torch.float16 if self.fp16 else torch.float32

        model   = L2CS()
        missing, unexpected = model.load_state_dict(
            _remap_resnet_keys(_load_state_dict(path)), strict=False
        )
        strays = [k for k in missing if "num_batches_tracked" not in k]
        if strays or unexpected:
            raise RuntimeError(
                f"checkpoint does not match L2CS-Net: missing={strays[:5]} unexpected={list(unexpected)[:5]}"
            )
        self.model = model.to(self.device, dtype=self.dtype).eval()

        self._bins = torch.arange(NUM_BINS, device=self.device, dtype=torch.float32)
        self.warmup()

    # --- inference ---

    def warmup(self, iters: int = 3) -> None:
        """Run a few dummy batches so the first real frame is not paying for JIT."""
        dummy = torch.zeros(1, 3, INPUT_PX, INPUT_PX, device=self.device, dtype=self.dtype)
        with torch.inference_mode():
            for _ in range(iters):
                self.model(dummy)
        if self.device.type == "cuda":
            torch.cuda.synchronize()

    def preprocess(self, bgr: np.ndarray, box: tuple[int, int, int, int]) -> torch.Tensor | None:
        """Crop, resize and normalise one face into a `(1, 3, INPUT_PX, INPUT_PX)` tensor.

        The whole face box goes in. An earlier version of this function also took
        a centre crop -- copied from the archived `demo.py` -- which threw away
        the outer half of the box and left the network looking at a nose and a
        mouth with the eyes cut off above the frame. It produced stable, plausible
        numbers that tracked nothing. The maintained reference has no such crop.
        """
        crop = crop_padded(bgr, box)
        if crop is None:
            return None

        crop = cv2.resize(crop, (INPUT_PX, INPUT_PX), interpolation=cv2.INTER_LINEAR)

        rgb = cv2.cvtColor(crop, cv2.COLOR_BGR2RGB).astype(np.float32) / 255.0
        rgb = (rgb - MEAN) / STD
        chw = np.ascontiguousarray(rgb.transpose(2, 0, 1))[None]
        return torch.from_numpy(chw).to(self.device, dtype=self.dtype, non_blocking=True)

    def infer(self, batch: torch.Tensor) -> GazeAngles:
        """Run the network on a preprocessed batch of one and decode the angles."""
        with torch.inference_mode():
            yaw_logits, pitch_logits = self.model(batch)
            yaw_p   = F.softmax(yaw_logits.float(), dim=1)
            pitch_p = F.softmax(pitch_logits.float(), dim=1)
            yaw     = (yaw_p * self._bins).sum(dim=1) * BIN_DEG - 180.0
            pitch   = (pitch_p * self._bins).sum(dim=1) * BIN_DEG - 180.0
            sharp   = 0.5 * (yaw_p.max(dim=1).values + pitch_p.max(dim=1).values)
        return GazeAngles(
            yaw_deg   = float(yaw[0]),
            pitch_deg = float(pitch[0]),
            sharpness = float(sharp[0]),
        )

    def estimate(self, bgr: np.ndarray, box: tuple[int, int, int, int]) -> GazeAngles | None:
        """Preprocess and run one face crop. Returns `None` for a degenerate box."""
        batch = self.preprocess(bgr, box)
        return None if batch is None else self.infer(batch)


# --- cropping ---


def crop_padded(bgr: np.ndarray, box: tuple[int, int, int, int]) -> np.ndarray | None:
    """Extract `box` from `bgr`, zero-padding whatever falls outside the frame.

    Clamping to the frame instead would change the box's aspect ratio and move
    the face off centre within it, which the network reads as a head-pose cue.
    Padding keeps the geometry honest at the cost of some black.
    """
    x0, y0, x1, y1 = box
    side = x1 - x0
    if side < 8 or (y1 - y0) != side:
        return None
    height, width = bgr.shape[:2]
    sx0, sy0 = max(0, x0), max(0, y0)
    sx1, sy1 = min(width, x1), min(height, y1)
    if sx1 <= sx0 or sy1 <= sy0:
        return None
    out = np.zeros((side, side, 3), dtype=bgr.dtype)
    out[sy0 - y0 : sy1 - y0, sx0 - x0 : sx1 - x0] = bgr[sy0:sy1, sx0:sx1]
    return out


# --- decoding, exposed for tests ---


def decode_bins(probs: np.ndarray) -> float:
    """Softmax expectation over bin centres, in degrees. `probs` is `(NUM_BINS,)`."""
    idx = np.arange(len(probs), dtype=np.float64)
    return float((probs * idx).sum() * BIN_DEG - 180.0)
