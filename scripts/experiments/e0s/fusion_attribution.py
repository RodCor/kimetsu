#!/usr/bin/env python3
"""Step-1 fusion attribution for Kimetsu hybrid retrieval (issue #68).

Reads existing E0 JSONL results (read-only) plus the prepared gold sets used
by those runs. Does not launch a benchmark.

A gold hit is a retrieved label that is in the question's evidence set, same
definition as scripts/retrieval_eval.py:rank_metrics. Recall/MRR are valid
only over scored questions (scripts/e0_eval.py). Top-8 is labels[:8].

    python3 /workspace/fuse/e0s/attribution/fusion_attribution.py
"""

from __future__ import annotations

import argparse
import hashlib
import json
import statistics
import sys
from collections import Counter
from pathlib import Path

TOP_K = 8
FILL_ARM = "tinybert_floor_0_fill_to_cap"
DEFAULTS_ARM = "fused_defaults"
VECTOR_ARM = "vector"
LEXICAL_ARM = "lexical"
DATASETS = ("locomo-conv26", "lme-oracle")
KILL_THRESHOLD = 20  # conv-26 questions affected vs fill_to_cap
KILL_DATASET = "locomo-conv26"

DEFAULT_RESULTS = Path("/workspace/fuse/e0s/results")
DEFAULT_DATA = Path("/workspace/fuse/e0/data")
DEFAULT_OUT = Path("/workspace/fuse/e0s/attribution")


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def load_gold(path: Path) -> dict[str, dict]:
    payload = json.loads(path.read_text())
    out: dict[str, dict] = {}
    for question in payload["questions"]:
        gold = {chunk["id"] for chunk in question["chunks"] if chunk.get("evidence")}
        out[question["id"]] = {"gold": gold, "scored": bool(question["scored"])}
    return out


def load_jsonl(path: Path) -> dict[str, dict[str, dict]]:
    by_id: dict[str, dict[str, dict]] = {}
    with path.open() as handle:
        for line in handle:
            row = json.loads(line)
            by_id.setdefault(row["id"], {})[row["arm"]] = row
    return by_id


def top_k(row: dict, k: int = TOP_K) -> list[str]:
    labels = row.get("labels") or []
    return [label for label in labels[:k] if label]


def gold_in_top(labels: list[str], gold: set[str]) -> list[str]:
    seen: set[str] = set()
    ordered: list[str] = []
    for label in labels:
        if label in gold and label not in seen:
            seen.add(label)
            ordered.append(label)
    return ordered


def first_hit_rank(labels: list[str], gold: set[str]) -> int | None:
    for index, label in enumerate(labels, 1):
        if label in gold:
            return index
    return None


def median(values: list[float]) -> float | None:
    if not values:
        return None
    return float(statistics.median(values))


def mean(values: list[float]) -> float | None:
    if not values:
        return None
    return float(statistics.mean(values))


def count_dist(values: list[int], lo: int, hi: int) -> dict[str, int]:
    tallies = Counter(values)
    return {str(n): int(tallies.get(n, 0)) for n in range(lo, hi + 1)}


def check_row(row: dict, gold: set[str], scored: bool) -> None:
    labels = top_k(row)
    rank = first_hit_rank(labels, gold)
    rec8 = int(rank is not None and rank <= TOP_K)
    if bool(row["scored"]) != scored:
        raise SystemExit(f"scored flag mismatch {row['dataset']} {row['id']} {row['arm']}")
    if row.get("n_evidence") != len(gold):
        raise SystemExit(f"n_evidence mismatch {row['dataset']} {row['id']} {row['arm']}")
    if scored and row["recall"]["@8"] != rec8:
        raise SystemExit(
            f"recall@8 mismatch {row['dataset']} {row['id']} {row['arm']}: "
            f"jsonl={row['recall']['@8']} computed={rec8}"
        )
    jsonl_rank = row.get("rank")
    if jsonl_rank != rank and not (jsonl_rank is None and rank is None):
        # rank_metrics uses the full labels list; we sliced to 8, which is the
        # stored list length (MAX_CAPSULES). A mismatch here is a data error.
        raise SystemExit(
            f"rank mismatch {row['dataset']} {row['id']} {row['arm']}: "
            f"jsonl={jsonl_rank} computed={rank}"
        )


