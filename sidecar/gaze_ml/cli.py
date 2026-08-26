"""`gaze-ml` command line: `serve`, `bench`, `dump-crops`, `fetch-models`."""

from __future__ import annotations

import argparse
import json
import signal
import sys
import time
from pathlib import Path

import cv2
import numpy as np

from gaze_ml import fetch, schema
from gaze_ml.camera import open_source
from gaze_ml.intrinsics import DEFAULT_HFOV_DEG
from gaze_ml.iris import DEFAULT_GAIN_DEG
from gaze_ml.pipeline import MODELS_DIR, Pipeline, PipelineConfig, pick_device
from gaze_ml.server import BroadcastServer
from gaze_ml.stats import StageTimer, StatsReporter

# --- argument plumbing ---


def _add_common(parser: argparse.ArgumentParser) -> None:
    """Options shared by `serve` and `bench`."""
    parser.add_argument("--camera", default="/dev/video0", help="V4L2 device node")
    parser.add_argument("--width", type=int, default=1920, help="requested capture width")
    parser.add_argument("--height", type=int, default=1080, help="requested capture height")
    parser.add_argument("--fps", type=int, default=30, help="requested capture rate")
    parser.add_argument("--intrinsics", type=Path, default=None, help="camera intrinsics JSON")
    parser.add_argument(
        "--hfov-deg", type=float, default=DEFAULT_HFOV_DEG,
        help="horizontal FOV used when --intrinsics is absent",
    )
    parser.add_argument("--models", type=Path, default=MODELS_DIR, help="weights directory")
    parser.add_argument(
        "--device", default="auto", choices=("auto", "cuda", "cpu"),
        help="auto falls back to CPU and says so; cuda fails loudly instead",
    )
    parser.add_argument("--no-fp16", dest="fp16", action="store_false", help="run the gaze net in fp32")
    parser.add_argument("--crop-scale", type=float, default=1.0, help="face box expansion for the gaze crop")
    parser.add_argument("--min-score", type=float, default=0.5, help="face detector threshold")
    parser.add_argument(
        "--estimator", default="l2cs", choices=("l2cs", "iris", "both"),
        help="gaze source; `both` streams the other one alongside as gaze_iris/gaze_l2cs",
    )
    parser.add_argument(
        "--iris-gain", type=float, default=DEFAULT_GAIN_DEG,
        help="degrees of eye rotation per unit of normalised iris offset",
    )
    parser.add_argument("--show", action="store_true", help="open a debug window")
    parser.add_argument(
        "--no-threaded-capture", dest="threaded", action="store_false",
        help="read the camera inline instead of from a drain thread (slower, simpler)",
    )
    parser.add_argument(
        "--no-fixed-framerate", dest="fixed_framerate", action="store_false",
        help="leave exposure_auto_priority alone; brighter frames, 15 fps in dim light",
    )


def build_parser() -> argparse.ArgumentParser:
    """The full CLI, including subcommands."""
    parser = argparse.ArgumentParser(prog="gaze-ml", description=__doc__)
    subs   = parser.add_subparsers(dest="command", required=True)

    serve = subs.add_parser("serve", help="stream gaze records over a Unix socket")
    _add_common(serve)
    serve.add_argument("--socket", type=Path, required=True, help="Unix socket path to bind")
    serve.add_argument("--queue-depth", type=int, default=8, help="per-client backlog before dropping")
    serve.add_argument("--stats-interval", type=float, default=5.0, help="stderr summary period, seconds")
    serve.add_argument("--duration", type=float, default=None, help="exit after this many seconds")

    bench = subs.add_parser("bench", help="time the pipeline over a fixed number of frames")
    _add_common(bench)
    bench.add_argument("--frames", type=int, default=200, help="frames to measure")
    bench.add_argument("--warmup", type=int, default=10, help="frames to discard first")
    bench.add_argument("--input", default=None, help="video or image file instead of the camera")
    bench.add_argument("--json", action="store_true", help="emit the summary as JSON on stdout")

    dump = subs.add_parser("dump-crops", help="save the exact crops fed to the gaze net")
    _add_common(dump)
    dump.add_argument("--frames", type=int, default=12, help="crops to save")
    dump.add_argument("--input", default=None, help="video or image file instead of the camera")
    dump.add_argument("--out", type=Path, default=Path("debug/crops"), help="output directory")
    dump.add_argument(
        "--sweep", default="", help="comma-separated crop scales to dump side by side",
    )
    dump.add_argument("--save-frames", action="store_true", help="also save the raw frames as PNG")

    fetch_cmd = subs.add_parser("fetch-models", help="download model weights")
    fetch_cmd.add_argument("--models", type=Path, default=MODELS_DIR, help="destination directory")
    fetch_cmd.add_argument("--force", action="store_true", help="re-download files that exist")
    return parser


