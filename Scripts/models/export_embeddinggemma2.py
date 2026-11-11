# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""EmbeddingGemma 2 (text + vision towers) -> OpenVINO IR for the BetterRecall worker, plus a manifest.

Layout written to <models>/embeddinggemma-2/:
  tokenizer.model        SentencePiece model from the official repo (copied)
  embed_tokens.bf16      token embedding table [262144, 512], bf16 exactly as published (the worker gathers rows)
  text.xml/.bin          inputs_embeds [1,S,512] + attention_mask [1,S] -> L2-normalised mean-pooled embedding [1,768].
                         S is dynamic; the worker reshapes it to each static length (64/256/512, and the image
                         sequence length) before compiling, so one file serves every NPU shape.
  vision_<N>.xml/.bin    patches [1,N*9,768] + positions [1,N*9,2] + pool_weights [1,N,N*9] -> N soft-token
                         slots [1,N,512], N = 70/140/280. pool_weights is the 3x3 average-pooling matrix,
                         computed by the worker because the NPU cannot build it from runtime positions.
                         Exactly the official processor input: aspect-preserving resize to multiples of 48, /255,
                         16x16 patches, padded to N*9 with position (-1,-1). The first n = (h/48)*(w/48) slots are the
                         image's soft tokens; the worker does resize + patchify and keeps those n. One static graph per
                         budget serves every aspect ratio. The sqrt(768) pooler scale is folded into the following
                         RMSNorm (scale-invariant), which keeps fp16 devices (NPU) from overflowing there.
  manifest.json          what the worker reads (see src/BetterRecall.Worker/Models/ModelManifest.cs)
  export_report.json     cosine vs the PyTorch / sentence-transformers reference, NaN check, compile and run times

Checks: every graph on OpenVINO CPU against the PyTorch float32 reference (cosine >= 0.999), then each --devices
entry (NPU by default) against the same reference, with first-compile time, median latency and NaN check.

Usage (after fetch_models.py):
  python export_embeddinggemma2.py                       # real weights, writes the model folder
  python export_embeddinggemma2.py --random-init --config-dir DIR --no-save
                                                          # architecture check without weights (compile + timing)
