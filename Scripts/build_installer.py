# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
r"""Build and pack the BetterRecall per-user offline installer (R6/R8/R9).

Usage:
  python Scripts/build_installer.py [--pubkey public.pem --key private.pem] [--payload-version V] [--out DIR] [--skip-build]
  python Scripts/build_installer.py --self-test

Without --pubkey and --key, builds an unsigned dev installer (no integrity.json, no update.json).
"""
import argparse
import hashlib
import os
import re
import shutil
import struct
import subprocess
import sys
import tomllib
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))
import release_layout  # noqa: E402

ROOT = Path(__file__).resolve().parent.parent
SETUP_EXE = "BetterRecall-Setup.exe"
VCRUNTIME_DLLS = {"vcruntime140.dll", "vcruntime140_1.dll"}


def read_props_version(props_path: Path | None = None) -> str:
    path = props_path or (ROOT / "Directory.Build.props")
    text = path.read_text(encoding="utf-8")
    m = re.search(r"<Version>\s*([^<\s]+)\s*</Version>", text)
    if not m:
        raise SystemExit(f"build_installer: <Version> not found in {path}")
    return m.group(1)


def read_cargo_version(cargo_path: Path | None = None) -> str:
    path = cargo_path or (ROOT / "rust" / "Cargo.toml")
    data = tomllib.loads(path.read_text(encoding="utf-8"))
    ver = data.get("workspace", {}).get("package", {}).get("version")
    if not isinstance(ver, str) or not ver.strip():
        raise SystemExit(f"build_installer: [workspace.package] version not found in {path}")
    return ver.strip()


def check_versions(props_ver: str, cargo_ver: str) -> str:
    if props_ver != cargo_ver:
        raise SystemExit(
            f"build_installer: version mismatch: Directory.Build.props has '{props_ver}' "
            f"but rust/Cargo.toml has '{cargo_ver}'"
        )
    return props_ver


def pe_imports(data: bytes) -> list[str]:
    """Parses the PE32/PE32+ import directory and returns imported DLL names."""
    if len(data) < 0x40 or data[0:2] != b"MZ":
        raise ValueError("not a PE file (missing MZ header)")
    (e_lfanew,) = struct.unpack_from("<I", data, 0x3C)
    if e_lfanew + 24 > len(data) or data[e_lfanew : e_lfanew + 4] != b"PE\x00\x00":
        raise ValueError("not a PE file (missing PE signature)")

    num_sections = struct.unpack_from("<H", data, e_lfanew + 6)[0]
    opt_size = struct.unpack_from("<H", data, e_lfanew + 20)[0]
    opt_off = e_lfanew + 24
    if opt_off + opt_size > len(data) or opt_size < 2:
        raise ValueError("truncated PE optional header")

    magic = struct.unpack_from("<H", data, opt_off)[0]
    if magic == 0x10B:  # PE32
        dd_off = opt_off + 96
    elif magic == 0x20B:  # PE32+
        dd_off = opt_off + 112
    else:
        raise ValueError(f"unsupported PE optional header magic: 0x{magic:x}")

    if dd_off + 16 > opt_off + opt_size:
        return []
    import_rva, _import_size = struct.unpack_from("<II", data, dd_off + 8)
    if import_rva == 0:
        return []

    sec_off = opt_off + opt_size
    sections: list[tuple[int, int, int, int]] = []
    for i in range(num_sections):
        off = sec_off + i * 40
        if off + 40 > len(data):
            raise ValueError("truncated PE section table")
        vsize, va, raw_size, raw_ptr = struct.unpack_from("<IIII", data, off + 8)
        sections.append((va, max(vsize, raw_size), raw_ptr, raw_size))

    def rva_to_offset(rva: int) -> int | None:
        for va, span, raw_ptr, raw_size in sections:
            if va <= rva < va + span:
                delta = rva - va
                if delta < raw_size:
                    return raw_ptr + delta
                return None
        return None

    desc_off = rva_to_offset(import_rva)
    if desc_off is None:
        raise ValueError(f"import directory RVA 0x{import_rva:x} not mapped in any section")

    dlls: list[str] = []
    while desc_off + 20 <= len(data):
        orig_thunk, _ts, _fwd, name_rva, first_thunk = struct.unpack_from("<IIIII", data, desc_off)
        if orig_thunk == 0 and name_rva == 0 and first_thunk == 0:
            break
        if name_rva != 0:
            name_off = rva_to_offset(name_rva)
            if name_off is not None:
                end = data.find(b"\x00", name_off)
                if end != -1:
                    dlls.append(data[name_off:end].decode("ascii", errors="replace"))
        desc_off += 20
    return dlls


def pe_imports_file(path: Path) -> list[str]:
    return pe_imports(path.read_bytes())


