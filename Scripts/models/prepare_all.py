# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""One command for the whole model pipeline: venvs, downloads, exports, GenAI runtime, worker rebuild.

Usage: python Scripts/models/prepare_all.py [--skip-venv] [--only embeddinggemma2,siglip2,whisper,nsfw,nsfw-detector]
Venvs live in %BETTERRECALL_DEV% (default %LOCALAPPDATA%\\BetterRecall-dev), pip cache in its pip folder. Models and
the Hugging Face cache go to %LOCALAPPDATA%\\BetterRecall\\. See Scripts/models/README.md for what each step downloads.
"""
import argparse
import os
import subprocess
import sys
from pathlib import Path

HERE = Path(__file__).resolve().parent
ROOT = HERE.parent.parent
LOCAL = Path(os.environ["LOCALAPPDATA"]) / "BetterRecall"
# Venvs and pip cache (large, rebuildable): BETTERRECALL_DEV, default %LOCALAPPDATA%\BetterRecall-dev.
VENV_DIR = Path(os.environ.get("BETTERRECALL_DEV") or Path(os.environ["LOCALAPPDATA"]) / "BetterRecall-dev")

MAIN_PKGS = ["transformers==5.22.2", "sentence-transformers==6.2.0", "openvino==2026.4.1", "openvino-tokenizers==2026.4.1.0",
             "onnx", "onnxruntime", "onnxscript", "pillow", "huggingface_hub", "safetensors", "accelerate", "sentencepiece", "protobuf"]
WHISPER_PKGS = ["optimum-intel[openvino]==2.2.0", "openvino==2026.4.1", "openvino-tokenizers==2026.4.1.0", "openvino-genai==2026.4.1.0", "nncf"]
TORCH_INDEX = "https://download.pytorch.org/whl/cpu"

EXPORTS = {  # name -> (script, venv)
    "embeddinggemma2": ("export_embeddinggemma2.py", "venv"),
    "siglip2": ("export_siglip2.py", "venv"),
    "whisper": ("export_whisper.py", "venv-whisper"),
    "nsfw": ("export_nsfw.py", "venv"),
    "nsfw-detector": ("export_nsfw_detector.py", "venv"),
}


def run(cmd, env):
    print(">", " ".join(str(c) for c in cmd), flush=True)
    if subprocess.run([str(c) for c in cmd], cwd=ROOT, env=env).returncode != 0:
        sys.exit(f"step failed: {' '.join(str(c) for c in cmd)}")


def py(venv):
    return VENV_DIR / venv / "Scripts" / "python.exe"


def materialize_links(hf_home: Path):
    """Replace HF cache symlinks with hardlinks. Symlinks made from Git Bash can be unreadable for Windows Python."""
    n = 0
    for f in hf_home.rglob("*"):
        if f.is_symlink():
            target = Path(os.path.realpath(f))
            if not target.exists():
                target = (f.parent / os.readlink(f)).resolve()
            f.unlink()
            os.link(target, f)
            n += 1
    print(f"materialised {n} cache links")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--skip-venv", action="store_true", help="venvs already exist")
    ap.add_argument("--only", default="", help="comma list of: " + ",".join(EXPORTS))
    a = ap.parse_args()
    only = [x.strip() for x in a.only.split(",") if x.strip()] or list(EXPORTS)

    env = os.environ.copy()
    env["HF_HOME"] = str(LOCAL / "cache" / "hf")
    env.setdefault("PIP_CACHE_DIR", str(VENV_DIR / "pip-cache"))
    env["PYTHONNOUSERSITE"] = "1"

    if not a.skip_venv:
        VENV_DIR.mkdir(parents=True, exist_ok=True)
        for venv, pkgs, torch in (("venv", MAIN_PKGS, ["torch", "torchvision"]), ("venv-whisper", WHISPER_PKGS, ["torch"])):
            if not py(venv).exists():
                run([sys.executable, "-m", "venv", VENV_DIR / venv], env)
            run([py(venv), "-m", "pip", "install", *torch, "--index-url", TORCH_INDEX], env)
            run([py(venv), "-m", "pip", "install", *pkgs], env)

    run([py("venv"), HERE / "fetch_models.py"], env)
    materialize_links(Path(env["HF_HOME"]))
    for name in only:
        script, venv = EXPORTS[name]
        run([py(venv), HERE / script], env)
    run([py("venv"), HERE / "stage_genai_runtime.py"], env)
    run([sys.executable, ROOT / "Scripts" / "dev_build.py", "--only", "Worker"], env)
    print(f"\nmodels: {LOCAL / 'models'}\nworker rebuilt into artifacts\\dev\\worker")


if __name__ == "__main__":
    main()
