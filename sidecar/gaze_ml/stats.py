"""Rolling throughput/latency accounting for the periodic stderr summary."""

from __future__ import annotations

import sys
import time
from dataclasses import dataclass, field

import numpy as np

# --- types ---


@dataclass
class Window:
    """Counters for one reporting interval."""

    frames:     int         = 0
    valid:      int         = 0
    latencies:  list[float] = field(default_factory=list)
    t_start:    float       = field(default_factory=time.monotonic)

    def add(self, lat_ms: float, valid: bool) -> None:
        """Record one processed frame."""
        self.frames += 1
        self.valid  += int(valid)
        self.latencies.append(lat_ms)


class StatsReporter:
    """Prints a one-line summary to stderr every `interval_s` seconds."""

    def __init__(self, device_name: str, interval_s: float = 5.0) -> None:
        """`device_name` is echoed on every line so a CPU fallback stays visible."""
        self.device_name = device_name
        self.interval_s  = float(interval_s)
        self._window     = Window()

    def add(self, lat_ms: float, valid: bool) -> None:
        """Record one frame; emits a line when the interval has elapsed."""
        self._window.add(lat_ms, valid)

    def maybe_report(self, extra: str = "") -> str | None:
        """Emit and reset if the interval elapsed. Returns the line, or `None`."""
        now     = time.monotonic()
        elapsed = now - self._window.t_start
        if elapsed < self.interval_s or self._window.frames == 0:
            return None
        line = self.format(elapsed, extra)
        print(line, file=sys.stderr, flush=True)
        self._window = Window(t_start=now)
        return line

    def format(self, elapsed: float, extra: str = "") -> str:
        """Render the summary for the current window."""
        w    = self._window
        lat  = np.asarray(w.latencies, dtype=np.float64)
        fps  = w.frames / elapsed if elapsed > 0 else 0.0
        frac = w.valid / w.frames if w.frames else 0.0
        return (
            f"[gaze-ml] fps={fps:5.1f} "
            f"lat_mean={lat.mean():6.1f}ms lat_p90={np.percentile(lat, 90):6.1f}ms "
            f"valid={frac:4.0%} dev={self.device_name}"
            + (f" {extra}" if extra else "")
        )


# --- stage timing, for `bench` ---


class StageTimer:
    """Accumulates per-stage millisecond samples and prints a summary table."""

    def __init__(self) -> None:
        """Start empty; stages are created on first sight."""
        self.samples: dict[str, list[float]] = {}

    def add(self, stages: dict[str, float]) -> None:
        """Record one frame's stage timings."""
        for name, ms in stages.items():
            self.samples.setdefault(name, []).append(ms)

    def table(self) -> str:
        """A fixed-width mean/p50/p90/max table over everything recorded."""
        rows = ["stage          n     mean     p50     p90     max"]
        for name in sorted(self.samples):
            arr = np.asarray(self.samples[name], dtype=np.float64)
            rows.append(
                f"{name:<12} {len(arr):>4} {arr.mean():>8.2f} "
                f"{np.percentile(arr, 50):>7.2f} {np.percentile(arr, 90):>7.2f} {arr.max():>7.2f}"
            )
        return "\n".join(rows)
