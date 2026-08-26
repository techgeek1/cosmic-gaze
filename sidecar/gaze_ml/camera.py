"""V4L2 and file video sources.

MJPEG is requested explicitly: the C920 cannot sustain 1080p30 over USB in
uncompressed YUYV (it tops out around 5 fps), so the FOURCC has to be set before
the frame size or the driver silently negotiates YUYV and the pipeline runs at a
fifth of the requested rate.
"""

from __future__ import annotations

import fcntl
import os
import struct
import sys
import threading
import time
from dataclasses import dataclass
from pathlib import Path

import cv2
import numpy as np

# --- V4L2 controls ---

#: `VIDIOC_S_CTRL`, i.e. `_IOWR('V', 28, struct v4l2_control)`.
VIDIOC_S_CTRL: int = 0xC008561C

#: `V4L2_CID_EXPOSURE_AUTO_PRIORITY`: when set, the driver is allowed to lengthen
#: exposure past the frame interval and halve the frame rate to compensate.
V4L2_CID_EXPOSURE_AUTO_PRIORITY: int = 0x009A0903


def set_v4l2_control(device: str, ctrl_id: int, value: int) -> bool:
    """Set one integer V4L2 control by ioctl. Returns False if the driver refuses.

    Done directly rather than through OpenCV because the property that matters
    here -- dynamic frame rate -- has no `CAP_PROP_*` equivalent.
    """
    try:
        fd = os.open(device, os.O_RDWR)
    except OSError:
        return False
    try:
        fcntl.ioctl(fd, VIDIOC_S_CTRL, struct.pack("Ii", ctrl_id, value))
        return True
    except OSError:
        return False
    finally:
        os.close(fd)


# --- types ---


@dataclass(frozen=True)
class Frame:
    """One captured frame with the monotonic clock reading taken at capture."""

    bgr:  np.ndarray
    t_s:  float
    seq:  int


class VideoSource:
    """Common surface for the camera and file sources."""

    width:  int
    height: int
    fps:    float

    def read(self) -> Frame | None:
        """Next frame, or `None` at end of stream / on a read failure."""
        raise NotImplementedError

    def release(self) -> None:
        """Release the underlying capture."""
        raise NotImplementedError

    def __enter__(self) -> VideoSource:
        return self

    def __exit__(self, *exc: object) -> None:
        self.release()


# --- sources ---


class Camera(VideoSource):
    """A V4L2 capture device configured for MJPEG at a requested mode."""

    def __init__(
        self,
        device: str = "/dev/video0",
        width:  int = 1920,
        height: int = 1080,
        fps:    int = 30,
        fourcc: str = "MJPG",
        fixed_framerate: bool = True,
    ) -> None:
        """Open and configure the device, then verify what the driver actually gave.

        `fixed_framerate` clears `exposure_auto_priority`, which a C920 sets by
        default: in a dim room the driver otherwise stretches the exposure to
        66 ms and quietly halves the capture rate to 15 fps. Clearing it holds 30
        fps at the cost of a darker image, which is the right trade for a gaze
        stream but is worth knowing about when detection starts failing.
        """
        if not Path(device).exists():
            raise FileNotFoundError(f"camera not found: {device}")
        if fixed_framerate and not set_v4l2_control(device, V4L2_CID_EXPOSURE_AUTO_PRIORITY, 0):
            print(
                f"[gaze-ml] warning: could not clear exposure_auto_priority on {device}; "
                "the driver may drop to 15 fps in low light",
                file = sys.stderr,
            )
        cap = cv2.VideoCapture(device, cv2.CAP_V4L2)
        if not cap.isOpened():
            raise RuntimeError(f"could not open {device} via V4L2")

        cap.set(cv2.CAP_PROP_FOURCC, cv2.VideoWriter_fourcc(*fourcc))
        cap.set(cv2.CAP_PROP_FRAME_WIDTH, width)
        cap.set(cv2.CAP_PROP_FRAME_HEIGHT, height)
        cap.set(cv2.CAP_PROP_FPS, fps)
        cap.set(cv2.CAP_PROP_BUFFERSIZE, 1)

        self._cap   = cap
        self._seq   = 0
        self.width  = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
        self.height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
        self.fps    = float(cap.get(cv2.CAP_PROP_FPS)) or float(fps)
        self.fourcc = _fourcc_name(int(cap.get(cv2.CAP_PROP_FOURCC)))
        self.device = device

    def read(self) -> Frame | None:
        """Grab and decode one frame, timestamping on return from the driver."""
        ok, bgr = self._cap.read()
        t_s = time.monotonic()
        if not ok or bgr is None:
            return None
        self._seq += 1
        return Frame(bgr=bgr, t_s=t_s, seq=self._seq - 1)

    def release(self) -> None:
        """Close the V4L2 device."""
        self._cap.release()


