#!/bin/bash
# MP-15d — variance + per-task timeline reporter over the
# stability-YYYY-MM-DD-{no-brain,brain} job dirs.
#
# Run any time:
#
#   bash kimetsu_harbor/stability-report.sh
#
# Outputs:
#   1. Per-day mean table (no-brain / brain side-by-side)
#   2. Summary statistics (mean / stdev / range) per leg
#   3. ±5pp gate-3 verdict per leg
#   4. Per-task win-rate over the series so we can see which
#      tasks are flaky and which are reliably won/lost.

set -u

JOBS_DIR=/home/kimetsu/harbor-jobs/jobs

python3 - <<'PY'
import glob, json, os, re, statistics
from collections import defaultdict

JOBS_DIR = "/home/kimetsu/harbor-jobs/jobs"

# Collect all stability-* job dirs, group by date.
runs = defaultdict(dict)   # runs[date][leg] = result.json path
pattern = re.compile(r"stability-(\d{4}-\d{2}-\d{2})-(no-brain|brain)$")
for d in sorted(glob.glob(f"{JOBS_DIR}/stability-*")):
    m = pattern.search(d)
    if not m:
        continue
    date, leg = m.group(1), m.group(2)
    rp = os.path.join(d, "result.json")
    if os.path.exists(rp):
        runs[date][leg] = rp

if not runs:
    print(f"no stability-* dirs under {JOBS_DIR}")
    raise SystemExit(0)

def metrics(path):
    d = json.load(open(path))
    s = d["stats"]
    ev = list(s["evals"].values())[0]
    rs = ev["reward_stats"]["reward"]
    wins  = set(w.split("__")[0] for w in rs.get("1.0", []))
    zeros = set(z.split("__")[0] for z in rs.get("0.0", []))
    errs  = {}
    for k, vs in ev.get("exception_stats", {}).items():
        for v in vs:
            errs[v.split("__")[0]] = k
    return {
        "mean": ev["metrics"][0]["mean"],
        "cost": s.get("cost_usd"),
        "wins": wins,
        "zeros": zeros,
        "errors": errs,
    }

print("=== per-day mean ===")
print(f"{'date':12s} {'no-brain':>10s} {'brain':>10s} {'no-brain $':>11s} {'brain $':>11s}")
print(f"{'-'*12} {'-'*10} {'-'*10} {'-'*11} {'-'*11}")
series = {"no-brain": [], "brain": []}
cost_series = {"no-brain": [], "brain": []}
per_task_winrate = {"no-brain": defaultdict(lambda: [0, 0]),  # [wins, total]
                    "brain":    defaultdict(lambda: [0, 0])}

for date in sorted(runs):
    cells = {}
    for leg in ("no-brain", "brain"):
        p = runs[date].get(leg)
        if not p:
            cells[leg] = ("?", "?")
            continue
        m = metrics(p)
        cells[leg] = (f"{m['mean']:.4f}", f"${m['cost']:.2f}" if m['cost'] is not None else "?")
        series[leg].append(m["mean"])
        if m["cost"] is not None:
            cost_series[leg].append(m["cost"])
        for t in (m["wins"] | m["zeros"] | set(m["errors"])):
            per_task_winrate[leg][t][1] += 1
        for t in m["wins"]:
            per_task_winrate[leg][t][0] += 1
    print(f"{date:12s} {cells['no-brain'][0]:>10s} {cells['brain'][0]:>10s} "
          f"{cells['no-brain'][1]:>11s} {cells['brain'][1]:>11s}")

print()
print("=== series statistics ===")
for leg in ("no-brain", "brain"):
    xs = series[leg]
    cs = cost_series[leg]
    if not xs:
        continue
    mn, mx = min(xs), max(xs)
    rng_pp = (mx - mn) * 100
    sd = statistics.stdev(xs) if len(xs) > 1 else 0.0
    mean = statistics.mean(xs)
    gate3 = "PASSES" if rng_pp <= 5.0 else "FAILS"
    cost_mean = statistics.mean(cs) if cs else 0.0
    print(f"  {leg:9s}: n={len(xs):2d}  mean={mean:.4f}  stdev={sd:.4f}  "
          f"range={rng_pp:.2f}pp  gate-3 ±5pp: {gate3}  avg cost=${cost_mean:.2f}")

print()
print("=== per-task win-rate over the series (only tasks with >=2 attempts shown) ===")
print(f"{'task':40s} {'no-brain (w/n)':>15s} {'brain (w/n)':>15s}")
all_tasks = sorted(set(per_task_winrate["no-brain"]) | set(per_task_winrate["brain"]))
for t in all_tasks:
    nb = per_task_winrate["no-brain"].get(t, [0, 0])
    br = per_task_winrate["brain"].get(t, [0, 0])
    if nb[1] < 2 and br[1] < 2:
        continue
    nb_s = f"{nb[0]}/{nb[1]}" if nb[1] else "-"
    br_s = f"{br[0]}/{br[1]}" if br[1] else "-"
    print(f"{t:40s} {nb_s:>15s} {br_s:>15s}")
PY
