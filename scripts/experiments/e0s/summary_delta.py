#!/usr/bin/env python3
"""Run1 vs run2 all-split metric deltas from committed E0 summary.json files.

Reads only the `all` split of each arm in run{1,2}/summary.json. Does not
launch a benchmark.

    python3 scripts/experiments/e0s/summary_delta.py \
      --run1 docs/audits/2026-11-e0s/run1/summary.json \
      --run2 docs/audits/2026-11-e0s/run2/summary.json \
      --out-dir docs/audits/2026-11-e0s
"""

from __future__ import annotations

import argparse
import json
import sys
from pathlib import Path

METRICS = ("r8", "mrr", "empty_n", "caps_median")
DATASETS = ("locomo-conv26", "lme-oracle")
ARM_ORDER = (
    "lexical",
    "vector",
    "fused_defaults",
    "rerank_off",
    "tinybert_floor_0_fill_to_cap",
    "tinybert_floor_0.1",
    "tinybert_floor_0.2",
    "semantic_floor_0",
    "rrf_k60",
)


def load_summary(path: Path) -> dict:
    payload = json.loads(path.read_text())
    if "stats" not in payload:
        raise SystemExit(f"{path} has no 'stats' key")
    return payload


def arm_order(arms: set[str]) -> list[str]:
    known = [arm for arm in ARM_ORDER if arm in arms]
    extra = sorted(arms - set(ARM_ORDER))
    return known + extra


def collect_rows(run1: dict, run2: dict) -> list[dict]:
    stats1 = run1["stats"]
    stats2 = run2["stats"]
    datasets = [name for name in DATASETS if name in stats1 or name in stats2]
    datasets += sorted((set(stats1) | set(stats2)) - set(datasets))
    rows: list[dict] = []
    for dataset in datasets:
        if dataset not in stats1 or dataset not in stats2:
            raise SystemExit(f"dataset {dataset} missing from one summary")
        arms = arm_order(set(stats1[dataset]) | set(stats2[dataset]))
        for arm in arms:
            if arm not in stats1[dataset] or arm not in stats2[dataset]:
                raise SystemExit(f"{dataset}/{arm} missing from one summary")
            a = stats1[dataset][arm]["all"]
            b = stats2[dataset][arm]["all"]
            row = {
                "dataset": dataset,
                "arm": arm,
                "n": a["n"],
                "n_scored": a["n_scored"],
            }
            for metric in METRICS:
                row[f"{metric}_run1"] = a[metric]
                row[f"{metric}_run2"] = b[metric]
                row[f"{metric}_delta"] = b[metric] - a[metric]
            if a["n"] != b["n"] or a["n_scored"] != b["n_scored"]:
                raise SystemExit(
                    f"{dataset}/{arm} n mismatch: "
                    f"run1 n={a['n']} n_scored={a['n_scored']} "
                    f"run2 n={b['n']} n_scored={b['n_scored']}"
                )
            rows.append(row)
    return rows


def fmt_num(value, metric: str) -> str:
    if metric in ("empty_n",):
        if isinstance(value, float) and value.is_integer():
            return str(int(value))
        return str(value)
    if metric in ("caps_median",):
        if isinstance(value, float) and value.is_integer():
            return str(int(value))
        return f"{value:g}"
    return f"{value:.6f}"


def fmt_delta(value, metric: str) -> str:
    if metric in ("empty_n", "caps_median"):
        if isinstance(value, float) and value.is_integer():
            return f"{int(value):+d}"
        return f"{value:+g}"
    return f"{value:+.6f}"