class VideoFile(VideoSource):
    """A file source, used by `bench --input` and by the offline tests."""

    def __init__(self, path: str | Path, loop: bool = False) -> None:
        """Open a video file. `loop` restarts from frame zero at end of stream."""
        if not Path(path).exists():
            raise FileNotFoundError(f"video not found: {path}")
        cap = cv2.VideoCapture(str(path))
        if not cap.isOpened():
            raise RuntimeError(f"could not open video {path}")
        self._cap   = cap
        self._loop  = loop
        self._seq   = 0
        self.width  = int(cap.get(cv2.CAP_PROP_FRAME_WIDTH))
        self.height = int(cap.get(cv2.CAP_PROP_FRAME_HEIGHT))
        self.fps    = float(cap.get(cv2.CAP_PROP_FPS)) or 30.0
        self.fourcc = "file"
        self.device = str(path)

    def read(self) -> Frame | None:
        """Decode the next frame, wrapping to the start when `loop` is set."""
        ok, bgr = self._cap.read()
        if (not ok or bgr is None) and self._loop:
            self._cap.set(cv2.CAP_PROP_POS_FRAMES, 0)
            ok, bgr = self._cap.read()
        t_s = time.monotonic()
        if not ok or bgr is None:
            return None
        self._seq += 1
        return Frame(bgr=bgr, t_s=t_s, seq=self._seq - 1)

    def release(self) -> None:
        """Close the file."""
        self._cap.release()


class AsyncCamera(VideoSource):
    """A camera drained by its own thread, so inference never throttles capture.

    Reading synchronously does not work at 1080p30: `cap.read()` returns the
    frame the driver already has, the pipeline spends ~16 ms on it, and by the
    time the buffer is re-queued the next frame has been and gone -- so the loop
    settles at every *other* frame, 15 fps, with 50 ms of that spent blocked in
    `read()`. This thread keeps the V4L2 queue drained at the sensor rate and
    hands the consumer whichever frame is newest; frames the consumer never asks
    for are counted, not queued, because a stale gaze sample is worthless.
    """

    def __init__(self, camera: Camera) -> None:
        """Take ownership of an open `Camera` and start draining it."""
        self._camera  = camera
        self._latest:  Frame | None = None
        self._lock    = threading.Lock()
        self._fresh   = threading.Event()
        self._stop    = threading.Event()
        self._failed  = False
        self.skipped  = 0

        self.width  = camera.width
        self.height = camera.height
        self.fps    = camera.fps
        self.fourcc = camera.fourcc
        self.device = camera.device

        self._thread = threading.Thread(target=self._loop, name="gaze-ml-capture", daemon=True)
        self._thread.start()

    def _loop(self) -> None:
        """Read frames as fast as the device produces them until stopped."""
        while not self._stop.is_set():
            frame = self._camera.read()
            if frame is None:
                self._failed = True
                self._fresh.set()
                return
            with self._lock:
                if self._fresh.is_set():
                    self.skipped += 1   # the consumer never asked for the last one
                self._latest = frame
            self._fresh.set()

    def read(self, timeout: float = 2.0) -> Frame | None:
        """Block until a frame newer than the last one returned is available."""
        if not self._fresh.wait(timeout):
            return None
        with self._lock:
            self._fresh.clear()
            frame = self._latest
        return None if self._failed else frame

    def release(self) -> None:
        """Stop the capture thread and close the device."""
        self._stop.set()
        self._thread.join(timeout=2.0)
        self._camera.release()


class StillImage(VideoSource):
    """A single image repeated, so the offline tests can exercise the stream path."""

    def __init__(self, path: str | Path, count: int = 1) -> None:
        """Load `path` and yield it `count` times."""
        bgr = cv2.imread(str(path), cv2.IMREAD_COLOR)
        if bgr is None:
            raise FileNotFoundError(f"could not read image {path}")
        self._bgr   = bgr
        self._left  = count
        self._seq   = 0
        self.height, self.width = bgr.shape[:2]
        self.fps    = 30.0
        self.fourcc = "still"
        self.device = str(path)

    def read(self) -> Frame | None:
        """Return the image until the requested count is exhausted."""
        if self._left <= 0:
            return None
        self._left -= 1
        self._seq  += 1
        return Frame(bgr=self._bgr, t_s=time.monotonic(), seq=self._seq - 1)

    def release(self) -> None:
        """Nothing to release."""


# --- helpers ---


def _fourcc_name(code: int) -> str:
    """Decode an OpenCV FOURCC integer into its four characters."""
    return "".join(chr((code >> (8 * i)) & 0xFF) for i in range(4))


def open_source(
    camera: str | None,
    input_path: str | None,
    width:  int,
    height: int,
    fps:    int,
    loop:   bool = False,
    threaded: bool = True,
    fixed_framerate: bool = True,
) -> VideoSource:
    """Pick a source: `input_path` wins over `camera` when both are given.

    Live cameras are wrapped in `AsyncCamera` unless `threaded` is off; file
    sources never are, because a bench over a file should measure decode cost
    rather than race the decoder.
    """
    if input_path is not None:
        path = Path(input_path)
        if path.suffix.lower() in (".png", ".jpg", ".jpeg", ".bmp", ".webp"):
            return StillImage(path, count=10**9 if loop else 1)
        return VideoFile(path, loop=loop)
    cam = Camera(camera or "/dev/video0", width, height, fps, fixed_framerate=fixed_framerate)
    return AsyncCamera(cam) if threaded else cam
