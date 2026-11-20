#!/usr/bin/env python3
"""Offline hybrid fusion rescore for Kimetsu issue #68.

Rebuilds today's fusion formula without calling Rust for retrieval, then
(if a sanity gate passes) rescores the fused pool with a TM2C2-scaled
lexical term (bm25/max_q) at alpha in {0.5, 0.6, 0.7, 0.8}.

This script does not launch the E0 sweep. Ingest (only when a cached brain
is missing) uses the same binary and the same add-batch path as
scripts/e0_eval.py / scripts/retrieval_eval.py.

Sanity gate
-----------
Reproduce the production formula, then take the admitted top-8 *set* and
compare it to the fill_to_cap arm (`tinybert_floor_0_fill_to_cap`) in the
run1 JSONL. The match rate must be >= 95% of rows or the process exits
nonzero and the alpha arms are not scored.

Today's formula (context.rs memory_row_to_candidate, fusion.rs union_max):

  lexical_overlap = matches / len(query_tokens) over haystack "{kind} {text}"
  FTS lexical term = max((-bm25).max(0), lexical_overlap)
  ANN lexical term = lexical_overlap
  raw = (1-alpha)*lexical_term + alpha*((cos+1)/2)   with alpha=0.5
  then usefulness/trust/decay multipliers, union_max, per-kind
  normalize_and_score, the auto bge semantic floor 0.35, embedding MMR
  (lambda 0.7), Jaccard MMR, then admit max_capsules=8 under
  budget_tokens=16000 (capsule budget 8000).

fill_to_cap still runs TinyBERT, but with rerank_min_score=0 and
max_capsules already 8 (RERANK_POOL is 6), so TinyBERT reorders and
should not change the admitted set. The gate compares sets, not order.

Brains
------
e0_eval builds one TemporaryDirectory brain per question
(`kimetsu-eval-{id}-/.kimetsu/brain.db`) under TMPDIR and deletes it
after the question. /workspace/fuse/e0s/tmp/run{1,2} only keeps
`kimetsu-eval-logs-*`. The sqlite files were not persisted.

This script therefore rebuilds brains with /workspace/fuse/e0s/bin/kimetsu-e0s
and caches them under --brains. locomo-conv26 has one shared chunk set
(419 memories, reused for all 199 questions). lme-oracle has 495 distinct
chunk sets. A full lme ingest is the expensive part: the original conv-26
eval ingested that corpus once per question and took hours on run2
(wall 14197.9 s). With chunk-set caching, conv-26 ingest should be a
single add-batch; lme-oracle still needs hundreds of add-batch calls.

Example (box paths; do not run until the command is reviewed):

    python3 scripts/experiments/e0s/fusion_rescore.py \\
      --bin /workspace/fuse/e0s/bin/kimetsu-e0s \\
      --data /workspace/fuse/e0/data \\
      --results /workspace/fuse/e0s/results/run1 \\
      --brains /workspace/fuse/e0s/rescore/brains \\
      --out /workspace/fuse/e0s/rescore/out
"""

from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import math
import os
import sqlite3
import struct
import sys
from dataclasses import dataclass, field
from datetime import datetime, timezone
from pathlib import Path

TOP_K = 8
ANN_LIMIT = 80
FTS_LIMIT = 80
FTS_TOKEN_CAP = 12
ALPHA_DEFAULT = 0.5
ALPHA_ARMS = (0.5, 0.6, 0.7, 0.8)
MMR_LAMBDA = 0.7
SEMANTIC_FLOOR_BGE = 0.35
BUDGET_TOKENS = 16000
CAPSULE_BUDGET = BUDGET_TOKENS // 2
MAX_CAPSULES = 8
HALF_LIFE_DAYS = 30.0
LOC_WEIGHTS = (0.70, 0.10, 0.10, 0.10)  # relevance, confidence, freshness, scope
SCOPE_WEIGHT_PROJECT = 0.7
SUPERSESSION_MIN_COSINE = 0.85
SUPERSESSION_CUE_MIN_COSINE = 0.82
SUPERSESSION_CUE_MIN_SHARED_TOKENS = 3
SUPERSESSION_CUE_MIN_LEXICAL_OVERLAP = 0.35
SUPERSESSION_PENALTY = 0.80
SUPERSESSION_MIN_AGE_GAP_SECS = 1.0
USEFULNESS_BOOST_CAP = 0.10
MULTIPLIER_MIN = 0.5
MULTIPLIER_MAX = 1.5
FULL_CONFIDENCE_USES = 3
FILL_ARM = "tinybert_floor_0_fill_to_cap"
RERANK_OFF_ARM = "rerank_off"
DATASETS = ("locomo-conv26", "lme-oracle")
BINARY_SHA256 = "262f17def1cfd67f07aaec325a2f2d23aa058d393adefc463d4bfc4c33391985"
DEFAULT_SANITY_THRESHOLD = 0.95
CLASS_HINTS = (
    (
        ("build", "compile", "make", "cargo", "cmake", "configure", "install",
         "train", "benchmark", "test suite", "ray trace", "render"),
        ("shell_background", "shell_status", "shell_output", "shell_stop", "long_running"),
    ),
    (
        ("edit", "modify", "change", "fix", "update", "patch", "refactor", "rename"),
        ("edit_file", "apply_patch", "old_string", "new_string"),
    ),
    (
        ("read", "inspect", "review", "analyze", "examine", "view", "show"),
        ("read_file", "offset", "limit", "multi_read"),
    ),
    (
        ("find", "locate", "search", "look up", "discover", "list"),
        ("glob", "search_files", "list_files"),
    ),
    (
        ("plan", "step", "checklist", "todo", "task list", "phase"),
        ("plan", "todos"),
    ),
    (
        ("verify", "check", "ensure", "validate", "pass test", "verifier"),
        ("finish", "verifier", "verification"),
    ),
    (
        ("image", "png", "jpeg", "jpg", "pdf", "diagram", "screenshot"),
        ("view_image", "base64", "sha256"),
    ),
    (("delete", "remove", "rm "), ("delete_file", "recursive")),
    (("rename", "move file", "mv "), ("move_file",)),
)
SUPERSESSION_CUES = (
    " now ", " no longer ", " renamed from ", " switched from ", " as of ",
    " currently ", " after the ", " supersedes ", " replaced by ", " silently breaks ",
)
TRUST_BY_SOURCE = {
    "pack": 0.85,
    "remote": 0.90,
    "sync": 0.90,
    "org": 0.90,
    "distiller": 0.95,
    "distilled": 0.95,
    "reflection": 0.95,
    "staple": 1.0,
    "merge": 1.0,
    "consolidation": 1.0,
}


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def find_scripts(explicit: Path | None) -> Path:
    candidates = []
    if explicit is not None:
        candidates.append(explicit)
    here = Path(__file__).resolve()
    candidates.extend(
        [
            here.parents[2] if len(here.parents) >= 2 else here.parent,  # repo: scripts/experiments/e0s
            Path("/workspace/fuse/e0s/kimetsu/scripts"),
            Path("/workspace/fuse/kimetsu-e0s-artifacts/scripts"),
        ]
    )
    for candidate in candidates:
        if (candidate / "retrieval_eval.py").is_file():
            return candidate
    raise SystemExit("could not find scripts/retrieval_eval.py (pass --scripts)")