def maxima(rows: list[dict]) -> dict:
    by_abs = sorted(rows, key=lambda row: abs(row["r8_delta"]), reverse=True)
    top = by_abs[0]
    without_rrf = [row for row in rows if row["arm"] != "rrf_k60"]
    other = max(without_rrf, key=lambda row: abs(row["r8_delta"])) if without_rrf else None
    # all arms tied at the non-rrf max (within exact float equality)
    other_tied = (
        [
            {"dataset": row["dataset"], "arm": row["arm"], "r8_delta": row["r8_delta"]}
            for row in without_rrf
            if abs(row["r8_delta"]) == abs(other["r8_delta"]) and abs(other["r8_delta"]) != 0
        ]
        if other
        else []
    )
    rrf_rows = [row for row in rows if row["arm"] == "rrf_k60"]
    return {
        "max_abs_r8_delta": {
            "dataset": top["dataset"],
            "arm": top["arm"],
            "run1": top["r8_run1"],
            "run2": top["r8_run2"],
            "delta": top["r8_delta"],
        },
        "rrf_k60": [
            {
                "dataset": row["dataset"],
                "arm": row["arm"],
                "run1": row["r8_run1"],
                "run2": row["r8_run2"],
                "delta": row["r8_delta"],
            }
            for row in rrf_rows
        ],
        "max_abs_r8_delta_excluding_rrf_k60": (
            {
                "value": other["r8_delta"] if other else None,
                "abs": abs(other["r8_delta"]) if other else None,
                "arms": other_tied
                if other_tied
                else (
                    [{"dataset": other["dataset"], "arm": other["arm"], "r8_delta": other["r8_delta"]}]
                    if other
                    else []
                ),
            }
            if other
            else None
        ),
    }


def render_markdown(payload: dict) -> str:
    lines = [
        "# E0 run1 vs run2 all-split metric deltas",
        "",
        "Generated from the committed `run{1,2}/summary.json` `all` split. Retrieval-only numbers, not an official judge. R@8 and MRR are over scored questions; empty count and median capsules are over all rows.",
        "",
        f"- run1: `{payload['run1']}`",
        f"- run2: `{payload['run2']}`",
        f"- regenerate: `{payload['command']}`",
        "",
        "## maxima (R@8)",
        "",
    ]
    mx = payload["maxima"]
    for row in mx["rrf_k60"]:
        lines.append(
            f"- `{row['dataset']}` `rrf_k60`: {row['run1']:.6f} → {row['run2']:.6f} "
            f"(Δ {row['delta']:+.6f})"
        )
    other = mx["max_abs_r8_delta_excluding_rrf_k60"]
    if other and other["arms"]:
        arm_list = ", ".join(f"`{item['dataset']}` `{item['arm']}`" for item in other["arms"])
        lines.append(
            f"- every arm other than `rrf_k60`: |Δ R@8| at most {other['abs']:.6f} "
            f"({arm_list})"
        )
    top = mx["max_abs_r8_delta"]
    lines.append(
        f"- overall max |Δ R@8|: {abs(top['delta']):.6f} on `{top['dataset']}` `{top['arm']}`"
    )
    lines.append("")
    lines.append("## all-split table")
    lines.append("")
    lines.append(
        "| dataset | arm | n | n_scored | R@8 run1 | R@8 run2 | Δ R@8 | "
        "MRR run1 | MRR run2 | Δ MRR | empty run1 | empty run2 | Δ empty | "
        "median caps run1 | median caps run2 | Δ median caps |"
    )
    lines.append("|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|")
    for row in payload["rows"]:
        lines.append(
            f"| {row['dataset']} | {row['arm']} | {row['n']} | {row['n_scored']} | "
            f"{fmt_num(row['r8_run1'], 'r8')} | {fmt_num(row['r8_run2'], 'r8')} | {fmt_delta(row['r8_delta'], 'r8')} | "
            f"{fmt_num(row['mrr_run1'], 'mrr')} | {fmt_num(row['mrr_run2'], 'mrr')} | {fmt_delta(row['mrr_delta'], 'mrr')} | "
            f"{fmt_num(row['empty_n_run1'], 'empty_n')} | {fmt_num(row['empty_n_run2'], 'empty_n')} | {fmt_delta(row['empty_n_delta'], 'empty_n')} | "
            f"{fmt_num(row['caps_median_run1'], 'caps_median')} | {fmt_num(row['caps_median_run2'], 'caps_median')} | {fmt_delta(row['caps_median_delta'], 'caps_median')} |"
        )
    lines.append("")
    return "\n".join(lines)


def build_payload(run1_path: Path, run2_path: Path, command: str) -> dict:
    run1 = load_summary(run1_path)
    run2 = load_summary(run2_path)
    rows = collect_rows(run1, run2)
    return {
        "run1": str(run1_path),
        "run2": str(run2_path),
        "split": "all",
        "metrics": list(METRICS),
        "command": command,
        "maxima": maxima(rows),
        "rows": rows,
    }