def _config(args: argparse.Namespace, device: str) -> PipelineConfig:
    """Turn parsed arguments into a `PipelineConfig`."""
    return PipelineConfig(
        models_dir = args.models,
        intrinsics = args.intrinsics,
        hfov_deg   = args.hfov_deg,
        device     = device,
        fp16       = args.fp16,
        crop_scale = args.crop_scale,
        min_score  = args.min_score,
        video_mode = getattr(args, "input", None) is None,
        estimator  = args.estimator,
        iris_gain  = args.iris_gain,
    )


def _preflight(args: argparse.Namespace) -> tuple[str, str]:
    """Check weights are present and resolve the compute device."""
    absent = fetch.missing(args.models)
    if getattr(args, "estimator", "l2cs") == "iris":
        absent = [name for name in absent if "l2cs" not in name]
    if absent:
        raise SystemExit(f"missing weights in {args.models}: {absent}\nrun: gaze-ml fetch-models")
    device, name = pick_device(args.device)
    if device != "cuda":
        print(
            "[gaze-ml] WARNING: running on CPU, not the GPU. "
            "Expect several hundred ms per frame.",
            file = sys.stderr,
        )
    return device, name


# --- commands ---


def cmd_serve(args: argparse.Namespace) -> int:
    """Capture, infer, and broadcast one JSON line per frame until stopped."""
    device, device_name = _preflight(args)
    source = open_source(
        args.camera, None, args.width, args.height, args.fps,
        threaded=args.threaded, fixed_framerate=args.fixed_framerate,
    )
    print(
        f"[gaze-ml] {source.device} {source.width}x{source.height} "
        f"{source.fourcc} @{source.fps:g} -> {args.socket}  device={device_name}",
        file = sys.stderr,
    )

    stop = {"now": False}

    def _handle(_sig: int, _frame: object) -> None:
        stop["now"] = True

    signal.signal(signal.SIGINT, _handle)
    signal.signal(signal.SIGTERM, _handle)

    reporter = StatsReporter(device_name, args.stats_interval)
    started  = time.monotonic()
    with (
        source,
        Pipeline(_config(args, device), source.width, source.height) as pipeline,
        BroadcastServer(args.socket, args.queue_depth) as server,
    ):
        while not stop["now"]:
            frame = source.read()
            if frame is None:
                print("[gaze-ml] capture returned no frame; stopping", file=sys.stderr)
                break
            result = pipeline.process(frame)
            lat_ms = (time.monotonic() - frame.t_s) * 1000.0
            record = schema.record(
                t         = result.t,
                seq       = result.seq,
                lat_ms    = lat_ms,
                valid     = result.valid,
                eye_mm    = result.eye_mm,
                gaze      = result.gaze,
                head_rot  = result.head_rot,
                conf      = result.conf,
                gaze_iris = result.gaze_iris if args.estimator == "both" else None,
                gaze_l2cs = result.gaze_l2cs if args.estimator == "both" else None,
            )
            server.broadcast(schema.encode(record))
            reporter.add(lat_ms, result.valid)
            reporter.maybe_report(
                f"clients={server.n_clients} dropped={server.dropped} "
                f"skipped={getattr(source, 'skipped', 0)}"
            )

            if args.show and not _show(frame, result, pipeline):
                break
            if args.duration is not None and time.monotonic() - started >= args.duration:
                break
    if args.show:
        _close_window()
    return 0


def cmd_bench(args: argparse.Namespace) -> int:
    """Run a fixed number of frames and print per-stage timings."""
    device, device_name = _preflight(args)
    source = open_source(
        args.camera, args.input, args.width, args.height, args.fps,
        loop=True, threaded=args.threaded, fixed_framerate=args.fixed_framerate,
    )
    print(
        f"[gaze-ml] bench {source.device} {source.width}x{source.height} "
        f"{source.fourcc} device={device_name} fp16={args.fp16}",
        file = sys.stderr,
    )

    timer   = StageTimer()
    lat     = []
    valid   = 0
    counted = 0
    with source, Pipeline(_config(args, device), source.width, source.height) as pipeline:
        for i in range(args.warmup + args.frames):
            t_read = time.perf_counter()
            frame  = source.read()
            if frame is None:
                break
            read_ms = (time.perf_counter() - t_read) * 1000.0
            result  = pipeline.process(frame)
            if i < args.warmup:
                continue
            if counted == 0:
                t_first = time.perf_counter()
            stages = dict(result.stages)
            stages["capture_ms"] = read_ms
            stages["age_ms"] = (time.monotonic() - frame.t_s) * 1000.0
            stages.setdefault("total_ms", sum(v for k, v in result.stages.items() if k != "total_ms"))
            stages["wall_ms"] = read_ms + stages["total_ms"]
            timer.add(stages)
            lat.append(stages["wall_ms"])
            valid   += int(result.valid)
            counted += 1
            if args.show and not _show(frame, result, pipeline):
                break
    if args.show:
        _close_window()

    if counted == 0:
        print("[gaze-ml] no frames measured", file=sys.stderr)
        return 1

    elapsed = time.perf_counter() - t_first
    fps     = counted / elapsed if elapsed > 0 else 0.0
    skipped = getattr(source, "skipped", 0)
    summary = {
        "device":       device_name,
        "skipped":      skipped,
        "fp16":         bool(args.fp16),
        "source":       source.device,
        "size":         [source.width, source.height],
        "frames":       counted,
        "fps":          fps,
        "valid_frac":   valid / counted,
        "lat_mean_ms":  float(np.mean(lat)),
        "lat_p90_ms":   float(np.percentile(lat, 90)),
        "stages_mean_ms": {k: float(np.mean(v)) for k, v in timer.samples.items()},
    }
    if args.json:
        print(json.dumps(summary, indent=2))
    else:
        print(timer.table())
        print(
            f"\nframes={counted} fps={fps:.1f} valid={valid / counted:.0%} "
            f"lat_mean={summary['lat_mean_ms']:.1f}ms lat_p90={summary['lat_p90_ms']:.1f}ms "
            f"skipped={skipped} device={device_name}"
        )
    return 0


