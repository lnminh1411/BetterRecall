# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Checks the NSFW gate on real images: the FP32 PyTorch reference against the INT8 ONNX model the worker uses.

Usage (main venv): python Scripts/models/nsfw_check.py [--threshold 0.8] [--csv out.csv] DIR_OR_IMAGE [...]
Prints per image: FP32 score, INT8 score, gap, and whether each would block at the threshold; then a summary
(label agreement, max/mean gap, blocked counts). Preprocessing follows the installed manifest.json, the same
values the worker reads. Put known-safe and known-NSFW samples in separate folders to read precision and recall.
"""
import argparse
import csv
import json
import sys
import time
from pathlib import Path

import numpy as np

sys.path.insert(0, str(Path(__file__).resolve().parent))
import common  # noqa: E402

EXTS = {".jpg", ".jpeg", ".png", ".webp", ".bmp"}


def images(paths):
    for p in map(Path, paths):
        if p.is_dir():
            yield from sorted(q for q in p.rglob("*") if q.suffix.lower() in EXTS)
        elif p.suffix.lower() in EXTS:
            yield p


def preprocess(img, spec):
    from PIL import Image
    pp = spec["preprocess"]
    resample = Image.BILINEAR if pp.get("resample", "bilinear") == "bilinear" else Image.BICUBIC
    a = np.asarray(img.convert("RGB").resize((spec["width"], spec["height"]), resample), dtype=np.float32)
    a = a * pp["rescale"]
    a = (a - np.array(pp["mean"], np.float32)) / np.array(pp["std"], np.float32)
    return a.transpose(2, 0, 1)[None].astype(np.float32)


def softmax_block(logits, labels, block):
    e = np.exp(logits - logits.max())
    p = e / e.sum()
    return float(sum(p[labels.index(b)] for b in block))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("paths", nargs="+")
    ap.add_argument("--threshold", type=float, default=0.8)
    ap.add_argument("--csv", default="")
    ap.add_argument("--model-dir", default=str(common.MODELS / "nsfw-classifier"))
    a = ap.parse_args()

    from PIL import Image
    import onnxruntime as ort
    import torch
    from transformers import AutoModelForImageClassification

    mdir = Path(a.model_dir)
    manifest = json.loads((mdir / "manifest.json").read_text(encoding="utf-8"))
    spec = manifest["classifier"]
    labels, block = spec["labels"], spec["blockLabels"]

    sess = ort.InferenceSession(str(mdir / spec["file"]), providers=["CPUExecutionProvider"])
    ref = AutoModelForImageClassification.from_pretrained(
        manifest["id"], revision=manifest["revision"], torch_dtype=torch.float32).eval()

    rows = []
    for p in images(a.paths):
        try:
            x = preprocess(Image.open(p), spec)
        except Exception as e:  # unreadable file
            print(f"skip {p}: {e}")
            continue
        t0 = time.perf_counter()
        s8 = softmax_block(sess.run([spec["output"]], {spec["input"]: x})[0][0], labels, block)
        t8 = (time.perf_counter() - t0) * 1000
        with torch.no_grad():
            s32 = softmax_block(ref(pixel_values=torch.from_numpy(x)).logits[0].numpy(), labels, block)
        rows.append((str(p), s32, s8, abs(s32 - s8), s32 >= a.threshold, s8 >= a.threshold, t8))
        print(f"{s32:6.3f} {s8:6.3f} gap {abs(s32 - s8):5.3f} {'BLOCK' if s8 >= a.threshold else 'pass ':5} {p}")

    if not rows:
        sys.exit("no images")
    gaps = [r[3] for r in rows]
    agree = sum(r[4] == r[5] for r in rows)
    print(f"\n{len(rows)} images, threshold {a.threshold}: FP32 blocks {sum(r[4] for r in rows)}, "
          f"INT8 blocks {sum(r[5] for r in rows)}, decision agreement {agree}/{len(rows)}, "
          f"gap max {max(gaps):.3f} mean {np.mean(gaps):.3f}, INT8 {np.median([r[6] for r in rows]):.0f} ms/image")
    if a.csv:
        with open(a.csv, "w", newline="", encoding="utf-8") as f:
            w = csv.writer(f)
            w.writerow(["image", "fp32", "int8", "gap", "fp32_block", "int8_block", "int8_ms"])
            w.writerows(rows)


if __name__ == "__main__":
    main()
