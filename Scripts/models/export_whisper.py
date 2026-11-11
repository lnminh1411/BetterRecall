# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Whisper large-v3-turbo (openai/whisper-large-v3-turbo) -> OpenVINO IR (INT8 weights) for OpenVINO GenAI
WhisperPipeline, which the worker drives through openvino_genai_c.dll.

Needs its own venv: optimum-intel 2.2 pins transformers < 5.6 (see README.md):
  python -m venv venv-whisper
  venv-whisper\\Scripts\\pip install torch --index-url https://download.pytorch.org/whl/cpu
  venv-whisper\\Scripts\\pip install "optimum-intel[openvino]==2.2.0" openvino==2026.4.1 openvino-tokenizers==2026.4.1.0 openvino-genai==2026.4.1.0 nncf

Writes <models>/whisper-large-v3-turbo/ (encoder, decoder, tokenizer, detokenizer IR + configs + manifest.json).
--bench WAV runs the exported folder with openvino_genai.WhisperPipeline on CPU and NPU: first compile time (cold and
cached), real-time factor, transcript. No WAV? --tts makes one with Windows SAPI (System.Speech), offline.
"""
from __future__ import annotations

import argparse
import json
import subprocess
import sys
import time
import wave
from pathlib import Path

import numpy as np

from common import MODELS, snapshot, write_manifest


def export(out: Path) -> None:
    src = snapshot("whisper-large-v3-turbo")
    out.mkdir(parents=True, exist_ok=True)
    cmd = [sys.executable, "-m", "optimum.commands.optimum_cli", "export", "openvino", "--model", str(src),
           "--task", "automatic-speech-recognition-with-past", "--weight-format", "int8", str(out)]
    print(" ".join(cmd))
    subprocess.run(cmd, check=True)
    files = sorted(p.name for p in out.iterdir() if p.is_file() and p.name != "manifest.json")
    write_manifest(out, {"name": "whisper-large-v3-turbo", "dim": 0}, files)


def tts_wav(path: Path, text: str) -> None:
    ps = (
        "Add-Type -AssemblyName System.Speech; $s = New-Object System.Speech.Synthesis.SpeechSynthesizer; "
        f"$s.SetOutputToWaveFile('{path}'); $s.Speak('{text}'); $s.Dispose()"
    )
    subprocess.run(["powershell", "-NoProfile", "-Command", ps], check=True)


def load_16k(path: Path) -> np.ndarray:
    with wave.open(str(path)) as w:
        sr, ch, width, n = w.getframerate(), w.getnchannels(), w.getsampwidth(), w.getnframes()
        raw = w.readframes(n)
    a = np.frombuffer(raw, {2: np.int16, 4: np.int32}[width]).astype(np.float32)
    a /= float(np.iinfo({2: np.int16, 4: np.int32}[width]).max)
    if ch > 1:
        a = a.reshape(-1, ch).mean(1)
    if sr != 16000:
        x = np.arange(0, len(a), sr / 16000)
        a = np.interp(x, np.arange(len(a)), a).astype(np.float32)
    return a


def bench(model_dir: Path, wav: Path, devices: list[str], cache: Path | None) -> dict:
    import openvino_genai as ov_genai
    audio = load_16k(wav)
    secs = len(audio) / 16000
    res = {"audio_s": round(secs, 2), "devices": {}}
    for dev in devices:
        r = {}
        try:
            props = {"CACHE_DIR": str(cache)} if cache else {}
            if dev == "NPU":
                props["NPU_TURBO"] = "NO"
            t = time.perf_counter()
            pipe = ov_genai.WhisperPipeline(str(model_dir), dev, **props)
            r["load_ms"] = round((time.perf_counter() - t) * 1000)
            cfg = pipe.get_generation_config()
            cfg.task = "transcribe"
            cfg.return_timestamps = True
            pipe.generate(audio, cfg)  # warm-up
            t = time.perf_counter()
            out = pipe.generate(audio, cfg)
            el = time.perf_counter() - t
            r.update({"run_s": round(el, 3), "rtf": round(el / secs, 4), "text": str(out.texts[0]).strip()[:300]})
            del pipe
            t = time.perf_counter()
            pipe = ov_genai.WhisperPipeline(str(model_dir), dev, **props)
            r["load_cached_ms"] = round((time.perf_counter() - t) * 1000)
        except Exception as e:  # noqa: BLE001
            r["error"] = f"{type(e).__name__}: {str(e).splitlines()[0][:300]}"
        res["devices"][dev] = r
        print(dev, json.dumps(r, ensure_ascii=False))
    return res


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=MODELS / "whisper-large-v3-turbo")
    ap.add_argument("--skip-export", action="store_true")
    ap.add_argument("--bench", type=Path, help="16-bit PCM WAV to transcribe")
    ap.add_argument("--tts", type=Path, help="write a SAPI test WAV here and bench with it")
    ap.add_argument("--devices", nargs="*", default=["NPU", "CPU"])
    ap.add_argument("--cache-dir", type=Path)
    args = ap.parse_args()
    if not args.skip_export:
        export(args.out)
    wav = args.bench
    if args.tts:
        tts_wav(args.tts, "BetterRecall keeps a private, searchable memory of your screen. The quarterly sales report "
                          "is due on Friday, and the build failed because a test timed out on the second run.")
        wav = args.tts
    if wav:
        rep = bench(args.out, wav, args.devices, args.cache_dir)
        (args.out / "bench_report.json").write_text(json.dumps(rep, indent=1, ensure_ascii=False), encoding="utf-8")


if __name__ == "__main__":
    main()
