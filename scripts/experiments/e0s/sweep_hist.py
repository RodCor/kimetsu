#!/usr/bin/env python3
"""Capsule-count histogram (0..8) and median per dataset/arm over ALL rows.
usage: sweep_hist.py RUN_DIR  -> RUN_DIR/capsule-hist.{json,md}"""
import json, statistics, sys
from pathlib import Path
run = Path(sys.argv[1]); out = {}; md = [f"# capsule histogram: {run.name} (all rows, scored or not)", ""]
for f in sorted(run.glob("*.jsonl")):
    rows = [json.loads(l) for l in f.read_text().splitlines() if l.strip()]
    for arm in dict.fromkeys(r["arm"] for r in rows):
        caps = [int(r.get("capsules") or 0) for r in rows if r["arm"] == arm]
        hist = [sum(1 for c in caps if c == k) for k in range(9)]
        over = sum(1 for c in caps if c > 8)
        out.setdefault(f.stem, {})[arm] = {"n": len(caps), "median": statistics.median(caps), "hist_0_8": hist, "over_8": over,
                                           "empty_n": sum(1 for r in rows if r["arm"] == arm and r.get("empty"))}
    md += [f"## {f.stem}", "", "| arm | n | median | empty | caps 0 1 2 3 4 5 6 7 8 |", "|---|---|---|---|---|"]
    md += [f"| {a} | {s['n']} | {s['median']} | {s['empty_n']} | {' '.join(map(str, s['hist_0_8']))} |" for a, s in out[f.stem].items()] + [""]
(run / "capsule-hist.json").write_text(json.dumps(out, indent=2))
(run / "capsule-hist.md").write_text("\n".join(md))
