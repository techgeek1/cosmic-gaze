#!/usr/bin/env -S uv run --python 3.12 --script
# /// script
# requires-python = ">=3.10,<3.14"
# dependencies = [
#     "ultralytics>=8.4.37",
#     "onnx",
#     "onnxslim",
#     "onnxruntime",
# ]
# ///
"""Fetch and export the two models `gaze-detect` needs into `models/` at the repo root.

Widget detector
    TargetFinder (arXiv 2607.19907), a YOLO26n fine-tuned on 520 annotated desktop
    screenshots with six widget classes. The authors ship PyTorch checkpoints only, so
    this script downloads one and exports it to ONNX with Ultralytics.

    Repo:    https://github.com/ahmedbenakouche/target_finder_toolkit  (MIT)
    PyPI:    target-finder-toolkit
    Dataset: https://osf.io/fr6y4/overview

    Note the weights are an Ultralytics fine-tune and the checkpoint metadata carries
    Ultralytics' own "AGPL-3.0 License" string even though the repo's LICENSE.txt is MIT.
    See README.md.

Text detector
    PP-OCRv5 mobile detection stage, DBNet, no recognition head. Apache-2.0, converted to
    ONNX by the `webnn` org on Hugging Face from PaddlePaddle's official release.

Usage
    uv run --python 3.12 crates/gaze-detect/scripts/fetch_models.py
    uv run --python 3.12 crates/gaze-detect/scripts/fetch_models.py --variant yolo26s-640
    uv run --python 3.12 crates/gaze-detect/scripts/fetch_models.py --no-export

Set UV_TORCH_BACKEND=cpu so uv resolves CPU torch instead of the CUDA wheels; there is no
CUDA on this machine.
"""

import argparse
import hashlib
import shutil
import sys
import urllib.request
from pathlib import Path

# --- Sources ---

# Raw checkpoint URLs in the TargetFinder toolkit repo. The suffix is the training input
# size; the ONNX export must use the same one.
TARGETFINDER_BASE = (
    "https://raw.githubusercontent.com/ahmedbenakouche/target_finder_toolkit"
    "/HEAD/target_finder_toolkit/models"
)

TARGETFINDER_VARIANTS = [
    "yolo26n-640", "yolo26n-1280", "yolo26n-1920",
    "yolo26s-640", "yolo26s-1280", "yolo26s-1920",
    "yolo26m-640", "yolo26m-1280", "yolo26m-1920",
]

# PP-OCRv5 mobile detection stage, already in ONNX.
PPOCR_URL = "https://huggingface.co/webnn/PP-OCRv5-ONNX/resolve/main/ch_PP-OCRv5_det.onnx"
PPOCR_NAME = "ch_PP-OCRv5_det.onnx"


# --- Helpers ---

def repo_root() -> Path:
    """Repo root, two levels up from this script's crate directory."""
    return Path(__file__).resolve().parents[3]


def download(url: str, dest: Path) -> None:
    """Fetch `url` to `dest`, skipping the transfer if the file is already there."""
    if dest.exists():
        print(f"have    {dest.name} ({dest.stat().st_size / 1e6:.1f} MB)")
        return

    dest.parent.mkdir(parents=True, exist_ok=True)
    tmp = dest.with_suffix(dest.suffix + ".part")

    print(f"fetch   {url}")
    with urllib.request.urlopen(url) as src, tmp.open("wb") as out:
        shutil.copyfileobj(src, out)

    tmp.rename(dest)
    digest = hashlib.sha256(dest.read_bytes()).hexdigest()[:16]
    print(f"wrote   {dest} ({dest.stat().st_size / 1e6:.1f} MB, sha256:{digest}...)")


def export_onnx(pt: Path, imgsz: int) -> Path:
    """Export an Ultralytics checkpoint to ONNX beside it, and report its tensor shapes."""
    from ultralytics import YOLO

    model = YOLO(str(pt))
    print(f"classes {model.names}")

    # opset 17 is what onnxruntime 1.29 handles without any fallback kernels. `dynamic` is
    # off because the Rust side always feeds exactly one square tile, and a static shape
    # lets onnxruntime pre-plan its memory.
    out = model.export(
        format="onnx",
        imgsz=imgsz,
        opset=17,
        simplify=True,
        dynamic=False,
        nms=False,
    )

    return Path(out)


def describe(path: Path) -> None:
    """Print a model's input and output tensor names, shapes and dtypes."""
    import onnxruntime as rt

    session = rt.InferenceSession(str(path), providers=["CPUExecutionProvider"])

    print(f"\n{path.name}")
    for t in session.get_inputs():
        print(f"  input   {t.name:12} {t.shape} {t.type}")
    for t in session.get_outputs():
        print(f"  output  {t.name:12} {t.shape} {t.type}")


# --- Entry point ---

def main() -> int:
    """Download, export and describe both models."""
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--variant",
        default="yolo26n-640",
        choices=TARGETFINDER_VARIANTS,
        help="TargetFinder checkpoint to fetch. n-640 is the paper's best F1 config.",
    )
    parser.add_argument(
        "--models-dir",
        type=Path,
        default=None,
        help="where to put the models (default: <repo>/models)",
    )
    parser.add_argument(
        "--no-export",
        action="store_true",
        help="download only, skip the torch import and the ONNX export",
    )
    args = parser.parse_args()

    models = args.models_dir or (repo_root() / "models")
    models.mkdir(parents=True, exist_ok=True)

    # Widget detector: PyTorch checkpoint, then ONNX.
    pt = models / f"{args.variant}.pt"
    download(f"{TARGETFINDER_BASE}/{args.variant}.pt", pt)

    onnx = pt.with_suffix(".onnx")

    if not args.no_export and not onnx.exists():
        imgsz = int(args.variant.rsplit("-", 1)[1])
        exported = export_onnx(pt, imgsz)

        if exported != onnx:
            exported.replace(onnx)

        print(f"wrote   {onnx}")

    # Text detector: already ONNX.
    download(PPOCR_URL, models / PPOCR_NAME)

    if not args.no_export:
        describe(onnx)
        describe(models / PPOCR_NAME)

    print(f"\nmodels ready in {models}")

    return 0


if __name__ == "__main__":
    sys.exit(main())
