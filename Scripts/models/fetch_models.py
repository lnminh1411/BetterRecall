# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Downloads the official model files (pinned revisions) into HF_HOME, and Silero VAD into the models dir.

Needs the user's go-ahead: see Scripts/models/README.md for every download, its source, size and licence.
Usage: python fetch_models.py [all | embeddinggemma-2 siglip2-base whisper-large-v3-turbo nsfw-classifier nsfw-detector silero-vad]
"""
import sys
import urllib.request

from common import MODELS, SOURCES, sha256, snapshot, write_manifest


def fetch_vad() -> None:
    s = SOURCES["silero-vad"]
    out = MODELS / "silero-vad"
    out.mkdir(parents=True, exist_ok=True)
    dst = out / "silero_vad.onnx"
    print(f"silero-vad <- {s['url']}")
    urllib.request.urlretrieve(s["url"], dst)
    print(f"  {dst} {dst.stat().st_size} bytes sha256 {sha256(dst)}")
    write_manifest(out, {"name": "silero-vad"}, ["silero_vad.onnx"])


def main(names: list[str]) -> None:
    if not names or names == ["all"]:
        names = list(SOURCES)
    for n in names:
        if n not in SOURCES:
            raise SystemExit(f"unknown model {n}; one of {', '.join(SOURCES)}")
        if n == "silero-vad":
            fetch_vad()
        else:
            print(f"{n} <- {SOURCES[n]['repo']}@{SOURCES[n]['revision'][:12]}")
            print("  ", snapshot(n))


if __name__ == "__main__":
    main(sys.argv[1:])
