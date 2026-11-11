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
    src = f"https://huggingface.co/{s['repo']}/tree/{s['revision']}" if "url" not in s else s["url"]
    manifest.setdefault("id", s["repo"])
    manifest.setdefault("revision", s["revision"])
    manifest.setdefault("licence", s["licence"])
    manifest.setdefault("source", src)
    manifest["files"] = [file_entry(model_dir, f, src) for f in files]
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