def load_modules(scripts: Path):
    sys.path.insert(0, str(scripts))
    def _load(name: str, path: Path):
        spec = importlib.util.spec_from_file_location(name, path)
        if spec is None or spec.loader is None:
            raise SystemExit(f"could not load {path}")
        module = importlib.util.module_from_spec(spec)
        spec.loader.exec_module(module)
        return module

    retrieval = _load("retrieval_eval", scripts / "retrieval_eval.py")
    profiles = _load("retrieval_eval_profiles", scripts / "retrieval_eval_profiles.py")
    profiles.capture_script(retrieval)
    profiles.apply_to(retrieval, "bench-sweep")
    return retrieval, profiles


def light_stem(token: str) -> str:
    stem = token
    for suffix in ("ing", "ed", "es", "s"):
        if token.endswith(suffix) and len(token) - len(suffix) >= 4:
            stem = token[: -len(suffix)]
            break
    if len(stem) >= 5:
        trimmed = stem[:-1] if stem.endswith(("y", "i")) else None
        if trimmed and trimmed[-1] not in "aeiou":
            return trimmed
    return stem


def query_tokens(query: str) -> list[str]:
    tokens = [
        light_stem(part.lower())
        for part in "".join(ch if ch.isalnum() or ch == "_" else " " for ch in query).split()
        if len(part) >= 2
    ]
    lower = query.lower()
    for triggers, expansions in CLASS_HINTS:
        if any(trigger in lower for trigger in triggers):
            tokens.extend(expansions)
    return tokens


def fts_query(query: str) -> str | None:
    tokens = query_tokens(query)
    if not tokens:
        return None
    return " OR ".join(f"{token}*" for token in tokens[:FTS_TOKEN_CAP])


def lexical_relevance(tokens: list[str], haystack: str) -> float:
    if not tokens:
        return 0.0
    lowered = haystack.lower()
    matches = sum(1 for token in tokens if token in lowered)
    return matches / len(tokens)


def cosine(a: list[float], b: list[float]) -> float:
    if not a or not b or len(a) != len(b):
        return 0.0
    dot = sum(x * y for x, y in zip(a, b))
    na = sum(x * x for x in a) ** 0.5
    nb = sum(y * y for y in b) ** 0.5
    if na == 0.0 or nb == 0.0:
        return 0.0
    return dot / (na * nb)


def decode_embedding(blob: bytes | None) -> list[float] | None:
    if not blob or len(blob) % 4:
        return None
    return list(struct.unpack(f"<{len(blob) // 4}f", blob))


def parse_rfc3339(value: str | None) -> datetime | None:
    if not value:
        return None
    text = value.replace("Z", "+00:00")
    try:
        parsed = datetime.fromisoformat(text)
    except ValueError:
        return None
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=timezone.utc)
    return parsed.astimezone(timezone.utc)


def freshness(created_at: str) -> float:
    parsed = parse_rfc3339(created_at)
    if parsed is None:
        return 0.5
    age_days = max(0.0, (datetime.now(timezone.utc) - parsed).total_seconds()) / 86400.0
    return max(0.0, min(1.0, math.exp(-math.log(2.0) * age_days / 30.0)))


def usefulness_multiplier(usefulness_score: float, use_count: int) -> float:
    if use_count == 0:
        return 1.0
    ratio = usefulness_score / use_count
    normalized = max(0.0, min(1.0, (ratio + 1.0) / 2.0))
    full = MULTIPLIER_MIN + normalized * (MULTIPLIER_MAX - MULTIPLIER_MIN)
    confidence = min(1.0, use_count / FULL_CONFIDENCE_USES)
    return 1.0 * (1.0 - confidence) + full * confidence