"""
from __future__ import annotations

import argparse
import json
import math
import shutil
import time
from pathlib import Path

import numpy as np
import torch
import torch.nn as nn
import torch.nn.functional as F

from common import MODELS, SOURCES, bench_image, check, cosine, sha256, snapshot, write_manifest

PATCH, POOL = 16, 3
TEXT_SEQS = [64, 256, 320, 512]   # the NPU 3720 compiler rejects < 256; the worker drops those there
BUDGETS = [70, 140, 280]
FRAME = (720, 1280)  # height, width of a 16:9 screen frame


def aspect_size(height: int, width: int, max_patches: int) -> tuple[int, int]:
    """transformers.models.gemma4.image_processing_pil_gemma4.get_aspect_ratio_preserving_size."""
    target_px = max_patches * PATCH ** 2
    factor = math.sqrt(target_px / (height * width))
    side = POOL * PATCH
    th = int(math.floor(factor * height / side)) * side
    tw = int(math.floor(factor * width / side)) * side
    if th == 0 or tw == 0:
        raise ValueError("degenerate size")
    return th, tw


def additive_mask(config=None, inputs_embeds=None, attention_mask=None, **kwargs):
    """NPU-friendly replacement for transformers' bidirectional (sliding) mask builders: an additive bias of -1e4 on
    padded keys (finite in fp16, no Select op). Equal to the sliding-window mask while the sequence is no longer than
    the window (512), which every worker length satisfies."""
    m = attention_mask.to(inputs_embeds.dtype if inputs_embeds is not None else torch.float32)
    return ((1.0 - m) * -1e4)[:, None, None, :]


class TraceMode:
    """While converting: eager attention and additive masks. The PyTorch reference keeps the library code path."""

    NAMES = ("create_bidirectional_mask", "create_bidirectional_sliding_window_mask")

    def __init__(self, model):
        self.model = model
        self.saved = []

    def __enter__(self):
        import transformers.models.embedding_gemma2.modeling_embedding_gemma2 as eg2
        import transformers.models.gemma4.modeling_gemma4 as g4
        for mod in (eg2, g4):
            for n in self.NAMES:
                if hasattr(mod, n):
                    self.saved.append((mod, n, getattr(mod, n)))
                    setattr(mod, n, additive_mask)
        self.attn = []
        for c in (self.model.config, getattr(self.model.config, "text_config", None), getattr(self.model.config, "vision_config", None)):
            if c is not None:
                self.attn.append((c, c._attn_implementation))
                c._attn_implementation = "eager"
        return self

    def __exit__(self, *exc):
        for mod, n, f in self.saved:
            setattr(mod, n, f)
        for c, a in self.attn:
            c._attn_implementation = a
        return False


class TextWrapper(nn.Module):
    """inputs_embeds + mask -> masked mean of the projected last hidden state -> L2 normalise (ST Pooling+Normalize)."""

    def __init__(self, lm):
        super().__init__()
        self.lm = lm

    def forward(self, inputs_embeds, attention_mask):
        h = self.lm(inputs_embeds=inputs_embeds, attention_mask=attention_mask).last_hidden_state
        m = attention_mask.unsqueeze(-1).to(h.dtype)
        pooled = (h * m).sum(1) / m.sum(1).clamp(min=1.0)
        return F.normalize(pooled, dim=-1)


class VisionWrapper(nn.Module):
    """[1,M,768] patches + [1,M,2] positions + [1,budget,M] pool_weights (M = budget*9, padding at (-1,-1)) ->
    [1,budget,512] soft-token slots in text space; the first n slots are valid (n = real patches / 9), the rest are zero."""

    def __init__(self, model, budget: int):
        super().__init__()
        self.vt = model.vision_tower
        self.embed = model.embed_vision
        self.budget = budget
        self.max_patches = budget * POOL * POOL
        # Fold pooler's * sqrt(hidden) into embed_vision's scale-free RMSNorm: RMSNorm(c*x, eps) == RMSNorm(x, eps/c^2).
        self.c2 = float(model.config.vision_config.hidden_size)

    def forward(self, pixel_values, position_ids, pool_weights):
        padding = (position_ids == -1).all(dim=-1)
        emb = self.vt.patch_embedder(pixel_values, position_ids, padding)
        out = self.vt.encoder(inputs_embeds=emb, attention_mask=~padding, pixel_position_ids=position_ids).last_hidden_state
        out = out.masked_fill(padding.unsqueeze(-1), 0.0)
        pooled = torch.matmul(pool_weights, out.float())
        x = pooled.float()                       # valid soft tokens come first; no sqrt(hidden) scale (folded below)
        norm = self.embed.embedding_pre_projection_norm
        x = x * torch.rsqrt(x.pow(2).mean(-1, keepdim=True) + norm.eps / self.c2)
        return self.embed.embedding_projection(x)


def reference_vision(model, pv, pos):
    """Official path: Gemma4VisionModel + embed_vision on processor-style patches."""
    with torch.no_grad():
        feats = model.get_image_features(pv, pos, return_dict=True).pooler_output
    return feats[0].unsqueeze(0)


NPU_EXTRA: dict = {}


def ov_props(device: str, cache_dir: str | None) -> dict:
    p = {"PERFORMANCE_HINT": "LATENCY", "PERFORMANCE_HINT_NUM_REQUESTS": "1"}
    if device.startswith("NPU"):
        p.update({"NPU_TURBO": "NO", "WORKLOAD_TYPE": "EFFICIENT"})
        p.update(NPU_EXTRA)
    else:
        p.update({"INFERENCE_PRECISION_HINT": "f32"})
    if cache_dir:
        p["CACHE_DIR"] = cache_dir
    return p


def name_inputs(model, names):
    """Leave exactly one tensor name per input (the worker reads ov_port_get_any_name)."""
    for port, name in zip(model.inputs, names):
        port.get_tensor().set_names({name})


def timed_compile(core, model, device, cache_dir):
    t = time.perf_counter()
    cm = core.compile_model(model, device, ov_props(device, cache_dir))
    return cm, (time.perf_counter() - t) * 1000


def median_run(cm, inputs, n=5):
    req = cm.create_infer_request()
    req.infer(inputs)
    ts = []
    for _ in range(n):
        t = time.perf_counter()
        req.infer(inputs)
        ts.append((time.perf_counter() - t) * 1000)
    return req.get_output_tensor(0).data.copy(), float(np.median(ts))


def patch_inputs(img, budget):
    """What the worker feeds the vision graph (ImagePipeline.ToPatches): aspect-preserving size (multiples of 48, at
    most budget*9 patches), PIL bicubic resize, /255, 16x16 patches in raster order, (x, y) positions, padding to
    budget*9 with zero patches at (-1,-1), and pooling matrix [1,budget,m]. Returns (patches [1,M,768],
    positions [1,M,2], n soft tokens, pool_weights [1,budget,M])."""
    from PIL import Image
    m = budget * POOL * POOL
    h, w = aspect_size(img.height, img.width, m)
    r = img.convert("RGB")
    if (r.width, r.height) != (w, h):
        r = r.resize((w, h), Image.BICUBIC)
    x = (np.asarray(r, dtype=np.float32) / 255.0).transpose(2, 0, 1)          # C,H,W
    nh, nw = h // PATCH, w // PATCH
    pv = x.reshape(3, nh, PATCH, nw, PATCH).transpose(1, 3, 2, 4, 0).reshape(nh * nw, -1)
    gx, gy = np.meshgrid(np.arange(nw), np.arange(nh), indexing="xy")
    pos = np.stack([gx, gy], -1).reshape(nh * nw, 2)
    pv = np.concatenate([pv, np.zeros((m - nh * nw, pv.shape[1]), np.float32)], 0)
    pos = np.concatenate([pos, -np.ones((m - nh * nw, 2), np.int64)], 0).astype(np.int64)
    pool = np.zeros((1, budget, m), dtype=np.float32)
    for py in range(nh):
        for px in range(nw):
            k = (px // POOL) + (nw // POOL) * (py // POOL)
            idx = py * nw + px
            pool[0, k, idx] = 1.0 / (POOL * POOL)
    return pv[None], pos[None], (nh * nw) // (POOL * POOL), pool


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--random-init", action="store_true", help="no weights: random model from --config-dir")
    ap.add_argument("--config-dir", type=Path)
    ap.add_argument("--out", type=Path, default=MODELS / "embeddinggemma-2")
    ap.add_argument("--no-save", action="store_true")
    ap.add_argument("--budgets", type=int, nargs="*", default=BUDGETS)
    ap.add_argument("--seqs", type=int, nargs="*", default=TEXT_SEQS)
    ap.add_argument("--devices", nargs="*", default=["NPU"])
    ap.add_argument("--fp32", action="store_true", help="store IR weights in fp32 instead of fp16")
    ap.add_argument("--cache-dir", type=Path, help="OpenVINO CACHE_DIR for the device checks")
    ap.add_argument("--npu-prop", action="append", default=[], help="extra NPU compile property KEY=VALUE")
    args = ap.parse_args()
    NPU_EXTRA.update(dict(kv.split("=", 1) for kv in args.npu_prop))

    import openvino as ov
    from transformers import AutoConfig
    from transformers.models.embedding_gemma2.modeling_embedding_gemma2 import EmbeddingGemma2Model

    torch.manual_seed(0)
    st = None
    if args.random_init:
        cfg = AutoConfig.from_pretrained(args.config_dir)
        cfg.audio_config = None
        model = EmbeddingGemma2Model(cfg).float().eval()
        src_dir = args.config_dir
    else:
        from sentence_transformers import SentenceTransformer
        src_dir = snapshot("embeddinggemma-2")
        st = SentenceTransformer(str(src_dir), device="cpu", model_kwargs={"torch_dtype": torch.float32},
                                 config_kwargs={"audio_config": None})
        model = st[0].auto_model.float().eval()
    cfg = model.config
    hidden = cfg.text_config.hidden_size
    lm = model.language_model
    table = lm.embed_tokens.weight.detach().float()
    scale = float(lm.embed_tokens.scalar_embed_scale)
    report = {"random_init": args.random_init, "openvino": ov.get_version(), "torch": torch.__version__, "graphs": {}, "devices": {}}
    core = ov.Core()
    if "NPU" in core.available_devices:
        report["npu_driver"] = core.get_property("NPU", "NPU_DRIVER_VERSION")

    # ---- tokenizer and prompt template --------------------------------------------------------------------------
    import sentencepiece as spm
    sp = spm.SentencePieceProcessor(model_file=str(Path(src_dir) / "tokenizer.model"))
    bos, eos, pad = 2, 1, 0
    prompts = {"query": "task: search result | query: {text}", "document": "title: none | text: {text}"}
    add_bos, add_eos = True, True
    if st is not None:
        probe = "title: none | text: hello"
        ids = st.tokenize([probe])["input_ids"][0].tolist()
        body = sp.encode(probe)
        add_bos, add_eos = ids[:1] == [bos], ids[-1:] == [eos]
        if ids != ([bos] if add_bos else []) + body + ([eos] if add_eos else []):
            print("warning: sentence-transformers ids differ from sentencepiece ids:", ids[:12], body[:12])
    report["tokenizer"] = {"add_bos": add_bos, "add_eos": add_eos}

    def text_ids(text, kind):
        return ([bos] if add_bos else []) + sp.encode(prompts[kind].replace("{text}", text)) + ([eos] if add_eos else [])

    def embeds_for(ids, seq):
        ids = ids[:seq]
        n = len(ids)
        e = table[torch.tensor(ids + [pad] * (seq - n))] * scale
        m = torch.zeros(1, seq, dtype=torch.long)
        m[:, :n] = 1
        return e.unsqueeze(0), m

    # ---- text graph ------------------------------------------------------------------------------------------------
    tw = TextWrapper(lm).eval()
    ex_e, _ = embeds_for(text_ids("example text for tracing", "query"), 32)
    ex_m = torch.zeros(1, 32, dtype=torch.long)
    ex_m[:, :12] = 1                       # padded example keeps the mask path in the trace
    if max(args.seqs) > cfg.text_config.sliding_window:
        raise SystemExit(f"text lengths above the sliding window ({cfg.text_config.sliding_window}) need the sliding mask")
    t0 = time.perf_counter()
    with TraceMode(model):
        text_ov = ov.convert_model(tw, example_input=(ex_e, ex_m),
                                   input=[("inputs_embeds", [1, -1, hidden], ov.Type.f32), ("attention_mask", [1, -1], ov.Type.i64)])
    text_ov.outputs[0].get_tensor().set_names({"embedding"})
    name_inputs(text_ov, ["inputs_embeds", "attention_mask"])
    report["graphs"]["text"] = {"convert_s": round(time.perf_counter() - t0, 1)}

    texts = [("quarterly sales figures", "query"), ("Tabelle mit Umsatzzahlen für das dritte Quartal", "document"),
             ("bảng doanh thu theo quý của công ty", "document"), ("東京の天気 и погода в Москве", "query")]

    def torch_text(text, kind):
        if st is not None:
            return st.encode([text], prompt_name=kind, normalize_embeddings=True, convert_to_numpy=True)[0]
        ids = text_ids(text, kind)
        e, m = embeds_for(ids, len(ids))
        with torch.no_grad():
            return tw(e, m)[0].numpy()

    # ---- vision graphs ---------------------------------------------------------------------------------------------
    images = [bench_image()]
    from PIL import Image
    rng = np.random.default_rng(3)
    images.append(Image.fromarray(rng.integers(0, 256, (900, 1600, 3), dtype=np.uint8)))  # noise, 16:9
    images.append(bench_image(1024, 768))                                                    # 4:3 frame
    images.append(bench_image(2560, 1080))                                                   # 21:9 ultrawide
    images.append(bench_image(1080, 1920))                                                   # portrait monitor
    vision = {}
    for n_budget in args.budgets:
        h, w = aspect_size(*FRAME, n_budget * POOL * POOL)
        vw = VisionWrapper(model, n_budget).eval()
        m = vw.max_patches
        pv0, pos0, n0, pool0 = patch_inputs(images[0], n_budget)
        t0 = time.perf_counter()
        with TraceMode(model):
            vov = ov.convert_model(vw, example_input=(torch.from_numpy(pv0), torch.from_numpy(pos0), torch.from_numpy(pool0)),
                                   input=[("pixel_values", [1, m, PATCH * PATCH * 3], ov.Type.f32), ("position_ids", [1, m, 2], ov.Type.i64), ("pool_weights", [1, n_budget, m], ov.Type.f32)])
        vov.outputs[0].get_tensor().set_names({"image_embeds"})
        name_inputs(vov, ["pixel_values", "position_ids", "pool_weights"])
        vision[n_budget] = (vw, vov, h, w)
        report["graphs"][f"vision_{n_budget}"] = {"max_patches": m, "frame_16x9": [h, w], "soft_tokens_16x9": n0, "convert_s": round(time.perf_counter() - t0, 1)}
        print(f"vision {n_budget}: {m} patches, 16:9 frame {w}x{h} -> {n0} soft tokens")

    # Image sequence template: [prefix] + N image tokens + [suffix]
    boi, eoi = cfg.boi_token_id, cfg.eoi_token_id
    prefix, suffix = [bos, boi], [eoi, eos]
    if st is not None:
        try:
            feats = st.preprocess([images[0]]) if hasattr(st, "preprocess") else st[0].preprocess([images[0]])
            ids = feats["input_ids"][0].tolist()
            first = ids.index(cfg.image_token_id)
            last = len(ids) - 1 - ids[::-1].index(cfg.image_token_id)
            prefix, suffix = ids[:first], [i for i in ids[last + 1:] if i != pad]
        except Exception as e:  # noqa: BLE001
            print("warning: could not read the image template from sentence-transformers:", e)
    report["image_template"] = {"prefix": prefix, "suffix": suffix}

    from transformers.models.gemma4.image_processing_pil_gemma4 import Gemma4ImageProcessorPil

    def official_inputs(img, n_budget):
        """The official PIL processor at this budget: patches, positions, soft-token count."""
        f = Gemma4ImageProcessorPil(max_soft_tokens=n_budget)(img, return_tensors="pt")
        return f["pixel_values"].float(), f["image_position_ids"].long(), int(f["num_soft_tokens_per_image"][0])

    def torch_image(img, n_budget):
        """Reference: sentence-transformers end to end at its default budget (280); other budgets: the official
        processor + Gemma4 vision model + the text transformer over [prefix, soft tokens, suffix]."""
        if st is not None and n_budget == 280:
            return st.encode([img], normalize_embeddings=True, convert_to_numpy=True)[0]
        pv, pos, _ = official_inputs(img, n_budget)
        with torch.no_grad():
            soft = reference_vision(model, pv, pos)[0]
            e = torch.cat([table[torch.tensor(prefix)] * scale, soft.float(), table[torch.tensor(suffix)] * scale], 0).unsqueeze(0)
            return tw(e, torch.ones(1, e.shape[1], dtype=torch.long))[0].numpy()

    # Worker-style patches vs the official processor, and wrapper vs the official vision path, for every test image.
    for n_budget, (vw, _, h, w) in vision.items():
        g = report["graphs"][f"vision_{n_budget}"]
        g["patches_max_abs_diff"], g["wrapper_vs_official_cos"] = [], []
        for img in images:
            pv, pos, n, pool = patch_inputs(img, n_budget)
            opv, opos, on = official_inputs(img, n_budget)
            if n != on or not np.array_equal(pos, opos.numpy()):
                raise SystemExit(f"patch layout differs from the official processor at {n_budget} for {img.size}")
            g["patches_max_abs_diff"].append(float(np.abs(pv - opv.numpy()).max()))
            with torch.no_grad():
                mine = vw(torch.from_numpy(pv), torch.from_numpy(pos), torch.from_numpy(pool))[0, :n]
                ref = reference_vision(model, opv, opos)[0]
            g["wrapper_vs_official_cos"].append(cosine(mine.numpy(), ref.numpy()))

    # ---- checks on OpenVINO CPU and the requested devices --------------------------------------------------------
    def run_text(cm_cache, device, seq, text, kind):
        ids = text_ids(text, kind)
        e, m = embeds_for(ids, seq)
        key = (device, seq)
        if key not in cm_cache:
            mdl = text_ov.clone()
            mdl.reshape({"inputs_embeds": [1, seq, hidden], "attention_mask": [1, seq]})
            cm_cache[key] = timed_compile(core, mdl, device, str(args.cache_dir) if args.cache_dir else None)
        cm, compile_ms = cm_cache[key]
        out, ms = median_run(cm, {"inputs_embeds": e.numpy(), "attention_mask": m.numpy()})
        return out[0], ms, compile_ms

    def run_image(cm_cache, device, img, n_budget):
        vw, vov, h, w = vision[n_budget]
        key = (device, "v", n_budget)
        if key not in cm_cache:
            cm_cache[key] = timed_compile(core, vov, device, str(args.cache_dir) if args.cache_dir else None)
        cmv, vcompile = cm_cache[key]
        pv, pos, n, pool = patch_inputs(img, n_budget)
        soft, vms = median_run(cmv, {"pixel_values": pv, "position_ids": pos, "pool_weights": pool})
        soft = soft[:, :n]
        n_real = len(prefix) + n + len(suffix)
        usable = [x for x in sorted(args.seqs) if x >= n_real and x not in dropped.get(device, set())]
        seq = usable[0] if usable else n_real            # same rule as Embedder.FusionSeq in the worker
        pad_e = (table[torch.tensor([pad] * (seq - n_real))] * scale).numpy() if seq > n_real else np.zeros((0, hidden), np.float32)
        e = np.concatenate([(table[torch.tensor(prefix)] * scale).numpy(), soft[0], (table[torch.tensor(suffix)] * scale).numpy(), pad_e], 0)[None]
        mask = np.zeros((1, seq), np.int64)
        mask[:, :n_real] = 1
        key2 = (device, seq)
        if key2 not in cm_cache:
            mdl = text_ov.clone()
            mdl.reshape({"inputs_embeds": [1, seq, hidden], "attention_mask": [1, seq]})
            cm_cache[key2] = timed_compile(core, mdl, device, str(args.cache_dir) if args.cache_dir else None)
        cmt, tcompile = cm_cache[key2]
        out, tms = median_run(cmt, {"inputs_embeds": e.astype(np.float32), "attention_mask": mask})
        return out[0], vms + tms, vcompile + tcompile, bool(np.isfinite(soft).all())

    def err(e):
        return f"{type(e).__name__}: {str(e).splitlines()[0][:300]}"

    dropped: dict[str, set] = {}

    all_ok = True
    for device in ["CPU"] + [d for d in args.devices if d != "CPU"]:
        if device not in core.available_devices:
            report["devices"][device] = {"error": "not present"}
            continue
        cache, dev = {}, {"text": [], "vision_only": {}, "image": {}}
        dropped.setdefault(device, set())
        # 1. text tower per length; on an accelerator a length that does not compile is dropped (as the worker does)
        for text, kind in texts:
            n_ids = len(text_ids(text, kind))
            for seq in [x for x in sorted(args.seqs) if x >= n_ids and x not in dropped[device]]:
                try:
                    out, ms, cms = run_text(cache, device, seq, text, kind)
                    c = cosine(out, torch_text(text, kind))
                    dev["text"].append({"seq": seq, "cos": round(c, 6), "ms": round(ms, 2), "first_compile_ms": round(cms), "nan": bool(not np.isfinite(out).all())})
                    all_ok &= check(f"{device} text[{seq}] {kind}", c)
                    break
                except Exception as e:  # noqa: BLE001
                    print(f"  FAIL {device} text[{seq}]: {err(e)}")
                    if device == "CPU":
                        dev["text"].append({"seq": seq, "error": err(e)})
                        all_ok = False
                        break
                    dropped[device].add(seq)
        dev["dropped_lengths"] = sorted(dropped[device])
        for seq in args.seqs:
            if (device, seq) in cache:
                dev.setdefault("text_compile_ms", {})[seq] = round(cache[(device, seq)][1])
        # 2. vision tower alone: compile, latency, NaN, cosine of the soft tokens against PyTorch
        for n_budget, (vw, vov, h, w) in vision.items():
            r = {}
            try:
                key = (device, "v", n_budget)
                cache[key] = timed_compile(core, vov, device, str(args.cache_dir) if args.cache_dir else None)
                pv, pos, n, pool = patch_inputs(images[0], n_budget)
                soft, ms = median_run(cache[key][0], {"pixel_values": pv, "position_ids": pos, "pool_weights": pool})
                soft = soft[:, :n]
                with torch.no_grad():
                    ref = vw(torch.from_numpy(pv), torch.from_numpy(pos), torch.from_numpy(pool))[:, :n].numpy()
                r = {"first_compile_ms": round(cache[key][1]), "ms": round(ms, 1), "nan": bool(not np.isfinite(soft).all()),
                     "soft_tokens_cos": round(cosine(soft, ref), 6)}
                all_ok &= check(f"{device} vision[{n_budget}] soft tokens", r["soft_tokens_cos"])
            except Exception as e:  # noqa: BLE001
                r = {"error": err(e)}
                print(f"  FAIL {device} vision[{n_budget}]: {err(e)}")
                all_ok = False if device == "CPU" else all_ok
            dev["vision_only"][n_budget] = r
        # 3. full image path (vision + text transformer)
        for n_budget in vision:
            if "error" in dev["vision_only"][n_budget]:
                continue
            rows = []
            for i, img in enumerate(images):
                try:
                    out, ms, cms, soft_ok = run_image(cache, device, img, n_budget)
                    c = cosine(out, torch_image(img, n_budget))
                    rows.append({"image": i, "size": list(img.size), "cos": round(c, 6), "ms": round(ms, 1), "first_compile_ms": round(cms), "nan": bool(not np.isfinite(out).all() or not soft_ok)})
                    all_ok &= check(f"{device} image[{n_budget}] #{i}", c)
                except Exception as e:  # noqa: BLE001
                    rows.append({"image": i, "error": err(e)})
                    print(f"  FAIL {device} image[{n_budget}]: {err(e)}")
                    break
            dev["image"][n_budget] = rows
        report["devices"][device] = dev

    print(json.dumps(report, indent=1, default=str)[:6000])
    if args.no_save:
        return
    cpu = report["devices"].get("CPU", {})
    if not cpu or any("error" in t for t in cpu.get("text", [])) or any("error" in v for v in cpu.get("vision_only", {}).values()):
        raise SystemExit("CPU check failed; nothing written")

    out = args.out
    out.mkdir(parents=True, exist_ok=True)
    compress = not args.fp32
    ov.save_model(text_ov, out / "text.xml", compress_to_fp16=compress)
    for n_budget, (_, vov, _, _) in vision.items():
        ov.save_model(vov, out / f"vision_{n_budget}.xml", compress_to_fp16=compress)
    shutil.copy2(Path(src_dir) / "tokenizer.model", out / "tokenizer.model")
    bf = table.numpy().astype(np.float32).view(np.uint32)
    (bf >> 16).astype(np.uint16).tofile(out / "embed_tokens.bf16")      # exact: weights are bf16 upstream
    (out / "export_report.json").write_text(json.dumps(report, indent=1, default=str), encoding="utf-8")

    files = ["tokenizer.model", "embed_tokens.bf16", "text.xml", "text.bin"]
    vis = []
    for n_budget, (vw, _, h, w) in vision.items():
        files += [f"vision_{n_budget}.xml", f"vision_{n_budget}.bin"]
        vis.append({
            "tokens": n_budget, "file": f"vision_{n_budget}.xml", "input": "pixel_values", "positionsInput": "position_ids",
            "poolInput": "pool_weights",
            "output": "image_embeds", "patchSize": PATCH, "pool": POOL, "maxPatches": vw.max_patches,
            # Matches the official PIL processor: aspect-preserving bicubic resize, /255, no normalise.
            "preprocess": {"resample": "bicubic", "rescale": 1 / 255, "mean": [0, 0, 0], "std": [1, 1, 1], "fit": "aspect"},
            "fusion": {"file": "text.xml", "inputsEmbeds": "inputs_embeds", "attentionMaskInput": "attention_mask",
                       "prefixIds": prefix, "suffixIds": suffix, "output": "embedding"},
        })
    write_manifest(out, {
        "name": "embeddinggemma-2", "dim": 768,
        "tokenizer": {"file": "tokenizer.model", "addBos": add_bos, "addEos": add_eos, "bosId": bos, "eosId": eos, "padId": pad},
        "embeddings": {"file": "embed_tokens.bf16", "rows": int(table.shape[0]), "dim": hidden, "dType": "bf16", "scale": scale},
        "prompts": prompts,
        "text": [{"file": "text.xml", "seq": s, "inputsEmbeds": "inputs_embeds", "attentionMask": "attention_mask", "output": "embedding"} for s in sorted(args.seqs)],
        "vision": vis,
    }, files)
    if not all_ok:
        print("warning: some checks failed; see export_report.json")


if __name__ == "__main__":
    main()
