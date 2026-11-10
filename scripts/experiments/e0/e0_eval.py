#!/usr/bin/env python3
"""E0 wrapper around the committed scripts/retrieval_eval.py (PR #67, on main).

Imports the pinned worktree's retrieval_eval as a module and reuses its
evaluate() loop, MCP client, RSS sampler and metrics unchanged. It swaps only
module-level knobs (BASE, BINARY_ARMS, MCP_ARGS) per --profile, and feeds the
adapter-built question lists (prepare_data.py) directly so unscored questions
still count toward empty rate and capsule counts.

Profiles:
  bench   (E0 default) fresh `kimetsu init` config (level=deep, broker defaults)
          + the bench's MCP call args exactly (pilot_harness.retrieve):
          budget_tokens=16000 max_capsules=8 min_score=0 min_lexical_coverage=0
          abstain_evidence=0, min_semantic_score NOT passed -> config auto
          (-1 -> 0.35 for bge, project.rs:380-394). bench setup() toggles:
          use_user_brain=false session_dedupe=false warm_start=false.
  script  retrieval_eval.py's own BASE/MCP_ARGS (level=custom, every floor 0,
          incl. min_semantic_score=0). Not E0; kept for comparison only.

Resolved config per arm: `kimetsu config get <key>` on a scratch workspace with
the arm's project.toml and env, plus the request-level floor resolution the MCP
server applies (derived from mcp_server.rs:1008-1019 + project.rs:380-410).
"""
from __future__ import annotations

import argparse, importlib.util, json, os, resource, shutil, subprocess, sys, tempfile, time
from pathlib import Path

E0 = Path('/workspace/fuse/e0')
WT = E0 / 'kimetsu'
spec = importlib.util.spec_from_file_location('retrieval_eval', WT / 'scripts' / 'retrieval_eval.py')
rs = importlib.util.module_from_spec(spec)
spec.loader.exec_module(rs)

TINY = rs.RERANKER
PROFILES = {
    'bench': {
        'base': {
            'embedder.model': f'"{rs.EMBEDDER}"',
            'embedder.enabled': 'true',
            'kimetsu.use_user_brain': 'false',
            'broker.session_dedupe': 'false',
            'broker.warm_start': 'false',
        },
        'arms': [
            # reranker must be off too: mcp_server.rs:813 loads the CE from embedder.reranker
            # regardless of embedder.enabled, and level=deep sets tinybert.
            ('lexical', {'embedder.enabled': 'false', 'embedder.reranker': '"off"'},
             'embedder.enabled=false, embedder.reranker=off (FTS only)'),
            ('fused', {'embedder.reranker': '"off"'}, 'embedder.reranker=off (explicit opt-out outranks level=deep)'),
            ('fused_rerank_floor_0', {'embedder.reranker': f'"{TINY}"', 'broker.rerank_min_score': '0.0'},
             'tinybert, broker.rerank_min_score=0.0'),
            ('fused_rerank_floor_0.3', {'embedder.reranker': f'"{TINY}"', 'broker.rerank_min_score': '0.3'},
             'tinybert, broker.rerank_min_score=0.3 (= shipped default)'),
        ],
        'mcp': {'budget_tokens': 16000, 'max_capsules': 8, 'min_score': 0.0,
                'min_lexical_coverage': 0.0, 'abstain_evidence': 0.0},
    },
    'script': {'base': dict(rs.BASE), 'arms': list(rs.BINARY_ARMS), 'mcp': dict(rs.MCP_ARGS)},
}
CONFIG_KEYS = ['retrieval.level', 'embedder.enabled', 'embedder.model', 'embedder.reranker',
               'broker.max_capsules', 'broker.min_semantic_score', 'broker.min_lexical_coverage',
               'broker.abstain_min_score', 'broker.rerank_min_score', 'broker.fusion',
               'broker.normalization', 'broker.default_budget_tokens', 'broker.compress_capsules',
               'broker.session_dedupe', 'broker.warm_start', 'broker.ambient', 'kimetsu.use_user_brain']