def usefulness_decay(last_useful_at: str | None, created_at: str, half_life_days: float) -> float:
    if half_life_days <= 0.0:
        return 1.0
    parsed = parse_rfc3339(last_useful_at or created_at)
    if parsed is None:
        return 1.0
    age_days = max(0.0, (datetime.now(timezone.utc) - parsed).total_seconds()) / 86400.0
    return max(0.0, min(1.0, math.exp(-math.log(2.0) * age_days / half_life_days)))


def apply_usefulness_boost(raw_relevance: float, multiplier: float) -> float:
    if multiplier <= 1.0:
        return raw_relevance * multiplier
    return min(raw_relevance * multiplier, raw_relevance + USEFULNESS_BOOST_CAP)


def trust_multiplier(snapshot: str | None) -> float:
    if not snapshot:
        return 1.0
    try:
        payload = json.loads(snapshot)
    except json.JSONDecodeError:
        return 1.0
    source = ""
    if isinstance(payload, dict):
        source = str(payload.get("source") or "")
    return TRUST_BY_SOURCE.get(source, 1.0)


def estimate_tokens(text: str) -> int:
    return math.ceil(len(text.split()) * 1.33) + 8


def summary_token_set(text: str) -> set[str]:
    parts = "".join(ch if ch.isalnum() or ch == "_" else " " for ch in text).split()
    return {part.lower() for part in parts if len(part) >= 3}


def jaccard(a: set[str], b: set[str]) -> float:
    if not a and not b:
        return 0.0
    union = len(a | b)
    return len(a & b) / max(1, union)


def has_explicit_supersession_signal(text: str) -> bool:
    words = "".join(ch.lower() if ch.isalnum() else " " for ch in text)
    padded = " " + " ".join(words.split()) + " "
    return any(cue in padded for cue in SUPERSESSION_CUES)


def content_tokens(text: str) -> list[str]:
    return query_tokens(text)


def has_supersession_lexical_identity(a: str, b: str) -> bool:
    left = content_tokens(a)
    right = content_tokens(b)
    smaller = min(len(left), len(right))
    if smaller == 0:
        return False
    shared = sum(1 for token in left if token in right)
    return (
        shared >= SUPERSESSION_CUE_MIN_SHARED_TOKENS
        and shared / smaller >= SUPERSESSION_CUE_MIN_LEXICAL_OVERLAP
    )


@dataclass
class MemoryRow:
    memory_id: str
    scope: str
    kind: str
    text: str
    confidence: float
    created_at: str
    use_count: int
    usefulness_score: float
    last_useful_at: str | None
    provenance_snapshot: str | None
    embedding: list[float] | None


@dataclass
class Candidate:
    key: str
    label: str
    kind: str
    memory_kind: str
    text: str
    summary: str
    raw_relevance: float
    cosine: float | None
    embedding: list[float] | None
    confidence: float
    freshness: float
    scope_weight: float
    created_at: str
    lexical_term: float
    bm25_term: float | None
    token_estimate: int
    relevance: float = 0.0
    score: float = 0.0
    superseded_hint: bool = False


def open_brain(db_path: Path) -> sqlite3.Connection:
    uri = f"file:{db_path}?mode=ro"
    return sqlite3.connect(uri, uri=True)


def load_memories(conn: sqlite3.Connection) -> dict[str, MemoryRow]:
    rows = conn.execute(
        """
        SELECT memory_id, scope, kind, text, confidence, created_at,
               use_count, usefulness_score, last_useful_at,
               provenance_snapshot_json, embedding
        FROM memories
        WHERE invalidated_at IS NULL
          AND superseded_by IS NULL
          AND (valid_from IS NULL OR julianday(valid_from) <= julianday('now'))
          AND (valid_to IS NULL OR julianday(valid_to) > julianday('now'))
        """
    ).fetchall()
    out: dict[str, MemoryRow] = {}
    for row in rows:
        out[row[0]] = MemoryRow(
            memory_id=row[0],
            scope=row[1],
            kind=row[2],
            text=row[3],
            confidence=float(row[4]),
            created_at=row[5],
            use_count=int(row[6]),
            usefulness_score=float(row[7]),
            last_useful_at=row[8],
            provenance_snapshot=row[9],
            embedding=decode_embedding(row[10]),
        )
    return out


def fts_top(conn: sqlite3.Connection, match: str, limit: int) -> list[tuple[str, float]]:
    rows = conn.execute(
        """
        SELECT m.memory_id, bm25(memories_fts) AS rank
        FROM memories_fts
        JOIN memories m ON m.memory_id = memories_fts.memory_id
        WHERE m.invalidated_at IS NULL
          AND m.superseded_by IS NULL
          AND (m.valid_from IS NULL OR julianday(m.valid_from) <= julianday('now'))
          AND (m.valid_to IS NULL OR julianday(m.valid_to) > julianday('now'))
          AND memories_fts MATCH ?
        ORDER BY rank
        LIMIT ?
        """,
        (match, limit),
    ).fetchall()
    return [(memory_id, float(rank)) for memory_id, rank in rows]


def repo_counts(conn: sqlite3.Connection) -> tuple[int, int]:
    def count(table: str) -> int:
        try:
            return int(conn.execute(f"SELECT COUNT(*) FROM {table}").fetchone()[0])
        except sqlite3.DatabaseError:
            return 0
    return count("repo_files"), count("repo_manifests")


