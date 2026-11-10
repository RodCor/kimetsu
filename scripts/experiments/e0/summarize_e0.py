#!/usr/bin/env python3
"""E0 summary + determinism diff.

  summarize_e0.py summary RUN_DIR            -> RUN_DIR/summary.md + RUN_DIR/summary.json
  summarize_e0.py diff RUN1 RUN2 OUT.md      -> exit 0 identical, 1 differences

Recall@1/3/8 and MRR: mean over SCORED questions (>=1 gold chunk in haystack).
Empty rate and capsule distribution: over ALL questions (incl. unscored).
p90 = nearest-rank (ceil(0.9 n)). Scorer: scripts/retrieval_eval.py metrics
(retrieval only; not the official LoCoMo scorer, not the LME judge, no reader).
"""
import json, math, statistics, sys
from pathlib import Path

ARM_ORDER = ['lexical', 'vector', 'fused', 'fused_rerank_floor_0', 'fused_rerank_floor_0.3']
DIFF_KEYS = ['labels', 'rank', 'capsules', 'empty', 'excluded', 'error', 'skipped']


def load(run):
    rows = []
    for p in sorted(Path(run).glob('*.jsonl')):
        rows += [json.loads(l) for l in p.read_text().splitlines() if l.strip()]
    return rows


def p90(xs):
    s = sorted(xs)
    return s[max(0, math.ceil(0.9 * len(s)) - 1)]


def stats(group):
    sc = [r for r in group if r['scored']]
    caps = [r['capsules'] for r in group]
    rss = [r['peak_rss_mib'] for r in group if r.get('peak_rss_mib')]
    m = lambda f: (sum(f(r) for r in sc) / len(sc)) if sc else None
    return {'n': len(group), 'n_scored': len(sc),
            'r1': m(lambda r: r['recall']['@1']), 'r3': m(lambda r: r['recall']['@3']), 'r8': m(lambda r: r['recall']['@8']),
            'mrr': m(lambda r: r['mrr']),
            'empty_n': sum(r['empty'] for r in group), 'empty_rate': sum(r['empty'] for r in group) / len(group),
            'caps_min': min(caps), 'caps_median': statistics.median(caps), 'caps_p90': p90(caps), 'caps_max': max(caps),
            'caps_mean': sum(caps) / len(caps), 'errors': sum(1 for r in group if r.get('error')),
            'peak_rss_mib': max(rss) if rss else None}


def fmt(v, d=3):
    return '-' if v is None else (f'{v:.{d}f}' if isinstance(v, float) else str(v))


def summary(run):
    run = Path(run)
    rows = load(run)
    out, md = {}, [f'# E0 summary: {run.name}', '',
                   'scorer: scripts/retrieval_eval.py metrics (retrieval only, NOT an official judge, no reader). '
                   'recall/MRR over scored questions; empty rate and capsules over all questions. '
                   'vector arm = script extra (cosine over stored blobs, not a binary path).', '']
    for ds in sorted({r['dataset'] for r in rows}):
        d = [r for r in rows if r['dataset'] == ds]
        arms = sorted({r['arm'] for r in d}, key=lambda a: ARM_ORDER.index(a) if a in ARM_ORDER else 99)
        types = sorted({r['type'] for r in d})
        out[ds] = {}
        md += [f'## {ds}', '', '| arm | type | n | scored | R@1 | R@3 | R@8 | MRR | empty | empty rate | caps min/med/p90/max | caps mean | errors | peak RSS MiB |',
               '|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---|---:|---:|---:|']
        for arm in arms:
            out[ds][arm] = {'all': stats([r for r in d if r['arm'] == arm])}
            for t in types:
                g = [r for r in d if r['arm'] == arm and r['type'] == t]
                if g:
                    out[ds][arm][t] = stats(g)
            for t, s in out[ds][arm].items():
                md.append(f"| {arm} | {t} | {s['n']} | {s['n_scored']} | {fmt(s['r1'])} | {fmt(s['r3'])} | {fmt(s['r8'])} | {fmt(s['mrr'])} | "
                          f"{s['empty_n']} | {fmt(s['empty_rate'])} | {s['caps_min']}/{fmt(s['caps_median'],1)}/{s['caps_p90']}/{s['caps_max']} | "
                          f"{fmt(s['caps_mean'],2)} | {s['errors']} | {fmt(s['peak_rss_mib'],1)} |")
        md.append('')
    metas = {p.name: json.loads(p.read_text()) for p in sorted(run.glob('*.meta.json'))}
    md += ['## run meta', '', '```', json.dumps(metas, indent=1), '```', '']
    (run / 'summary.json').write_text(json.dumps({'stats': out, 'meta': metas}, indent=2))
    (run / 'summary.md').write_text('\n'.join(md))
    print('\n'.join(md))
    return 0


def diff(r1, r2, out):
    a = {(r['dataset'], r['id'], r['arm']): r for r in load(r1)}
    b = {(r['dataset'], r['id'], r['arm']): r for r in load(r2)}
    only1, only2 = sorted(set(a) - set(b)), sorted(set(b) - set(a))
    changed = []
    for k in sorted(set(a) & set(b)):
        dk = [f for f in DIFF_KEYS if a[k].get(f) != b[k].get(f)]
        if dk:
            changed.append((k, dk, {f: (a[k].get(f), b[k].get(f)) for f in dk}))
    ok = not (only1 or only2 or changed)
    md = ['# E0 determinism diff', '', f'run1: {r1}', f'run2: {r2}',
          f'compared fields: {DIFF_KEYS} (peak_rss_mib ignored)', '',
          f'rows run1={len(a)} run2={len(b)} only_in_run1={len(only1)} only_in_run2={len(only2)} changed={len(changed)}', '',
          f'**verdict: {"IDENTICAL" if ok else "NOT DETERMINISTIC"}**', '']
    if changed:
        by = {}
        for (ds, _, arm), _, _ in changed:
            by[(ds, arm)] = by.get((ds, arm), 0) + 1
        md += ['changed rows by dataset/arm:', ''] + [f'- {ds} / {arm}: {n}' for (ds, arm), n in sorted(by.items())] + ['', 'first 50:', '']
        md += [f'- {k}: {json.dumps(v)}' for k, _, v in changed[:50]]
    md += [f'- only run1: {k}' for k in only1[:50]] + [f'- only run2: {k}' for k in only2[:50]]
    Path(out).write_text('\n'.join(md) + '\n')
    print('\n'.join(md[:10]))
    return 0 if ok else 1


if __name__ == '__main__':
    if sys.argv[1] == 'summary':
        sys.exit(summary(sys.argv[2]))
    if sys.argv[1] == 'diff':
        sys.exit(diff(*sys.argv[2:5]))
    raise SystemExit(__doc__)
