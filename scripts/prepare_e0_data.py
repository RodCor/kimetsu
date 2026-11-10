#!/usr/bin/env python3
"""Adapter from LoCoMo conv-26 and LongMemEval oracle inputs to retrieval_eval JSON.

Same chunking as scripts/experiments/e0/prepare_data.py (the pinned run). That
copy reads /workspace paths. This one takes the three source files and an
output directory on the command line.

Chunk text matches the bench (hybrid_worker.py / run_full.py):

  LoCoMo: full-dataset.json history line
          "[D1:1] <date> | <speaker>: <text>[ image caption]"
          split every 1200 characters.
  LongMemEval: "[<sid>:t<i>] <date> | <role>: <content>"
          split every 1200 characters.

The leading "[label] " is stripped because retrieval_eval.chunk_text adds
"[<id>] " again, so the stored text of the first piece matches the bench line.
Continuation pieces (past 1200 characters) use id "<label>#p<k>". Every piece
of an evidence turn is gold.

Gold labels: LoCoMo qa[].evidence, split on ';', ',' and whitespace.
LongMemEval: turns with has_answer true. Questions with no gold in the
haystack are kept with scored false so empty rate covers the full set.
Recall and MRR use scored questions only (see summarize_e0.py).

    python3 scripts/prepare_e0_data.py --self-test
    python3 scripts/prepare_e0_data.py \\
        --full-dataset /path/to/full-dataset.json \\
        --locomo /path/to/locomo10.json \\
        --longmemeval /path/to/longmemeval_oracle.json \\
        --out /path/to/e0-data
"""

from __future__ import annotations

import argparse
import hashlib
import json
import re
import sys
import tempfile
from pathlib import Path

LABEL = re.compile(r"^\[([^\]]+)\] ")
SOURCE_NAMES = ("full-dataset.json", "locomo10.json", "longmemeval_oracle.json")


def sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def pieces(label: str, text: str) -> list[dict]:
    out = []
    for index, offset in enumerate(range(0, len(text), 1200)):
        piece = text[offset : offset + 1200]
        if index == 0:
            match = LABEL.match(piece)
            if not match or match.group(1) != label:
                raise SystemExit(f"chunk {label!r} does not start with its label: {piece[:60]!r}")
            out.append({"id": label, "text": piece[match.end() :], "turn": label})
        else:
            out.append({"id": f"{label}#p{index}", "text": piece, "turn": label})
    return out


def count(questions: list[dict]) -> dict:
    out: dict = {}
    for question in questions:
        bucket = out.setdefault(question["type"], {"n": 0, "scored": 0})
        bucket["n"] += 1
        bucket["scored"] += int(question["scored"])
    return dict(sorted(out.items()))


def build_locomo(full_dataset: Path, locomo: Path, conv_id: str = "conv-26") -> tuple[list[dict], dict]:
    groups = json.loads(full_dataset.read_text())
    group = next(item for item in groups if item["id"] == conv_id)
    raw = next(item for item in json.loads(locomo.read_text()) if item["sample_id"] == conv_id)
    chunks: list[dict] = []
    for line in group["history"]:
        match = LABEL.match(line)
        if not match:
            raise SystemExit(f"history line has no label: {line[:80]!r}")
        chunks += pieces(match.group(1), line)
    turns = {chunk["turn"] for chunk in chunks}
    questions = []
    unmatched = []
    for question in group["questions"]:
        evidence = question.get("evidence") or []
        if isinstance(evidence, str):
            evidence = [evidence]
        gold_turns = {token for item in evidence for token in re.split(r"[;,\s]+", str(item)) if token}
        missing = sorted(gold_turns - turns)
        if missing:
            unmatched.append({"id": question["id"], "evidence": evidence, "missing": missing})
        present = gold_turns & turns
        questions.append(
            {
                "id": question["id"],
                "type": str(question["category"]),
                "query": question["question"],
                "scored": bool(present),
                "chunks": [
                    {"id": chunk["id"], "text": chunk["text"], "evidence": chunk["turn"] in present}
                    for chunk in chunks
                ],
            }
        )
    if len(group["questions"]) != len(raw["qa"]):
        raise SystemExit(f"question count mismatch: {len(group['questions'])} vs {len(raw['qa'])}")
    for left, right in zip(group["questions"], raw["qa"]):
        if left["question"] != right["question"]:
            raise SystemExit(f"question text mismatch: {left['question']!r} vs {right['question']!r}")
    report = {
        "conversation": conv_id,
        "questions": len(questions),
        "scored": sum(question["scored"] for question in questions),
        "unscored_ids": [question["id"] for question in questions if not question["scored"]],
        "chunks": len(chunks),
        "turns": len(turns),
        "split_pieces": len(chunks) - len(turns),
        "by_category": count(questions),
        "evidence_ids_not_in_haystack": unmatched,
    }
    return questions, report