def hybrid_raw(lexical_term: float, cosine_score: float | None, alpha: float) -> float:
    if cosine_score is None:
        return lexical_term
    normalized = max(0.0, min(1.0, (cosine_score + 1.0) * 0.5))
    return (1.0 - alpha) * lexical_term + alpha * normalized


def apply_multipliers(row: MemoryRow, raw: float) -> float:
    raw_mult = usefulness_multiplier(row.usefulness_score, row.use_count)
    decay = usefulness_decay(row.last_useful_at, row.created_at, HALF_LIFE_DAYS)
    multiplier = 1.0 + (raw_mult - 1.0) * decay
    biased = apply_usefulness_boost(raw, multiplier)
    return biased * trust_multiplier(row.provenance_snapshot)


def make_candidate(
    row: MemoryRow,
    label: str,
    lexical_term: float,
    bm25_term: float | None,
    cosine_score: float | None,
    alpha: float,
    tokens: list[str],
) -> Candidate | None:
    raw = hybrid_raw(lexical_term, cosine_score, alpha)
    if raw <= 0.0 and tokens:
        return None
    raw = apply_multipliers(row, raw)
    scope_label = "project" if row.scope == "project" else row.scope
    summary = f"{scope_label}:{row.kind} - {row.text}"
    return Candidate(
        key=f"memory:{row.memory_id}",
        label=label,
        kind="memory",
        memory_kind=row.kind,
        text=row.text,
        summary=summary,
        raw_relevance=raw,
        cosine=cosine_score,
        embedding=row.embedding,
        confidence=row.confidence,
        freshness=freshness(row.created_at),
        scope_weight=SCOPE_WEIGHT_PROJECT if row.scope == "project" else 0.3,
        created_at=row.created_at,
        lexical_term=lexical_term,
        bm25_term=bm25_term,
        token_estimate=estimate_tokens(row.text),
    )


def union_max(lists: list[list[Candidate]]) -> list[Candidate]:
    seen: dict[str, int] = {}
    merged: list[Candidate] = []
    for group in lists:
        for candidate in group:
            idx = seen.get(candidate.key)
            if idx is None:
                seen[candidate.key] = len(merged)
                merged.append(candidate)
            elif candidate.raw_relevance > merged[idx].raw_relevance:
                merged[idx] = candidate
    return merged


def normalize_and_score(candidates: list[Candidate]) -> None:
    w_rel, w_conf, w_fresh, w_scope = LOC_WEIGHTS
    max_by_kind: dict[str, float] = {}
    for candidate in candidates:
        kind = candidate.kind
        max_by_kind[kind] = max(max_by_kind.get(kind, candidate.raw_relevance), candidate.raw_relevance)
    for candidate in candidates:
        peak = max_by_kind.get(candidate.kind, 0.0)
        if peak <= sys.float_info.epsilon:
            relevance = 1.0 if candidate.raw_relevance > 0.0 else 0.0
        else:
            relevance = max(0.0, min(1.0, candidate.raw_relevance / peak))
        candidate.relevance = relevance
        candidate.score = (
            w_rel * relevance
            + w_conf * candidate.confidence
            + w_fresh * candidate.freshness
            + w_scope * candidate.scope_weight
        )


def apply_supersession_penalty(candidates: list[Candidate]) -> None:
    parsed = [parse_rfc3339(c.created_at) if c.kind == "memory" and c.embedding else None for c in candidates]
    penalized = [False] * len(candidates)
    for i, ti in enumerate(parsed):
        if ti is None:
            continue
        for j in range(i + 1, len(candidates)):
            tj = parsed[j]
            if tj is None or candidates[i].embedding is None or candidates[j].embedding is None:
                continue
            signals = (
                has_explicit_supersession_signal(candidates[i].summary),
                has_explicit_supersession_signal(candidates[j].summary),
            )
            exactly_one = signals[0] ^ signals[1]
            similarity = cosine(candidates[i].embedding or [], candidates[j].embedding or [])
            cue_related = exactly_one and (
                similarity >= SUPERSESSION_CUE_MIN_COSINE
                or has_supersession_lexical_identity(candidates[i].summary, candidates[j].summary)
            )
            if similarity < SUPERSESSION_MIN_COSINE and not cue_related:
                continue
            gap = abs((ti - tj).total_seconds())
            subsecond = gap < SUPERSESSION_MIN_AGE_GAP_SECS
            if subsecond:
                if signals == (True, False):
                    older = j
                elif signals == (False, True):
                    older = i
                else:
                    continue
            else:
                if ti < tj:
                    older = i
                elif ti > tj:
                    older = j
                else:
                    continue
            if similarity < SUPERSESSION_CUE_MIN_COSINE:
                replacement = j if older == i else i
                replacement_has_signal = signals[0] if replacement == i else signals[1]
                rc, oc = candidates[replacement].cosine, candidates[older].cosine
                query_favors = rc is not None and oc is not None and rc >= oc
                if not replacement_has_signal or not query_favors:
                    continue
            if not penalized[older]:
                penalized[older] = True
                candidates[older].score *= SUPERSESSION_PENALTY
                candidates[older].superseded_hint = True


def sort_candidates(candidates: list[Candidate]) -> list[Candidate]:
    return sorted(
        candidates,
        key=lambda c: (-c.score, -c.freshness, c.key),
    )


