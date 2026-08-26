"""Per-frame orchestration: detect -> landmarks -> PnP -> gaze."""

from __future__ import annotations

import time
from dataclasses import dataclass, field
from pathlib import Path

import numpy as np

from gaze_ml.camera import Frame
from gaze_ml.intrinsics import DEFAULT_HFOV_DEG, resolve
from gaze_ml.iris import DEFAULT_GAIN_DEG, IrisGaze, estimate_iris_gaze
from gaze_ml.l2cs import GazeAngles, GazeEstimator
from gaze_ml.landmarks import FaceFrontEnd, FaceObs
from gaze_ml.pnp import HeadPose, solve_head_pose

# --- defaults ---

#: Where `fetch-models` puts weights, relative to the sidecar package root.
MODELS_DIR: Path = Path(__file__).resolve().parent.parent / "models"

DETECTOR_FILE   = "blaze_face_short_range.tflite"
LANDMARKER_FILE = "face_landmarker.task"
GAZE_FILE       = "l2cs_gaze360_resnet50.safetensors"


# --- types ---


@dataclass
class PipelineConfig:
    """Everything the pipeline needs that is not a frame."""

    models_dir: Path        = MODELS_DIR
    intrinsics: Path | None = None
    hfov_deg:   float       = DEFAULT_HFOV_DEG
    device:     str         = "cuda"
    fp16:       bool        = True
    crop_scale: float       = 1.0
    min_score:  float       = 0.5
    video_mode: bool        = True
    #: `l2cs` (appearance), `iris` (geometric), or `both` -- which also streams the
    #: other one as `gaze_iris` / `gaze_l2cs` so a single run can compare them.
    estimator:  str         = "l2cs"
    iris_gain:  float       = DEFAULT_GAIN_DEG

    @property
    def wants_l2cs(self) -> bool:
        """Whether the appearance model needs loading and running."""
        return self.estimator in ("l2cs", "both")

    @property
    def wants_iris(self) -> bool:
        """Whether the geometric estimator runs."""
        return self.estimator in ("iris", "both")


@dataclass
class FrameResult:
    """The pipeline's answer for one frame, plus the stage timings that produced it."""

    t:        float
    seq:      int
    valid:    bool
    eye_mm:   np.ndarray | None = None
    gaze:     np.ndarray | None = None
    head_rot: np.ndarray | None = None
    conf:     float | None      = None
    stages:   dict[str, float]  = field(default_factory=dict)
    face:     FaceObs | None    = None
    pose:     HeadPose | None   = None
    angles:   GazeAngles | None = None
    #: Populated when the iris estimator ran; `gaze` mirrors whichever estimator
    #: the run selected as primary.
    gaze_iris: np.ndarray | None = None
    gaze_l2cs: np.ndarray | None = None
    iris:      IrisGaze | None   = None


# --- the pipeline ---


class Pipeline:
    """Holds the three models and the intrinsics; `process` is the whole hot path."""

    def __init__(self, cfg: PipelineConfig, width: int, height: int) -> None:
        """Load models and resolve intrinsics for the given frame size."""
        self.cfg  = cfg
        self.intr = resolve(cfg.intrinsics, width, height, cfg.hfov_deg)
        self.face = FaceFrontEnd(
            detector_path   = cfg.models_dir / DETECTOR_FILE,
            landmarker_path = cfg.models_dir / LANDMARKER_FILE,
            video_mode      = cfg.video_mode,
            min_score       = cfg.min_score,
        )
        self.gaze = (
            GazeEstimator(
                weights    = cfg.models_dir / GAZE_FILE,
                device     = cfg.device,
                fp16       = cfg.fp16,
                crop_scale = cfg.crop_scale,
            )
            if cfg.wants_l2cs
            else None
        )
        self.size = (width, height)

    # --- hot path ---

    def process(self, frame: Frame) -> FrameResult:
        """Run one frame end to end. Never raises on a missing face; returns invalid."""
        stages: dict[str, float] = {}
        height, width = frame.bgr.shape[:2]
        if (width, height) != self.size:
            self.intr = self.intr.scaled_to(width, height)
            self.size = (width, height)

        t0  = time.perf_counter()
        obs = self.face.detect(frame.bgr, int(frame.t_s * 1000.0))
        stages["face_ms"] = (time.perf_counter() - t0) * 1000.0

        if obs is None:
            return FrameResult(t=frame.t_s, seq=frame.seq, valid=False, stages=stages)

        t1   = time.perf_counter()
        pose = solve_head_pose(obs.names, obs.named_px, self.intr)
        stages["pnp_ms"] = (time.perf_counter() - t1) * 1000.0

        if pose is None:
            return FrameResult(
                t=frame.t_s, seq=frame.seq, valid=False, stages=stages, face=obs
            )

        ang: GazeAngles | None = None
        gaze_l2cs: np.ndarray | None = None
        if self.gaze is not None:
            t2  = time.perf_counter()
            box = obs.detection.square(self.cfg.crop_scale)
            ang = self.gaze.estimate(frame.bgr, box)
            stages["gaze_ms"] = (time.perf_counter() - t2) * 1000.0
            if ang is not None:
                gaze_l2cs = ang.vector()

        iris: IrisGaze | None = None
        gaze_iris: np.ndarray | None = None
        if self.cfg.wants_iris:
            t3   = time.perf_counter()
            iris = estimate_iris_gaze(obs.points_px, pose, self.cfg.iris_gain)
            stages["iris_ms"] = (time.perf_counter() - t3) * 1000.0
            if iris is not None:
                gaze_iris = iris.vector(pose)

        primary = gaze_iris if self.cfg.estimator == "iris" else gaze_l2cs
        if primary is None:
            return FrameResult(
                t=frame.t_s, seq=frame.seq, valid=False, stages=stages, face=obs, pose=pose
            )

        stages["total_ms"] = (time.perf_counter() - t0) * 1000.0
        return FrameResult(
            t         = frame.t_s,
            seq       = frame.seq,
            valid     = True,
            eye_mm    = pose.eye_mm,
            gaze      = primary,
            head_rot  = pose.rvec,
            conf      = obs.detection.score,
            stages    = stages,
            face      = obs,
            pose      = pose,
            angles    = ang,
            gaze_iris = gaze_iris,
            gaze_l2cs = gaze_l2cs,
            iris      = iris,
        )

    def close(self) -> None:
        """Release the MediaPipe graphs."""
        self.face.close()

    def __enter__(self) -> Pipeline:
        return self

    def __exit__(self, *exc: object) -> None:
        self.close()


# --- device selection ---


def pick_device(requested: str) -> tuple[str, str]:
    """Resolve a device request to `(device, human-readable name)`.

    `auto` prefers the GPU and falls back to the CPU *loudly* -- the caller is
    expected to print the name, because a silent CPU fallback would turn a 6 ms
    stage into a 200 ms one with no other symptom.
    """
    import torch

    if requested in ("cuda", "auto") and torch.cuda.is_available():
        return "cuda", torch.cuda.get_device_name(0)
    if requested == "cuda":
        raise RuntimeError("--device cuda requested but torch.cuda.is_available() is False")
    return "cpu", "cpu"