def assert_no_vcruntime(path: Path) -> list[str]:
    dlls = pe_imports_file(path)
    bad = [d for d in dlls if d.lower() in VCRUNTIME_DLLS]
    if bad:
        raise SystemExit(
            f"build_installer: {path} unexpectedly imports VC++ runtime DLL(s): {', '.join(bad)} "
            "(expected static CRT)"
        )
    return dlls


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while chunk := f.read(1024 * 1024):
            h.update(chunk)
    return h.hexdigest()


def run(cmd: list[str], env: dict[str, str], cwd: Path = ROOT) -> None:
    print(">", " ".join(cmd), flush=True)
    r = subprocess.run(cmd, cwd=cwd, env=env)
    if r.returncode != 0:
        raise SystemExit(f"build_installer: command failed ({r.returncode}): {' '.join(cmd)}")


def crt_static_target_dir(env: dict[str, str]) -> Path:
    base = Path(env["CARGO_TARGET_DIR"]) if env.get("CARGO_TARGET_DIR") else (ROOT / "artifacts" / "cargo")
    return base.resolve() / "crt-static"


def _make_synthetic_pe(dll_names: list[str]) -> bytes:
    """Builds a minimal PE32+ image with one .rdata section containing an import table."""
    buf = bytearray(0x400)
    buf[0:2] = b"MZ"
    e_lfanew = 0x40
    struct.pack_into("<I", buf, 0x3C, e_lfanew)
    buf[e_lfanew : e_lfanew + 4] = b"PE\x00\x00"
    # COFF header: AMD64, 1 section, 240-byte optional header
    struct.pack_into("<HHIIIHH", buf, e_lfanew + 4, 0x8664, 1, 0, 0, 0, 240, 0x0022)
    opt_off = e_lfanew + 24
    struct.pack_into("<H", buf, opt_off, 0x20B)  # PE32+
    struct.pack_into("<I", buf, opt_off + 108, 16)  # NumberOfRvaAndSizes
    struct.pack_into("<II", buf, opt_off + 120, 0x2000, (len(dll_names) + 1) * 20)
    # Section header .rdata at opt_off + 240
    sec_off = opt_off + 240
    buf[sec_off : sec_off + 8] = b".rdata\x00\x00"
    struct.pack_into("<IIII", buf, sec_off + 8, 0x200, 0x2000, 0x200, 0x200)
    # Import descriptors at file offset 0x200 (RVA 0x2000); names at +0x100 (RVA 0x2100)
    str_pos = 0x300
    for idx, name in enumerate(dll_names):
        name_rva = 0x2000 + (str_pos - 0x200)
        struct.pack_into("<IIIII", buf, 0x200 + idx * 20, 0, 0, 0, name_rva, 0)
        raw = name.encode("ascii") + b"\x00"
        buf[str_pos : str_pos + len(raw)] = raw
        str_pos += len(raw)
    return bytes(buf)


def _self_test() -> None:
    # 1. Version check on this repo and mismatch rejection.
    props_ver = read_props_version()
    cargo_ver = read_cargo_version()
    assert check_versions(props_ver, cargo_ver) == props_ver
    try:
        check_versions("0.3.0-dev", "0.3.1")
        raise AssertionError("check_versions must fail on mismatch")
    except SystemExit:
        pass

    # 2. PE import parser on a synthetic PE32+ image.
    pe_bad = _make_synthetic_pe(["KERNEL32.dll", "VCRUNTIME140.dll"])
    assert pe_imports(pe_bad) == ["KERNEL32.dll", "VCRUNTIME140.dll"]
    pe_ok = _make_synthetic_pe(["KERNEL32.dll", "ADVAPI32.dll"])
    assert pe_imports(pe_ok) == ["KERNEL32.dll", "ADVAPI32.dll"]

    # 3. PE import parser on the running Python executable itself.
    py_imports = pe_imports_file(Path(sys.executable))
    assert isinstance(py_imports, list) and all(isinstance(d, str) for d in py_imports)
    print("build_installer self-test ok")