def apply_embedding_mmr(sorted_cands: list[Candidate], lambda_weight: float = MMR_LAMBDA) -> list[Candidate]:
    if len(sorted_cands) <= 1:
        return list(sorted_cands)
    summaries = [summary_token_set(c.summary) for c in sorted_cands]
    picked: list[int] = []
    remaining = list(range(len(sorted_cands)))
    picked.append(remaining.pop(0))
    while remaining:
        best_i = 0
        best_score = -math.inf
        for i, cand in enumerate(remaining):
            max_overlap = 0.0
            for p in picked:
                if sorted_cands[cand].embedding and sorted_cands[p].embedding:
                    raw = max(0.0, cosine(sorted_cands[cand].embedding or [], sorted_cands[p].embedding or []))
                else:
                    raw = jaccard(summaries[cand], summaries[p])
                overlap = raw if sorted_cands[cand].kind == sorted_cands[p].kind else raw * 0.5
                if overlap > max_overlap:
                    max_overlap = overlap
            mmr = lambda_weight * sorted_cands[cand].score - (1.0 - lambda_weight) * max_overlap
            if mmr > best_score:
                best_score = mmr
                best_i = i
        picked.append(remaining.pop(best_i))
    return [sorted_cands[i] for i in picked]


def apply_jaccard_mmr(sorted_cands: list[Candidate], lambda_weight: float = MMR_LAMBDA) -> list[Candidate]:
    if len(sorted_cands) <= 1:
        return list(sorted_cands)
    summaries = [summary_token_set(c.summary) for c in sorted_cands]
    picked: list[int] = []
    remaining = list(range(len(sorted_cands)))
    picked.append(remaining.pop(0))
    while remaining:
        best_i = 0
        best_score = -math.inf
        for i, cand in enumerate(remaining):
            max_overlap = 0.0
            for p in picked:
                raw = jaccard(summaries[cand], summaries[p])
                overlap = raw if sorted_cands[cand].kind == sorted_cands[p].kind else raw * 0.5
                if overlap > max_overlap:
                    max_overlap = overlap
            mmr = lambda_weight * sorted_cands[cand].score - (1.0 - lambda_weight) * max_overlap
            if mmr > best_score:
                best_score = mmr
                best_i = i
        picked.append(remaining.pop(best_i))
    return [sorted_cands[i] for i in picked]


def admit(candidates: list[Candidate], max_capsules: int = MAX_CAPSULES, budget: int = CAPSULE_BUDGET) -> list[Candidate]:
    included: list[Candidate] = []
    used = 0
    for candidate in candidates:
        if max_capsules > 0 and len(included) >= max_capsules:
            break
        if used + candidate.token_estimate <= budget:
            used += candidate.token_estimate
            included.append(candidate)
    return included


def label_of(row: MemoryRow, known_ids: set[str], rs) -> str:
    return rs.evidence_label(row.text, known_ids)


def build_lists(
    memories: dict[str, MemoryRow],
    fts_hits: list[tuple[str, float]],
    query_vector: list[float],
    tokens: list[str],
    known_ids: set[str],
    rs,
    alpha: float,
    lexical_mode: str,
) -> tuple[list[Candidate], list[Candidate], float]:
    """lexical_mode: 'current' or 'tm2c2'."""
    fts_relevance = {memory_id: max(-rank, 0.0) for memory_id, rank in fts_hits}
    max_q = max(fts_relevance.values()) if fts_relevance else 0.0

    def lexical_for(row: MemoryRow, from_fts: bool) -> tuple[float, float | None]:
        overlap = lexical_relevance(tokens, f"{row.kind} {row.text}")
        bm25_term = fts_relevance.get(row.memory_id)
        if lexical_mode == "tm2c2":
            if bm25_term is None or max_q == 0.0:
                return 0.0, bm25_term
            return bm25_term / max_q, bm25_term
        if from_fts and bm25_term is not None:
            return max(bm25_term, overlap), bm25_term
        return overlap, bm25_term

    def scored(row: MemoryRow, from_fts: bool) -> Candidate | None:
        lex, bm25_term = lexical_for(row, from_fts)
        cos = cosine(query_vector, row.embedding) if row.embedding else None
        return make_candidate(row, label_of(row, known_ids, rs), lex, bm25_term, cos, alpha, tokens)

    fts_cands: list[Candidate] = []
    for memory_id, _rank in fts_hits:
        row = memories.get(memory_id)
        if row is None:
            continue
        candidate = scored(row, True)
        if candidate is not None:
            fts_cands.append(candidate)

    scored_ann = []
    for row in memories.values():
        if not row.embedding:
            continue
        scored_ann.append((cosine(query_vector, row.embedding), row))
    scored_ann.sort(key=lambda item: (-item[0], item[1].memory_id))
    ann_cands: list[Candidate] = []
    for _cos, row in scored_ann[:ANN_LIMIT]:
        candidate = scored(row, False)
        if candidate is not None:
            ann_cands.append(candidate)
    return fts_cands, ann_cands, max_q


def rank_pipeline(candidates: list[Candidate], semantic_floor: float) -> list[Candidate]:
    normalize_and_score(candidates)
    apply_supersession_penalty(candidates)
    if semantic_floor > 0.0:
        candidates = [
            c
            for c in candidates
            if c.cosine is None or c.cosine >= semantic_floor
        ]
    ordered = sort_candidates(candidates)
    ordered = apply_embedding_mmr(ordered)
    ordered = apply_jaccard_mmr(ordered)
    return admit(ordered)


