# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
r"""Build every BetterRecall process into the install layout (docs/v1-design.md section 2).

Usage: python Scripts/dev_build.py [--out DIR] [--config Release|Debug] [--test] [--only Agent,UI,...] [--run]
Default output: artifacts/dev. Run from anywhere; paths resolve from the repo root.
--run starts the built agent afterwards with its own data folder (BETTERRECALL_DEV_HOME, default
K:\scratch\betterrecall-dev-home) and --no-autostart; --run-only starts it without building.
The output folder gets a .devbuild marker: an agent started from it refuses the real data folder
(%LOCALAPPDATA%\BetterRecall) unless given --allow-default-home.
--release (implied for an output folder under artifacts/release, as package_single.py uses): no marker, third-party
licence notices in LICENSES/third-party, no .pdb, .lib or createdump.exe (Scripts/release_layout.py).
"""
import argparse
import os
import shutil
import subprocess
import sys
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release_layout  # noqa: E402

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
DEV_MARKER = release_layout.DEV_MARKER  # InstallLayout.DevBuildMarker


def dev_home() -> Path:
    return Path(os.environ.get("BETTERRECALL_DEV_HOME", r"K:\scratch\betterrecall-dev-home"))


def start_agent(out: Path) -> None:
    """Starts the dev agent detached with BETTERRECALL_HOME in its environment (works from any shell)."""
    home = dev_home()
    home.mkdir(parents=True, exist_ok=True)
    env = os.environ.copy()
    env["BETTERRECALL_HOME"] = str(home)
    exe = out / "BetterRecall.Agent.exe"
    flags = getattr(subprocess, "DETACHED_PROCESS", 0) | getattr(subprocess, "CREATE_NEW_PROCESS_GROUP", 0)
    subprocess.Popen([str(exe), "--no-autostart"], cwd=out, env=env, creationflags=flags, close_fds=True)
    print(f"started {exe} with BETTERRECALL_HOME={home}")


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
    ap.add_argument("--run", action="store_true", help="start the dev agent on its own data folder after the build")
    ap.add_argument("--run-only", action="store_true", help="start the dev agent without building")
    ap.add_argument("--release", action="store_true", help="release layout: no dev marker, licence notices, no pdb/lib/createdump")
    a = ap.parse_args()

    env = os.environ.copy()
    # Native AOT linking calls vswhere by name; Git Bash PATH lacks it.
    if VSWHERE_DIR not in env.get("PATH", ""):
        env["PATH"] = env.get("PATH", "") + os.pathsep + VSWHERE_DIR

    out = Path(a.out).resolve()
    if a.run_only:
        start_agent(out)
        return
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

    if a.release or (ROOT / "artifacts" / "release") in out.parents:
        if a.run:
            sys.exit("--run needs a dev build (a release layout has no .devbuild marker)")
        release_layout.finish(out, list(LAYOUT))
        print(f"\nrelease layout: {out}")
        return

    # The agent refuses the default data folder when it finds this marker next to it (InstallLayout.RefusesHome).
    (out / DEV_MARKER).write_text("dev build: needs its own BETTERRECALL_HOME\n", encoding="utf-8")

    print(f"\ninstall dir: {out}")
    # A dev agent gets its own data folder and never registers autostart: on the real folder it would open the user's
    # store with dev code and repoint their startup entry at artifacts\dev. `set X=Y && ...` only works in cmd.exe.
    home = dev_home()
    exe = out / "BetterRecall.Agent.exe"
    print("start (any shell):  python Scripts/dev_build.py --run-only" + (f' --out "{out}"' if a.out != str(ROOT / "artifacts" / "dev") else ""))
    print(f'  cmd:        set "BETTERRECALL_HOME={home}" && "{exe}" --no-autostart')
    print(f"  PowerShell: $env:BETTERRECALL_HOME='{home}'; & '{exe}' --no-autostart")
    print(f"  bash:       BETTERRECALL_HOME='{home}' '{exe.as_posix()}' --no-autostart &")
    if a.run:
        start_agent(out)


if __name__ == "__main__":
    main()
