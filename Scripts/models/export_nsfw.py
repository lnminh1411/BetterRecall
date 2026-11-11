# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""NSFW gate: Falconsai/nsfw_image_detection (ViT-base/16, Apache-2.0, labels normal/nsfw) -> ONNX FP32 -> INT8
(ONNX Runtime dynamic quantisation: per-channel INT8 weights, float activations), plus manifest.json.

Writes two graphs: model_int8.onnx (manifest classifier.file) for the CPU, and model_fp32.onnx
(classifier.acceleratorFile) for the NPU and GPUs, which do not run the dynamic INT8 ops (MatMulInteger,
DynamicQuantizeLinear) well. Both have the static input [1,3,224,224].

Usage: <main venv>\Scripts\python.exe Scripts/models/export_nsfw.py [--out <models>\nsfw-classifier] [--eval-dir <folder>]
(main venv, made by prepare_all.py).

Dynamic quantisation needs no calibration data. Static QDQ with generated calibration frames (the first export) moved
NSFW scores by up to 0.16 on real screenshots and photos, and calibrating on real images was worse (0.40); dynamic
stays within 0.03 of FP32 and runs faster. The script reports FP32 vs INT8 agreement on generated frames and on
--eval-dir if given. Accuracy on real NSFW content is not measured here (no such data is downloaded); use
Scripts/models/nsfw_check.py on your own labelled folders for that.
"""
from __future__ import annotations

import argparse
import json
import time
from pathlib import Path

import numpy as np
import torch

from common import MODELS, snapshot, write_manifest


def synthetic_images(n: int, seed: int):
    from PIL import Image, ImageDraw
    rng = np.random.default_rng(seed)
    for i in range(n):
        w, h = int(rng.integers(320, 1280)), int(rng.integers(240, 900))
        kind = i % 4
        if kind == 0:  # desktop-like
            img = Image.new("RGB", (w, h), tuple(int(c) for c in rng.integers(180, 255, 3)))
            d = ImageDraw.Draw(img)
            for _ in range(int(rng.integers(3, 12))):
                x0, y0 = int(rng.integers(0, w - 20)), int(rng.integers(0, h - 20))
                d.rectangle([x0, y0, x0 + int(rng.integers(20, w // 2)), y0 + int(rng.integers(10, h // 3))],
                            fill=tuple(int(c) for c in rng.integers(0, 255, 3)))
        elif kind == 1:  # gradient
            y, x = np.mgrid[0:h, 0:w]
            a = np.stack([x * 255 // w, y * 255 // h, (x + y) % 256], -1).astype(np.uint8)
            img = Image.fromarray(a)
        elif kind == 2:  # noise
            img = Image.fromarray(rng.integers(0, 256, (h, w, 3), dtype=np.uint8))
        else:  # skin-tone blobs
            img = Image.new("RGB", (w, h), tuple(int(c) for c in rng.integers(0, 120, 3)))
            d = ImageDraw.Draw(img)
            for _ in range(int(rng.integers(2, 6))):
                cx, cy, r = int(rng.integers(0, w)), int(rng.integers(0, h)), int(rng.integers(30, 250))
                tone = (int(rng.integers(170, 240)), int(rng.integers(120, 190)), int(rng.integers(90, 160)))
                d.ellipse([cx - r, cy - r, cx + r, cy + r], fill=tone)
        yield img


def folder_images(folder: Path):
    from PIL import Image
    for p in sorted(folder.rglob("*")):
        if p.suffix.lower() in {".jpg", ".jpeg", ".png", ".webp", ".bmp"}:
            try:
                yield Image.open(p).convert("RGB")
            except Exception:  # noqa: BLE001
                pass


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=MODELS / "nsfw-classifier")
    ap.add_argument("--eval-dir", type=Path)
    args = ap.parse_args()

    from PIL import Image
    from transformers import AutoImageProcessor, AutoModelForImageClassification
    import onnxruntime as ort
    from onnxruntime.quantization import QuantType, quantize_dynamic
    from onnxruntime.quantization.shape_inference import quant_pre_process

    src = snapshot("nsfw-classifier")
    model = AutoModelForImageClassification.from_pretrained(src, torch_dtype=torch.float32).eval()
    proc = AutoImageProcessor.from_pretrained(src)
    size = (proc.size["height"], proc.size["width"])
    mean, std = list(proc.image_mean), list(proc.image_std)
    resample = {2: "bilinear", 3: "bicubic"}.get(int(proc.resample), "bilinear")
    labels = [model.config.id2label[i] for i in range(len(model.config.id2label))]
    block = [l for l in labels if l.lower() in ("nsfw", "porn", "hentai", "sexy")]

    def pixels(img):
        r = img.convert("RGB").resize((size[1], size[0]), Image.BILINEAR if resample == "bilinear" else Image.BICUBIC)
        a = np.asarray(r, np.float32) / 255.0
        return ((a - mean) / std).transpose(2, 0, 1)[None].astype(np.float32)

    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    fp32 = out / "model_fp32.onnx"

    class Wrap(torch.nn.Module):
        def __init__(self, m):
            super().__init__()
            self.m = m

        def forward(self, pixel_values):
            return self.m(pixel_values=pixel_values).logits

    torch.onnx.export(Wrap(model), (torch.zeros(1, 3, *size),), str(fp32), input_names=["pixel_values"],
                      output_names=["logits"], opset_version=17, dynamo=False)
    pre = out / "model_pre.onnx"
    quant_pre_process(str(fp32), str(pre))

    int8 = out / "model_int8.onnx"
    quantize_dynamic(str(pre), str(int8), weight_type=QuantType.QInt8, per_channel=True)
    pre.unlink(missing_ok=True)

    so = ort.SessionOptions()
    s32 = ort.InferenceSession(str(fp32), so, providers=["CPUExecutionProvider"])
    s8 = ort.InferenceSession(str(int8), so, providers=["CPUExecutionProvider"])
    bi = [labels.index(b) for b in block]

    def prob(s, x):
        z = s.run(None, {"pixel_values": x})[0][0].astype(np.float64)
        e = np.exp(z - z.max())
        return float(e[bi].sum() / e.sum())

    evals = list(synthetic_images(100, 99)) + (list(folder_images(args.eval_dir)) if args.eval_dir else [])
    gaps, agree, t32, t8 = [], 0, [], []
    for img in evals:
        x = pixels(img)
        t = time.perf_counter(); p32 = prob(s32, x); t32.append(time.perf_counter() - t)
        t = time.perf_counter(); p8 = prob(s8, x); t8.append(time.perf_counter() - t)
        gaps.append(abs(p32 - p8))
        agree += (p32 >= 0.5) == (p8 >= 0.5)
    with torch.no_grad():
        x = pixels(evals[0])
        tp = torch.softmax(model(pixel_values=torch.from_numpy(x)).logits[0].double(), -1)[bi].sum().item()
    report = {
        "labels": labels, "block": block, "eval_images": len(evals), "label_agreement": agree / len(evals),
        "max_prob_gap": round(max(gaps), 4), "mean_prob_gap": round(float(np.mean(gaps)), 4),
        "fp32_vs_torch_gap": round(abs(prob(s32, x) - tp), 6),
        "cpu_ms_fp32": round(float(np.median(t32)) * 1000, 1), "cpu_ms_int8": round(float(np.median(t8)) * 1000, 1),
        "quantisation": "dynamic, per-channel INT8 weights",
    }
    print(json.dumps(report, indent=1))
    (out / "export_report.json").write_text(json.dumps(report, indent=1), encoding="utf-8")
    write_manifest(out, {
        "name": "nsfw-classifier", "dim": 0,
        "classifier": {"file": "model_int8.onnx", "acceleratorFile": "model_fp32.onnx", "input": "pixel_values", "output": "logits",
                       "height": size[0], "width": size[1],
                       "preprocess": {"resample": resample, "rescale": 1 / 255, "mean": mean, "std": std, "fit": "stretch"},
                       "labels": labels, "blockLabels": block},
    }, ["model_int8.onnx", "model_fp32.onnx"])


if __name__ == "__main__":
    main()