def chunk_signature(chunks: list[dict]) -> str:
    payload = json.dumps([(c["id"], c["text"]) for c in chunks], separators=(",", ":"))
    return hashlib.sha256(payload.encode()).hexdigest()


def existing_brain_hint() -> str:
    tmp = Path("/workspace/fuse/e0s/tmp")
    found: list[str] = []
    if tmp.is_dir():
        for path in tmp.rglob("brain.db"):
            found.append(str(path))
    if found:
        return "found persisted brains: " + ", ".join(found[:8])
    run1 = tmp / "run1"
    run2 = tmp / "run2"
    notes = []
    for folder in (run1, run2):
        if folder.is_dir():
            children = sorted(p.name for p in folder.iterdir())
            notes.append(f"{folder} has {children}, no brain.db")
    if notes:
        return "no persisted brains; " + "; ".join(notes)
    return "no TMPDIR leftovers under /workspace/fuse/e0s/tmp"


def configure_eval_env(model_cache: Path | None) -> None:
    os.environ.setdefault("PYTHONHASHSEED", "0")
    os.environ["KIMETSU_USER_BRAIN"] = "0"
    os.environ["KIMETSU_BRAIN_AMBIENT"] = "off"
    os.environ["KIMETSU_INTRA_THREADS"] = "1"
    os.environ["OMP_NUM_THREADS"] = "1"
    os.environ["KIMETSU_BRAIN_LOG_RETRIEVAL"] = "0"
    cache = model_cache or Path("/workspace/fuse/e0s/model-cache")
    if cache.is_dir():
        os.environ.setdefault("FASTEMBED_CACHE_DIR", str(cache))
        os.environ.setdefault("HF_HOME", str(cache))


def ensure_brain(
    binary: Path,
    question: dict,
    brains_dir: Path,
    rs,
    log,
    peaks: list[int],
    cache: dict[str, Path],
) -> Path:
    signature = chunk_signature(question["chunks"])
    if signature in cache:
        return cache[signature]
    dest = brains_dir / signature
    db = dest / ".kimetsu" / "brain.db"
    if db.is_file():
        cache[signature] = dest
        return dest
    if dest.exists():
        import shutil
        shutil.rmtree(dest)
    dest.mkdir(parents=True, exist_ok=True)
    # init_workspace mkdir + git init + kimetsu init; work in dest directly
    rs.init_workspace(str(binary), dest, log, peaks)
    rs.add_chunks(str(binary), dest, question["chunks"], log, peaks)
    if not db.is_file():
        raise SystemExit(f"ingest produced no {db}")
    cache[signature] = dest
    return dest


def load_gold_and_questions(path: Path) -> list[dict]:
    payload = json.loads(path.read_text())
    return payload["questions"]


def load_arm_labels(jsonl: Path, arm: str) -> dict[str, list[str]]:
    out: dict[str, list[str]] = {}
    with jsonl.open() as handle:
        for line in handle:
            row = json.loads(line)
            if row.get("arm") != arm:
                continue
            out[row["id"]] = [label for label in (row.get("labels") or [])[:TOP_K] if label]
    return out