def compare_loss(
    scored_ids: list[str],
    rows: dict[str, dict[str, dict]],
    golds: dict[str, dict],
    other_arm: str,
) -> dict:
    questions_lost = 0
    questions_gained = 0
    questions_net_lost = 0
    questions_net_gained = 0
    items_lost = 0
    items_gained = 0
    items_lost_vector_only = 0
    items_lost_both = 0
    questions_lost_vector_only = 0
    questions_lost_both = 0
    questions_vector_has_gold = 0
    questions_other_has_gold = 0
    questions_r8_drop = 0
    questions_r8_gain = 0
    lexical_overlap_on_vo_loss: list[int] = []
    fill_caps_on_vo_loss: list[int] = []
    lexical_overlap_all_loss: list[int] = []

    for qid in scored_ids:
        gold = golds[qid]["gold"]
        vector = top_k(rows[qid][VECTOR_ARM])
        other = top_k(rows[qid][other_arm])
        lexical = top_k(rows[qid][LEXICAL_ARM])
        v_gold = set(gold_in_top(vector, gold))
        o_gold = set(gold_in_top(other, gold))
        lex_set = set(lexical)
        if v_gold:
            questions_vector_has_gold += 1
        if o_gold:
            questions_other_has_gold += 1
        lost = v_gold - o_gold
        gained = o_gold - v_gold
        if lost:
            questions_lost += 1
            items_lost += len(lost)
            lexical_overlap_all_loss.append(sum(1 for label in other if label in lex_set))
            vo = {item for item in lost if item not in lex_set}
            both = lost - vo
            items_lost_vector_only += len(vo)
            items_lost_both += len(both)
            if vo:
                questions_lost_vector_only += 1
            if both:
                questions_lost_both += 1
            if vo:
                fill_caps_on_vo_loss.append(len(other))
                lexical_overlap_on_vo_loss.append(sum(1 for label in other if label in lex_set))
        if gained:
            questions_gained += 1
            items_gained += len(gained)
        if len(lost) > len(gained):
            questions_net_lost += 1
        elif len(gained) > len(lost):
            questions_net_gained += 1
        v_r8 = int(bool(v_gold))
        o_r8 = int(bool(o_gold))
        if v_r8 == 1 and o_r8 == 0:
            questions_r8_drop += 1
        if v_r8 == 0 and o_r8 == 1:
            questions_r8_gain += 1

    return {
        "other_arm": other_arm,
        "n_scored": len(scored_ids),
        "questions_where_vector_has_ge1_gold_in_top8": questions_vector_has_gold,
        "questions_where_other_has_ge1_gold_in_top8": questions_other_has_gold,
        "questions_vector_gold_missed_by_other": questions_lost,
        "lost_gold_items": items_lost,
        "lost_breakdown": {
            "vector_only_items": items_lost_vector_only,
            "vector_only_questions": questions_lost_vector_only,
            "both_items": items_lost_both,
            "both_questions": questions_lost_both,
            "note": (
                "vector-only: the lost gold item was in vector's top 8 and not in "
                "lexical's top 8. both: the lost gold item was also in lexical's top 8. "
                "a question can contribute to both counts if it lost one of each."
            ),
        },
        "reverse": {
            "questions_other_gold_missed_by_vector": questions_gained,
            "gained_gold_items": items_gained,
        },
        "net": {
            "lost_minus_gained_gold_items": items_lost - items_gained,
            "questions_with_more_lost_than_gained": questions_net_lost,
            "questions_with_more_gained_than_lost": questions_net_gained,
        },
        "binary_recall_at_8_flips": {
            "note": (
                "binary R@8 is the official retrieval_eval recall (at least one gold "
                "in top 8). item counts above can change without a flip."
            ),
            "questions_vector_r8_1_other_r8_0": questions_r8_drop,
            "questions_vector_r8_0_other_r8_1": questions_r8_gain,
            "net_r8_question_change": questions_r8_gain - questions_r8_drop,
        },
        "crowding_on_vector_only_losses": crowding_block(
            lexical_overlap_on_vo_loss, fill_caps_on_vo_loss, other_arm
        ),
        "crowding_on_all_losses": {
            "questions": len(lexical_overlap_all_loss),
            "other_capsules_also_in_lexical_top8": {
                "mean": mean(lexical_overlap_all_loss),
                "median": median(lexical_overlap_all_loss),
                "distribution_0_to_8": count_dist(lexical_overlap_all_loss, 0, TOP_K),
            },
        },
        "per_capsule_scores": "not available: jsonl rows have no per-capsule scores or ranks; only ordered labels and a first-hit rank",
    }