def main() -> None:
    ap = argparse.ArgumentParser(description="Build and pack the BetterRecall offline installer.")
    ap.add_argument("--pubkey", default="", help="ECDSA P-256 public key PEM (requires --key)")
    ap.add_argument("--key", default="", help="ECDSA P-256 private key PEM (requires --pubkey)")
    ap.add_argument("--payload-version", default="", help="override version written to setup.json (test only)")
    ap.add_argument("--out", default=str(ROOT / "artifacts" / "release"), help="output folder (default: artifacts/release)")
    ap.add_argument("--skip-build", action="store_true", help="skip dev_build.py and static-CRT cargo builds")
    ap.add_argument("--self-test", action="store_true", help="run PE parser and version-check self-tests")
    a = ap.parse_args()

    if a.self_test:
        _self_test()
        return

    if bool(a.pubkey) != bool(a.key):
        raise SystemExit("build_installer: --pubkey and --key must be provided together")

    signed = bool(a.pubkey)
    pubkey_path = Path(a.pubkey).resolve() if signed else None
    key_path = Path(a.key).resolve() if signed else None
    if signed:
        assert pubkey_path is not None and key_path is not None
        if not pubkey_path.is_file():
            raise SystemExit(f"build_installer: --pubkey not found: {pubkey_path}")
        if not key_path.is_file():
            raise SystemExit(f"build_installer: --key not found: {key_path}")
    else:
        print("build_installer: building UNSIGNED dev installer (no --pubkey/--key supplied)", flush=True)

    app_version = check_versions(read_props_version(), read_cargo_version())
    payload_version = a.payload_version.strip() or app_version

    out_dir = Path(a.out).resolve()
    layout_dir = out_dir / "layout"
    out_dir.mkdir(parents=True, exist_ok=True)

    env = os.environ.copy()
    static_target = crt_static_target_dir(env)
    static_release = static_target / "release"
    installer_stub = static_release / "br-installer.exe"
    sign_exe = static_release / "br-sign.exe"
    static_mcp_exe = static_release / "betterrecall-mcp.exe"

    if not a.skip_build:
        dev_cmd = [sys.executable, str(ROOT / "Scripts" / "dev_build.py"), "--release", "--out", str(layout_dir)]
        if signed and pubkey_path is not None:
            dev_cmd += ["--release-pubkey", str(pubkey_path)]
        run(dev_cmd, env)

        if not shutil.which("cargo"):
            raise SystemExit("build_installer: cargo not found on PATH")
        static_env = env.copy()
        existing_flags = static_env.get("RUSTFLAGS", "").strip()
        static_env["RUSTFLAGS"] = (existing_flags + " -C target-feature=+crt-static").strip()
        if signed and pubkey_path is not None:
            static_env["BR_RELEASE_PUBKEY"] = str(pubkey_path)
        else:
            static_env.pop("BR_RELEASE_PUBKEY", None)
        run(
            [
                "cargo",
                "build",
                "--release",
                "-p",
                "br-installer",
                "-p",
                "br-sign",
                "-p",
                "br-mcp",
                "-j",
                "4",
                "--target-dir",
                str(static_target),
            ],
            static_env,
            cwd=ROOT / "rust",
        )

    for required in (installer_stub, sign_exe, static_mcp_exe):
        if not required.is_file():
            raise SystemExit(f"build_installer: expected binary not found: {required}")
    if not layout_dir.is_dir():
        raise SystemExit(f"build_installer: layout folder not found: {layout_dir}")

    layout_mcp = layout_dir / "betterrecall-mcp.exe"
    shutil.copy2(static_mcp_exe, layout_mcp)

    assert_no_vcruntime(installer_stub)
    assert_no_vcruntime(layout_mcp)

    layout_setup = layout_dir / SETUP_EXE
    if signed and pubkey_path is not None and key_path is not None:
        # Include BetterRecall-Setup.exe in the layout before signing so integrity.json hashes it;
        # br-sign pack skips it when zipping because br-installer copies it from its own stub bytes.
        shutil.copy2(installer_stub, layout_setup)
        run(
            [str(sign_exe), "manifest", str(layout_dir), "--key", str(key_path), "--version", app_version],
            env,
        )
        run(
            [str(sign_exe), "verify", str(layout_dir), "--public", str(pubkey_path)],
            env,
        )
    else:
        for stale in (layout_dir / "integrity.json", layout_dir / "integrity.sig", layout_setup):
            if stale.exists():
                stale.unlink()

    setup_out = out_dir / f"BetterRecall-Setup-{payload_version}.exe"
    run(
        [
            str(sign_exe),
            "pack",
            "--stub",
            str(installer_stub),
            "--layout",
            str(layout_dir),
            "--version",
            payload_version,
            "--out",
            str(setup_out),
        ],
        env,
    )

    notices_out = out_dir / f"BetterRecall-Setup-{payload_version}.notices.txt"
    release_layout.write_installer_notices(notices_out)

    if signed and key_path is not None:
        run(
            [
                str(sign_exe),
                "update",
                "--installer",
                str(setup_out),
                "--version",
                payload_version,
                "--key",
                str(key_path),
                "--out",
                str(out_dir),
            ],
            env,
        )

    size = setup_out.stat().st_size
    digest = sha256_file(setup_out)
    status = "signed" if signed else "UNSIGNED dev build"
    print(f"\ninstaller: {setup_out}")
    print(f"size:      {size} bytes")
    print(f"sha256:    {digest}")
    print(f"status:    {status}")
    print(f"notices:   {notices_out}")


if __name__ == "__main__":
    main()
