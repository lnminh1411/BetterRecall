# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
r"""Offline Windows Sandbox test for Authenticode-signed BetterRecall installers.

Runs INSIDE Windows Sandbox (networking disabled) with an unsigned installer and the
Windows Kits `bin` folder mapped read-only. Refuses to run unless USERNAME is
`WDAGUtilityAccount` so it can never touch the host certificate store.

Usage (inside Windows Sandbox):
  python Scripts/sandbox_sign_test.py --installer C:\BRTest\in\BetterRecall-Setup.exe \
      --kits-bin C:\BRTest\kits --out C:\BRTest\results\sign_results.json
"""
import argparse
import hashlib
import json
import os
import shutil
import subprocess
import time
from pathlib import Path

SANDBOX_USER = "WDAGUtilityAccount"
SETUP_EXE = "BetterRecall-Setup.exe"
DEFAULT_REG_PREFIX = r"Software\BetterRecallTests\sandbox-sign"


def find_signtool(kits_bin: Path) -> Path:
    if kits_bin.is_file() and kits_bin.name.lower() == "signtool.exe":
        return kits_bin
    direct = kits_bin / "signtool.exe"
    if direct.is_file():
        return direct
    x64_direct = kits_bin / "x64" / "signtool.exe"
    if x64_direct.is_file():
        return x64_direct
    cands = sorted(
        kits_bin.glob("*/x64/signtool.exe"),
        key=lambda p: [int(x) if x.isdigit() else 0 for x in p.parent.parent.name.split(".")],
    )
    if cands:
        return cands[-1]
    raise RuntimeError(f"signtool.exe not found under {kits_bin}")