def crowding_block(overlap: list[int], n_caps: list[int], other_arm: str) -> dict:
    return {
        "note": (
            f"for each scored question that lost at least one vector-only gold item, "
            f"how many of {other_arm}'s top-8 capsules also appear in lexical's top 8. "
            "high overlap is evidence that FTS/bm25 hits occupy slots that could have "
            "held the missing vector-only gold."
        ),
        "questions": len(overlap),
        "other_capsule_count": {
            "mean": mean(n_caps),
            "median": median(n_caps),
        },
        "other_capsules_also_in_lexical_top8": {
            "mean": mean(overlap),
            "median": median(overlap),
            "distribution_0_to_8": count_dist(overlap, 0, TOP_K),
        },
    }


def analyze_dataset(jsonl_path: Path, gold_path: Path, dataset: str) -> dict:
    golds = load_gold(gold_path)
    rows = load_jsonl(jsonl_path)
    if set(rows) != set(golds):
        raise SystemExit(f"id set mismatch between {jsonl_path} and {gold_path}")
    needed = (VECTOR_ARM, LEXICAL_ARM, FILL_ARM, DEFAULTS_ARM)
    scored_ids: list[str] = []
    for qid, arms in rows.items():
        missing = [arm for arm in needed if arm not in arms]
        if missing:
            raise SystemExit(f"{jsonl_path} {qid} missing arms {missing}")
        scored = golds[qid]["scored"]
        for arm in needed:
            check_row(arms[arm], golds[qid]["gold"], scored)
        if scored:
            scored_ids.append(qid)
    scored_ids.sort()
    return {
        "dataset": dataset,
        "jsonl": str(jsonl_path),
        "gold": str(gold_path),
        "n_questions": len(rows),
        "n_scored": len(scored_ids),
        "vs_tinybert_floor_0_fill_to_cap": compare_loss(scored_ids, rows, golds, FILL_ARM),
        "vs_fused_defaults": compare_loss(scored_ids, rows, golds, DEFAULTS_ARM),
    }


def fmt(value) -> str:
    if value is None:
        return "n/a"
    if isinstance(value, float):
        return f"{value:.3f}"
    return str(value)


def dist_line(dist: dict[str, int]) -> str:
    parts = [f"{k}:{v}" for k, v in dist.items() if v]
    return "{" + ", ".join(parts) + "}" if parts else "{}"


def write_comparison(lines: list[str], title: str, block: dict) -> None:
    lost = block["lost_breakdown"]
    rev = block["reverse"]
    net = block["net"]
    r8 = block["binary_recall_at_8_flips"]
    crowd = block["crowding_on_vector_only_losses"]
    lines.append(f"### {title}")
    lines.append("")
    lines.append(f"- scored questions: {block['n_scored']} (measured)")
    lines.append(
        f"- questions where vector has ≥1 gold in top 8: "
        f"{block['questions_where_vector_has_ge1_gold_in_top8']} (measured)"
    )
    lines.append(
        f"- questions where {block['other_arm']} has ≥1 gold in top 8: "
        f"{block['questions_where_other_has_ge1_gold_in_top8']} (measured)"
    )
    lines.append(
        f"- questions where vector has ≥1 gold in top 8 that {block['other_arm']} misses: "
        f"{block['questions_vector_gold_missed_by_other']} (measured)"
    )
    lines.append(f"- lost gold items: {block['lost_gold_items']} (measured)")
    lines.append(
        f"- of those lost items, vector-only (not in lexical top 8): "
        f"{lost['vector_only_items']} items across {lost['vector_only_questions']} questions (measured)"
    )
    lines.append(
        f"- of those lost items, also in lexical top 8: "
        f"{lost['both_items']} items across {lost['both_questions']} questions (measured)"
    )
    lines.append(
        f"- reverse: questions where {block['other_arm']} has ≥1 gold in top 8 that vector misses: "
        f"{rev['questions_other_gold_missed_by_vector']} (measured)"
    )
    lines.append(f"- reverse: gained gold items: {rev['gained_gold_items']} (measured)")
    lines.append(
        f"- net gold items (lost − gained): {net['lost_minus_gained_gold_items']} (measured)"
    )
    lines.append(
        f"- binary R@8 flips (retrieval_eval definition, ≥1 gold in top 8): "
        f"vector=1 other=0 on {r8['questions_vector_r8_1_other_r8_0']} questions; "
        f"vector=0 other=1 on {r8['questions_vector_r8_0_other_r8_1']}; "
        f"net question change {r8['net_r8_question_change']:+d} (measured)"
    )
    lines.append(
        f"- per-capsule scores/ranks: {block['per_capsule_scores']}"
    )
    if crowd["questions"]:
        ov = crowd["other_capsules_also_in_lexical_top8"]
        caps = crowd["other_capsule_count"]
        lines.append(
            f"- crowding (questions with a vector-only loss, n={crowd['questions']}): "
            f"{block['other_arm']} returned median {fmt(caps['median'])} capsules "
            f"(mean {fmt(caps['mean'])}); of those, median {fmt(ov['median'])} "
            f"(mean {fmt(ov['mean'])}) were also in lexical's top 8; "
            f"distribution of overlap 0–8: {dist_line(ov['distribution_0_to_8'])} (measured)"
        )
    else:
        lines.append("- crowding (questions with a vector-only loss): none (measured)")
    lines.append("")


