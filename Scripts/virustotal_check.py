# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Checks a BetterRecall release on VirusTotal before it is published.

Looks up the SHA-256 of the installer and of every PE file (.exe, .dll) in the release layout. Nothing is uploaded
unless --upload is given: an upload makes the file public on VirusTotal, so only upload the final release build.
Needs an API key in the VT_API_KEY environment variable. Run by hand at release time (docs/release-procedure.md);
never from tests or CI.

usage: python Scripts/virustotal_check.py --installer PATH [--layout DIR] [--upload] [--json OUT]
exit codes: 0 clean (no engine flags anything), 1 detections found, 2 unknown files left (not on VirusTotal and not
uploaded) or analysis still pending, 3 usage or network error
"""
from __future__ import annotations

import argparse
import hashlib
import json
import os
import sys
import time
import urllib.error
import urllib.request
from pathlib import Path

API = "https://www.virustotal.com/api/v3"
PE_SUFFIXES = {".exe", ".dll"}
# Engines whose verdict blocks a release on its own (the "major engines" of the release procedure).
MAJOR = {"Microsoft", "Kaspersky", "BitDefender", "ESET-NOD32", "Avast", "AVG", "Sophos", "Symantec", "TrendMicro",
         "McAfee", "CrowdStrike", "SentinelOne", "Malwarebytes", "Google"}
UPLOAD_LIMIT = 32 * 1024 * 1024   # larger files need an upload URL first


def sha256(path: Path) -> str:
    h = hashlib.sha256()
    with path.open("rb") as f:
        for chunk in iter(lambda: f.read(1 << 20), b""):
            h.update(chunk)
    return h.hexdigest()


class QuotaError(Exception):
    pass


def request(method: str, url: str, key: str, body: bytes | None = None, ctype: str | None = None) -> dict | None:
    for _ in range(5):
        req = urllib.request.Request(url, data=body, method=method, headers={"x-apikey": key, "accept": "application/json"})
        if ctype:
            req.add_header("content-type", ctype)
        try:
            with urllib.request.urlopen(req, timeout=120) as r:
                return json.loads(r.read() or b"{}")
        except urllib.error.HTTPError as e:
            if e.code == 404:
                return None
            if e.code != 429:
                raise
        time.sleep(60)   # public API: 4 requests a minute
    raise QuotaError("VirusTotal kept answering 429 (quota used up?)")


def multipart(path: Path) -> tuple[bytes, str]:
    boundary = "----betterrecall" + hashlib.sha256(path.name.encode()).hexdigest()[:16]
    head = (f"--{boundary}\r\nContent-Disposition: form-data; name=\"file\"; filename=\"{path.name}\"\r\n"
            "Content-Type: application/octet-stream\r\n\r\n").encode()
    return head + path.read_bytes() + f"\r\n--{boundary}--\r\n".encode(), f"multipart/form-data; boundary={boundary}"


def upload(path: Path, key: str) -> str | None:
    url = f"{API}/files"
    if path.stat().st_size > UPLOAD_LIMIT:
        r = request("GET", f"{API}/files/upload_url", key)
        url = r["data"] if r else url
    body, ctype = multipart(path)
    r = request("POST", url, key, body, ctype)
    return r["data"]["id"] if r else None


def verdict(report: dict) -> dict:
    attrs = report["data"]["attributes"]
    results = attrs.get("last_analysis_results", {})
    flagged = {name: r.get("result") for name, r in results.items() if r.get("category") in ("malicious", "suspicious")}
    return {"stats": attrs.get("last_analysis_stats", {}), "flagged": flagged,
            "major": sorted(n for n in flagged if n in MAJOR), "link": f"https://www.virustotal.com/gui/file/{report['data']['id']}"}


def main() -> int:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--installer", required=True)
    ap.add_argument("--layout", help="release layout folder; every .exe and .dll in it is checked too")
    ap.add_argument("--upload", action="store_true", help="upload files VirusTotal does not know (makes them public)")
    ap.add_argument("--wait", type=int, default=600, help="seconds to wait for uploaded files to be analysed")
    ap.add_argument("--json", help="write the full result here")
    a = ap.parse_args()
    key = os.environ.get("VT_API_KEY", "")
    if not key:
        print("VT_API_KEY is not set", file=sys.stderr)
        return 3

    files = [Path(a.installer)]
    if a.layout:
        layout = Path(a.layout)
        if not layout.is_dir():
            print(f"--layout is not a folder: {layout}", file=sys.stderr)
            return 3
        pes = sorted(p for p in layout.rglob("*") if p.suffix.lower() in PE_SUFFIXES)
        if not pes:
            print(f"--layout holds no .exe or .dll files: {layout}", file=sys.stderr)
            return 3
        files += pes
    for path in files:
        if not path.is_file():
            print(f"not a file: {path}", file=sys.stderr)
            return 3
    out, unknown, pending, dirty = {}, [], [], False
    for path in files:
        try:
            digest = sha256(path)
            report = request("GET", f"{API}/files/{digest}", key)
            if report is None and a.upload:
                print(f"upload  {path.name}")
                upload(path, key)
                deadline = time.time() + a.wait
                while report is None or not report["data"]["attributes"].get("last_analysis_results"):
                    if time.time() > deadline:
                        break
                    time.sleep(30)
                    report = request("GET", f"{API}/files/{digest}", key)
            if report is not None:
                v = verdict(report)
        except (urllib.error.URLError, OSError, KeyError, ValueError, QuotaError) as e:
            print(f"error   {path.name}: {e}", file=sys.stderr)
            return 3
        if report is None:
            unknown.append(str(path))
            print(f"unknown {path.name} {digest}")
            continue
        # D4: no engine results yet means pending, whatever the stats say
        if not v["stats"] or not report["data"]["attributes"].get("last_analysis_results"):
            pending.append(str(path))
            print(f"pending {path.name}")
            continue
        out[str(path)] = {"sha256": digest, **v}
        dirty |= bool(v["flagged"])
        mark = "CLEAN  " if not v["flagged"] else ("BLOCK  " if v["major"] else "FLAGGED")
        print(f"{mark} {path.name} {v['stats'].get('malicious', 0)} malicious, {v['stats'].get('suspicious', 0)} suspicious {v['link']}")
        for name, res in sorted(v["flagged"].items()):
            print(f"          {name}: {res}")
    if a.json:
        Path(a.json).write_text(json.dumps({"files": out, "unknown": unknown, "pending": pending}, indent=2), encoding="utf-8")
    if dirty:
        return 1
    return 2 if unknown or pending else 0


if __name__ == "__main__":
    sys.exit(main())
