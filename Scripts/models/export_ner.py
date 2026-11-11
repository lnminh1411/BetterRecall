# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Names and addresses for redaction: Wismut/nym-pii-multilingual-small (mmBERT-small token classifier, 41 PII labels,
22 languages, MIT) as its published INT8 ONNX graph, into <models>\\ner, plus manifest.json with a "ner" section.

No conversion. The pinned int8/model_int8.onnx and int8/tokenizer.json (revision, size and SHA-256 in
common.SOURCES["ner"]) are copied after their size and hash are checked; config.json gives the BIO label list
(id2label). The manifest maps upstream labels to the worker's entity labels:
  person   GIVEN_NAME, SURNAME (and any label containing NAME except STREET_NAME, USERNAME)
  address  STREET_ADDRESS, STREET_NAME, BUILDING_NUMBER, SECONDARY_ADDRESS, ZIP_CODE, CITY
Other labels (emails, phones, card numbers...) are left to the worker's pattern detectors.

The smoke test runs a few sentences through the Python tokenizers package and onnxruntime (1.20 or later; skipped
otherwise) and prints the entities found.

Usage: python Scripts/models/export_ner.py [--out <dir>] [--no-smoke]
  --out   default <models>\\ner
"""
from __future__ import annotations

import argparse
import json
import shutil
from pathlib import Path

from common import MODELS, SOURCES, die, sha256, write_manifest

NAME = "ner"
ADDRESS = {"STREET_ADDRESS", "STREET_NAME", "BUILDING_NUMBER", "SECONDARY_ADDRESS", "ZIP_CODE", "ZIPCODE", "POSTCODE", "CITY"}
NOT_PERSON = {"STREET_NAME", "USERNAME", "USER_NAME", "COMPANY_NAME", "ORGANIZATION_NAME"}
SAMPLES = [
    "Please send the contract to Anna Schmidt, Hauptstraße 12, 10115 Berlin.",
    "Liên hệ Nguyễn Văn An, 25 Lý Thường Kiệt, Hoàn Kiếm, Hà Nội.",
    "Ship to John Carter, 742 Evergreen Terrace, Springfield, IL 62704.",
    "Marie Dubois habite au 8 rue de la Paix, 75002 Paris.",
]


def fetch(file: str) -> Path:
    from huggingface_hub import hf_hub_download
    s = SOURCES[NAME]
    return Path(hf_hub_download(s["repo"], file, revision=s["revision"]))


def verify(path: Path, spec: dict) -> None:
    if not spec:
        return
    size = path.stat().st_size
    if size != spec["size"]:
        die(f"{path}: {size} bytes, expected {spec['size']}")
    digest = sha256(path)
    if digest != spec["sha256"]:
        die(f"{path}: sha256 {digest}, expected {spec['sha256']}")


def entity_of(label: str) -> str | None:
    if label in ADDRESS:
        return "address"
    if label in NOT_PERSON:
        return None
    if label in ("GIVEN_NAME", "SURNAME", "FIRST_NAME", "LAST_NAME", "MIDDLE_NAME", "PERSON", "NAME", "FULL_NAME") or label.endswith("_NAME") and "NAME" in label:
        return "person"
    return None


def smoke(out: Path, labels: list[str]) -> dict:
    try:
        import numpy as np
        import onnxruntime as ort
        from tokenizers import Tokenizer
    except ImportError as e:
        print(f"{e.name} not installed: smoke test skipped")
        return {}
    if tuple(int(p) for p in ort.__version__.split(".")[:2]) < (1, 20):
        print(f"onnxruntime {ort.__version__}: smoke test skipped (needs 1.20 or later)")
        return {}
    tok = Tokenizer.from_file(str(out / "tokenizer.json"))
    sess = ort.InferenceSession(str(out / "model.onnx"), providers=["CPUExecutionProvider"])
    names = [i.name for i in sess.get_inputs()]
    report = {}
    for text in SAMPLES:
        enc = tok.encode(text)
        feed = {"input_ids": np.array([enc.ids], np.int64), "attention_mask": np.ones((1, len(enc.ids)), np.int64)}
        logits = sess.run(None, {k: v for k, v in feed.items() if k in names})[0][0]
        tags = [labels[i] for i in logits.argmax(-1)]
        found = []
        for (a, b), t in zip(enc.offsets, tags):
            if t != "O" and b > a:
                found.append(f"{text[a:b]}:{t}")
        report[text] = found
        print(f"  {text}\n    {found}")
    return report


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--out", type=Path, default=MODELS / NAME)
    ap.add_argument("--no-smoke", action="store_true")
    args = ap.parse_args()
    s = SOURCES[NAME]
    out: Path = args.out
    out.mkdir(parents=True, exist_ok=True)
    copies = {"int8/model_int8.onnx": "model.onnx", "int8/tokenizer.json": "tokenizer.json", "int8/config.json": "config.json"}
    for src_name, dst_name in copies.items():
        src = fetch(src_name)
        verify(src, s["files"][src_name])
        shutil.copyfile(src, out / dst_name)
        verify(out / dst_name, s["files"][src_name])
    cfg = json.loads((out / "config.json").read_text(encoding="utf-8"))
    id2label = {int(k): v for k, v in cfg["id2label"].items()}
    labels = [id2label[i] for i in range(len(id2label))]
    entities = {}
    for lab in labels:
        base = lab[2:] if lab[:2] in ("B-", "I-", "E-", "S-") else lab
        if (e := entity_of(base)) is not None:
            entities[base] = e
    if "person" not in entities.values() or "address" not in entities.values():
        die(f"label map has no person or address labels: {sorted(set(l[2:] for l in labels if len(l) > 2))}")
    print(f"{len(labels)} tags; mapped {entities}")
    report = {} if args.no_smoke else smoke(out, labels)
    (out / "export_report.json").write_text(json.dumps(report, indent=1, ensure_ascii=False), encoding="utf-8")
    write_manifest(out, {
        "name": NAME, "dim": 0,
        "ner": {"file": "model.onnx", "tokenizer": "tokenizer.json", "inputIds": "input_ids", "attentionMask": "attention_mask",
                "output": "logits", "maxTokens": 512, "labels": labels, "entities": entities},
    }, ["model.onnx", "tokenizer.json", "config.json"])


if __name__ == "__main__":
    main()
