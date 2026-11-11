# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""SigLIP 2 base (google/siglip2-base-patch16-224) -> OpenVINO IR: the NPU fallback when the EmbeddingGemma 2 vision
tower fails there. Image and text towers, static shapes, 768-d pooled embeddings, plus manifest.json.

  vision.xml   pixel_values [1,3,224,224] (bilinear, /255, mean 0.5, std 0.5) -> embedding [1,768]
  text.xml     input_ids [1,64] (padded to 64 with the pad id, no attention mask, as SigLIP was trained) -> [1,768]

Checks OpenVINO CPU and --devices (NPU) against PyTorch float32 by cosine, with compile time and latency.
Note: SigLIP 2 has no Matryoshka training; the worker still truncates to index.dim when that is below 768.
"""
from __future__ import annotations

import argparse
import json
import shutil
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn

from common import MODELS, bench_image, check, cosine, snapshot, write_manifest

SEQ = 64


class Vision(nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, pixel_values):
        out = self.m.get_image_features(pixel_values=pixel_values)
        out = out.pooler_output if hasattr(out, "pooler_output") else out
        return torch.nn.functional.normalize(out, dim=-1)


class Text(nn.Module):
    def __init__(self, m):
        super().__init__()
        self.m = m

    def forward(self, input_ids):
        out = self.m.get_text_features(input_ids=input_ids)
        out = out.pooler_output if hasattr(out, "pooler_output") else out
        return torch.nn.functional.normalize(out, dim=-1)


def name_inputs(model, names):
    """Leave exactly one tensor name per input (the worker reads ov_port_get_any_name)."""
    for port, name in zip(model.inputs, names):
        port.get_tensor().set_names({name})


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=MODELS / "siglip2-base")
    ap.add_argument("--devices", nargs="*", default=["NPU"])
    ap.add_argument("--no-save", action="store_true")
    args = ap.parse_args()

    import openvino as ov
    from PIL import Image
    from transformers import AutoModel, AutoProcessor

    src = snapshot("siglip2-base")
    model = AutoModel.from_pretrained(src, torch_dtype=torch.float32).eval()
    proc = AutoProcessor.from_pretrained(src)
    ip = proc.image_processor
    size = (ip.size["height"], ip.size["width"])
    mean, std = list(ip.image_mean), list(ip.image_std)
    resample = {2: "bilinear", 3: "bicubic"}.get(int(ip.resample), "bilinear")

    def pixels(img):
        r = img.convert("RGB").resize((size[1], size[0]), Image.BILINEAR if resample == "bilinear" else Image.BICUBIC)
        a = np.asarray(r, np.float32) / 255.0
        return ((a - mean) / std).transpose(2, 0, 1)[None].astype(np.float32)

    tok = proc.tokenizer
    import sentencepiece as spm
    sp = spm.SentencePieceProcessor(model_file=str(Path(src) / "tokenizer.model"))
    probe = "a photo of a cat"
    hf = tok([probe.lower()], padding="max_length", max_length=SEQ, truncation=True)["input_ids"][0]
    body = sp.encode(probe.lower())
    add_bos = hf[:1] == [sp.bos_id()]
    add_eos = (sp.eos_id() in hf)
    pad = tok.pad_token_id

    def ids(text):
        t = ([sp.bos_id()] if add_bos else []) + sp.encode(text.lower())[: SEQ - 2] + ([sp.eos_id()] if add_eos else [])
        return np.array([t + [pad] * (SEQ - len(t))], np.int64)

    if ids(probe)[0].tolist() != hf:
        print("warning: sentencepiece ids differ from the HF tokenizer:", ids(probe)[0][:12].tolist(), hf[:12])

    vis, txt = Vision(model).eval(), Text(model).eval()
    images = [bench_image(), bench_image(1024, 768)]
    texts = ["quarterly sales figures", "a terminal window with a build error", "Tabelle mit Umsatzzahlen"]
    with torch.no_grad():
        ref_i = [vis(torch.from_numpy(pixels(i)))[0].numpy() for i in images]
        ref_t = [txt(torch.from_numpy(ids(t)))[0].numpy() for t in texts]
        # official processor path for the image, to measure the preprocessing gap
        off = vis(proc(images=images[0], return_tensors="pt")["pixel_values"])[0].numpy()
    report = {"preprocess_vs_official_cos": cosine(ref_i[0], off), "devices": {}}
    vov = ov.convert_model(vis, example_input=(torch.from_numpy(pixels(images[0])),), input=[("pixel_values", [1, 3, *size], ov.Type.f32)])
    tov = ov.convert_model(txt, example_input=(torch.from_numpy(ids(texts[0])),), input=[("input_ids", [1, SEQ], ov.Type.i64)])
    vov.outputs[0].get_tensor().set_names({"embedding"})
    tov.outputs[0].get_tensor().set_names({"embedding"})
    name_inputs(vov, ["pixel_values"])
    name_inputs(tov, ["input_ids"])

    core = ov.Core()
    ok = True
    for dev in ["CPU"] + [d for d in args.devices if d != "CPU"]:
        if dev not in core.available_devices:
            continue
        r = {}
        try:
            props = {"PERFORMANCE_HINT": "LATENCY"} | ({"NPU_TURBO": "NO"} if dev == "NPU" else {"INFERENCE_PRECISION_HINT": "f32"})
            for name, g, feed, refs in (("vision", vov, [pixels(i) for i in images], ref_i), ("text", tov, [ids(t) for t in texts], ref_t)):
                t = time.perf_counter()
                cm = core.compile_model(g, dev, props)
                r[f"{name}_compile_ms"] = round((time.perf_counter() - t) * 1000)
                req = cm.create_infer_request()
                coss, ts = [], []
                for x, ref in zip(feed, refs):
                    req.infer([x])
                    t = time.perf_counter()
                    req.infer([x])
                    ts.append((time.perf_counter() - t) * 1000)
                    out = req.get_output_tensor(0).data[0]
                    coss.append(cosine(out, ref))
                    ok &= check(f"{dev} {name}", coss[-1])
                r[f"{name}_cos_min"] = round(min(coss), 6)
                r[f"{name}_ms"] = round(float(np.median(ts)), 2)
        except Exception as e:  # noqa: BLE001
            r["error"] = f"{type(e).__name__}: {str(e).splitlines()[0][:300]}"
        report["devices"][dev] = r
    print(json.dumps(report, indent=1))
    if args.no_save:
        return
    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    ov.save_model(vov, out / "vision.xml", compress_to_fp16=True)
    ov.save_model(tov, out / "text.xml", compress_to_fp16=True)
    shutil.copy2(Path(src) / "tokenizer.model", out / "tokenizer.model")
    (out / "export_report.json").write_text(json.dumps(report, indent=1), encoding="utf-8")
    write_manifest(out, {
        "name": "siglip2-base", "dim": 768,
        "tokenizer": {"file": "tokenizer.model", "addBos": add_bos, "addEos": add_eos, "bosId": sp.bos_id(), "eosId": sp.eos_id(),
                      "padId": pad, "lowercase": True, "padToMax": True},
        "prompts": {"query": "{text}", "document": "{text}"},
        "text": [{"file": "text.xml", "seq": SEQ, "inputIds": "input_ids", "attentionMask": None, "output": "embedding"}],
        "vision": [{"tokens": 196, "file": "vision.xml", "input": "pixel_values", "output": "embedding", "height": size[0], "width": size[1],
                    "preprocess": {"resample": resample, "rescale": 1 / 255, "mean": mean, "std": std, "fit": "stretch"}}],
    }, ["tokenizer.model", "vision.xml", "vision.bin", "text.xml", "text.bin"])
    if not ok:
        print("warning: some checks failed; see export_report.json")


if __name__ == "__main__":
    main()