def render_markdown(payload: dict) -> str:
    lines = [
        "# E0 step-1 fusion attribution (run1, with run2 stability)",
        "",
        "retrieval-only numbers, not an official judge. gold hit = retrieved `labels` entry in the question's evidence set, same as `scripts/retrieval_eval.py:rank_metrics`. counts are over scored questions only (`scripts/e0_eval.py`). top 8 = `labels[:8]`.",
        "",
        "hypothesis: unbounded bm25 on FTS candidates lets lexical hits outrank vector-only gold, so `tinybert_floor_0_fill_to_cap` (and `fused_defaults`) lose gold that the vector arm holds in its top 8.",
        "",
        "kill rule (gauge): if fewer than 20 conv-26 questions are affected (vector has ≥1 gold in top 8 that `tinybert_floor_0_fill_to_cap` misses), fusion is not the main loss.",
        "",
    ]
    kill = payload["kill_rule"]
    verdict = "fires" if kill["fires"] else "does not fire"
    lines.append(
        f"**kill rule {verdict}.** conv-26 run1 affected questions vs fill_to_cap: "
        f"{kill['run1_affected_conv26_vs_fill_to_cap']} (threshold {kill['threshold']}). "
        f"{'fusion is not the main loss on conv-26.' if kill['fires'] else 'at least 20 conv-26 questions are affected; the fusion arm stays live.'}"
    )
    lines.append("")

    for run_name in ("run1", "run2"):
        lines.append(f"## {run_name}")
        lines.append("")
        for dataset in DATASETS:
            block = payload["runs"][run_name][dataset]
            lines.append(f"### {dataset}")
            lines.append("")
            lines.append(
                f"n = {block['n_questions']} questions, {block['n_scored']} scored "
                f"(from `{block['jsonl']}`, gold `{block['gold']}`)."
            )
            lines.append("")
            write_comparison(
                lines,
                f"{run_name} / {dataset}: vector vs tinybert_floor_0_fill_to_cap",
                block["vs_tinybert_floor_0_fill_to_cap"],
            )
            write_comparison(
                lines,
                f"{run_name} / {dataset}: vector vs fused_defaults",
                block["vs_fused_defaults"],
            )

    lines.append("## run1 vs run2 stability")
    lines.append("")
    for dataset in DATASETS:
        a = payload["runs"]["run1"][dataset]["vs_tinybert_floor_0_fill_to_cap"]
        b = payload["runs"]["run2"][dataset]["vs_tinybert_floor_0_fill_to_cap"]
        lines.append(f"- {dataset} vs fill_to_cap: affected questions {a['questions_vector_gold_missed_by_other']} → {b['questions_vector_gold_missed_by_other']}; lost items {a['lost_gold_items']} → {b['lost_gold_items']}; vector-only items {a['lost_breakdown']['vector_only_items']} → {b['lost_breakdown']['vector_only_items']}; net items {a['net']['lost_minus_gained_gold_items']} → {b['net']['lost_minus_gained_gold_items']} (measured)")
        c = payload["runs"]["run1"][dataset]["vs_fused_defaults"]
        d = payload["runs"]["run2"][dataset]["vs_fused_defaults"]
        lines.append(f"- {dataset} vs fused_defaults: affected questions {c['questions_vector_gold_missed_by_other']} → {d['questions_vector_gold_missed_by_other']}; lost items {c['lost_gold_items']} → {d['lost_gold_items']}; vector-only items {c['lost_breakdown']['vector_only_items']} → {d['lost_breakdown']['vector_only_items']}; net items {c['net']['lost_minus_gained_gold_items']} → {d['net']['lost_minus_gained_gold_items']} (measured)")
    lines.append("")

    lines.append("## inputs and rerun")
    lines.append("")
    lines.append(f"- script: `{payload['script']}` sha256 `{payload['script_sha256']}`")
    lines.append("- inputs (sha256):")
    for name, digest in payload["inputs_sha256"].items():
        lines.append(f"  - `{name}`: `{digest}`")
    lines.append(f"- rerun: `{payload['command']}`")
    lines.append("")
    return "\n".join(lines)


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--results", type=Path, default=DEFAULT_RESULTS)
    parser.add_argument("--data", type=Path, default=DEFAULT_DATA)
    parser.add_argument("--out-dir", type=Path, default=DEFAULT_OUT)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    script_path = Path(__file__).resolve()
    gold_paths = {name: args.data / f"{name}.json" for name in DATASETS}
    input_paths: dict[str, Path] = {str(path): path for path in gold_paths.values()}
    runs: dict[str, dict] = {}
    for run_name in ("run1", "run2"):
        runs[run_name] = {}
        for dataset in DATASETS:
            jsonl = args.results / run_name / f"{dataset}.jsonl"
            input_paths[str(jsonl)] = jsonl
            if not jsonl.is_file():
                raise SystemExit(f"missing {jsonl}")
            runs[run_name][dataset] = analyze_dataset(jsonl, gold_paths[dataset], dataset)

    affected = runs["run1"][KILL_DATASET]["vs_tinybert_floor_0_fill_to_cap"][
        "questions_vector_gold_missed_by_other"
    ]
    payload = {
        "scorer": "scripts/retrieval_eval.py rank_metrics via scripts/e0_eval.py; retrieval only, not an official judge",
        "top_k": TOP_K,
        "scored_only": True,
        "kill_rule": {
            "rule": (
                "if fewer than 20 conv-26 questions are affected, fusion is not the main loss"
            ),
            "dataset": KILL_DATASET,
            "other_arm": FILL_ARM,
            "threshold": KILL_THRESHOLD,
            "run1_affected_conv26_vs_fill_to_cap": affected,
            "fires": affected < KILL_THRESHOLD,
        },
        "runs": runs,
        "script": str(script_path),
        "script_sha256": sha256_file(script_path),
        "inputs_sha256": {path: sha256_file(handle) for path, handle in sorted(input_paths.items())},
        "command": (
            f"python3 {script_path} --results {args.results.resolve()} "
            f"--data {args.data.resolve()} --out-dir {args.out_dir.resolve()}"
        ),
    }

    args.out_dir.mkdir(parents=True, exist_ok=True)
    md_path = args.out_dir / "attribution-run1.md"
    json_path = args.out_dir / "attribution-run1.json"
    run2_json = args.out_dir / "attribution-run2.json"
    md_path.write_text(render_markdown(payload))
    json_path.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n")
    run2_only = {
        "run": "run2",
        "kill_rule_note": "kill rule is evaluated on run1; run2 is a stability check",
        "datasets": runs["run2"],
        "script_sha256": payload["script_sha256"],
        "command": payload["command"],
    }
    run2_json.write_text(json.dumps(run2_only, indent=2, sort_keys=False) + "\n")
    print(f"wrote {md_path}")
    print(f"wrote {json_path}")
    print(f"wrote {run2_json}")
    print(
        f"kill rule {'fires' if payload['kill_rule']['fires'] else 'does not fire'}: "
        f"conv-26 run1 affected={affected}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
