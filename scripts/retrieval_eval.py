#!/usr/bin/env python3
"""Retrieval eval over one kimetsu binary.

Arms, each changing one knob from a shared base (retrieval level custom,
bge-small-en-v1.5, reranker off, score floors 0, abstain 0):

  lexical                 embedder.enabled = false
  fused                   the base (lexical + vector broker, reranker off)
  fused_rerank_floor_0    embedder.reranker = ms-marco-tinybert-l-2-v2
  fused_rerank_floor_0.3  broker.rerank_min_score = 0.3 on top of that reranker
  vector                  cosine over embeddings the same binary wrote

There is no broker knob for vector-only retrieval: hybrid alpha is fixed and
reciprocal-rank fusion is a separate change. The vector arm reads the sqlite
blobs that binary stored and ranks them by cosine with a query vector from
the same embedder. It does not call the cross-encoder.

Not measured here (follow-ups): the bge query instruction prefix, RRF or
bounded fusion, and the chronological date prefix applied before reranking.

Profiles (`--profile`, default script):

  script  the base above: level custom, reranker off, every floor 0,
          including min_semantic_score.
  bench   a fresh init (level deep, TinyBERT, rerank floor 0.3) plus
          use_user_brain, session_dedupe, and warm_start off. MCP calls
          omit min_semantic_score. Lexical and fused set embedder.reranker
          off so a deep init does not load TinyBERT. See
          scripts/retrieval_eval_profiles.py.

Datasets:
  * this repo's fixture (scripts/fixtures/retrieval_eval.json):
    {"questions": [{id, type, query, chunks: [{id, text, evidence}]}]}
  * LongMemEval oracle JSON (question_id, question, question_type, haystack_*)
  * LoCoMo JSON (conversation turns with dia_id, qa[].evidence)

ONNX intra-op threads are forced to 1. Peak RSS is the max VmHWM of the
kimetsu child processes, in MiB.

    python3 scripts/retrieval_eval.py --self-test
    python3 scripts/retrieval_eval.py \\
        --dataset scripts/fixtures/retrieval_eval.json \\
        --bin target/debug/kimetsu \\
        --out retrieval-eval.jsonl
    python3 scripts/retrieval_eval.py --bin target/debug/kimetsu --rss-reindex

Numbers from the bundled fixture are fixture measurements, not LoCoMo or
LongMemEval scores.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import selectors
import shutil
import sqlite3
import struct
import subprocess
import sys
import tempfile
import threading
import time
from pathlib import Path

LABEL_RE = re.compile(r"\[([^\]]+)\]")
RERANKER = "ms-marco-tinybert-l-2-v2"
EMBEDDER = "bge-small-en-v1.5"
TOP_K = (1, 3, 8)
MAX_CAPSULES = 8

# Binary arms. `patch` is the single knob relative to the previous arm in the
# fused chain, applied on top of BASE. lexical branches off BASE alone.
BASE = {
    "retrieval.level": '"custom"',
    "embedder.model": f'"{EMBEDDER}"',
    "embedder.enabled": "true",
    "embedder.reranker": '"off"',
    "broker.rerank_min_score": "0.0",
    "broker.min_lexical_coverage": "0.0",
    "broker.min_semantic_score": "0.0",
    "broker.abstain_min_score": "0.0",
}
BINARY_ARMS = [
    ("lexical", {"embedder.enabled": "false"}, "embedder.enabled=false"),
    ("fused", {}, "base: embedder on, reranker off, floors 0"),
    (
        "fused_rerank_floor_0",
        {"embedder.reranker": f'"{RERANKER}"'},
        "embedder.reranker=ms-marco-tinybert-l-2-v2",
    ),
    (
        "fused_rerank_floor_0.3",
        {"embedder.reranker": f'"{RERANKER}"', "broker.rerank_min_score": "0.3"},
        "broker.rerank_min_score=0.3",
    ),
]
MCP_ARGS = {
    "budget_tokens": 16000,
    "max_capsules": MAX_CAPSULES,
    "min_score": 0.0,
    "min_lexical_coverage": 0.0,
    "min_semantic_score": 0.0,
    "abstain_evidence": 0.0,
    "include_ambient": False,
    "stage": "localization",
}


def load_dataset(path: Path) -> list[dict]:
    data = json.loads(path.read_text())
    if isinstance(data, dict) and "questions" in data:
        questions = data["questions"]
    elif isinstance(data, list) and data and "haystack_sessions" in data[0]:
        questions = load_longmemeval(data)
    elif isinstance(data, list) and data and "qa" in data[0] and "conversation" in data[0]:
        questions = load_locomo(data)
    elif isinstance(data, dict) and "qa" in data and "conversation" in data:
        questions = load_locomo([data])
    else:
        raise SystemExit(f"unrecognized dataset shape: {path}")
    out = []
    for question in questions:
        gold = [c["id"] for c in question["chunks"] if c.get("evidence")]
        if not gold or not str(question.get("query", "")).strip():
            continue
        out.append(question)
    if not out:
        raise SystemExit(f"no scored questions in {path}")
    return out


def load_longmemeval(rows: list) -> list[dict]:
    questions = []
    for row in rows:
        chunks = []
        sessions = row.get("haystack_sessions") or []
        ids = row.get("haystack_session_ids") or []
        for sid, session in zip(ids, sessions):
            for index, turn in enumerate(session):
                text = turn.get("content") or turn.get("text") or ""
                chunks.append(
                    {
                        "id": f"{sid}:t{index}",
                        "text": text,
                        "evidence": bool(turn.get("has_answer")),
                    }
                )
        questions.append(
            {
                "id": row.get("question_id") or row.get("id"),
                "type": row.get("question_type") or "unknown",
                "query": row.get("question") or "",
                "chunks": chunks,
            }
        )
    return questions


def load_locomo(samples: list) -> list[dict]:
    questions = []
    for sample in samples:
        chunks = []
        conversation = sample.get("conversation") or {}
        for key, turns in conversation.items():
            if not isinstance(turns, list):
                continue
            for turn in turns:
                dia = turn.get("dia_id") or turn.get("id")
                text = turn.get("text") or turn.get("content") or ""
                if dia and text:
                    chunks.append({"id": str(dia), "text": text, "evidence": False})
        for index, qa in enumerate(sample.get("qa") or []):
            evidence = qa.get("evidence") or []
            if isinstance(evidence, str):
                evidence = [evidence]
            gold = {str(item) for item in evidence}
            if not gold or "question" not in qa:
                continue
            marked = []
            for chunk in chunks:
                marked.append({**chunk, "evidence": chunk["id"] in gold})
            sample_id = sample.get("sample_id") or sample.get("id") or "locomo"
            questions.append(
                {
                    "id": f"{sample_id}:{index}",
                    "type": str(qa.get("category") or qa.get("question_type") or "locomo"),
                    "query": qa["question"],
                    "chunks": marked,
                }
            )
    return questions


def chunk_text(chunk: dict) -> str:
    return f"[{chunk['id']}] {chunk['text'].strip()}"


def labels_in(text: str) -> list[str]:
    return LABEL_RE.findall(text or "")


def evidence_label(text: str, known_ids: set[str]) -> str:
    """Pick the chunk id out of a summary.

    Retrieval may prefix a chronological date in brackets before the stored
    `[chunk-id]` marker. The date is not an evidence id.
    """
    found = labels_in(text)
    for label in found:
        if label in known_ids:
            return label
    return found[-1] if found else ""


def set_toml_key(text: str, dotted: str, rendered: str) -> str:
    section, key = dotted.split(".", 1)
    header = f"[{section}]"
    lines = text.splitlines()
    out: list[str] = []
    in_section = False
    replaced = False
    inserted = False
    for line in lines:
        stripped = line.strip()
        if stripped.startswith("[") and stripped.endswith("]"):
            if in_section and not replaced:
                out.append(f"{key} = {rendered}")
                inserted = True
            in_section = stripped == header
            out.append(line)
            continue
        if in_section and not replaced and stripped.startswith(f"{key} ") or (
            in_section and not replaced and stripped.startswith(f"{key}=")
        ):
            out.append(f"{key} = {rendered}")
            replaced = True
            continue
        out.append(line)
    if in_section and not replaced and not inserted:
        out.append(f"{key} = {rendered}")
        replaced = True
    if not replaced and not inserted:
        if out and out[-1].strip():
            out.append("")
        out.append(header)
        out.append(f"{key} = {rendered}")
    return "\n".join(out) + "\n"


def apply_config(path: Path, patch: dict) -> None:
    text = path.read_text()
    for dotted, rendered in patch.items():
        text = set_toml_key(text, dotted, rendered)
    path.write_text(text)


def child_env(embedder: str | None) -> dict:
    env = os.environ.copy()
    env["KIMETSU_USER_BRAIN"] = "0"
    env["KIMETSU_BRAIN_AMBIENT"] = "off"
    env["KIMETSU_INTRA_THREADS"] = "1"
    env["OMP_NUM_THREADS"] = "1"
    env["KIMETSU_BRAIN_LOG_RETRIEVAL"] = "0"
    if embedder:
        env["KIMETSU_BRAIN_EMBEDDER"] = embedder
    else:
        env.pop("KIMETSU_BRAIN_EMBEDDER", None)
    return env


def run_kimetsu(binary: str, args: list[str], cwd: Path, env: dict, log) -> int:
    """Run kimetsu to completion. Returns the child peak RSS in KiB."""
    proc = subprocess.Popen(
        [binary, *args],
        cwd=cwd,
        env=env,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    sampler = RssSampler(proc.pid)
    stdout, stderr = proc.communicate()
    peak = sampler.close()
    log.write(f"$ kimetsu {' '.join(args)} -> {proc.returncode} peak_kib={peak}\n")
    if stdout:
        log.write(stdout[-4000:])
        log.write("\n")
    if stderr:
        log.write(stderr[-4000:])
        log.write("\n")
    if proc.returncode != 0:
        raise RuntimeError(f"kimetsu {' '.join(args)} failed ({proc.returncode}): {(stderr or '')[-500:]}")
    return peak


def init_workspace(binary: str, root: Path, log, peaks: list[int]) -> None:
    root.mkdir(parents=True, exist_ok=True)
    subprocess.run(["git", "init", "--quiet"], cwd=root, check=True, capture_output=True)
    peaks.append(run_kimetsu(binary, ["init"], root, child_env(EMBEDDER), log))
    apply_config(root / ".kimetsu" / "project.toml", BASE)


def add_chunks(binary: str, root: Path, chunks: list[dict], log, peaks: list[int]) -> None:
    payload = [{"text": chunk_text(c), "scope": "project", "kind": "fact"} for c in chunks]
    batch = root / "batch.json"
    batch.write_text(json.dumps(payload))
    peaks.append(
        run_kimetsu(
            binary,
            ["brain", "memory", "add-batch", str(batch), "--json", "--scope", "project", "--kind", "fact"],
            root,
            child_env(EMBEDDER),
            log,
        )
    )


class RssSampler:
    def __init__(self, pid: int):
        self.pid = pid
        self.peak_kib = 0
        self._stop = threading.Event()
        self._thread = threading.Thread(target=self._run, daemon=True)
        self._thread.start()

    def _sample(self) -> None:
        path = Path(f"/proc/{self.pid}/status")
        try:
            text = path.read_text()
        except OSError:
            return
        for line in text.splitlines():
            if line.startswith("VmHWM:"):
                kib = int(line.split()[1])
                if kib > self.peak_kib:
                    self.peak_kib = kib
                return

    def _run(self) -> None:
        while not self._stop.is_set():
            self._sample()
            self._stop.wait(0.05)
        self._sample()

    def close(self) -> int:
        self._stop.set()
        self._thread.join(timeout=1)
        return self.peak_kib


class MCP:
    def __init__(self, binary: str, workspace: Path, env: dict, log_path: Path):
        self.stderr = open(log_path, "a")
        self.proc = subprocess.Popen(
            [binary, "mcp", "serve", "--workspace", str(workspace), "--no-user-skills"],
            stdin=subprocess.PIPE,
            stdout=subprocess.PIPE,
            stderr=self.stderr,
            text=True,
            bufsize=1,
            env=env,
            cwd=workspace,
        )
        self.next_id = 0
        self.sampler = RssSampler(self.proc.pid)
        self.call(
            "initialize",
            {
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": {"name": "retrieval-eval", "version": "1"},
            },
        )
        assert self.proc.stdin is not None
        self.proc.stdin.write(json.dumps({"jsonrpc": "2.0", "method": "notifications/initialized"}) + "\n")
        self.proc.stdin.flush()

    def call(self, method: str, params: dict) -> dict:
        assert self.proc.stdin is not None and self.proc.stdout is not None
        self.next_id += 1
        self.proc.stdin.write(
            json.dumps({"jsonrpc": "2.0", "id": self.next_id, "method": method, "params": params}) + "\n"
        )
        self.proc.stdin.flush()
        selector = selectors.DefaultSelector()
        selector.register(self.proc.stdout, selectors.EVENT_READ)
        if not selector.select(300):
            raise TimeoutError(f"MCP {method} timed out")
        line = self.proc.stdout.readline()
        if not line:
            raise RuntimeError(f"MCP {method} closed stdout")
        message = json.loads(line)
        if "error" in message:
            raise RuntimeError(f"MCP {method} error: {message['error']}")
        return message["result"]

    def context(self, query: str) -> dict:
        result = self.call(
            "tools/call",
            {"name": "kimetsu_brain_context", "arguments": {**MCP_ARGS, "query": query}},
        )
        content = result.get("content") or []
        if not content:
            return {"error": "empty MCP content", "capsules": [], "skipped": True}
        return json.loads(content[0]["text"])

    def close(self) -> int:
        try:
            if self.proc.stdin:
                self.proc.stdin.close()
        except BrokenPipeError:
            pass
        try:
            self.proc.wait(timeout=20)
        except subprocess.TimeoutExpired:
            self.proc.kill()
        peak = self.sampler.close()
        self.stderr.close()
        return peak


def read_embeddings(db_path: Path) -> list[tuple[str, list[float]]]:
    uri = f"file:{db_path}?mode=ro"
    conn = sqlite3.connect(uri, uri=True)
    try:
        rows = conn.execute(
            "SELECT text, embedding FROM memories "
            "WHERE invalidated_at IS NULL AND superseded_by IS NULL AND embedding IS NOT NULL"
        ).fetchall()
    finally:
        conn.close()
    out = []
    for text, blob in rows:
        if not blob or len(blob) % 4:
            continue
        vector = list(struct.unpack(f"<{len(blob) // 4}f", blob))
        out.append((text, vector))
    return out


def dot(a: list[float], b: list[float]) -> float:
    return sum(x * y for x, y in zip(a, b))


def norm(vector: list[float]) -> float:
    return dot(vector, vector) ** 0.5


def cosine(a: list[float], b: list[float]) -> float:
    na, nb = norm(a), norm(b)
    if na == 0 or nb == 0:
        return 0.0
    return dot(a, b) / (na * nb)


def rank_metrics(ordered_labels: list[str], gold: set[str], capsule_count: int, skipped: bool) -> dict:
    rank = None
    for index, label in enumerate(ordered_labels, 1):
        if label in gold:
            rank = index
            break
    return {
        "rank": rank,
        "recall": {f"@{k}": int(rank is not None and rank <= k) for k in TOP_K},
        "mrr": 0.0 if rank is None else 1.0 / rank,
        "empty": int(skipped or capsule_count == 0),
        "capsules": capsule_count,
    }


def vector_arm(rows: list[tuple[str, list[float]]], query_vector: list[float], gold: set[str], known_ids: set[str]) -> dict:
    scored = []
    for text, vector in rows:
        scored.append((cosine(query_vector, vector), evidence_label(text, known_ids)))
    scored.sort(key=lambda item: (-item[0], item[1]))
    ordered = [label for _, label in scored[:MAX_CAPSULES]]
    metrics = rank_metrics(ordered, gold, len(ordered), len(ordered) == 0)
    return {
        "arm": "vector",
        "knob": "cosine over embeddings written by this binary",
        "labels": ordered,
        "skipped": len(ordered) == 0,
        "excluded": None,
        "error": None,
        **metrics,
    }


def binary_arm(binary: str, built: Path, arm: str, patch: dict, knob: str, query: str, gold: set[str], known_ids: set[str], log_dir: Path) -> dict:
    work = built.parent / f"{built.name}-{arm}"
    if work.exists():
        shutil.rmtree(work)
    shutil.copytree(built, work)
    apply_config(work / ".kimetsu" / "project.toml", patch)
    embedder_env = None if patch.get("embedder.enabled") == "false" else EMBEDDER
    mcp = MCP(binary, work, child_env(embedder_env), log_dir / f"{arm}.log")
    try:
        payload = mcp.context(query)
    finally:
        peak = mcp.close()
    capsules = payload.get("capsules") or []
    ordered = [evidence_label(capsule.get("summary") or "", known_ids) for capsule in capsules]
    skipped = bool(payload.get("skipped")) or bool(payload.get("error"))
    metrics = rank_metrics(ordered, gold, len(capsules), skipped and not capsules)
    if payload.get("error"):
        metrics["empty"] = 1
    return {
        "arm": arm,
        "knob": knob,
        "labels": ordered,
        "skipped": bool(payload.get("skipped")),
        "excluded": payload.get("excluded_count"),
        "error": payload.get("error"),
        "peak_rss_mib": round(peak / 1024, 1) if peak else None,
        **metrics,
    }


def embed_query_vectors(binary: str, questions: list[dict], log, peaks: list[int]) -> dict[str, list[float]]:
    root = Path(tempfile.mkdtemp(prefix="kimetsu-eval-queries-"))
    try:
        init_workspace(binary, root, log, peaks)
        chunks = [{"id": f"q:{q['id']}", "text": q["query"]} for q in questions]
        add_chunks(binary, root, chunks, log, peaks)
        rows = read_embeddings(root / ".kimetsu" / "brain.db")
    finally:
        shutil.rmtree(root, ignore_errors=True)
    found = {}
    for text, vector in rows:
        labels = labels_in(text)
        if labels:
            found[labels[0]] = vector
    missing = [q["id"] for q in questions if f"q:{q['id']}" not in found]
    if missing:
        raise RuntimeError(f"query embeddings missing for {missing}")
    return {q["id"]: found[f"q:{q['id']}"] for q in questions}


def build_question(binary: str, question: dict, log, peaks: list[int]) -> Path:
    root = Path(tempfile.mkdtemp(prefix=f"kimetsu-eval-{question['id']}-"))
    init_workspace(binary, root, log, peaks)
    add_chunks(binary, root, question["chunks"], log, peaks)
    return root


def summarize(rows: list[dict]) -> str:
    arms = []
    for row in rows:
        if row["arm"] not in arms:
            arms.append(row["arm"])
    lines = [
        f"{'arm':<56} {'n':>4} {'R@1':>6} {'R@3':>6} {'R@8':>6} {'MRR':>6} {'empty':>7} {'caps':>6} {'rss':>7}"
    ]

    def emit(title: str, group: list[dict]) -> None:
        if not group:
            return
        n = len(group)
        def mean(key):
            return sum(item[key] for item in group) / n
        rss = [item["peak_rss_mib"] for item in group if item.get("peak_rss_mib")]
        rss_s = f"{max(rss):.0f}" if rss else "-"
        lines.append(
            f"{title[:56]:<56} {n:4d} {mean('r1'):6.3f} {mean('r3'):6.3f} {mean('r8'):6.3f} "
            f"{mean('mrr'):6.3f} {mean('empty'):7.3f} {mean('capsules'):6.2f} {rss_s:>7}"
        )

    flat = []
    for row in rows:
        flat.append(
            {
                **row,
                "r1": row["recall"]["@1"],
                "r3": row["recall"]["@3"],
                "r8": row["recall"]["@8"],
            }
        )
    lines.append("all questions")
    for arm in arms:
        emit(arm, [row for row in flat if row["arm"] == arm])
    types = []
    for row in flat:
        if row["type"] not in types:
            types.append(row["type"])
    if len(types) > 1:
        lines.append("by question type")
        for arm in arms:
            for qtype in types:
                emit(
                    f"{arm} / {qtype}",
                    [row for row in flat if row["arm"] == arm and row["type"] == qtype],
                )
    return "\n".join(lines)


def write_jsonl(path: Path, rows: list[dict]) -> None:
    with path.open("w") as handle:
        for row in rows:
            handle.write(json.dumps(row, sort_keys=True) + "\n")


def evaluate(binary: str, questions: list[dict], out_path: Path) -> int:
    log_dir = Path(tempfile.mkdtemp(prefix="kimetsu-eval-logs-"))
    log_handle = open(log_dir / "build.log", "w")
    rows = []
    peaks: list[int] = []
    try:
        print(f"embedding {len(questions)} queries with {EMBEDDER}, 1 ONNX thread", flush=True)
        query_vectors = embed_query_vectors(binary, questions, log_handle, peaks)
        for question in questions:
            gold = {c["id"] for c in question["chunks"] if c.get("evidence")}
            print(f"question {question['id']} ({question['type']}) gold={sorted(gold)}", flush=True)
            built = build_question(binary, question, log_handle, peaks)
            try:
                stored = read_embeddings(built / ".kimetsu" / "brain.db")
                known_ids = {c["id"] for c in question["chunks"]}
                vector = vector_arm(stored, query_vectors[question["id"]], gold, known_ids)
                vector.update(
                    {
                        "id": question["id"],
                        "type": str(question["type"]),
                        "n_evidence": len(gold),
                        "peak_rss_mib": None,
                    }
                )
                rows.append(vector)
                for arm, patch, knob in BINARY_ARMS:
                    result = binary_arm(
                        binary, built, arm, patch, knob, question["query"], gold, known_ids, log_dir
                    )
                    result.update(
                        {
                            "id": question["id"],
                            "type": str(question["type"]),
                            "n_evidence": len(gold),
                        }
                    )
                    if result.get("peak_rss_mib"):
                        peaks.append(int(result["peak_rss_mib"] * 1024))
                    print(
                        f"  {arm}: capsules={result['capsules']} rank={result['rank']} "
                        f"empty={result['empty']} excluded={result['excluded']} "
                        f"rss_mib={result.get('peak_rss_mib')}",
                        flush=True,
                    )
                    rows.append(result)
            finally:
                shutil.rmtree(built, ignore_errors=True)
                for arm, _, _ in BINARY_ARMS:
                    shutil.rmtree(built.parent / f"{built.name}-{arm}", ignore_errors=True)
    finally:
        log_handle.close()
    # Stable arm order: lexical, vector, fused, rerank floors.
    order = ["lexical", "vector", "fused", "fused_rerank_floor_0", "fused_rerank_floor_0.3"]
    rows.sort(key=lambda row: (row["id"], order.index(row["arm"]) if row["arm"] in order else 99))
    write_jsonl(out_path, rows)
    print()
    print(summarize(rows))
    peak_mib = max(peaks) / 1024 if peaks else 0
    print(f"\npeak kimetsu RSS: {peak_mib:.1f} MiB" if peak_mib else "\npeak kimetsu RSS: unavailable")
    print(f"jsonl: {out_path}")
    print(f"logs: {log_dir}")
    errors = [row for row in rows if row.get("error")]
    return 1 if errors else 0


def rss_reindex(binary: str, docs: int, tokens: int) -> int:
    """Ingest `docs` long rows with the embedder off, then reindex with it on.

    Peak RSS is the reindex process, which is the embed_batch path.
    """
    root = Path(tempfile.mkdtemp(prefix="kimetsu-rss-reindex-"))
    log = open(root / "rss.log", "w")
    peaks: list[int] = []
    try:
        init_workspace(binary, root, log, peaks)
        apply_config(root / ".kimetsu" / "project.toml", {"embedder.enabled": "false"})
        word = "token "
        text = word * tokens
        payload = [{"text": f"[rss:{i}] {text}", "scope": "project", "kind": "fact"} for i in range(docs)]
        batch = root / "batch.json"
        batch.write_text(json.dumps(payload))
        run_kimetsu(
            binary,
            ["brain", "memory", "add-batch", str(batch), "--json"],
            root,
            child_env(None),
            log,
        )
        apply_config(
            root / ".kimetsu" / "project.toml",
            {"embedder.enabled": "true", "embedder.model": f'"{EMBEDDER}"'},
        )
        proc = subprocess.Popen(
            [binary, "brain", "reindex"],
            cwd=root,
            env=child_env(EMBEDDER),
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
        )
        sampler = RssSampler(proc.pid)
        stdout, stderr = proc.communicate()
        peak = sampler.close()
        log.write(stdout or "")
        log.write(stderr or "")
        if proc.returncode != 0:
            raise RuntimeError(f"reindex failed: {stderr[-500:]}")
        print(
            f"reindex docs={docs} approx_tokens={tokens} "
            f"peak_rss_mib={peak / 1024:.1f} exit={proc.returncode}"
        )
        print(f"workspace kept at {root}")
        return 0
    finally:
        log.close()


def self_test(fixture: Path) -> int:
    sample = "[retrieval]\nlevel = \"deep\"\n\n[embedder]\nenabled = true\nreranker = \"off\"\n"
    updated = set_toml_key(sample, "embedder.reranker", f'"{RERANKER}"')
    updated = set_toml_key(updated, "broker.rerank_min_score", "0.3")
    if 'reranker = "ms-marco-tinybert-l-2-v2"' not in updated:
        raise SystemExit("toml rewrite missed reranker")
    if "rerank_min_score = 0.3" not in updated or "[broker]" not in updated:
        raise SystemExit(f"toml rewrite missed new section:\n{updated}")
    questions = load_dataset(fixture)
    if len(questions) < 2:
        raise SystemExit("fixture has too few questions")
    types = {str(q["type"]) for q in questions}
    if len(types) < 2:
        raise SystemExit("fixture should cover more than one question type")
    for question in questions:
        gold = [c["id"] for c in question["chunks"] if c.get("evidence")]
        if len(gold) != 1 or len(question["chunks"]) < 3:
            raise SystemExit(f"fixture question {question['id']} is not a small labeled set")
    if labels_in(chunk_text(question["chunks"][0]))[0] != question["chunks"][0]["id"]:
        raise SystemExit("chunk label was not recoverable")
    known = {c["id"] for c in questions[0]["chunks"]}
    dated = f"[2026-11-09] project:fact - {chunk_text(questions[0]['chunks'][0])}"
    if evidence_label(dated, known) != questions[0]["chunks"][0]["id"]:
        raise SystemExit("date prefix hid the evidence id")
    synthetic = [
        {"arm": "lexical", "type": "a", "recall": {"@1": 1, "@3": 1, "@8": 1}, "mrr": 1.0, "empty": 0, "capsules": 4, "peak_rss_mib": 10},
        {"arm": "lexical", "type": "a", "recall": {"@1": 0, "@3": 0, "@8": 1}, "mrr": 0.25, "empty": 0, "capsules": 4, "peak_rss_mib": None},
        {"arm": "lexical", "type": "b", "recall": {"@1": 0, "@3": 0, "@8": 0}, "mrr": 0.0, "empty": 1, "capsules": 0, "peak_rss_mib": 12},
    ]
    table = summarize(synthetic)
    if "0.333" not in table or "0.417" not in table:
        raise SystemExit(f"metric table mismatch:\n{table}")
    # 3 rows: recall@1 mean 1/3, recall@8 mean 2/3, MRR (1+0.25+0)/3 = 0.4166, empty 1/3
    longmem = load_longmemeval(
        [
            {
                "question_id": "q",
                "question": "where?",
                "question_type": "single-session-user",
                "haystack_session_ids": ["s"],
                "haystack_sessions": [[{"content": "answer", "has_answer": True}, {"content": "noise"}]],
            }
        ]
    )
    if longmem[0]["chunks"][0]["id"] != "s:t0" or not longmem[0]["chunks"][0]["evidence"]:
        raise SystemExit("LongMemEval loader failed")
    locomo = load_locomo(
        [
            {
                "sample_id": "c1",
                "conversation": {"session_1": [{"dia_id": "D1:1", "text": "the fact"}]},
                "qa": [{"question": "what?", "evidence": ["D1:1"], "category": 1}],
            }
        ]
    )
    if not locomo[0]["chunks"][0]["evidence"] or locomo[0]["type"] != "1":
        raise SystemExit("LoCoMo loader failed")
    import retrieval_eval_profiles as profiles

    profiles.self_test(sys.modules[__name__], fixture)
    print("self-test ok")
    print(f"fixture questions: {len(questions)} types: {sorted(types)}")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Rerunnable kimetsu retrieval eval")
    parser.add_argument("--dataset", type=Path, default=Path("scripts/fixtures/retrieval_eval.json"))
    parser.add_argument("--bin", default="kimetsu")
    parser.add_argument("--out", type=Path, default=Path("retrieval-eval.jsonl"))
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument(
        "--profile",
        choices=("script", "bench"),
        default="script",
        help="script (default): level custom and every floor 0. bench: fresh init defaults plus the bench setup toggles",
    )
    parser.add_argument("--rss-reindex", action="store_true", help="measure peak RSS of a long reindex")
    parser.add_argument("--docs", type=int, default=32)
    parser.add_argument("--tokens", type=int, default=400)
    parser.add_argument("--limit", type=int, default=0, help="evaluate only the first N questions")
    args = parser.parse_args()
    here = Path(__file__).resolve().parents[1]
    dataset = args.dataset if args.dataset.is_absolute() else here / args.dataset
    if args.self_test:
        return self_test(dataset if dataset.exists() else here / "scripts/fixtures/retrieval_eval.json")
    if args.profile == "bench":
        import retrieval_eval_profiles as profiles

        profiles.apply_to(sys.modules[__name__], "bench")
    binary = args.bin
    if Path(binary).exists():
        binary = str(Path(binary).resolve())
    elif shutil.which(binary) is None:
        raise SystemExit(f"kimetsu binary not found: {binary}")
    if args.rss_reindex:
        return rss_reindex(binary, args.docs, args.tokens)
    questions = load_dataset(dataset)
    if args.limit:
        questions = questions[: args.limit]
    return evaluate(str(Path(binary).resolve() if Path(binary).exists() else binary), questions, args.out)


if __name__ == "__main__":
    sys.exit(main())
