# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Build every BetterRecall process into the install layout (docs/v1-design.md section 2).

Usage: python Scripts/dev_build.py [--out DIR] [--config Release|Debug] [--test] [--only Agent,UI,...]
Default output: artifacts/dev. Run from anywhere; paths resolve from the repo root.
"""
import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
VSWHERE_DIR = r"C:\Program Files (x86)\Microsoft Visual Studio\Installer"

# project -> subfolder of the install dir
LAYOUT = {
    "Agent": "",
    "Cli": "",
    "Mcp": "",
    "Worker": "worker",
    "UI": "ui",
}
TESTS = ["Core", "Agent", "Worker", "UI"]


def run(cmd: list[str], env: dict) -> None:
    print(">", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, cwd=ROOT, env=env)
    if r.returncode != 0:
        sys.exit(f"failed ({r.returncode}): {' '.join(cmd)}")


def main() -> None:
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", default=str(ROOT / "artifacts" / "dev"))
    ap.add_argument("--config", default="Release")
    ap.add_argument("--test", action="store_true", help="run every test project first")
    ap.add_argument("--only", default="", help="comma list of projects from: " + ",".join(LAYOUT))
    a = ap.parse_args()

    env = os.environ.copy()
    # Native AOT linking calls vswhere by name; Git Bash PATH lacks it.
    if VSWHERE_DIR not in env.get("PATH", ""):
        env["PATH"] = env.get("PATH", "") + os.pathsep + VSWHERE_DIR

    out = Path(a.out).resolve()
    only = [p.strip() for p in a.only.split(",") if p.strip()] or list(LAYOUT)
    for p in only:
        if p not in LAYOUT:
            sys.exit(f"unknown project {p}")

    if a.test:
        for t in TESTS:
            run(["dotnet", "test", f"tests/BetterRecall.{t}.Tests", "-c", a.config], env)

    for p in only:
        dest = out / LAYOUT[p] if LAYOUT[p] else out
        if LAYOUT[p] and dest.exists():
            shutil.rmtree(dest)  # subfolder belongs to one project only
        dest.mkdir(parents=True, exist_ok=True)
        run(["dotnet", "publish", f"src/BetterRecall.{p}", "-c", a.config, "-o", str(dest)], env)

    print(f"\ninstall dir: {out}")
    print(f"start:  \"{out / 'BetterRecall.Agent.exe'}\"")


if __name__ == "__main__":
    main()
