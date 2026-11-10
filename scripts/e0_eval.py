#!/usr/bin/env python3
"""E0 wrapper around scripts/retrieval_eval.py.

Loads that module and reuses evaluate(), the MCP client, the RSS sampler, and
the metrics. `--profile` swaps BASE, BINARY_ARMS, and MCP_ARGS. Question lists
produced by prepare_e0_data.py are passed through with their `scored` flag, so
unscored questions still count toward empty rate and capsule counts.

Profiles live in scripts/retrieval_eval_profiles.py. `bench` is the default
here. `script` is retrieval_eval.py's own base (level custom, every floor 0).

The pinned copy that the E0 run executed is scripts/experiments/e0/e0_eval.py.
This file is the repo-relative entry point: it imports scripts/retrieval_eval.py
next to itself.

    python3 scripts/e0_eval.py --self-test
    python3 scripts/e0_eval.py \\
        --bin target/release/kimetsu \\
        --profile bench \\
        --dataset /path/to/e0-data/locomo-conv26.json \\
        --name locomo-conv26 \\
        --out /path/to/run1/locomo-conv26.jsonl
"""

from __future__ import annotations

import argparse
import importlib.util
import json
import resource
import shutil
import subprocess
import sys
import tempfile
import time
from pathlib import Path

HERE = Path(__file__).resolve().parent
if str(HERE) not in sys.path:
    sys.path.insert(0, str(HERE))

import retrieval_eval_profiles as profiles

_spec = importlib.util.spec_from_file_location("retrieval_eval", HERE / "retrieval_eval.py")
if _spec is None or _spec.loader is None:
    raise SystemExit(f"could not load {HERE / 'retrieval_eval.py'}")
rs = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(rs)
profiles.capture_script(rs)

CONFIG_KEYS = [
    "retrieval.level",
    "embedder.enabled",
    "embedder.model",
    "embedder.reranker",
    "broker.max_capsules",
    "broker.min_semantic_score",
    "broker.min_lexical_coverage",
    "broker.abstain_min_score",
    "broker.rerank_min_score",
    "broker.fusion",
    "broker.normalization",
    "broker.default_budget_tokens",
    "broker.compress_capsules",
    "broker.session_dedupe",
    "broker.warm_start",
    "broker.ambient",
    "kimetsu.use_user_brain",
]


def apply_profile(name: str) -> dict:
    return profiles.apply_to(rs, name)


def resolve_configs(binary: str, profile_name: str, out_path: Path) -> dict:
    chosen = profiles.profile_for(rs, profile_name)
    log = open(out_path.with_suffix(".log"), "w")
    root = Path(tempfile.mkdtemp(prefix="kimetsu-e0-config-"))
    result = {
        "profile": profile_name,
        "binary": binary,
        "mcp_args": chosen["mcp"],
        "base_patch": chosen["base"],
        "arms": {},
    }
    try:
        rs.init_workspace(binary, root, log, [])
        result["init_project_toml"] = (root / ".kimetsu" / "project.toml").read_text()
        for arm, patch, knob in chosen["arms"]:
            work = root.parent / f"{root.name}-{arm}"
            shutil.copytree(root, work)
            rs.apply_config(work / ".kimetsu" / "project.toml", patch)
            embedder_on = patch.get("embedder.enabled") != "false"
            env = rs.child_env(rs.EMBEDDER if embedder_on else None)
            cfg = {}
            for key in CONFIG_KEYS:
                completed = subprocess.run(
                    [binary, "config", "get", key],
                    cwd=work,
                    env=env,
                    text=True,
                    capture_output=True,
                    timeout=60,
                )
                cfg[key] = {"value": completed.stdout.strip(), "rc": completed.returncode}
                if completed.returncode:
                    cfg[key]["stderr"] = completed.stderr.strip()[-300:]
            model = rs.EMBEDDER if embedder_on else cfg["embedder.model"]["value"].strip('"')
            result["arms"][arm] = {
                "knob": knob,
                "patch": patch,
                "env": {
                    key: env.get(key)
                    for key in (
                        "KIMETSU_BRAIN_EMBEDDER",
                        "KIMETSU_INTRA_THREADS",
                        "OMP_NUM_THREADS",
                        "KIMETSU_USER_BRAIN",
                        "KIMETSU_BRAIN_AMBIENT",
                        "FASTEMBED_CACHE_DIR",
                        "HF_HOME",
                    )
                },
                "project_toml": (work / ".kimetsu" / "project.toml").read_text(),
                "config_get": cfg,
                "resolved": {
                    "retrieval_level": cfg["retrieval.level"]["value"],
                    "embedder_enabled": cfg["embedder.enabled"]["value"],
                    "embedder_model_effective": model
                    + (" (KIMETSU_BRAIN_EMBEDDER env wins over config)" if embedder_on else ""),
                    "reranker": cfg["embedder.reranker"]["value"],
                    "rerank_min_score": cfg["broker.rerank_min_score"]["value"],
                    "fusion": cfg["broker.fusion"]["value"],
                    "normalization": cfg["broker.normalization"]["value"],
                    "max_capsules_config": cfg["broker.max_capsules"]["value"],
                    "request_floors": profiles.request_floors(chosen["mcp"], cfg, embedder_on, model),
                },
            }
    finally:
        log.close()
        for directory in root.parent.glob(root.name + "*"):
            shutil.rmtree(directory, ignore_errors=True)
    out_path.write_text(json.dumps(result, indent=2, sort_keys=True))
    bad = [(arm, key) for arm, arm_result in result["arms"].items() for key, item in arm_result["config_get"].items() if item["rc"]]
    if bad:
        print(f"WARNING: config get failed for {bad} (see {out_path})", file=sys.stderr)
    return result