def build_lme(path: Path) -> tuple[list[dict], dict]:
    rows = json.loads(path.read_text())
    questions = []
    split = 0
    for row in rows:
        chunks = []
        sessions = zip(row["haystack_session_ids"], row["haystack_dates"], row["haystack_sessions"])
        for sid, date, session in sessions:
            for index, turn in enumerate(session):
                label = f"{sid}:t{index}"
                text = f"[{label}] {date} | {turn['role']}: {turn['content']}"
                turn_pieces = pieces(label, text)
                split += len(turn_pieces) - 1
                chunks += [
                    {"id": piece["id"], "text": piece["text"], "evidence": bool(turn.get("has_answer"))}
                    for piece in turn_pieces
                ]
        question_id = row["question_id"]
        question_type = "abstention" if str(question_id).endswith("_abs") else row["question_type"]
        questions.append(
            {
                "id": question_id,
                "type": question_type,
                "query": row["question"],
                "scored": any(chunk["evidence"] for chunk in chunks),
                "chunks": chunks,
            }
        )
    sizes = [len(question["chunks"]) for question in questions]
    report = {
        "questions": len(questions),
        "scored": sum(question["scored"] for question in questions),
        "unscored_by_type": count([question for question in questions if not question["scored"]]),
        "by_type": count(questions),
        "chunks_total": sum(sizes),
        "chunks_per_question_min_median_max": [min(sizes), sorted(sizes)[len(sizes) // 2], max(sizes)],
        "split_pieces": split,
    }
    return questions, report


def write_outputs(
    full_dataset: Path,
    locomo: Path,
    longmemeval: Path,
    out_dir: Path,
    conv_id: str = "conv-26",
) -> dict:
    locomo_questions, locomo_report = build_locomo(full_dataset, locomo, conv_id)
    lme_questions, lme_report = build_lme(longmemeval)
    out_dir.mkdir(parents=True, exist_ok=True)
    locomo_path = out_dir / "locomo-conv26.json"
    lme_path = out_dir / "lme-oracle.json"
    report_path = out_dir / "labels-report.json"
    locomo_path.write_text(
        json.dumps(
            {
                "source": f"full-locomo/full-dataset.json {conv_id} (bench chunk text)",
                "questions": locomo_questions,
            }
        )
    )
    lme_path.write_text(
        json.dumps(
            {
                "source": "data/longmemeval_oracle.json (run_full.chunks text)",
                "questions": lme_questions,
            }
        )
    )
    sources = {
        "full-dataset.json": full_dataset,
        "locomo10.json": locomo,
        "longmemeval_oracle.json": longmemeval,
    }
    report = {
        "locomo_conv26": locomo_report,
        "lme_oracle": lme_report,
        "sources_sha256": {name: sha256(path) for name, path in sources.items()},
        "outputs_sha256": {
            "locomo-conv26.json": sha256(locomo_path),
            "lme-oracle.json": sha256(lme_path),
        },
        "inputs": {name: str(path) for name, path in sources.items()},
    }
    report_path.write_text(json.dumps(report, indent=2))
    return report


def _fail(message: str) -> None:
    raise SystemExit(message)


def self_test() -> int:
    label = "D1:1"
    body = "x" * 1300
    long_line = f"[{label}] {body}"
    short_line = "[D1:2] 1 Jan | Ada: noise"
    if len(long_line) <= 1200:
        _fail("self-test line was not long enough to split")
    full = [
        {
            "id": "conv-26",
            "history": [long_line, short_line],
            "questions": [
                {"id": "q1", "category": 1, "question": "what happened?", "evidence": ["D1:1"]},
                {"id": "q2", "category": 2, "question": "who left?", "evidence": ["D9:9; D1:2"]},
                {"id": "q3", "category": 2, "question": "missing?", "evidence": "D9:9"},
            ],
        }
    ]
    locomo = [
        {
            "sample_id": "conv-26",
            "qa": [
                {"question": "what happened?"},
                {"question": "who left?"},
                {"question": "missing?"},
            ],
        }
    ]
    lme = [
        {
            "question_id": "q_abs",
            "question": "unknown?",
            "question_type": "single-session-user",
            "haystack_session_ids": ["s1"],
            "haystack_dates": ["2020-01-01"],
            "haystack_sessions": [[{"role": "user", "content": "nope", "has_answer": False}]],
        },
        {
            "question_id": "q1",
            "question": "where?",
            "question_type": "single-session-user",
            "haystack_session_ids": ["s1"],
            "haystack_dates": ["2020-01-02"],
            "haystack_sessions": [
                [
                    {"role": "user", "content": "Y" * 1300, "has_answer": True},
                    {"role": "assistant", "content": "other", "has_answer": False},
                ]
            ],
        },
    ]
    with tempfile.TemporaryDirectory(prefix="kimetsu-e0-prepare-") as tmp:
        root = Path(tmp)
        full_path = root / "full-dataset.json"
        locomo_path = root / "locomo10.json"
        lme_path = root / "longmemeval_oracle.json"
        full_path.write_text(json.dumps(full))
        locomo_path.write_text(json.dumps(locomo))
        lme_path.write_text(json.dumps(lme))
        out = root / "out"
        report = write_outputs(full_path, locomo_path, lme_path, out)
        questions = json.loads((out / "locomo-conv26.json").read_text())["questions"]
        if [question["id"] for question in questions] != ["q1", "q2", "q3"]:
            _fail(f"dropped a locomo question: {[question['id'] for question in questions]}")
        if questions[0]["scored"] is not True or questions[2]["scored"] is not False:
            _fail(f"scored flags: {[question['scored'] for question in questions]}")
        first = questions[0]["chunks"][0]
        second = questions[0]["chunks"][1]
        if first["id"] != "D1:1" or first["text"].startswith("["):
            _fail(f"first piece kept its label prefix: {first}")
        if f"[{first['id']}] {first['text']}" != long_line[:1200]:
            _fail("first piece is not the bench line with the label removed")
        if second["id"] != "D1:1#p1" or second["text"] != long_line[1200:]:
            _fail(f"continuation piece: {second}")
        if not first["evidence"] or not second["evidence"]:
            _fail("both pieces of an evidence turn should be gold")
        joined = questions[1]
        if joined["scored"] is not True:
            _fail("a question with one present evidence id should stay scored")
        noise = next(chunk for chunk in joined["chunks"] if chunk["id"] == "D1:2")
        if noise["evidence"] is not True:
            _fail("D1:2 should be gold when it is one of the evidence ids")
        if report["locomo_conv26"]["evidence_ids_not_in_haystack"][0]["missing"] != ["D9:9"]:
            _fail(report["locomo_conv26"]["evidence_ids_not_in_haystack"])
        if report["locomo_conv26"]["split_pieces"] != 1:
            _fail(f"split count: {report['locomo_conv26']}")
        lme_questions = json.loads((out / "lme-oracle.json").read_text())["questions"]
        if lme_questions[0]["type"] != "abstention" or lme_questions[0]["scored"] is not False:
            _fail(f"abstention row: {lme_questions[0]['type']} scored={lme_questions[0]['scored']}")
        answer = lme_questions[1]
        if answer["scored"] is not True:
            _fail("longmemeval gold turn was not scored")
        answer_pieces = [chunk for chunk in answer["chunks"] if chunk["id"].startswith("s1:t0")]
        if len(answer_pieces) != 2 or not all(chunk["evidence"] for chunk in answer_pieces):
            _fail(f"longmemeval split gold: {answer_pieces}")
        other = next(chunk for chunk in answer["chunks"] if chunk["id"] == "s1:t1")
        if other["evidence"]:
            _fail("non-answer turn marked gold")
        formatted = f"[s1:t0] 2020-01-02 | user: {'Y' * 1300}"
        if f"[{answer_pieces[0]['id']}] {answer_pieces[0]['text']}" != formatted[:1200]:
            _fail("longmemeval first piece does not match run_full chunk text")
        if report["lme_oracle"]["split_pieces"] != 1:
            _fail(f"lme split count: {report['lme_oracle']}")
        for name in SOURCE_NAMES:
            if name not in report["sources_sha256"]:
                _fail(f"missing source hash {name}")
    print("prepare_e0_data self-test ok")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Prepare E0 retrieval_eval fixtures")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--full-dataset", type=Path, help="LoCoMo bench full-dataset.json")
    parser.add_argument("--locomo", type=Path, help="LoCoMo locomo10.json (question-text check)")
    parser.add_argument("--longmemeval", type=Path, help="LongMemEval oracle JSON")
    parser.add_argument("--out", type=Path, help="Directory for locomo-conv26.json, lme-oracle.json, labels-report.json")
    parser.add_argument("--conv-id", default="conv-26")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    missing = [
        flag
        for flag, value in (
            ("--full-dataset", args.full_dataset),
            ("--locomo", args.locomo),
            ("--longmemeval", args.longmemeval),
            ("--out", args.out),
        )
        if value is None
    ]
    if missing:
        parser.error(f"required: {', '.join(missing)}")
    report = write_outputs(args.full_dataset, args.locomo, args.longmemeval, args.out, args.conv_id)
    printable = {
        "locomo_conv26": {
            key: value
            for key, value in report["locomo_conv26"].items()
            if key != "evidence_ids_not_in_haystack"
        },
        "lme_oracle": report["lme_oracle"],
        "outputs_sha256": report["outputs_sha256"],
    }
    print(json.dumps(printable, indent=1))
    print(
        "evidence ids not in haystack (locomo):",
        json.dumps(report["locomo_conv26"]["evidence_ids_not_in_haystack"]),
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
