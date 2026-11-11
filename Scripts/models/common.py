# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Shared helpers for the model-prep scripts. Paths, pinned sources, hashing, manifest writing, cosine checks.

Every script reads the same pinned revisions from SOURCES so the manifest records exactly what was converted.
"""
from __future__ import annotations

import hashlib
import json
import os
import sys
from pathlib import Path

LOCALAPPDATA = Path(os.environ.get("LOCALAPPDATA", str(Path.home() / "AppData" / "Local")))
HOME = Path(os.environ.get("BETTERRECALL_HOME", str(LOCALAPPDATA / "BetterRecall")))
MODELS = Path(os.environ.get("BETTERRECALL_MODELS", str(HOME / "models")))
HF_HOME = Path(os.environ.get("HF_HOME", str(HOME / "cache" / "hf")))
os.environ["HF_HOME"] = str(HF_HOME)
os.environ.setdefault("HF_HUB_DISABLE_TELEMETRY", "1")

# Official publishers only. Revisions pinned to the commits checked on 2026-11-10.
SOURCES = {
    "embeddinggemma-2": {
        "repo": "google/embeddinggemma-2", "revision": "f9972ac22e696a105c482202646979e33e4b5a1e",
        "licence": "Apache-2.0", "approx_mb": 1530,
        "allow": ["*.json", "*.jinja", "tokenizer.model", "model.safetensors", "1_Pooling/*", "2_Normalize/*"],
    },
    "siglip2-base": {
        "repo": "google/siglip2-base-patch16-224", "revision": "75de2d55ec2d0b4efc50b3e9ad70dba96a7b2fa2",
        "licence": "Apache-2.0", "approx_mb": 1540,
        "allow": ["*.json", "tokenizer.model", "model.safetensors"],
    },
    "whisper-large-v3-turbo": {
        "repo": "openai/whisper-large-v3-turbo", "revision": "41f01f3fe87f28c78e2fbf8b568835947dd65ed9",
        "licence": "MIT", "approx_mb": 1620,
        "allow": ["*.json", "*.txt", "model.safetensors"],
    },
    "nsfw-classifier": {
        "repo": "Falconsai/nsfw_image_detection", "revision": "96cb0d0342c7afb80cab76ecc58b265fa44da256",
        "licence": "Apache-2.0", "approx_mb": 345,
        # optimizer.pt, pytorch_model.bin and the YOLO .pt are not needed.
        "allow": ["config.json", "preprocessor_config.json", "labels.json", "model.safetensors", "README.md"],
    },
    "nsfw-detector": {
        # NudeNet 320n (YOLOv8n) as ONNX, Apache-2.0 export of notAI-tech/NudeNet. Used as is (no conversion).
        "repo": "deepghs/nudenet_onnx", "revision": "44956a0587943016350c7815f9e0dec089d45cae",
        "licence": "Apache-2.0", "approx_mb": 12,
        "allow": ["320n.onnx"],
        "file": "320n.onnx", "size": 12150158,
        "sha256": "c15d8273adad2d0a92f014cc69ab2d6c311a06777a55545f2c4eb46f51911f0f",
    },
    # OCR (PaddleOCR, official ONNX exports, Apache-2.0; pinned 2026-11-11). Each repo: inference.onnx + inference.yml
    # (pre/post-processing and, for recognisers, the character dictionary). Used as is; export_ocr.py writes dict files.
    "ocr": {
        "licence": "Apache-2.0", "approx_mb": 100,
        "det": {"repo": "PaddlePaddle/PP-OCRv6_small_det_onnx", "revision": "28fe5895c24fd108c19eb3e8479f4ab385fbfc62",
                "size": 9880512, "sha256": "d73e0058b7a8086bbd57f3d10b8bcd4ff95363f67e06e2762b5e814fe9c9410e"},
        "rec": {
            "main": {"repo": "PaddlePaddle/PP-OCRv6_small_rec_onnx", "revision": "b8f84f0b80c529de40b4fbb3544b84fa7233a513",
                     "size": 21159378, "sha256": "5435fd747c9e0efe15a96d0b378d5bd157e9492ed8fd80edf08f30d02fa24634"},
            "korean": {"repo": "PaddlePaddle/korean_PP-OCRv5_mobile_rec_onnx", "revision": "5c6f574b8e2230adf4287b33e736d71b9fabd28e",
                       "size": 13418787, "sha256": "92f0b7785e64fc9090106a241cf4c1eb97472824558272751b88a2a4476d3a08"},
            "cyrillic": {"repo": "PaddlePaddle/cyrillic_PP-OCRv5_mobile_rec_onnx", "revision": "2cef88145434beb8afa9dd82d77d799eb1ad7b29",
                         "size": 8048799, "sha256": "5371ee1ddaa7983cc62d0818d99e982b6804638c85e4f960d59a574094e172e5"},
            "arabic": {"repo": "PaddlePaddle/arabic_PP-OCRv5_mobile_rec_onnx", "revision": "14aaedcd75825982689ecf5cd64ab33ee083215a",
                       "size": 7998947, "sha256": "799113ebf267fbe742deb99eb36e8d42c9ddc5291ceacf92add41b4d52a59110"},
            "devanagari": {"repo": "PaddlePaddle/devanagari_PP-OCRv5_mobile_rec_onnx", "revision": "251aec19e36739540d35e2cc943f6aa7503b98e5",
                           "size": 7912311, "sha256": "cb789212ce96c69d3e74728ae4309d179281d68cb3945d0616b67cafab41c986"},
            "th": {"repo": "PaddlePaddle/th_PP-OCRv5_mobile_rec_onnx", "revision": "1d4adbbafb1034a2fd6618498575b81ea7b69f69",
                   "size": 7891015, "sha256": "27618be66018f8598ac0a526a593f9f1cebf794e7eded93428e8fb016e537f5f"},
            "el": {"repo": "PaddlePaddle/el_PP-OCRv5_mobile_rec_onnx", "revision": "8152b89d2ee0e1d4c92decab75fe75e5e3d836b6",
                   "size": 7808735, "sha256": "2acf17fcaea2bc81b878e311e6263b8885f48bb03796f75f9f30ed3242bbaa6d"},
            "ta": {"repo": "PaddlePaddle/ta_PP-OCRv5_mobile_rec_onnx", "revision": "0db89798af0d5218fad9ea7ace0557526e9077f1",
                   "size": 7885691, "sha256": "c6d2b682d2a0ea4cb1fccdba295976f93fd439964d16cdc666cadef531accbee"},
            "te": {"repo": "PaddlePaddle/te_PP-OCRv5_mobile_rec_onnx", "revision": "f1197c83430241c58c64b0952b3a80366d04eeba",
                   "size": 7898759, "sha256": "8238bfc46d4cffe720ed6706e3842802467343497428693ff2bfb4e6b3caa36b"},
        },
    },
    # Names and addresses for redaction: mmBERT-small token classifier, 41 PII labels, 22 languages, MIT (pinned 2026-11-11).
    "ner": {
        "repo": "Wismut/nym-pii-multilingual-small", "revision": "4348999cd3c2e20c49615e9af7c6bbb45b64cd85",
        "licence": "MIT", "approx_mb": 151,
        "files": {
            "int8/model_int8.onnx": {"size": 138730982, "sha256": "139006aea2cbd8e709d322f056232570de54661f624143be4893aaa387190286"},
            "int8/tokenizer.json": {"size": 12389891, "sha256": "c299144e68dfec1dc536204a7ae3712710c5c6cade9269a83f5042250d47d8de"},
            "int8/config.json": {},
        },
    },
    "silero-vad": {
        "url": "https://github.com/snakers4/silero-vad/raw/v6.2.3/src/silero_vad/data/silero_vad.onnx",
        "repo": "snakers4/silero-vad", "revision": "v6.2.3", "licence": "MIT", "approx_mb": 2.3,
    },
}

GENAI_RUNTIME = {
    "url": "https://storage.openvinotoolkit.org/repositories/openvino_genai/packages/2026.4.1/windows/openvino_genai_windows_2026.4.1.0_x86_64.zip",
    "sha256_url": "https://storage.openvinotoolkit.org/repositories/openvino_genai/packages/2026.4.1/windows/openvino_genai_windows_2026.4.1.0_x86_64.zip.sha256",
    "licence": "Apache-2.0", "approx_mb": 254,
}


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with open(path, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


def snapshot(name: str) -> Path:
    """Local path of the pinned Hugging Face snapshot (downloads on first call into HF_HOME)."""
    from huggingface_hub import snapshot_download
    s = SOURCES[name]
    return Path(snapshot_download(s["repo"], revision=s["revision"], allow_patterns=s.get("allow")))


def file_entry(model_dir: Path, rel: str, source: str | None = None) -> dict:
    p = model_dir / rel
    e = {"file": rel.replace("\\", "/"), "size": p.stat().st_size, "sha256": sha256(p)}
    if source:
        e["source"] = source
    return e


def write_manifest(model_dir: Path, manifest: dict, files: list[str]) -> Path:
    """manifest.json in the C# ModelManifest shape (camelCase), with size and sha256 of every file."""
    s = SOURCES[manifest["name"]]
    if "repo" in s:
        src = f"https://huggingface.co/{s['repo']}/tree/{s['revision']}" if "url" not in s else s["url"]
        manifest.setdefault("id", s["repo"])
        manifest.setdefault("revision", s["revision"])
        manifest.setdefault("source", src)
    else:  # several upstream repos (ocr): the caller sets id, revision and source; per-file sources via fileSources
        src = manifest["source"]
    manifest.setdefault("licence", s["licence"])
    per_file = manifest.pop("fileSources", {})
    manifest["files"] = [file_entry(model_dir, f, per_file.get(f, src)) for f in files]
    out = model_dir / "manifest.json"
    out.write_text(json.dumps(manifest, indent=2, ensure_ascii=False), encoding="utf-8")
    print(f"wrote {out}")
    return out