def sha256_file(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        while chunk := f.read(1024 * 1024):
            h.update(chunk)
    return h.hexdigest()


def create_self_signed_cert() -> str:
    ps_cmd = (
        "$ErrorActionPreference = 'Stop'; "
        "$cert = New-SelfSignedCertificate -Type CodeSigningCert "
        "-Subject 'CN=BetterRecall Test Signing' "
        "-CertStoreLocation Cert:\\CurrentUser\\My; "
        "Write-Output $cert.Thumbprint"
    )
    r = subprocess.run(
        ["powershell.exe", "-NoProfile", "-ExecutionPolicy", "Bypass", "-Command", ps_cmd],
        capture_output=True,
        text=True,
    )
    if r.returncode != 0:
        raise RuntimeError(
            f"New-SelfSignedCertificate failed ({r.returncode}): {(r.stderr or r.stdout).strip()}"
        )
    lines = [ln.strip() for ln in r.stdout.splitlines() if ln.strip()]
    if not lines:
        raise RuntimeError("New-SelfSignedCertificate returned empty thumbprint")
    return lines[-1]


def verify_authenticode_signature(signtool: Path, exe: Path, thumbprint: str) -> tuple[bool, str]:
    r = subprocess.run(
        [str(signtool), "verify", "/pa", "/v", str(exe)],
        capture_output=True,
        text=True,
    )
    combined = (r.stdout + "\n" + r.stderr).strip()
    if r.returncode == 0:
        return True, "signtool verify /pa succeeded"

    low = combined.lower()
    # Accept untrusted-root failure from a self-signed sandbox cert, but reject missing/corrupt signatures.
    bad_markers = ("0x800b0100", "no signature was present", "0x80096010", "digest")
    if any(m in low for m in bad_markers):
        return False, f"signtool verify /pa reported bad/missing signature: {combined}"

    untrusted_markers = (
        "0x800b0109",
        "not trusted",
        "untrusted",
        "root certificate",
    )
    has_thumb = thumbprint.lower() in low
    if any(m in low for m in untrusted_markers) and has_thumb:
        return True, "signature present and valid except untrusted self-signed root (expected)"

    return False, f"unexpected signtool verify /pa result ({r.returncode}): {combined}"


def main() -> None:
    ap = argparse.ArgumentParser(
        description="Test Authenticode-signed BetterRecall installer inside Windows Sandbox."
    )
    ap.add_argument("--installer", required=True, help="path to unsigned BetterRecall-Setup-*.exe")
    ap.add_argument(
        "--kits-bin",
        required=True,
        help="path to mapped read-only Windows Kits bin folder (or signtool.exe)",
    )
    ap.add_argument(
        "--out",
        default=r"C:\BRTest\results\sign_results.json",
        help="path to JSON result file",
    )
    ap.add_argument(
        "--work-dir",
        default=r"C:\BRTest\sign-work",
        help="writable working folder inside the sandbox",
    )
    ap.add_argument(
        "--reg-prefix",
        default=DEFAULT_REG_PREFIX,
        help=f"HKCU test registry prefix (default: {DEFAULT_REG_PREFIX})",
    )
    a = ap.parse_args()

    current_user = os.environ.get("USERNAME", "")
    if current_user != SANDBOX_USER:
        raise SystemExit(
            f"sandbox_sign_test: refusing to run outside Windows Sandbox "
            f"(expected USERNAME={SANDBOX_USER!r}, got {current_user!r})"
        )

    installer_src = Path(a.installer).resolve()
    kits_bin = Path(a.kits_bin).resolve()
    out_file = Path(a.out).resolve()
    work_dir = Path(a.work_dir).resolve()

    steps: list[dict[str, object]] = []
    overall_ok = False
    error_msg = ""
    thumbprint = ""

    def record(name: str, ok: bool, detail: str = "") -> None:
        steps.append({"name": name, "ok": ok, "detail": detail})
        print(f"[{'PASS' if ok else 'FAIL'}] {name}: {detail}", flush=True)
        if not ok:
            raise RuntimeError(f"Step failed: {name} -- {detail}")

    try:
        if not installer_src.is_file():
            record("check_inputs", False, f"installer not found: {installer_src}")
        signtool = find_signtool(kits_bin)
        record("find_signtool", True, str(signtool))

        if work_dir.exists():
            shutil.rmtree(work_dir)
        work_dir.mkdir(parents=True, exist_ok=True)

        thumbprint = create_self_signed_cert()
        record("create_self_signed_cert", True, f"thumbprint={thumbprint}")

        signed_copy = work_dir / "BetterRecall-Setup-signed.exe"
        shutil.copy2(installer_src, signed_copy)
        sign_r = subprocess.run(
            [str(signtool), "sign", "/fd", "SHA256", "/sha1", thumbprint, str(signed_copy)],
            capture_output=True,
            text=True,
        )
        record(
            "sign_installer",
            sign_r.returncode == 0,
            (sign_r.stdout + " " + sign_r.stderr).strip(),
        )

        verify_ok, verify_detail = verify_authenticode_signature(signtool, signed_copy, thumbprint)
        record("verify_signature", verify_ok, verify_detail)

        dev_root = work_dir / "dev-root"
        install_dir = dev_root / "install"
        home_dir = dev_root / "home"
        install_r = subprocess.run(
            [
                str(signed_copy),
                "/S",
                "/models=later",
                "/norun",
                "--dev-root",
                str(dev_root),
                "--dev-reg-prefix",
                a.reg_prefix,
            ],
            capture_output=True,
            text=True,
            timeout=180,
        )
        record(
            "silent_install",
            install_r.returncode == 0,
            f"exit={install_r.returncode}",
        )

        installed_setup = install_dir / SETUP_EXE
        record("installed_setup_exists", installed_setup.is_file(), str(installed_setup))
        actual_sha = sha256_file(installed_setup)

        integrity_path = install_dir / "integrity.json"
        if integrity_path.is_file():
            manifest = json.loads(integrity_path.read_text(encoding="utf-8"))
            entry = next(
                (f for f in manifest.get("files", []) if f.get("path") == SETUP_EXE),
                None,
            )
            expected_sha = entry.get("sha256", "").lower() if isinstance(entry, dict) else ""
            record(
                "stub_matches_integrity_json",
                bool(expected_sha) and actual_sha.lower() == expected_sha,
                f"actual={actual_sha} expected={expected_sha}",
            )
        else:
            installed_len = installed_setup.stat().st_size
            with installer_src.open("rb") as f:
                orig_prefix_sha = hashlib.sha256(f.read(installed_len)).hexdigest()
            record(
                "stub_matches_unsigned_prefix",
                actual_sha.lower() == orig_prefix_sha.lower(),
                f"actual={actual_sha} expected={orig_prefix_sha} (no integrity.json in dev build)",
            )

        setup_log = home_dir / "logs" / "setup.log"
        log_text = setup_log.read_text(encoding="utf-8", errors="replace") if setup_log.is_file() else ""
        log_ok = ("install complete" in log_text) and ("integrity check failed" not in log_text)
        record("setup_log_ok", log_ok, str(setup_log))

        uninstall_r = subprocess.run(
            [
                str(installed_setup),
                "/uninstall",
                "/S",
                "--dev-root",
                str(dev_root),
                "--dev-reg-prefix",
                a.reg_prefix,
            ],
            capture_output=True,
            text=True,
            timeout=180,
        )
        record("silent_uninstall_start", uninstall_r.returncode == 0, f"exit={uninstall_r.returncode}")

        deadline = time.monotonic() + 30.0
        while install_dir.exists() and time.monotonic() < deadline:
            time.sleep(0.3)
        record(
            "silent_uninstall_clean",
            not install_dir.exists(),
            f"install_dir_exists={install_dir.exists()}",
        )

        overall_ok = True
    except Exception as exc:
        error_msg = str(exc)
    finally:
        out_file.parent.mkdir(parents=True, exist_ok=True)
        payload = {
            "ok": overall_ok,
            "error": error_msg,
            "thumbprint": thumbprint,
            "steps": steps,
        }
        out_file.write_text(json.dumps(payload, indent=2) + "\n", encoding="utf-8")

    if not overall_ok:
        raise SystemExit(f"sandbox_sign_test: FAILED: {error_msg}")


if __name__ == "__main__":
    main()