def load_questions(path: Path, limit: int) -> tuple[list[dict], dict]:
    questions = json.loads(path.read_text())["questions"]
    if limit:
        questions = questions[:limit]
    # The bundled fixture has no scored flag; every question there has gold.
    # prepare_e0_data.py always writes scored.
    scored = {question["id"]: bool(question.get("scored", True)) for question in questions}
    return questions, scored


def plan(dataset: Path, name: str, profile_name: str, limit: int) -> dict:
    apply_profile(profile_name)
    questions, scored = load_questions(dataset, limit)
    return {
        "dataset": str(dataset),
        "name": name,
        "questions": len(questions),
        "scored": sum(scored.values()),
        "profile": profile_name,
        "base": rs.BASE,
        "arms": [arm for arm, _, _ in rs.BINARY_ARMS] + ["vector (script extra)"],
        "mcp_args": rs.MCP_ARGS,
        "retrieval_eval": str(Path(rs.__file__).resolve()),
    }


def self_test() -> int:
    fixture = HERE / "fixtures" / "retrieval_eval.json"
    bench = plan(fixture, "fixture", "bench", 0)
    if bench["questions"] < 2 or bench["scored"] != bench["questions"]:
        raise SystemExit(f"bench fixture plan: {bench['questions']} scored={bench['scored']}")
    if "min_semantic_score" in bench["mcp_args"]:
        raise SystemExit(f"bench plan passed min_semantic_score: {bench['mcp_args']}")
    if "retrieval.level" in bench["base"]:
        raise SystemExit(f"bench plan pinned retrieval.level: {bench['base']}")
    if bench["base"].get("embedder.reranker"):
        raise SystemExit("bench base should leave the init reranker in place")
    lexical = rs.BINARY_ARMS[0][1]
    if lexical.get("embedder.enabled") != "false" or lexical.get("embedder.reranker") != '"off"':
        raise SystemExit(f"bench lexical patch: {lexical}")
    if "fused_rerank_floor_0.3" not in bench["arms"]:
        raise SystemExit(f"bench arms: {bench['arms']}")
    script = plan(fixture, "fixture", "script", 0)
    if script["mcp_args"].get("min_semantic_score") != 0.0:
        raise SystemExit(f"script plan dropped min_semantic_score: {script['mcp_args']}")
    if script["base"].get("retrieval.level") != '"custom"':
        raise SystemExit(f"script plan base: {script['base']}")
    if rs.BINARY_ARMS[0][1] != {"embedder.enabled": "false"}:
        raise SystemExit(f"script lexical patch changed: {rs.BINARY_ARMS[0][1]}")
    print(json.dumps({"profile": "bench", "questions": bench["questions"], "scored": bench["scored"], "arms": bench["arms"]}))
    print("e0 bench self-test ok")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description="Run retrieval_eval.py under an E0 profile")
    parser.add_argument("--self-test", action="store_true")
    parser.add_argument("--bin")
    parser.add_argument("--dataset", type=Path)
    parser.add_argument("--name")
    parser.add_argument("--profile", choices=("bench", "script"), default="bench")
    parser.add_argument("--out", type=Path, help="JSONL rows (retrieval_eval format + dataset/scored)")
    parser.add_argument("--config-only", action="store_true")
    parser.add_argument("--limit", type=int, default=0)
    parser.add_argument("--dry-run", action="store_true", help="load data + profile, print plan, run nothing")
    args = parser.parse_args()
    if args.self_test:
        return self_test()
    missing = [
        flag
        for flag, value in (("--bin", args.bin), ("--dataset", args.dataset), ("--name", args.name), ("--out", args.out))
        if value is None
    ]
    if missing:
        parser.error(f"required: {', '.join(missing)}")
    if args.dry_run:
        print(json.dumps(plan(args.dataset, args.name, args.profile, args.limit), indent=1))
        return 0
    apply_profile(args.profile)
    questions, scored = load_questions(args.dataset, args.limit)
    binary = str(Path(args.bin).resolve())
    args.out.parent.mkdir(parents=True, exist_ok=True)
    resolve_configs(binary, args.profile, args.out.parent / f"resolved-config-{args.name}.json")
    if args.config_only:
        return 0
    started = time.time()
    payload = [{key: value for key, value in question.items() if key != "scored"} for question in questions]
    rc = rs.evaluate(binary, payload, args.out)
    rows = [json.loads(line) for line in args.out.read_text().splitlines()]
    with args.out.open("w") as handle:
        for row in rows:
            row["dataset"] = args.name
            row["scored"] = scored[row["id"]]
            handle.write(json.dumps(row, sort_keys=True) + "\n")
    meta = {
        "dataset": args.name,
        "questions": len(questions),
        "rows": len(rows),
        "exit": rc,
        "wall_s": round(time.time() - started, 1),
        "peak_child_rss_mib_rusage": round(resource.getrusage(resource.RUSAGE_CHILDREN).ru_maxrss / 1024, 1),
        "peak_self_rss_mib": round(resource.getrusage(resource.RUSAGE_SELF).ru_maxrss / 1024, 1),
        "profile": args.profile,
        "seed": None,
        "seed_note": "no component takes a seed; PYTHONHASHSEED pinned by runner",
    }
    args.out.with_suffix(".meta.json").write_text(json.dumps(meta, indent=2))
    print(json.dumps(meta))
    return rc


if __name__ == "__main__":
    sys.exit(main())