def cosine(a, b) -> float:
    import numpy as np
    a = np.asarray(a, dtype=np.float64).ravel()
    b = np.asarray(b, dtype=np.float64).ravel()
    if not (np.isfinite(a).all() and np.isfinite(b).all()):
        return float("nan")
    return float(a @ b / (np.linalg.norm(a) * np.linalg.norm(b)))


def check(name: str, cos: float, minimum: float = 0.999) -> bool:
    ok = cos == cos and cos >= minimum
    print(f"  {'OK  ' if ok else 'FAIL'} {name}: cosine {cos:.6f} (min {minimum})")
    return ok


def bench_image(width: int = 1280, height: int = 720):
    """Same deterministic test frame as RealEngine.BenchImage in the worker."""
    import numpy as np
    from PIL import Image
    y, x = np.mgrid[0:height, 0:width]
    grid = (x % 64 == 0) | (y % 48 == 0)
    block = ((x // 160 + y // 120) % 3) == 0
    r = np.where(grid, 30, np.where(block, 220, x * 255 // width))
    g = np.where(grid, 30, np.where(block, 230, y * 255 // height))
    b = np.where(grid, 30, np.where(block, 240, (x + y) % 256))
    return Image.fromarray(np.stack([r, g, b], -1).astype("uint8"), "RGB")


def die(msg: str) -> None:
    print("error:", msg, file=sys.stderr)
    sys.exit(1)
