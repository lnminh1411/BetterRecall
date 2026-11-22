# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Writes rust/crates/br-fetch/models-catalog.json: every file of the converted models on Hugging Face at one pinned
commit, with size and SHA-256, so the installer and the app download only what the catalog names and check each file.

Reads metadata only: the repo tree (sizes, LFS SHA-256) and the small non-LFS files (manifest.json, LICENSE, XML and
JSON configs), which it hashes itself. Downloads no model weights. Needs network access, so run it only with the
maintainer's go-ahead, when the converted models on Hugging Face change.

Usage: python model_catalog.py [REVISION]   (default: the repo's current head commit)
"""
import hashlib
import json
import sys
import urllib.request
from pathlib import Path

REPO = "lnminh1411/BetterRecall-models"
API = f"https://huggingface.co/api/models/{REPO}"
OUT = Path(__file__).resolve().parents[2] / "rust" / "crates" / "br-fetch" / "models-catalog.json"
UA = {"User-Agent": "br-catalog/1"}

# Which components each install mode downloads (docs/rust-port.md, R6).
MINIMAL = ["embeddinggemma-2", "nsfw-classifier", "nsfw-detector", "ocr", "ner"]
FULL_ONLY = ["siglip2-base", "whisper-large-v3-turbo", "silero-vad"]


def get(url: str) -> bytes:
    with urllib.request.urlopen(urllib.request.Request(url, headers=UA), timeout=60) as r:
        return r.read()


def main(revision: str | None) -> None:
    if not revision:
        revision = json.loads(get(API))["sha"]
    tree = json.loads(get(f"{API}/tree/{revision}?recursive=true"))
    base = f"https://huggingface.co/{REPO}/resolve/{revision}/"
    components = []
    for comp in MINIMAL + FULL_ONLY:
        files = []
        for e in sorted((e for e in tree if e["type"] == "file" and e["path"].startswith(comp + "/")), key=lambda e: e["path"]):
            rel = e["path"].split("/", 1)[1]
            if "lfs" in e:
                size, sha = e["lfs"]["size"], e["lfs"]["oid"]
            else:
                data = get(base + e["path"])
                if len(data) != e["size"]:
                    raise SystemExit(f"{e['path']}: size {len(data)} != tree {e['size']}")
                size, sha = len(data), hashlib.sha256(data).hexdigest()
            files.append({"path": rel, "size": size, "sha256": sha})
        if not any(f["path"] == "manifest.json" for f in files):
            raise SystemExit(f"{comp}: no manifest.json at {revision}")
        sets = ["full", "minimal"] if comp in MINIMAL else ["full"]
        components.append({"id": comp, "sets": sets, "bytes": sum(f["size"] for f in files), "files": files})
    catalog = {"repo": REPO, "revision": revision, "baseUrl": base, "components": components}
    OUT.parent.mkdir(parents=True, exist_ok=True)
    OUT.write_text(json.dumps(catalog, indent=2) + "\n", encoding="utf-8", newline="\n")
    for c in components:
        print(f"{c['id']:24} {len(c['files']):3} files {c['bytes'] / 1e9:6.3f} GB  {'/'.join(c['sets'])}")
    full = sum(c["bytes"] for c in components)
    minimal = sum(c["bytes"] for c in components if "minimal" in c["sets"])
    print(f"full {full / 1e9:.2f} GB, minimal {minimal / 1e9:.2f} GB -> {OUT}")


if __name__ == "__main__":
    main(sys.argv[1] if len(sys.argv) > 1 else None)
