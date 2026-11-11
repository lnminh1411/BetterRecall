# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""NSFW detector: NudeNet 320n (YOLOv8n; ONNX by deepghs/nudenet_onnx, Apache-2.0, of notAI-tech/NudeNet) into
<models>\\nsfw-detector\\model.onnx, plus manifest.json with a detector section.

No conversion: the pinned file (revision, size and SHA-256 in common.SOURCES["nsfw-detector"]) is copied as is after
its size and hash are checked. The graph has a dynamic input "images" [batch,3,H,W] and output "output0"
[batch, 22, anchors]: rows 0-3 box (cx, cy, w, h), rows 4-21 sigmoid scores of 18 labels. The worker runs it at 640 px
on the whole frame (padded to a square at the bottom/right with black), after the classifier passed the frame, and
blocks when any "suggestive" block label reaches privacy.nsfwDetectorThreshold. An offline evaluation picked that
setup: classifier on the whole frame OR this detector at 640 px.

Usage: <main venv>\\Scripts\\python.exe Scripts/models/export_nsfw_detector.py [--from <320n.onnx>] [--out <dir>]
  --from  a local copy of 320n.onnx (checked against the pinned hash) instead of the Hugging Face cache/download
  --out   default <models>\\nsfw-detector (main venv, made by prepare_all.py)
"""
from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

from common import MODELS, SOURCES, die, sha256, write_manifest

NAME = "nsfw-detector"
SIZE = 640
LABELS = [
    "FEMALE_GENITALIA_COVERED", "FACE_FEMALE", "BUTTOCKS_EXPOSED", "FEMALE_BREAST_EXPOSED", "FEMALE_GENITALIA_EXPOSED",
    "MALE_BREAST_EXPOSED", "ANUS_EXPOSED", "FEET_EXPOSED", "BELLY_COVERED", "FEET_COVERED", "ARMPITS_COVERED",
    "ARMPITS_EXPOSED", "FACE_MALE", "BELLY_EXPOSED", "MALE_GENITALIA_EXPOSED", "ANUS_COVERED", "FEMALE_BREAST_COVERED",
    "BUTTOCKS_COVERED",
]
# Exposed parts plus covered breasts, genitals, buttocks and anus ("suggestive").
BLOCK = [
    "BUTTOCKS_EXPOSED", "FEMALE_BREAST_EXPOSED", "FEMALE_GENITALIA_EXPOSED", "MALE_GENITALIA_EXPOSED", "ANUS_EXPOSED",
    "FEMALE_BREAST_COVERED", "FEMALE_GENITALIA_COVERED", "BUTTOCKS_COVERED", "ANUS_COVERED",
]


def source_file(local: Path | None) -> Path:
    s = SOURCES[NAME]
    if local is not None:
        return local
    from huggingface_hub import hf_hub_download
    return Path(hf_hub_download(s["repo"], s["file"], revision=s["revision"]))


def verify(path: Path) -> None:
    s = SOURCES[NAME]
    if not path.is_file():
        die(f"{path} not found")
    size = path.stat().st_size
    if size != s["size"]:
        die(f"{path}: {size} bytes, expected {s['size']}")
    digest = sha256(path)
    if digest != s["sha256"]:
        die(f"{path}: sha256 {digest}, expected {s['sha256']}")
    print(f"verified {path} ({size} bytes, sha256 {digest[:12]})")


def smoke(model: Path) -> dict:
    """One run on a zero frame: output rows must be 4 + labels. Skipped when onnxruntime is missing."""
    try:
        import numpy as np
        import onnxruntime as ort
    except ImportError:
        print("onnxruntime not installed: shape check skipped")
        return {}
    so = ort.SessionOptions()
    so.intra_op_num_threads = 2
    sess = ort.InferenceSession(str(model), so, providers=["CPUExecutionProvider"])
    inp, out = sess.get_inputs()[0], sess.get_outputs()[0]
    y = sess.run([out.name], {inp.name: np.zeros((1, 3, SIZE, SIZE), np.float32)})[0]
    if inp.name != "images" or out.name != "output0" or y.ndim != 3 or y.shape[1] != 4 + len(LABELS):
        die(f"unexpected graph: input {inp.name} {inp.shape}, output {out.name} {list(y.shape)}")
    block_rows = [4 + LABELS.index(b) for b in BLOCK]
    report = {"input": inp.name, "output": out.name, "output_shape": list(y.shape),
              "zero_frame_block_score": round(float(y[0, block_rows].max()), 4)}
    print(json.dumps(report))
    return report


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--from", dest="src", type=Path, help="local 320n.onnx")
    ap.add_argument("--out", type=Path, default=MODELS / NAME)
    args = ap.parse_args()

    src = source_file(args.src)
    verify(src)
    out: Path = args.out
    out.mkdir(parents=True, exist_ok=True)
    dst = out / "model.onnx"
    shutil.copyfile(src, dst)
    verify(dst)
    report = smoke(dst)
    (out / "export_report.json").write_text(json.dumps(report, indent=1), encoding="utf-8")
    s = SOURCES[NAME]
    write_manifest(out, {
        "name": NAME, "dim": 0,
        "source": f"https://huggingface.co/{s['repo']}/resolve/{s['revision']}/{s['file']}",
        "detector": {"file": "model.onnx", "input": "images", "output": "output0", "size": SIZE,
                     "labels": LABELS, "blockLabels": BLOCK},
    }, ["model.onnx"])


if __name__ == "__main__":
    main()