def metrics_for(rows: list[dict], rs) -> dict:
    empty = sum(1 for row in rows if row["empty"])
    capsules = [row["capsules"] for row in rows]
    scored = [row for row in rows if row.get("scored")]
    def mean(key_fn, group):
        return sum(key_fn(item) for item in group) / len(group) if group else 0.0
    hist = {}
    for count in capsules:
        hist[str(count)] = hist.get(str(count), 0) + 1
    return {
        "n": len(rows),
        "scored": len(scored),
        "R@1": mean(lambda r: r["recall"]["@1"], scored),
        "R@3": mean(lambda r: r["recall"]["@3"], scored),
        "R@8": mean(lambda r: r["recall"]["@8"], scored),
        "MRR": mean(lambda r: r["mrr"], scored),
        "empty": empty,
        "empty_rate": empty / len(rows) if rows else 0.0,
        "capsules_min": min(capsules) if capsules else 0,
        "capsules_median": sorted(capsules)[len(capsules) // 2] if capsules else 0,
        "capsules_max": max(capsules) if capsules else 0,
        "capsules_mean": mean(lambda r: r["capsules"], rows),
        "capsule_hist": hist,
        "scorer": "scripts/retrieval_eval.py rank_metrics; retrieval only, not an official judge",
    }


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    parser.add_argument("--bin", type=Path, default=Path("/workspace/fuse/e0s/bin/kimetsu-e0s"))
    parser.add_argument("--data", type=Path, default=Path("/workspace/fuse/e0/data"))
    parser.add_argument("--results", type=Path, default=Path("/workspace/fuse/e0s/results/run1"))
    parser.add_argument("--brains", type=Path, default=Path("/workspace/fuse/e0s/rescore/brains"))
    parser.add_argument("--out", type=Path, default=Path("/workspace/fuse/e0s/rescore/out"))
    parser.add_argument("--scripts", type=Path)
    parser.add_argument("--model-cache", type=Path, default=Path("/workspace/fuse/e0s/model-cache"))
    parser.add_argument("--sanity-arm", default=FILL_ARM)
    parser.add_argument("--sanity-threshold", type=float, default=DEFAULT_SANITY_THRESHOLD)
    parser.add_argument("--alphas", default=",".join(str(a) for a in ALPHA_ARMS))
    parser.add_argument(
        "--datasets",
        default=",".join(DATASETS),
        help="comma-separated dataset names under --data and --results",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    scripts = find_scripts(args.scripts)
    rs, _profiles = load_modules(scripts)
    configure_eval_env(args.model_cache)
    binary = args.bin.resolve()
    if not binary.is_file():
        raise SystemExit(f"missing binary {binary}")
    binary_sha = sha256_file(binary)
    if binary_sha != BINARY_SHA256:
        print(
            f"warning: binary sha256 {binary_sha} != expected {BINARY_SHA256}",
            file=sys.stderr,
        )
    datasets = tuple(name.strip() for name in args.datasets.split(",") if name.strip())
    alphas = tuple(float(part) for part in args.alphas.split(",") if part.strip())
    args.brains.mkdir(parents=True, exist_ok=True)
    args.out.mkdir(parents=True, exist_ok=True)
    print(existing_brain_hint(), flush=True)
    print(
        "brains are per-question TemporaryDirectory workspaces in e0_eval "
        "and were deleted after each question; this script rebuilds them "
        f"with {binary} and caches by chunk-set sha under {args.brains}",
        flush=True,
    )

    questions_by_ds: dict[str, list[dict]] = {}
    fill_labels: dict[str, dict[str, list[str]]] = {}
    rerank_off_labels: dict[str, dict[str, list[str]]] = {}
    for name in datasets:
        data_path = args.data / f"{name}.json"
        jsonl = args.results / f"{name}.jsonl"
        if not data_path.is_file():
            raise SystemExit(f"missing {data_path}")
        if not jsonl.is_file():
            raise SystemExit(f"missing {jsonl}")
        questions_by_ds[name] = load_gold_and_questions(data_path)
        fill_labels[name] = load_arm_labels(jsonl, args.sanity_arm)
        rerank_off_labels[name] = load_arm_labels(jsonl, RERANK_OFF_ARM)

    # Query embeddings, one pass, cached.
    all_questions = [q for name in datasets for q in questions_by_ds[name]]
    query_cache = args.brains / "query_vectors.json"
    query_vectors: dict[str, list[float]]
    if query_cache.is_file():
        query_vectors = {key: list(val) for key, val in json.loads(query_cache.read_text()).items()}
        missing = [q["id"] for q in all_questions if q["id"] not in query_vectors]
        if missing:
            query_cache.unlink()
            query_vectors = {}
    else:
        query_vectors = {}
    peaks: list[int] = []
    ingest_log = (args.out / "ingest.log").open("w")
    try:
        if not query_vectors:
            print(f"embedding {len(all_questions)} queries", flush=True)
            query_vectors = rs.embed_query_vectors(str(binary), all_questions, ingest_log, peaks)
            query_cache.write_text(json.dumps(query_vectors))

        brain_cache: dict[str, Path] = {}
        sanity_rows: dict[str, list[dict]] = {name: [] for name in datasets}
        match_counts = {name: {"n": 0, "match_fill": 0, "match_rerank_off": 0} for name in datasets}
        mismatches: list[dict] = []

        for name in datasets:
            for question in questions_by_ds[name]:
                qid = question["id"]
                print(f"question {name} {qid}", flush=True)
                built = ensure_brain(binary, question, args.brains, rs, ingest_log, peaks, brain_cache)
                db = built / ".kimetsu" / "brain.db"
                conn = open_brain(db)
                try:
                    files_n, manifests_n = repo_counts(conn)
                    memories = load_memories(conn)
                    match = fts_query(question["query"])
                    hits = fts_top(conn, match, FTS_LIMIT) if match else []
                finally:
                    conn.close()
                known_ids = {chunk["id"] for chunk in question["chunks"]}
                gold = {chunk["id"] for chunk in question["chunks"] if chunk.get("evidence")}
                tokens = query_tokens(question["query"])
                qvec = query_vectors[qid]
                fts_cands, ann_cands, _max_q = build_lists(
                    memories, hits, qvec, tokens, known_ids, rs, ALPHA_DEFAULT, "current"
                )
                fused = union_max([fts_cands, ann_cands])
                admitted = rank_pipeline(fused, SEMANTIC_FLOOR_BGE)
                labels = [c.label for c in admitted]
                metrics = rs.rank_metrics(labels, gold, len(labels), len(labels) == 0)
                row = {
                    "id": qid,
                    "dataset": name,
                    "type": str(question["type"]),
                    "arm": "offline_current",
                    "scored": bool(question.get("scored", True)),
                    "labels": labels,
                    "n_evidence": len(gold),
                    "repo_files": files_n,
                    "repo_manifests": manifests_n,
                    **metrics,
                }
                sanity_rows[name].append(row)
                expected = fill_labels[name].get(qid)
                rerank_off = rerank_off_labels[name].get(qid)
                match_counts[name]["n"] += 1
                if expected is not None and set(labels) == set(expected):
                    match_counts[name]["match_fill"] += 1
                else:
                    mismatches.append(
                        {
                            "dataset": name,
                            "id": qid,
                            "offline": labels,
                            "fill_to_cap": expected,
                            "rerank_off": rerank_off,
                        }
                    )
                if rerank_off is not None and set(labels) == set(rerank_off):
                    match_counts[name]["match_rerank_off"] += 1

        total_n = sum(item["n"] for item in match_counts.values())
        total_match = sum(item["match_fill"] for item in match_counts.values())
        rate = total_match / total_n if total_n else 0.0
        per_ds = {}
        for name, counts in match_counts.items():
            per_ds[name] = {
                **counts,
                "fill_match_rate": counts["match_fill"] / counts["n"] if counts["n"] else 0.0,
                "rerank_off_match_rate": counts["match_rerank_off"] / counts["n"] if counts["n"] else 0.0,
                "metrics": metrics_for(sanity_rows[name], rs),
            }
        sanity = {
            "arm": args.sanity_arm,
            "threshold": args.sanity_threshold,
            "rows": total_n,
            "set_matches": total_match,
            "match_rate": rate,
            "passed": rate >= args.sanity_threshold,
            "per_dataset": per_ds,
            "rerank_off_note": (
                "rerank_off is the same fused pipeline without TinyBERT; "
                "reported as a diagnostic only. the gate uses fill_to_cap sets."
            ),
            "brain_note": existing_brain_hint(),
            "binary": str(binary),
            "binary_sha256": binary_sha,
            "mismatch_count": len(mismatches),
            "mismatches_head": mismatches[:25],
            "limitations": [
                "ANN is exact cosine top-80 over stored blobs, not usearch.",
                "Rebuilt brains get new memory ULIDs, so expansion_handle tie-breaks and created_at freshness are not the original run's.",
                "usefulness/trust/decay are implemented but should be identity on a fresh eval ingest (use_count=0, local provenance).",
                "query-association routing is not applied (fresh brains have an empty query_routes table).",
                "repo_file and manifest candidates are not scored; a nonzero repo_files/repo_manifests count is reported per question.",
                "Python float64 vs Rust f32 can swap near-ties.",
                "TinyBERT is not run; fill_to_cap should preserve the admitted set at floor 0 and cap=8, but any residual set change is a gate miss.",
                "content_tokens for supersession lexical identity uses query_tokens (stem + class hints), which is close to but not a line-for-line port of content_tokens.",
            ],
        }
        (args.out / "sanity.json").write_text(json.dumps(sanity, indent=2) + "\n")
        print(
            f"sanity fill_to_cap set match {total_match}/{total_n} = {rate:.4f} "
            f"(threshold {args.sanity_threshold}) "
            f"{'PASS' if sanity['passed'] else 'FAIL'}",
            flush=True,
        )
        for name, info in per_ds.items():
            print(
                f"  {name}: fill {info['fill_match_rate']:.4f} "
                f"rerank_off {info['rerank_off_match_rate']:.4f}",
                flush=True,
            )
        if not sanity["passed"]:
            print("sanity gate failed; alpha arms not scored", flush=True)
            return 1

        # Alpha arms, only after the gate passes.
        arm_summaries: dict[str, dict] = {}
        for alpha in alphas:
            arm_name = f"tm2c2_alpha_{alpha}"
            arm_rows: dict[str, list[dict]] = {name: [] for name in datasets}
            for name in datasets:
                for question in questions_by_ds[name]:
                    qid = question["id"]
                    built = ensure_brain(binary, question, args.brains, rs, ingest_log, peaks, brain_cache)
                    conn = open_brain(built / ".kimetsu" / "brain.db")
                    try:
                        memories = load_memories(conn)
                        match = fts_query(question["query"])
                        hits = fts_top(conn, match, FTS_LIMIT) if match else []
                    finally:
                        conn.close()
                    known_ids = {chunk["id"] for chunk in question["chunks"]}
                    gold = {chunk["id"] for chunk in question["chunks"] if chunk.get("evidence")}
                    tokens = query_tokens(question["query"])
                    fts_cands, ann_cands, max_q = build_lists(
                        memories, hits, query_vectors[qid], tokens, known_ids, rs, alpha, "tm2c2"
                    )
                    fused = union_max([fts_cands, ann_cands])
                    admitted = rank_pipeline(fused, SEMANTIC_FLOOR_BGE)
                    labels = [c.label for c in admitted]
                    metrics = rs.rank_metrics(labels, gold, len(labels), len(labels) == 0)
                    arm_rows[name].append(
                        {
                            "id": qid,
                            "dataset": name,
                            "type": str(question["type"]),
                            "arm": arm_name,
                            "scored": bool(question.get("scored", True)),
                            "labels": labels,
                            "max_q": max_q,
                            **metrics,
                        }
                    )
            arm_summaries[arm_name] = {
                "alpha": alpha,
                "lexical": "bm25/max_q (0 if no FTS hit or max_q=0)",
                "datasets": {name: metrics_for(arm_rows[name], rs) for name in datasets},
            }
            out_jsonl = args.out / f"{arm_name}.jsonl"
            with out_jsonl.open("w") as handle:
                for name in datasets:
                    for row in arm_rows[name]:
                        handle.write(json.dumps(row, sort_keys=True) + "\n")
            print(f"wrote {out_jsonl}", flush=True)
            for name, mets in arm_summaries[arm_name]["datasets"].items():
                print(
                    f"  {arm_name} {name}: R@8={mets['R@8']:.3f} MRR={mets['MRR']:.3f} "
                    f"empty={mets['empty']} caps_med={mets['capsules_median']}",
                    flush=True,
                )
        (args.out / "arms.json").write_text(json.dumps(arm_summaries, indent=2) + "\n")
        return 0
    finally:
        ingest_log.close()


if __name__ == "__main__":
    raise SystemExit(main())
