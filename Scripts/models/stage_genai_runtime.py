# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Stages the OpenVINO GenAI C runtime (openvino_genai_c.dll and its OpenVINO Tokenizers dependencies) for Whisper.

Downloads Intel's official archive (about 254 MB, Apache-2.0) into %BETTERRECALL_DEV%\\cache (or --cache), checks
its published SHA-256, and copies only the GenAI / tokenizer DLLs into <repo>\\artifacts\\native\\openvino-genai\\.
The worker csproj picks that folder up at build and publish time. The OpenVINO core DLLs come from the
OpenVINO.runtime.win NuGet package (same 2026.4.1 build), so they are not copied.
"""
from __future__ import annotations

import argparse
import os
import hashlib
import shutil
import urllib.request
import zipfile
from pathlib import Path

from common import GENAI_RUNTIME

REPO = Path(__file__).resolve().parents[2]
KEEP = ("openvino_genai", "openvino_tokenizers", "core_tokenizers", "icudt", "icuuc")


def main():
    ap = argparse.ArgumentParser()
    dev = Path(os.environ.get("BETTERRECALL_DEV") or Path(os.environ["LOCALAPPDATA"]) / "BetterRecall-dev")
    ap.add_argument("--cache", type=Path, default=dev / "cache")
    ap.add_argument("--dest", type=Path, default=REPO / "artifacts" / "native" / "openvino-genai")
    args = ap.parse_args()
    args.cache.mkdir(parents=True, exist_ok=True)
    zpath = args.cache / Path(GENAI_RUNTIME["url"]).name
    if not zpath.exists():
        print("download", GENAI_RUNTIME["url"])
        urllib.request.urlretrieve(GENAI_RUNTIME["url"], zpath)
    want = urllib.request.urlopen(GENAI_RUNTIME["sha256_url"]).read().decode().split()[0].lower()
    h = hashlib.sha256()
    with open(zpath, "rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    if h.hexdigest() != want:
        zpath.unlink()
        raise SystemExit(f"sha256 mismatch: {h.hexdigest()} != {want}")
    args.dest.mkdir(parents=True, exist_ok=True)
    n = 0
    with zipfile.ZipFile(zpath) as z:
        for info in z.infolist():
            name = Path(info.filename).name
            if name.lower().endswith(".dll") and "release" in info.filename.lower() and name.lower().startswith(KEEP):
                with z.open(info) as src, open(args.dest / name, "wb") as dst:
                    shutil.copyfileobj(src, dst)
                n += 1
                print("  ", name)
    if not (args.dest / "openvino_genai_c.dll").exists():
        raise SystemExit("openvino_genai_c.dll not found in the archive")
    print(f"staged {n} DLLs into {args.dest}")


if __name__ == "__main__":
    main()
