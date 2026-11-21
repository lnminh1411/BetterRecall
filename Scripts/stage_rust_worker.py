# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Build the Rust worker in release and stage it next to native runtime DLLs.

Usage: python Scripts/stage_rust_worker.py <staging_dir> [--pivot debug_win-x64]

Builds `br-worker` in release (`CARGO_TARGET_DIR` from environment, default
`K:\\cache\\br-rust\\target`, `-j 4`), copies the exe as `BetterRecall.Worker.exe`,
and copies every native DLL from `artifacts\\bin\\BetterRecall.Worker\\<pivot>\\`
into `<staging_dir>` (excluding managed .NET assemblies and CLR runtime DLLs).
"""
import argparse
import json
import os
import shutil
import subprocess
import sys
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEFAULT_TARGET_DIR = Path(r"K:\cache\br-rust\target")

# Fallback CLR/managed exclusions if deps.json is absent.
MANAGED_OR_CLR_PREFIXES = (
    "system.",
    "microsoft.",
    "betterrecall.",
    "api-ms-win-",
    "mscordaccore",
)
MANAGED_OR_CLR_EXACT = {
    "clretwrc.dll",
    "clrgc.dll",
    "clrgcexp.dll",
    "clrjit.dll",
    "coreclr.dll",
    "directml.debug.dll",
    "google.protobuf.dll",
    "hostfxr.dll",
    "hostpolicy.dll",
    "libskiasharp.dll",
    # Trimmed from the C# worker publish too (TrimWorkerNatives in BetterRecall.Worker.csproj): unused frontends.
    "openvino_tensorflow_frontend.dll",
    "openvino_tensorflow_lite_frontend.dll",
    "openvino_paddle_frontend.dll",
    "openvino_pytorch_frontend.dll",
    "openvino_jax_frontend.dll",
    "openvino_gguf_frontend.dll",
    "mscordbi.dll",
    "mscorlib.dll",
    "mscorrc.dll",
    "msquic.dll",
    "netstandard.dll",
    "skiasharp.dll",
    "windowsbase.dll",
    "winrt.runtime.dll",
}


def pick_pivot_dir(worker_bin_root: Path, requested_pivot: str | None) -> Path:
    if requested_pivot:
        d = worker_bin_root / requested_pivot
        if not d.is_dir():
            sys.exit(f"C# worker pivot folder not found: {d}")
        return d
    if not worker_bin_root.is_dir():
        sys.exit(f"C# worker build output not found: {worker_bin_root}")
    candidates = [
        p
        for p in worker_bin_root.iterdir()
        if p.is_dir() and (p / "BetterRecall.Worker.exe").is_file()
    ]
    if not candidates:
        sys.exit(f"No BetterRecall.Worker.exe build found under {worker_bin_root}")
    # Prefer newest build (or release_win-x64 on identical timestamps).
    candidates.sort(
        key=lambda p: (
            (p / "BetterRecall.Worker.exe").stat().st_mtime,
            p.name.lower().startswith("release"),
        ),
        reverse=True,
    )
    return candidates[0]


def deps_exclusions(pivot_dir: Path) -> set[str]:
    """Basenames of managed runtime assemblies and .NET Core CLR native host DLLs."""
    excluded: set[str] = set()
    deps_path = pivot_dir / "BetterRecall.Worker.deps.json"
    if not deps_path.is_file():
        return excluded
    deps = json.loads(deps_path.read_text(encoding="utf-8"))
    for target in deps.get("targets", {}).values():
        for pkg, info in target.items():
            for rel in info.get("runtime", {}):
                excluded.add(Path(rel).name.lower())
            if pkg.startswith("runtimepack.Microsoft.NETCore.App."):
                for rel in info.get("native", {}):
                    excluded.add(Path(rel).name.lower())
    return excluded


def is_native_worker_dll(path: Path, deps_excluded: set[str]) -> bool:
    name = path.name.lower()
    if path.suffix.lower() != ".dll":
        return False
    if name in deps_excluded or name in MANAGED_OR_CLR_EXACT:
        return False
    if any(name.startswith(prefix) for prefix in MANAGED_OR_CLR_PREFIXES):
        return False
    return True


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Build br-worker in release and stage BetterRecall.Worker.exe with native DLLs."
    )
    ap.add_argument("staging_dir", help="Destination folder for BetterRecall.Worker.exe and native DLLs")
    ap.add_argument("--pivot", default=None, help="Specific pivot under artifacts\\bin\\BetterRecall.Worker\\")
    args = ap.parse_args()

    env = os.environ.copy()
    target_dir = Path(env.get("CARGO_TARGET_DIR") or DEFAULT_TARGET_DIR)
    env["CARGO_TARGET_DIR"] = str(target_dir)

    cmd = ["cargo", "build", "--release", "-p", "br-worker", "-j", "4"]
    print(">", " ".join(cmd), f"(CARGO_TARGET_DIR={target_dir})", flush=True)
    res = subprocess.run(cmd, cwd=ROOT / "rust", env=env)
    if res.returncode != 0:
        sys.exit(f"cargo build failed ({res.returncode})")

    built_exe = target_dir / "release" / "br-worker.exe"
    if not built_exe.is_file():
        sys.exit(f"Built worker binary not found: {built_exe}")

    pivot_dir = pick_pivot_dir(ROOT / "artifacts" / "bin" / "BetterRecall.Worker", args.pivot)
    exclusions = deps_exclusions(pivot_dir)
    native_dlls = sorted(
        p for p in pivot_dir.iterdir() if p.is_file() and is_native_worker_dll(p, exclusions)
    )

    staging = Path(args.staging_dir).resolve()
    staging.mkdir(parents=True, exist_ok=True)
    wanted = {dll.name.lower() for dll in native_dlls}
    for existing in staging.iterdir():
        if existing.is_file() and existing.suffix.lower() == ".dll" and existing.name.lower() not in wanted:
            existing.unlink()
    shutil.copy2(built_exe, staging / "BetterRecall.Worker.exe")
    for dll in native_dlls:
        shutil.copy2(dll, staging / dll.name)

    print(
        f"Staged BetterRecall.Worker.exe + {len(native_dlls)} native DLLs from {pivot_dir.name} -> {staging}"
    )


if __name__ == "__main__":
    main()