def cmd_dump_crops(args: argparse.Namespace) -> int:
    """Save the network's real input, denormalised, with its decoded angles."""
    from gaze_ml.debug_dump import annotate, tensor_to_bgr, write_frame

    device, device_name = _preflight(args)
    source = open_source(
        args.camera, args.input, args.width, args.height, args.fps,
        loop=True, threaded=args.threaded, fixed_framerate=args.fixed_framerate,
    )
    out_dir = args.out
    out_dir.mkdir(parents=True, exist_ok=True)
    scales = (
        [float(v) for v in args.sweep.split(",")] if args.sweep else [args.crop_scale]
    )

    with source, Pipeline(_config(args, device), source.width, source.height) as pipeline:
        for _ in range(5):          # let exposure and the tracker settle
            source.read()
        saved = 0
        index = 0
        while saved < args.frames:
            frame = source.read()
            if frame is None:
                break
            index += 1
            obs = pipeline.face.detect(frame.bgr, int(frame.t_s * 1000.0))
            if obs is None:
                continue
            det = obs.detection
            for scale in scales:
                box   = det.square(scale)
                batch = pipeline.gaze.preprocess(frame.bgr, box)
                if batch is None:
                    continue
                ang  = pipeline.gaze.infer(batch)
                tag  = f"s{scale:g}"
                name = (
                    f"crop_{saved:02d}_{tag}_yaw{ang.yaw_deg:+07.2f}"
                    f"_pitch{ang.pitch_deg:+07.2f}.png"
                )
                cv2.imwrite(
                    str(out_dir / name),
                    annotate(
                        tensor_to_bgr(batch),
                        [f"yaw {ang.yaw_deg:+.1f}", f"pitch {ang.pitch_deg:+.1f}", f"scale {scale:g}"],
                    ),
                )
                print(
                    f"{name}  box={box} det=({det.x:.0f},{det.y:.0f},{det.w:.0f},{det.h:.0f}) "
                    f"score={det.score:.2f}"
                )
            write_frame(
                out_dir / f"frame_{saved:02d}.png",
                frame.bgr,
                det.square(scales[0]),
                (det.x, det.y, det.w, det.h),
                det.score,
            )
            if args.save_frames:
                cv2.imwrite(str(out_dir / f"raw_{saved:02d}.png"), frame.bgr)
            saved += 1
    print(f"[gaze-ml] wrote {saved} crops to {out_dir}  device={device_name}", file=sys.stderr)
    return 0


def cmd_fetch(args: argparse.Namespace) -> int:
    """Download every model in the manifest."""
    fetch.fetch(args.models, force=args.force)
    return 0


# --- debug window ---


def _show(frame: object, result: object, pipeline: Pipeline) -> bool:
    """Draw one annotated frame. Returns False when the user asks to quit."""
    import cv2

    from gaze_ml.draw import overlay

    cv2.imshow("gaze-ml", overlay(frame.bgr, result, pipeline.intr))
    return cv2.waitKey(1) & 0xFF not in (27, ord("q"))


def _close_window() -> None:
    """Tear down the debug window if one was opened."""
    import cv2

    cv2.destroyAllWindows()


# --- entry point ---


def main(argv: list[str] | None = None) -> int:
    """Parse arguments and dispatch. Returns the process exit code."""
    args = build_parser().parse_args(argv)
    return {
        "serve":        cmd_serve,
        "bench":        cmd_bench,
        "dump-crops":   cmd_dump_crops,
        "fetch-models": cmd_fetch,
    }[args.command](args)


if __name__ == "__main__":
    raise SystemExit(main())