def apply_profile(name):
    p = PROFILES[name]
    rs.BASE = dict(p['base'])
    rs.BINARY_ARMS = list(p['arms'])
    rs.MCP_ARGS = dict(p['mcp'])
    return p


def request_floors(mcp, cfg, embedder_on, model):
    """Mirror of BrainSession::resolve_request_floors for the MCP path."""
    def num(k):
        try:
            return float(cfg.get(k, {}).get('value'))
        except (TypeError, ValueError):
            return None
    out = {}
    sem = mcp.get('min_semantic_score')
    if sem is None:
        sem = num('broker.min_semantic_score')
        src = f'config broker.min_semantic_score={sem}'
    else:
        src = f'mcp arg {sem}'
    if sem is not None and sem < 0:
        eff = 0.35 if model.startswith('bge') else 0.0
        src += f' -> auto ({model}) {eff}'
        sem = eff
    out['min_semantic_score'] = {'effective': sem, 'source': src,
                                 'note': None if embedder_on else 'inert: no query embedding (embedder off)'}
    for key, cfgkey in [('min_lexical_coverage', 'broker.min_lexical_coverage'), ('abstain_evidence', 'broker.abstain_min_score')]:
        v = mcp.get(key)
        out[key] = {'effective': v, 'source': f'mcp arg {v}'} if v is not None else {'effective': num(cfgkey), 'source': f'config {cfgkey} (auto if <0)'}
    out['max_capsules'] = {'effective': mcp.get('max_capsules', 3), 'source': 'mcp arg' if 'max_capsules' in mcp else 'mcp default 3'}
    out['min_score'] = {'effective': mcp.get('min_score', 0.15), 'source': 'mcp arg' if 'min_score' in mcp else 'mcp default 0.15'}
    out['budget_tokens'] = {'effective': mcp.get('budget_tokens', 6000), 'source': 'mcp arg'}
    out['stage'] = {'effective': mcp.get('stage', 'localization'), 'source': 'mcp arg' if 'stage' in mcp else 'mcp default'}
    return out


