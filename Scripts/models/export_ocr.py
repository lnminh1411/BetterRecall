# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""OCR models: PaddleOCR PP-OCRv6 small detector + recognisers (official ONNX exports, Apache-2.0) into <models>\\ocr,
plus manifest.json with an "ocr" section.

No conversion. Each pinned inference.onnx (revision, size and SHA-256 in common.SOURCES["ocr"]) is copied after its
size and hash are checked. Each recogniser's character dictionary comes from its inference.yml
(PostProcess.character_dict) and is written as dict-<name>.txt, one entry per line, so the worker needs no YAML parser.
CTC classes: 0 = blank, 1..n = dictionary, n+1 = space (checked against the graph's output width).

Recognisers (ocr.languages picks which ones run besides "main"):
  main        PP-OCRv6 small: Chinese (simplified + traditional), English, Japanese and Latin-script languages
  korean, cyrillic, arabic, devanagari, th, el, ta, te   PP-OCRv5 mobile, one script each (with English)

The smoke test renders known text with Windows fonts, runs detection and recognition with onnxruntime (the same
pre/post-processing the worker uses) and prints what each model read. It fails when the main model misreads plain
English and Latin text.

Usage: python Scripts/models/export_ocr.py [--out <dir>] [--only main,korean] [--no-smoke]
  --out   default <models>\\ocr
"""
from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

from common import MODELS, SOURCES, die, sha256, write_manifest

NAME = "ocr"
LANGUAGES = {
    # "latin" stands for every language written in Latin script (ocr.languages resolves codes through it).
    "main": ["zh", "en", "ja", "latin"],
    "korean": ["ko"],
    "cyrillic": ["ru", "uk", "be", "bg", "sr", "mk", "kk", "ky", "mn", "tg", "tt", "ba", "cv", "ab", "av", "ce", "lez", "dar", "inh", "lbe", "tab", "sah"],
    "arabic": ["ar", "fa", "ur", "ug", "ps", "sd", "ku", "bal"],
    "devanagari": ["hi", "mr", "ne", "sa", "bh", "mai", "bho", "mah", "sck", "new", "gom"],
    "th": ["th"],
    "el": ["el"],
    "ta": ["ta"],
    "te": ["te"],
}
SAMPLES = {  # text, Windows font file
    "main": [("Invoice 4471 due 2026-11-30", "arial.ttf"), ("Quarterly planning: Umsatz und Größe", "segoeui.ttf"),
             ("会议记录 第三季度", "msyh.ttc"), ("パスワードを入力", "YuGothM.ttc")],
    "korean": [("비밀번호를 입력하세요", "malgun.ttf")],
    "cyrillic": [("Пароль и дата рождения", "segoeui.ttf")],
    "arabic": [("كلمة المرور", "segoeui.ttf")],
    "devanagari": [("पासवर्ड दर्ज करें", "Nirmala.ttc")],
    "th": [("รหัสผ่าน", "LeelawUI.ttf")],
    "el": [("Κωδικός πρόσβασης", "segoeui.ttf")],
    "ta": [("கடவுச்சொல்", "Nirmala.ttc")],
    "te": [("పాస్‌వర్డ్", "Nirmala.ttc")],
}


def fetch(spec: dict, file: str) -> Path:
    from huggingface_hub import hf_hub_download
    return Path(hf_hub_download(spec["repo"], file, revision=spec["revision"]))


def verify(path: Path, spec: dict) -> None:
    size = path.stat().st_size
    if size != spec["size"]:
        die(f"{path}: {size} bytes, expected {spec['size']}")
    digest = sha256(path)
    if digest != spec["sha256"]:
        die(f"{path}: sha256 {digest}, expected {spec['sha256']}")


def load_yaml(path: Path) -> dict:
    import yaml
    return yaml.safe_load(path.read_text(encoding="utf-8"))


# ---------------- reference pipeline (mirrors BetterRecall.Worker PaddleOcr) ----------------

def det_boxes(sess, bgr, det: dict):
    """DB detection: resize so the long side is at most maxSide and both sides are multiples of 32; threshold the
    probability map, connected components, axis-aligned boxes expanded by area * unclip / perimeter."""
    import cv2
    import numpy as np
    h, w = bgr.shape[:2]
    scale = min(1.0, det["maxSide"] / max(h, w))
    nh, nw = max(32, int(round(h * scale / 32)) * 32), max(32, int(round(w * scale / 32)) * 32)
    img = cv2.resize(bgr, (nw, nh)).astype(np.float32) / 255.0
    img = (img - np.array(det["mean"], np.float32)) / np.array(det["std"], np.float32)
    x = img.transpose(2, 0, 1)[None]
    prob = sess.run(None, {sess.get_inputs()[0].name: x})[0][0, 0]
    mask = (prob > det["thresh"]).astype(np.uint8)
    n, labels, stats, _ = cv2.connectedComponentsWithStats(mask, connectivity=4)
    boxes = []
    for i in range(1, n):
        x0, y0, bw, bh, area = stats[i]
        if min(bw, bh) < 3:
            continue
        score = float(prob[labels == i].mean())
        if score < det["boxThresh"]:
            continue
        d = (bw * bh) * det["unclip"] / (2 * (bw + bh))
        sx, sy = w / nw, h / nh
        l, t = max(0, (x0 - d) * sx), max(0, (y0 - d) * sy)
        r, b = min(w, (x0 + bw + d) * sx), min(h, (y0 + bh + d) * sy)
        boxes.append((int(l), int(t), int(np.ceil(r)), int(np.ceil(b)), score))
    boxes.sort(key=lambda bx: (bx[1] // 10, bx[0]))
    return boxes


def recognise(sess, bgr_crop, chars: list[str]):
    import cv2
    import numpy as np
    h, w = bgr_crop.shape[:2]
    nw = max(8, min(3200, int(np.ceil(48 * w / h))))
    img = cv2.resize(bgr_crop, (nw, 48)).astype(np.float32) / 255.0
    x = ((img - 0.5) / 0.5).transpose(2, 0, 1)[None]
    y = sess.run(None, {sess.get_inputs()[0].name: x})[0][0]
    idx, conf = y.argmax(-1), y.max(-1)
    out, cs, prev = [], [], 0
    table = ["<blank>"] + chars + [" "]
    for i, c in enumerate(idx):
        if c != 0 and c != prev:
            out.append(table[c])
            cs.append(conf[i])
        prev = c
    return "".join(out), float(np.mean(cs)) if cs else 0.0, y.shape[-1]


def render(text: str, font_file: str):
    import numpy as np
    from PIL import Image, ImageDraw, ImageFont
    font = ImageFont.truetype(str(Path("C:/Windows/Fonts") / font_file), 28)
    img = Image.new("RGB", (900, 120), "white")
    ImageDraw.Draw(img).text((30, 40), text, font=font, fill="black")
    return np.array(img)[:, :, ::-1].copy()  # BGR


def smoke(out: Path, manifest_ocr: dict, only: list[str]) -> dict:
    try:
        import onnxruntime as ort
    except ImportError:
        print("onnxruntime not installed: smoke test skipped")
        return {}
    if tuple(int(p) for p in ort.__version__.split(".")[:2]) < (1, 20):
        # The PaddleOCR exports use ONNX IR version 10.
        print(f"onnxruntime {ort.__version__} reads ONNX IR 9 at most: smoke test skipped (needs 1.20 or later)")
        return {}
    so = ort.SessionOptions()
    so.intra_op_num_threads = 2
    det = manifest_ocr["det"]
    dsess = ort.InferenceSession(str(out / det["file"]), so, providers=["CPUExecutionProvider"])
    report = {}
    for rec in manifest_ocr["rec"]:
        if rec["name"] not in only:
            continue
        chars = (out / rec["dict"]).read_text(encoding="utf-8").split("\n")
        rsess = ort.InferenceSession(str(out / rec["file"]), so, providers=["CPUExecutionProvider"])
        rows = []
        for text, font in SAMPLES[rec["name"]]:
            if not (Path("C:/Windows/Fonts") / font).exists():
                rows.append({"text": text, "skipped": f"font {font} missing"})
                continue
            bgr = render(text, font)
            boxes = det_boxes(dsess, bgr, det)
            read = []
            for (l, t, r, b, _) in boxes:
                s, c, classes = recognise(rsess, bgr[t:b, l:r], chars)
                if classes != len(chars) + 2:
                    die(f"{rec['name']}: {classes} output classes, dictionary {len(chars)} + blank + space")
                read.append((s, round(c, 3)))
            got = " ".join(s for s, _ in read)
            rows.append({"text": text, "boxes": len(boxes), "read": got, "confidence": [c for _, c in read]})
            print(f"  {rec['name']:<10} {text!r} -> {got!r} {[c for _, c in read]}")
        report[rec["name"]] = rows
    main = report.get("main")
    if main:
        for row in main[:2]:
            if row.get("read", "").replace(" ", "") != row["text"].replace(" ", ""):
                die(f"main recogniser misread {row['text']!r} as {row.get('read')!r}")
    return report


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=MODELS / NAME)
    ap.add_argument("--only", default="", help="comma-separated recognisers (main is always included)")
    ap.add_argument("--no-smoke", action="store_true")
    args = ap.parse_args()
    s = SOURCES[NAME]
    names = list(s["rec"]) if not args.only else ["main"] + [n for n in args.only.split(",") if n and n != "main"]
    for n in names:
        if n not in s["rec"]:
            die(f"unknown recogniser {n}")
    out: Path = args.out
    out.mkdir(parents=True, exist_ok=True)
    files, sources = [], {}

    dspec = s["det"]
    src = fetch(dspec, "inference.onnx")
    verify(src, dspec)
    shutil.copyfile(src, out / "det.onnx")
    verify(out / "det.onnx", dspec)
    dyml = load_yaml(fetch(dspec, "inference.yml"))
    post = dyml["PostProcess"]
    norm = next(op["NormalizeImage"] for op in dyml["PreProcess"]["transform_ops"] if "NormalizeImage" in op)
    det = {"file": "det.onnx", "input": "x", "thresh": post["thresh"], "boxThresh": post["box_thresh"],
           "unclip": post["unclip_ratio"], "mean": norm["mean"], "std": norm["std"], "maxSide": 1920}
    files.append("det.onnx")
    sources["det.onnx"] = f"https://huggingface.co/{dspec['repo']}/tree/{dspec['revision']}"

    recs = []
    for n in names:
        spec = s["rec"][n]
        src = fetch(spec, "inference.onnx")
        verify(src, spec)
        dst = out / f"rec-{n}.onnx"
        shutil.copyfile(src, dst)
        verify(dst, spec)
        yml = load_yaml(fetch(spec, "inference.yml"))
        chars = [str(c) for c in yml["PostProcess"]["character_dict"]]
        if any("\n" in c for c in chars):
            die(f"{n}: dictionary entry with a newline")
        (out / f"dict-{n}.txt").write_bytes("\n".join(chars).encode("utf-8"))   # LF only, no newline translation
        recs.append({"name": n, "file": dst.name, "dict": f"dict-{n}.txt", "input": "x", "height": 48,
                     "languages": LANGUAGES[n], "classes": len(chars) + 2})
        files += [dst.name, f"dict-{n}.txt"]
        url = f"https://huggingface.co/{spec['repo']}/tree/{spec['revision']}"
        sources[dst.name] = sources[f"dict-{n}.txt"] = url
        print(f"{n}: {len(chars)} characters, {dst.stat().st_size} bytes")

    ocr = {"det": det, "rec": recs}
    report = {} if args.no_smoke else smoke(out, ocr, names)
    (out / "export_report.json").write_text(json.dumps(report, indent=1, ensure_ascii=False), encoding="utf-8")
    write_manifest(out, {
        "name": NAME, "dim": 0, "id": "PaddlePaddle/PP-OCRv6_small (+ PP-OCRv5 mobile script recognisers)",
        "revision": f"det {dspec['revision'][:12]}, rec {s['rec']['main']['revision'][:12]}",
        "source": sources["det.onnx"], "fileSources": sources, "ocr": ocr,
    }, files)


if __name__ == "__main__":
    main()
