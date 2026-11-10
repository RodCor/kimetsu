#!/usr/bin/env python3
"""E0 dataset adapter: bench inputs -> retrieval_eval fixture shape.

Reads ONLY copies under /workspace/fuse/e0/data/src (made by run_e0.sh / --copy).
Writes:
  data/locomo-conv26.json   {"questions":[{id,type,query,chunks:[{id,text,evidence}]}]}
  data/lme-oracle.json      same shape
  data/labels-report.json   counts, unmatched gold ids, sha256 of sources

Chunk text is the bench's exact chunk text (hybrid_worker.py / run_full.py):
  LoCoMo: full-dataset.json history line "[D1:1] <date> | <speaker>: <text>[ image caption]"
          split every 1200 chars (identical to hybrid_worker.py).
  LME:    "[<sid>:t<i>] <date> | <role>: <content>" split every 1200 chars (run_full.chunks).
The leading "[label] " is stripped here because retrieval_eval.chunk_text re-adds
"[<id>] ", so the stored text of the first piece is byte-identical to the bench.
Continuation pieces (>1200 chars) get id "<label>#p<k>" and that prefix; the bench
stored them without a label. Every piece of an evidence turn counts as gold.

Gold labels: LoCoMo qa[].evidence (normalized: split on ';', ',' and whitespace);
LME turns with has_answer=true. Questions with no gold present in the haystack are
kept (status unscored) so empty rate / capsule counts cover the full set; recall and
MRR are computed only over scored questions.
"""
import argparse, hashlib, json, re, shutil, sys
from pathlib import Path

E0 = Path('/workspace/fuse/e0')
SRC = E0 / 'data' / 'src'
BENCH = Path('/workspace/kimetsu-memory-bench')
SOURCES = {
    'full-dataset.json': BENCH / 'full-locomo' / 'full-dataset.json',
    'locomo10.json': BENCH / 'data' / 'locomo10.json',
    'longmemeval_oracle.json': BENCH / 'data' / 'longmemeval_oracle.json',
}
LABEL = re.compile(r'^\[([^\]]+)\] ')


def sha256(p):
    h = hashlib.sha256()
    with open(p, 'rb') as f:
        for b in iter(lambda: f.read(1 << 20), b''):
            h.update(b)
    return h.hexdigest()


def copy_sources():
    SRC.mkdir(parents=True, exist_ok=True)
    for name, src in SOURCES.items():
        dst = SRC / name
        if not dst.exists():
            shutil.copy2(src, dst)  # read from bench, write only under e0
        if sha256(dst) != sha256(src):
            raise SystemExit(f'copy of {name} differs from bench source {src}')


def pieces(label, text):
    out = []
    for k, off in enumerate(range(0, len(text), 1200)):
        piece = text[off:off + 1200]
        if k == 0:
            m = LABEL.match(piece)
            assert m and m.group(1) == label, (label, piece[:60])
            out.append({'id': label, 'text': piece[m.end():], 'turn': label})
        else:
            out.append({'id': f'{label}#p{k}', 'text': piece, 'turn': label})
    return out


