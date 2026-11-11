# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Reference embeddings with the official sentence-transformers path, plus prompt listing."""
import json, os, sys, glob
from pathlib import Path
import torch
from PIL import Image
from sentence_transformers import SentenceTransformer

img_dir, out = Path(sys.argv[1]), sys.argv[2]
snap = glob.glob(os.path.join(os.environ["LOCALAPPDATA"], "BetterRecall", "cache", "hf", "hub",
                              "models--google--embeddinggemma-2", "snapshots", "*"))[0]
st = SentenceTransformer(snap, device="cpu", model_kwargs={"torch_dtype": torch.float32}, config_kwargs={"audio_config": None})
print("prompts:", json.dumps(st.prompts, ensure_ascii=False, indent=1))
s = json.load(open(img_dir / "set.json", encoding="utf-8"))
imgs = {k: st.encode([Image.open(img_dir / (k + ".png")).convert("RGB")], normalize_embeddings=True)[0].tolist() for k in s["images"]}
texts = [q for v in s["queries"].values() for q in v] + s["negatives"]
res = {"images": imgs}
for pname in st.prompts:
    res["queries:" + pname] = {t: st.encode([t], prompt_name=pname, normalize_embeddings=True)[0].tolist() for t in texts}
res["queries:none"] = {t: st.encode([t], normalize_embeddings=True)[0].tolist() for t in texts}
json.dump(res, open(out, "w"))
print("done")
