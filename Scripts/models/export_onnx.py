# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""ONNX graphs of the embedders for ONNX Runtime (DirectML graphics cards, and the CPU on PCs without OpenVINO).

The OpenVINO IR export stays as it is; this adds next to it:
  embeddinggemma-2/text.onnx     inputs_embeds [1,S,512] + attention_mask [1,S] -> embedding [1,768], S dynamic
  embeddinggemma-2/vision.onnx   pixel_values [1,M,768] + position_ids [1,M,2] + pool_weights [1,N,M] -> image_embeds
                                 [1,N,512], M and N dynamic: one graph serves every token budget (70/140/280)
  siglip2-base/text.onnx         input_ids [1,64] -> embedding [1,768]
  siglip2-base/vision.onnx       pixel_values [1,3,224,224] -> embedding [1,768]
and an "onnx" map in manifest.json (IR file -> ONNX file) that the worker uses when the graph runs on ONNX Runtime.
Weights stay float32 (float16 gives NaN in EmbeddingGemma 2 on GPUs).

Checks: ONNX Runtime CPU against the PyTorch float32 wrapper, cosine >= 0.9999, every budget, several aspect ratios.

--src: model folder with the IR export (default the models folder). --out: where to write; default in place. With
--out elsewhere, the other files are hard-linked (same volume) or copied so the folder is complete for the worker.
--affinity MASK: run on those logical CPUs only (E-cores), at below-normal priority.
"""
from __future__ import annotations

import argparse
import json
import os
import shutil
import time
from pathlib import Path

import numpy as np
import torch

from common import MODELS, bench_image, cosine, sha256, snapshot

MIN_COS = 0.9999


def pin(mask: int | None) -> None:
    import psutil
    p = psutil.Process()
    p.nice(psutil.BELOW_NORMAL_PRIORITY_CLASS)
    if mask:
        cpus = [i for i in range(os.cpu_count() or 1) if mask >> i & 1]
        p.cpu_affinity(cpus)
        torch.set_num_threads(len(cpus))
        print(f"pinned to CPUs {cpus}, below-normal priority")


def stage(src: Path, out: Path) -> None:
    """Complete model folder at out: hard links (or copies) of src's files except manifest.json."""
    out.mkdir(parents=True, exist_ok=True)
    for f in src.iterdir():
        if f.is_file() and f.name != "manifest.json" and not (out / f.name).exists():
            try:
                os.link(f, out / f.name)
            except OSError:
                shutil.copy2(f, out / f.name)


def ort_session(path: Path, threads: int):
    import onnxruntime as ort
    o = ort.SessionOptions()
    o.intra_op_num_threads = threads
    o.inter_op_num_threads = 1
    return ort.InferenceSession(str(path), o, providers=["CPUExecutionProvider"])


def export(module, args, path: Path, names, outputs, dynamic) -> float:
    t = time.perf_counter()
    torch.onnx.export(module, args, str(path), input_names=names, output_names=outputs, dynamic_axes=dynamic,
                      opset_version=17, do_constant_folding=True, dynamo=False)
    return round(time.perf_counter() - t, 1)


def check(report: dict, name: str, c: float) -> bool:
    ok = c == c and c >= MIN_COS
    print(f"  {'OK  ' if ok else 'FAIL'} {name}: cosine {c:.6f}")
    report.setdefault("checks", []).append({"name": name, "cos": round(c, 6), "ok": ok})
    return ok


def gemma(src: Path, out: Path, threads: int) -> tuple[dict, list[str], bool]:
    import export_embeddinggemma2 as eg
    from sentence_transformers import SentenceTransformer
    snap = snapshot("embeddinggemma-2")
    st = SentenceTransformer(str(snap), device="cpu", model_kwargs={"torch_dtype": torch.float32}, config_kwargs={"audio_config": None})
    model = st[0].auto_model.float().eval()
    lm = model.language_model
    hidden = model.config.text_config.hidden_size
    table = lm.embed_tokens.weight.detach().float()
    scale = float(lm.embed_tokens.scalar_embed_scale)
    report, ok = {}, True

    tw = eg.TextWrapper(lm).eval()
    rng = np.random.default_rng(5)
    ids = rng.integers(3, table.shape[0], 40)

    def embeds(n, seq):
        e = (table[torch.tensor(list(ids[:n]) + [0] * (seq - n))] * scale).unsqueeze(0)
        m = torch.zeros(1, seq, dtype=torch.long)
        m[:, :n] = 1
        return e, m

    ex_e, ex_m = embeds(12, 32)
    with torch.no_grad(), eg.TraceMode(model):
        report["text_export_s"] = export(tw, (ex_e, ex_m), out / "text.onnx", ["inputs_embeds", "attention_mask"], ["embedding"],
                                         {"inputs_embeds": {1: "seq"}, "attention_mask": {1: "seq"}})
    s = ort_session(out / "text.onnx", threads)
    for n, seq in ((10, 64), (40, 64), (30, 256), (40, 512)):
        e, m = embeds(n, seq)
        with torch.no_grad():
            ref = tw(e, m)[0].numpy()
        got = s.run(None, {"inputs_embeds": e.numpy(), "attention_mask": m.numpy()})[0][0]
        ok &= check(report, f"text n={n} seq={seq}", cosine(got, ref))
    del s

    vw = eg.VisionWrapper(model, 280).eval()
    images = [bench_image(), bench_image(1024, 768), bench_image(1080, 1920),
              __import__("PIL.Image", fromlist=["Image"]).fromarray(rng.integers(0, 256, (900, 1600, 3), dtype=np.uint8))]
    pv0, pos0, _, pool0 = eg.patch_inputs(images[0], 70)
    with torch.no_grad(), eg.TraceMode(model):
        report["vision_export_s"] = export(vw, (torch.from_numpy(pv0), torch.from_numpy(pos0), torch.from_numpy(pool0)), out / "vision.onnx",
                                           ["pixel_values", "position_ids", "pool_weights"], ["image_embeds"],
                                           {"pixel_values": {1: "patches"}, "position_ids": {1: "patches"}, "pool_weights": {1: "tokens", 2: "patches"}})
    s = ort_session(out / "vision.onnx", threads)
    for budget in eg.BUDGETS:
        for i, img in enumerate(images):
            pv, pos, n, pool = eg.patch_inputs(img, budget)
            with torch.no_grad(), eg.TraceMode(model):
                ref = vw(torch.from_numpy(pv), torch.from_numpy(pos), torch.from_numpy(pool))[0, :n].numpy()
            got = s.run(None, {"pixel_values": pv, "position_ids": pos, "pool_weights": pool})[0]
            if got.shape[1] != budget:
                print(f"  FAIL vision[{budget}] #{i}: output shape {got.shape}")
                ok = False
                continue
            ok &= check(report, f"vision[{budget}] #{i} {img.size}", cosine(got[0, :n], ref))
    del s
    onnx_map = {"text.xml": "text.onnx"} | {f"vision_{b}.xml": "vision.onnx" for b in eg.BUDGETS}
    return {"onnx": onnx_map, "report": report}, ["text.onnx", "vision.onnx"], ok