def locomo(conv_id='conv-26'):
    groups = json.loads((SRC / 'full-dataset.json').read_text())
    g = next(x for x in groups if x['id'] == conv_id)
    raw = next(s for s in json.loads((SRC / 'locomo10.json').read_text()) if s['sample_id'] == conv_id)
    chunks = []
    for line in g['history']:
        m = LABEL.match(line)
        assert m, line[:80]
        chunks += pieces(m.group(1), line)
    turns = {c['turn'] for c in chunks}
    qs, unmatched, report_q = [], [], []
    for q in g['questions']:
        ev = q.get('evidence') or []
        if isinstance(ev, str):
            ev = [ev]
        gold_turns = {t for item in ev for t in re.split(r'[;,\s]+', str(item)) if t}
        missing = sorted(gold_turns - turns)
        if missing:
            unmatched.append({'id': q['id'], 'evidence': ev, 'missing': missing})
        present = gold_turns & turns
        qs.append({'id': q['id'], 'type': str(q['category']), 'query': q['question'],
                   'scored': bool(present),
                   'chunks': [{'id': c['id'], 'text': c['text'], 'evidence': c['turn'] in present} for c in chunks]})
    assert len(g['questions']) == len(raw['qa']), (len(g['questions']), len(raw['qa']))
    for a, b in zip(g['questions'], raw['qa']):
        assert a['question'] == b['question'], (a['question'], b['question'])
    return qs, {'conversation': conv_id, 'questions': len(qs), 'scored': sum(q['scored'] for q in qs),
                'unscored_ids': [q['id'] for q in qs if not q['scored']], 'chunks': len(chunks),
                'turns': len(turns), 'split_pieces': len(chunks) - len(turns),
                'by_category': count(qs), 'evidence_ids_not_in_haystack': unmatched}


def lme():
    rows = json.loads((SRC / 'longmemeval_oracle.json').read_text())
    qs = []
    split = 0
    for r in rows:
        chunks = []
        for sid, date, ses in zip(r['haystack_session_ids'], r['haystack_dates'], r['haystack_sessions']):
            for i, t in enumerate(ses):
                label = f'{sid}:t{i}'
                text = f"[{label}] {date} | {t['role']}: {t['content']}"
                ps = pieces(label, text)
                split += len(ps) - 1
                chunks += [{'id': p['id'], 'text': p['text'], 'evidence': bool(t.get('has_answer'))} for p in ps]
        qid = r['question_id']
        qtype = 'abstention' if qid.endswith('_abs') else r['question_type']
        qs.append({'id': qid, 'type': qtype, 'query': r['question'],
                   'scored': any(c['evidence'] for c in chunks), 'chunks': chunks})
    n_chunks = [len(q['chunks']) for q in qs]
    return qs, {'questions': len(qs), 'scored': sum(q['scored'] for q in qs),
                'unscored_by_type': count([q for q in qs if not q['scored']]),
                'by_type': count(qs), 'chunks_total': sum(n_chunks),
                'chunks_per_question_min_median_max': [min(n_chunks), sorted(n_chunks)[len(n_chunks) // 2], max(n_chunks)],
                'split_pieces': split}


def count(qs):
    out = {}
    for q in qs:
        d = out.setdefault(q['type'], {'n': 0, 'scored': 0})
        d['n'] += 1
        d['scored'] += int(q['scored'])
    return dict(sorted(out.items()))


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument('--copy', action='store_true', help='copy bench sources into data/src first')
    a = ap.parse_args()
    if a.copy:
        copy_sources()
    lq, lr = locomo()
    mq, mr = lme()
    (E0 / 'data' / 'locomo-conv26.json').write_text(json.dumps({'source': 'full-locomo/full-dataset.json conv-26 (bench chunk text)', 'questions': lq}))
    (E0 / 'data' / 'lme-oracle.json').write_text(json.dumps({'source': 'data/longmemeval_oracle.json (run_full.chunks text)', 'questions': mq}))
    report = {'locomo_conv26': lr, 'lme_oracle': mr,
              'sources_sha256': {n: sha256(SRC / n) for n in SOURCES},
              'outputs_sha256': {n: sha256(E0 / 'data' / n) for n in ('locomo-conv26.json', 'lme-oracle.json')}}
    (E0 / 'data' / 'labels-report.json').write_text(json.dumps(report, indent=2))
    print(json.dumps({k: {kk: vv for kk, vv in v.items() if kk not in ('evidence_ids_not_in_haystack',)} if isinstance(v, dict) and k != 'sources_sha256' and k != 'outputs_sha256' else v for k, v in report.items()}, indent=1))
    print('evidence ids not in haystack (locomo):', json.dumps(lr['evidence_ids_not_in_haystack']))


if __name__ == '__main__':
    sys.exit(main())