def resolve_configs(binary, profile, out_path):
    p = PROFILES[profile]
    log = open(out_path.with_suffix('.log'), 'w')
    root = Path(tempfile.mkdtemp(prefix='kimetsu-e0-config-'))
    result = {'profile': profile, 'binary': binary, 'mcp_args': p['mcp'], 'base_patch': p['base'], 'arms': {}}
    try:
        rs.init_workspace(binary, root, log, [])
        result['init_project_toml'] = (root / '.kimetsu' / 'project.toml').read_text()
        for arm, patch, knob in p['arms']:
            w = root.parent / f'{root.name}-{arm}'
            shutil.copytree(root, w)
            rs.apply_config(w / '.kimetsu' / 'project.toml', patch)
            embedder_on = patch.get('embedder.enabled') != 'false'
            env = rs.child_env(rs.EMBEDDER if embedder_on else None)
            cfg = {}
            for key in CONFIG_KEYS:
                r = subprocess.run([binary, 'config', 'get', key], cwd=w, env=env, text=True, capture_output=True, timeout=60)
                cfg[key] = {'value': r.stdout.strip(), 'rc': r.returncode, **({'stderr': r.stderr.strip()[-300:]} if r.returncode else {})}
            model = rs.EMBEDDER if embedder_on else cfg['embedder.model']['value'].strip('"')
            result['arms'][arm] = {
                'knob': knob, 'patch': patch,
                'env': {k: env.get(k) for k in ('KIMETSU_BRAIN_EMBEDDER', 'KIMETSU_INTRA_THREADS', 'OMP_NUM_THREADS',
                                                 'KIMETSU_USER_BRAIN', 'KIMETSU_BRAIN_AMBIENT', 'FASTEMBED_CACHE_DIR', 'HF_HOME')},
                'project_toml': (w / '.kimetsu' / 'project.toml').read_text(),
                'config_get': cfg,
                'resolved': {
                    'retrieval_level': cfg['retrieval.level']['value'],
                    'embedder_enabled': cfg['embedder.enabled']['value'],
                    'embedder_model_effective': model + (' (KIMETSU_BRAIN_EMBEDDER env wins over config)' if embedder_on else ''),
                    'reranker': cfg['embedder.reranker']['value'],
                    'rerank_min_score': cfg['broker.rerank_min_score']['value'],
                    'fusion': cfg['broker.fusion']['value'],
                    'normalization': cfg['broker.normalization']['value'],
                    'max_capsules_config': cfg['broker.max_capsules']['value'],
                    'request_floors': request_floors(p['mcp'], cfg, embedder_on, model),
                },
            }
    finally:
        log.close()
        for d in root.parent.glob(root.name + '*'):
            shutil.rmtree(d, ignore_errors=True)
    out_path.write_text(json.dumps(result, indent=2, sort_keys=True))
    bad = [(a, k) for a, v in result['arms'].items() for k, c in v['config_get'].items() if c['rc']]
    if bad:
        print(f'WARNING: config get failed for {bad} (see {out_path})', file=sys.stderr)
    return result


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--bin', required=True)
    ap.add_argument('--dataset', type=Path, required=True)
    ap.add_argument('--name', required=True)
    ap.add_argument('--profile', choices=sorted(PROFILES), default='bench')
    ap.add_argument('--out', type=Path, required=True, help='JSONL rows (retrieval_eval format + dataset/scored)')
    ap.add_argument('--config-only', action='store_true')
    ap.add_argument('--limit', type=int, default=0)
    ap.add_argument('--dry-run', action='store_true', help='load data + profile, print plan, run nothing')
    a = ap.parse_args()
    apply_profile(a.profile)
    data = json.loads(a.dataset.read_text())['questions']
    if a.limit:
        data = data[:a.limit]
    scored = {q['id']: q['scored'] for q in data}
    if a.dry_run:
        print(json.dumps({'dataset': str(a.dataset), 'name': a.name, 'questions': len(data), 'scored': sum(scored.values()),
                          'profile': a.profile, 'base': rs.BASE, 'arms': [x[0] for x in rs.BINARY_ARMS] + ['vector (script extra)'],
                          'mcp_args': rs.MCP_ARGS, 'retrieval_eval': str(WT / 'scripts/retrieval_eval.py')}, indent=1))
        return 0
    binary = str(Path(a.bin).resolve())
    a.out.parent.mkdir(parents=True, exist_ok=True)
    resolve_configs(binary, a.profile, a.out.parent / f'resolved-config-{a.name}.json')
    if a.config_only:
        return 0
    t = time.time()
    qs = [{k: v for k, v in q.items() if k != 'scored'} for q in data]
    rc = rs.evaluate(binary, qs, a.out)
    rows = [json.loads(l) for l in a.out.read_text().splitlines()]
    with a.out.open('w') as f:
        for r in rows:
            r['dataset'] = a.name
            r['scored'] = scored[r['id']]
            f.write(json.dumps(r, sort_keys=True) + '\n')
    meta = {'dataset': a.name, 'questions': len(data), 'rows': len(rows), 'exit': rc, 'wall_s': round(time.time() - t, 1),
            'peak_child_rss_mib_rusage': round(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss / 1024, 1),
            'peak_self_rss_mib': round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1),
            'profile': a.profile, 'seed': None, 'seed_note': 'no component takes a seed; PYTHONHASHSEED pinned by runner'}
    (a.out.with_suffix('.meta.json')).write_text(json.dumps(meta, indent=2))
    print(json.dumps(meta))
    return rc


if __name__ == '__main__':
    sys.exit(main())
