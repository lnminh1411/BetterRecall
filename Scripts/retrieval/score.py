# SPDX-License-Identifier: AGPL-3.0-only
# Copyright (c) BetterRecall contributors
"""Score retrieval: for each query, rank of its own image among 10; cosine stats."""
import json, sys
import numpy as np

s = json.load(open(sys.argv[1], encoding="utf-8"))
r = json.load(open(sys.argv[2], encoding="utf-8"))
qkey = sys.argv[3] if len(sys.argv) > 3 else "queries"
names = s["images"]
I = np.array([r["images"][n] for n in names])
Q = r[qkey]
rows, pos, top_other, neg = [], [], [], []
for target, qs in s["queries"].items():
    for q in qs:
        c = I @ np.array(Q[q])
        order = np.argsort(-c)
        rank = int(np.where(order == names.index(target))[0][0]) + 1
        rows.append((rank, q, target, names[order[0]], c[names.index(target)]))
        pos.append(c[names.index(target)]); top_other.append(max(c[i] for i in range(len(names)) if names[i] != target))
for q in s["negatives"]:
    neg.append(float(np.max(I @ np.array(Q[q]))))
ranks = np.array([x[0] for x in rows])
print(f"[{qkey}] R@1 {np.mean(ranks == 1):.2f}  R@3 {np.mean(ranks <= 3):.2f}  MRR {np.mean(1 / ranks):.3f}")
print(f"  own-image cos  mean {np.mean(pos):.3f} min {np.min(pos):.3f} | best other mean {np.mean(top_other):.3f} | negatives max {max(neg):.3f}")
for rank, q, t, got, c in rows:
    if rank > 1: print(f"  miss rank {rank}: {q!r} -> got {got}, want {t} ({c:.3f})")