def self_test() -> int:
    run1 = {
        "stats": {
            "locomo-conv26": {
                "lexical": {
                    "all": {
                        "n": 4,
                        "n_scored": 3,
                        "r8": 0.5,
                        "mrr": 0.25,
                        "empty_n": 0,
                        "caps_median": 8,
                    }
                },
                "rrf_k60": {
                    "all": {
                        "n": 4,
                        "n_scored": 3,
                        "r8": 0.5,
                        "mrr": 0.4,
                        "empty_n": 1,
                        "caps_median": 2,
                    }
                },
            }
        }
    }
    run2 = {
        "stats": {
            "locomo-conv26": {
                "lexical": {
                    "all": {
                        "n": 4,
                        "n_scored": 3,
                        "r8": 0.5,
                        "mrr": 0.25,
                        "empty_n": 0,
                        "caps_median": 8,
                    }
                },
                "rrf_k60": {
                    "all": {
                        "n": 4,
                        "n_scored": 3,
                        "r8": 0.475,
                        "mrr": 0.375,
                        "empty_n": 1,
                        "caps_median": 2,
                    }
                },
            }
        }
    }
    rows = collect_rows(run1, run2)
    if len(rows) != 2:
        raise SystemExit(f"self-test: expected 2 rows, got {len(rows)}")
    lex, rrf = rows
    if lex["arm"] != "lexical" or lex["r8_delta"] != 0 or lex["empty_n_delta"] != 0:
        raise SystemExit(f"self-test: lexical row wrong: {lex}")
    if rrf["arm"] != "rrf_k60" or abs(rrf["r8_delta"] - (-0.025)) > 1e-12:
        raise SystemExit(f"self-test: rrf_k60 row wrong: {rrf}")
    mx = maxima(rows)
    if mx["max_abs_r8_delta"]["arm"] != "rrf_k60":
        raise SystemExit(f"self-test: max arm wrong: {mx}")
    if abs(mx["max_abs_r8_delta"]["delta"] - (-0.025)) > 1e-12:
        raise SystemExit(f"self-test: max delta wrong: {mx}")
    text = render_markdown(
        {
            "run1": "run1/summary.json",
            "run2": "run2/summary.json",
            "command": "self-test",
            "maxima": mx,
            "rows": rows,
        }
    )
    if "rrf_k60" not in text or "0.475000" not in text:
        raise SystemExit("self-test: markdown missing expected values")
    print("summary_delta self-test ok")
    return 0


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--run1", type=Path)
    parser.add_argument("--run2", type=Path)
    parser.add_argument("--out-dir", type=Path)
    args = parser.parse_args(argv)
    if not args.self_test:
        missing = [
            flag
            for flag, value in (
                ("--run1", args.run1),
                ("--run2", args.run2),
                ("--out-dir", args.out_dir),
            )
            if value is None
        ]
        if missing:
            parser.error("the following arguments are required: " + ", ".join(missing))
    return args


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv or sys.argv[1:])
    if args.self_test:
        return self_test()
    run1_path = args.run1.resolve()
    run2_path = args.run2.resolve()
    out_dir = args.out_dir.resolve()
    script_path = Path(__file__).resolve()
    command = (
        f"python3 {script_path} --run1 {run1_path} --run2 {run2_path} --out-dir {out_dir}"
    )
    payload = build_payload(run1_path, run2_path, command)
    # store relative command in the committed artifact when possible
    payload["command"] = (
        "python3 scripts/experiments/e0s/summary_delta.py "
        "--run1 docs/audits/2026-11-e0s/run1/summary.json "
        "--run2 docs/audits/2026-11-e0s/run2/summary.json "
        "--out-dir docs/audits/2026-11-e0s"
    )
    out_dir.mkdir(parents=True, exist_ok=True)
    md_path = out_dir / "run1-run2-delta.md"
    json_path = out_dir / "run1-run2-delta.json"
    # keep source paths relative when they live in the audit dir
    payload["run1"] = "run1/summary.json"
    payload["run2"] = "run2/summary.json"
    md_path.write_text(render_markdown(payload))
    json_path.write_text(json.dumps(payload, indent=2, sort_keys=False) + "\n")
    print(f"wrote {md_path}")
    print(f"wrote {json_path}")
    top = payload["maxima"]["max_abs_r8_delta"]
    print(
        f"max |Δ R@8| = {abs(top['delta']):.6f} on {top['dataset']} {top['arm']}"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
