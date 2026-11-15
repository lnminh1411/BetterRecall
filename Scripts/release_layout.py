# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Release-layout rules shared by dev_build.py (--release) and package_single.py.

- Never shipped: .pdb, .lib (import libraries such as worker/onnxruntime.lib), createdump.exe (can write full memory
  dumps of processes holding decrypted data) and the .devbuild marker (the agent would refuse the real data folder).
- Third-party notices: every NuGet package a published project resolved (obj/project.assets.json) contributes its
  licence and notice files to LICENSES/third-party/<id>-<version>/, and LICENSES/third-party/README.txt lists each
  package with the licence its .nuspec declares. Native pieces in the repo (src/**/Native/*LICENSE*) are copied too.

Self-test: python Scripts/release_layout.py --self-test
"""
import json
import re
import shutil
import sys
import tempfile
from pathlib import Path

ROOT = Path(__file__).resolve().parent.parent
DEV_MARKER = ".devbuild"  # InstallLayout.DevBuildMarker
JUNK_NAMES = {"createdump.exe", DEV_MARKER}
JUNK_SUFFIXES = {".pdb", ".lib"}
NOTICE_WORDS = ("license", "licence", "notice", "copying", "copyright")
THIRD_PARTY = Path("LICENSES") / "third-party"


def is_shipped(rel: Path) -> bool:
    """False for files the release must not contain (rel: path inside the layout)."""
    name = rel.name.lower()
    return name not in JUNK_NAMES and rel.suffix.lower() not in JUNK_SUFFIXES


def prune(layout: Path) -> list[Path]:
    """Deletes every file is_shipped refuses. Returns the paths removed (relative)."""
    removed = []
    for p in sorted(layout.rglob("*")):
        if p.is_file() and not is_shipped(p.relative_to(layout)):
            p.unlink()
            removed.append(p.relative_to(layout))
    return removed


def _is_notice(file: str) -> bool:
    """Top-level or docs-level licence/notice file of a package (not code that happens to contain the word)."""
    p = Path(file)
    if len(p.parts) > 2 or p.suffix.lower() in {".dll", ".exe", ".so", ".dylib", ".xml", ".json", ".targets", ".props", ".ps1", ".cs"}:
        return False
    return any(w in p.name.lower() for w in NOTICE_WORDS)


def _nuspec_info(pkg_dir: Path) -> dict:
    info = {}
    for spec in pkg_dir.glob("*.nuspec"):
        text = spec.read_text(encoding="utf-8", errors="replace")
        for tag in ("license", "licenseUrl", "copyright", "projectUrl", "authors"):
            m = re.search(rf"<{tag}(?:\s[^>]*)?>(.*?)</{tag}>", text, re.S)
            if m:
                info[tag] = " ".join(m.group(1).split())
        break
    return info


def packages(assets_files: list[Path]) -> dict[tuple[str, str], tuple[Path, list[str]]]:
    """(id, version) -> (package folder, notice files) for every package in the given project.assets.json files."""
    found: dict[tuple[str, str], tuple[Path, list[str]]] = {}
    for assets in assets_files:
        data = json.loads(assets.read_text(encoding="utf-8"))
        folders = [Path(f) for f in data.get("packageFolders", {})]
        for key, lib in data.get("libraries", {}).items():
            if lib.get("type") != "package" or "/" not in key:
                continue
            pid, ver = key.split("/", 1)
            if (pid, ver) in found:
                continue
            rel = lib.get("path") or f"{pid.lower()}/{ver.lower()}"
            pkg_dir = next((f / rel for f in folders if (f / rel).is_dir()), None)
            if pkg_dir is None:
                continue
            found[(pid, ver)] = (pkg_dir, [f for f in lib.get("files", []) if _is_notice(f)])
    return found


def write_notices(layout: Path, assets_files: list[Path], native_notices: list[Path]) -> int:
    """Fills LICENSES/third-party in the layout. Returns the number of packages listed."""
    dest = layout / THIRD_PARTY
    if dest.exists():
        shutil.rmtree(dest)
    dest.mkdir(parents=True)
    lines = [
        "Third-party components in this build",
        "",
        "Each NuGet package below is listed with the licence its package metadata declares. Licence and notice files",
        "shipped inside a package are copied into the folder named after it. Native components built from source",
        "have their licence files in native/.",
        "",
    ]
    pkgs = packages(assets_files)
    for (pid, ver), (pkg_dir, files) in sorted(pkgs.items(), key=lambda kv: kv[0][0].lower()):
        info = _nuspec_info(pkg_dir)
        licence = info.get("license") or info.get("licenseUrl") or "see package"
        lines.append(f"{pid} {ver}: {licence}" + (f"; {info['copyright']}" if info.get("copyright") else "")
                     + (f"; {info['projectUrl']}" if info.get("projectUrl") else ""))
        for f in files:
            src = pkg_dir / f
            if src.is_file():
                target = dest / f"{pid}-{ver}" / Path(f).name
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(src, target)
                lines.append(f"    {target.relative_to(dest).as_posix()}")
    if native_notices:
        lines += ["", "Native components:"]
        (dest / "native").mkdir(exist_ok=True)
        for src in native_notices:
            shutil.copy2(src, dest / "native" / src.name)
            lines.append(f"    native/{src.name}")
    (dest / "README.txt").write_text("\n".join(lines) + "\n", encoding="utf-8")
    return len(pkgs)


def assets_for(projects: list[str]) -> list[Path]:
    """obj/project.assets.json of the given src projects (UseArtifactsOutput puts obj under artifacts/obj)."""
    out = []
    for p in projects:
        for cand in (ROOT / "artifacts" / "obj" / f"BetterRecall.{p}" / "project.assets.json",
                     ROOT / "src" / f"BetterRecall.{p}" / "obj" / "project.assets.json"):
            if cand.is_file():
                out.append(cand)
                break
    return out


def native_notice_files() -> list[Path]:
    """Licence files of native pieces kept in the repo (codecs, libvpl) and staged runtimes under artifacts/native."""
    found = sorted(p for p in (ROOT / "src").glob("*/Native/*") if p.is_file() and _is_notice(p.name))
    staged = ROOT / "artifacts" / "native"
    if staged.is_dir():
        found += sorted(p for p in staged.rglob("*") if p.is_file() and _is_notice(p.name))
    return found


def finish(layout: Path, projects: list[str]) -> None:
    """Release layout: own licences, third-party notices, junk removed."""
    for f in ("LICENSE", "LICENSING.md"):
        shutil.copy2(ROOT / f, layout / f)
    lic = layout / "LICENSES"
    if lic.exists():
        shutil.rmtree(lic)
    shutil.copytree(ROOT / "LICENSES", lic)
    n = write_notices(layout, assets_for(projects), native_notice_files())
    removed = prune(layout)
    print(f"release layout: {n} third-party packages listed, {len(removed)} files removed"
          + (f" ({', '.join(sorted({r.name for r in removed})[:6])}...)" if removed else ""))


def _self_test() -> None:
    assert not is_shipped(Path("worker/createdump.exe")) and not is_shipped(Path("ui/CreateDump.exe"))
    assert not is_shipped(Path("worker/onnxruntime.lib")) and not is_shipped(Path("BetterRecall.Agent.pdb"))
    assert not is_shipped(Path(DEV_MARKER))
    assert is_shipped(Path("worker/onnxruntime.dll")) and is_shipped(Path("LICENSES/third-party/README.txt"))
    assert _is_notice("LICENSE.txt") and _is_notice("THIRD-PARTY-NOTICES.TXT") and not _is_notice("lib/net8.0/License.dll")
    with tempfile.TemporaryDirectory() as tmp:
        t = Path(tmp)
        pkg = t / "pkgs" / "demo.pkg" / "1.0.0"
        pkg.mkdir(parents=True)
        (pkg / "LICENSE.txt").write_text("MIT demo", encoding="utf-8")
        (pkg / "demo.pkg.nuspec").write_text("<package><metadata><license type=\"expression\">MIT</license>"
                                             "<copyright>(c) Demo</copyright></metadata></package>", encoding="utf-8")
        assets = t / "project.assets.json"
        assets.write_text(json.dumps({"packageFolders": {str(t / "pkgs") + "\\": {}}, "libraries": {
            "Demo.Pkg/1.0.0": {"type": "package", "path": "demo.pkg/1.0.0", "files": ["LICENSE.txt", "lib/x.dll"]},
            "BetterRecall.Core/1.0.0": {"type": "project", "path": "../x.csproj"}}}), encoding="utf-8")
        layout = t / "layout"
        (layout / "worker").mkdir(parents=True)
        for f in ("worker/createdump.exe", "worker/onnxruntime.lib", "worker/a.dll", DEV_MARKER, "x.pdb"):
            (layout / f).write_bytes(b"x")
        assert write_notices(layout, [assets], []) == 1
        readme = (layout / THIRD_PARTY / "README.txt").read_text(encoding="utf-8")
        assert "Demo.Pkg 1.0.0: MIT; (c) Demo" in readme, readme
        assert (layout / THIRD_PARTY / "Demo.Pkg-1.0.0" / "LICENSE.txt").read_text(encoding="utf-8") == "MIT demo"
        removed = {r.as_posix() for r in prune(layout)}
        assert removed == {"worker/createdump.exe", "worker/onnxruntime.lib", DEV_MARKER, "x.pdb"}, removed
        assert (layout / "worker" / "a.dll").exists()
    print("release_layout self-test ok")


if __name__ == "__main__":
    if "--self-test" in sys.argv:
        _self_test()
    else:
        sys.exit(__doc__)