def siglip(src: Path, out: Path, threads: int) -> tuple[dict, list[str], bool]:
    import export_siglip2 as sg
    from transformers import AutoModel
    snap = snapshot("siglip2-base")
    model = AutoModel.from_pretrained(snap, torch_dtype=torch.float32).eval()
    vis, txt = sg.Vision(model).eval(), sg.Text(model).eval()
    report, ok = {}, True
    rng = np.random.default_rng(7)
    px = [rng.standard_normal((1, 3, 224, 224)).astype(np.float32) for _ in range(2)]
    tk = [np.array([[2] + list(rng.integers(3, 30000, 20)) + [1] + [0] * 42]), np.array([list(rng.integers(3, 30000, 64))])]
    with torch.no_grad():
        report["vision_export_s"] = export(vis, (torch.from_numpy(px[0]),), out / "vision.onnx", ["pixel_values"], ["embedding"], None)
        report["text_export_s"] = export(txt, (torch.from_numpy(tk[0]),), out / "text.onnx", ["input_ids"], ["embedding"], None)
    for name, mod, feeds, key in (("vision", vis, px, "pixel_values"), ("text", txt, tk, "input_ids")):
        s = ort_session(out / f"{name}.onnx", threads)
        for i, x in enumerate(feeds):
            with torch.no_grad():
                ref = mod(torch.from_numpy(x))[0].numpy()
            ok &= check(report, f"{name} #{i}", cosine(s.run(None, {key: x})[0][0], ref))
        del s
    return {"onnx": {"text.xml": "text.onnx", "vision.xml": "vision.onnx"}, "report": report}, ["text.onnx", "vision.onnx"], ok


def update_manifest(src: Path, out: Path, onnx_map: dict, new_files: list[str]) -> None:
    m = json.loads((src / "manifest.json").read_text(encoding="utf-8"))
    m["onnx"] = onnx_map
    source = m.get("source")
    files = [f for f in m["files"] if f["file"] not in new_files]
    for f in new_files:
        p = out / f
        files.append({"file": f, "size": p.stat().st_size, "sha256": sha256(p), "source": source})
    m["files"] = files
    (out / "manifest.json").write_text(json.dumps(m, indent=2, ensure_ascii=False), encoding="utf-8")
    print(f"wrote {out / 'manifest.json'}")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--models", nargs="*", default=["embeddinggemma-2", "siglip2-base"])
    ap.add_argument("--src", type=Path, default=MODELS)
    ap.add_argument("--out", type=Path, help="models root to write to (default: --src, in place)")
    ap.add_argument("--affinity", type=lambda s: int(s, 0))
    args = ap.parse_args()
    pin(args.affinity)
    threads = torch.get_num_threads()
    all_ok = True
    for name in args.models:
        src, out = args.src / name, (args.out or args.src) / name
        if not (src / "manifest.json").exists():
            raise SystemExit(f"{src} has no manifest.json; run the IR export first")
        if out != src:
            stage(src, out)
        print(f"== {name} -> {out}")
        res, files, ok = (gemma if name == "embeddinggemma-2" else siglip)(src, out, threads)
        (out / "export_onnx_report.json").write_text(json.dumps(res["report"], indent=1), encoding="utf-8")
        if not ok:
            print(f"{name}: checks failed; manifest not changed")
            all_ok = False
            continue
        update_manifest(src, out, res["onnx"], files)
    raise SystemExit(0 if all_ok else 1)


if __name__ == "__main__":
    main()
