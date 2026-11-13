#!/usr/bin/env python3
"""Bench and script profiles for scripts/retrieval_eval.py.

`script` is the profile authored on retrieval_eval (level custom, every floor
0). It is snapshotted the first time a module is configured, which is what
the pinned E0 wrapper does at import.

`bench` is a fresh `kimetsu init` plus the bench setup toggles. Init leaves
`retrieval.level = deep`, which loads TinyBERT
(`crates/kimetsu-core/src/config.rs`, `apply_retrieval_level`) and leaves
`broker.min_semantic_score` at its default of -1 (auto: 0.35 for bge).
`broker.rerank_min_score` stays at the shipped 0.3 until an arm sets it.
The bench base turns off `kimetsu.use_user_brain`, `broker.session_dedupe`,
and `broker.warm_start`. It does not pin retrieval level or score floors.

Lexical and fused also set `embedder.reranker = off`. A deep init writes
TinyBERT into that field, and `crates/kimetsu-chat/src/mcp_server.rs` loads
the cross-encoder whenever the field names a model, including when
`embedder.enabled` is false. Omitting `min_semantic_score` from the MCP
arguments leaves the config auto floor in place.

The pinned copies of the E0 scripts live in scripts/experiments/e0/. Their
bytes are the files from the run prepared at main
19b178741c95083d77b520d64fbd0496fdf1aa0a.
"""

from __future__ import annotations

import ast
import copy
import hashlib
import importlib.util
import io
import json
import subprocess
import sys
import tempfile
from contextlib import redirect_stdout
from pathlib import Path

SCRIPTS = Path(__file__).resolve().parent
CITATION = SCRIPTS / "experiments" / "e0"
CITATION_SHA256 = {
    "prepare_data.py": "4e1736abfdc822687e3fe094168e8b994ee67d497dc3ea210bab9bdf9b27b24f",
    "e0_eval.py": "9c68b8dcbb8ad09f815ccd746d5d4230b36f89cb3108965465c3ff363d25382d",
    "summarize_e0.py": "8d19cc49247ab28e6fc1e527a3ad706161c49c89d8fa2d9e95de8c0aef9cdb09",
}
# Box runner hash. The committed run_e0.sh is the portable script and must not match.
BOX_RUNNER_SHA256 = "bc7131291130d12a2f29114cb73eaafde5f310ec40a4d47ffb7ce11d23be6283"
RUNNER_BANNER = "Wait for free RAM, build release with embeddings, run both passes, diff."

_SCRIPT_SNAPSHOTS: dict[int, dict] = {}


def bench_profile(embedder: str, reranker: str) -> dict:
    return {
        "base": {
            "embedder.model": f'"{embedder}"',
            "embedder.enabled": "true",
            "kimetsu.use_user_brain": "false",
            "broker.session_dedupe": "false",
            "broker.warm_start": "false",
        },
        "arms": [
            (
                "lexical",
                {"embedder.enabled": "false", "embedder.reranker": '"off"'},
                "embedder.enabled=false, embedder.reranker=off (FTS only)",
            ),
            (
                "fused",
                {"embedder.reranker": '"off"'},
                "embedder.reranker=off (explicit opt-out outranks level=deep)",
            ),
            (
                "fused_rerank_floor_0",
                {"embedder.reranker": f'"{reranker}"', "broker.rerank_min_score": "0.0"},
                "tinybert, broker.rerank_min_score=0.0",
            ),
            (
                "fused_rerank_floor_0.3",
                {
                    "embedder.reranker": f'"{reranker}"',
                    "broker.rerank_min_score": "0.3",
                },
                "tinybert, broker.rerank_min_score=0.3 (= shipped default)",
            ),
        ],
        "mcp": {
            "budget_tokens": 16000,
            "max_capsules": 8,
            "min_score": 0.0,
            "min_lexical_coverage": 0.0,
            "abstain_evidence": 0.0,
        },
    }


# One-variable sweep from fused defaults (gauge, #68). Fused defaults = a deep
# init: linear fusion, auto semantic floor (0.35 for bge), TinyBERT at the
# shipped rerank floor 0.3. Each arm after `fused_defaults` changes exactly one
# knob. RRF k is hardcoded on main (fusion.rs DEFAULT_RRF_K = 60), so only
# k=60 is expressible without a code change. main has no capsule-fill knob;
# TinyBERT floor 0 is the fill-to-cap arm (rerank reorders, drops nothing).
SWEEP_ARMS = [
    "lexical",
    "fused_defaults",
    "rerank_off",
    "tinybert_floor_0_fill_to_cap",
    "tinybert_floor_0.1",
    "tinybert_floor_0.2",
    "semantic_floor_0",
    "rrf_k60",
]


def bench_sweep_profile(embedder: str, reranker: str) -> dict:
    bench = bench_profile(embedder, reranker)
    tiny = f'"{reranker}"'
    defaults = {"embedder.reranker": tiny, "broker.rerank_min_score": "0.3"}
    arms = [
        bench["arms"][0],
        ("fused_defaults", dict(defaults), "fused defaults: linear, auto semantic floor, tinybert floor 0.3"),
        ("rerank_off", {"embedder.reranker": '"off"'}, "embedder.reranker=off"),
        (
            "tinybert_floor_0_fill_to_cap",
            {**defaults, "broker.rerank_min_score": "0.0"},
            "fill-to-cap (reorder only), tinybert floor 0: broker.rerank_min_score=0.0",
        ),
        ("tinybert_floor_0.1", {**defaults, "broker.rerank_min_score": "0.1"}, "broker.rerank_min_score=0.1"),
        ("tinybert_floor_0.2", {**defaults, "broker.rerank_min_score": "0.2"}, "broker.rerank_min_score=0.2"),
        ("semantic_floor_0", {**defaults, "broker.min_semantic_score": "0.0"}, "broker.min_semantic_score=0.0"),
        (
            "rrf_k60",
            {**defaults, "broker.fusion": '"rrf"'},
            "rrf k=60 (hardcoded DEFAULT_RRF_K): broker.fusion=rrf",
        ),
    ]
    return {"base": bench["base"], "arms": arms, "mcp": bench["mcp"]}


def capture_script(module) -> dict:
    """Snapshot script-profile knobs the first time this module is seen."""
    key = id(module)
    if key not in _SCRIPT_SNAPSHOTS:
        _SCRIPT_SNAPSHOTS[key] = {
            "base": dict(module.BASE),
            "arms": [(arm, dict(patch), knob) for arm, patch, knob in module.BINARY_ARMS],
            "mcp": dict(module.MCP_ARGS),
        }
    return _SCRIPT_SNAPSHOTS[key]


def profile_for(module, name: str) -> dict:
    script = capture_script(module)
    if name == "script":
        return copy.deepcopy(script)
    if name == "bench":
        return bench_profile(module.EMBEDDER, module.RERANKER)
    if name == "bench-sweep":
        return bench_sweep_profile(module.EMBEDDER, module.RERANKER)
    raise SystemExit(f"unknown retrieval profile: {name}")


def apply_to(module, name: str) -> dict:
    chosen = profile_for(module, name)
    module.BASE = dict(chosen["base"])
    module.BINARY_ARMS = [(arm, dict(patch), knob) for arm, patch, knob in chosen["arms"]]
    module.MCP_ARGS = dict(chosen["mcp"])
    return chosen


def request_floors(mcp: dict, cfg: dict, embedder_on: bool, model: str) -> dict:
    """Mirror of BrainSession::resolve_request_floors for the MCP path.

    Same resolution the pinned e0_eval.py records in resolved-config-*.json.
    """

    def num(key: str):
        try:
            return float(cfg.get(key, {}).get("value"))
        except (TypeError, ValueError):
            return None

    out: dict = {}
    semantic = mcp.get("min_semantic_score")
    if semantic is None:
        semantic = num("broker.min_semantic_score")
        source = f"config broker.min_semantic_score={semantic}"
    else:
        source = f"mcp arg {semantic}"
    if semantic is not None and semantic < 0:
        effective = 0.35 if model.startswith("bge") else 0.0
        source += f" -> auto ({model}) {effective}"
        semantic = effective
    out["min_semantic_score"] = {
        "effective": semantic,
        "source": source,
        "note": None if embedder_on else "inert: no query embedding (embedder off)",
    }
    for key, cfgkey in (
        ("min_lexical_coverage", "broker.min_lexical_coverage"),
        ("abstain_evidence", "broker.abstain_min_score"),
    ):
        value = mcp.get(key)
        if value is not None:
            out[key] = {"effective": value, "source": f"mcp arg {value}"}
        else:
            out[key] = {"effective": num(cfgkey), "source": f"config {cfgkey} (auto if <0)"}
    out["max_capsules"] = {
        "effective": mcp.get("max_capsules", 3),
        "source": "mcp arg" if "max_capsules" in mcp else "mcp default 3",
    }
    out["min_score"] = {
        "effective": mcp.get("min_score", 0.15),
        "source": "mcp arg" if "min_score" in mcp else "mcp default 0.15",
    }
    out["budget_tokens"] = {"effective": mcp.get("budget_tokens", 6000), "source": "mcp arg"}
    out["stage"] = {
        "effective": mcp.get("stage", "localization"),
        "source": "mcp arg" if "stage" in mcp else "mcp default",
    }
    return out


def _sha256(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as handle:
        for block in iter(lambda: handle.read(1 << 20), b""):
            digest.update(block)
    return digest.hexdigest()


def _names(node: ast.AST) -> list[str]:
    if isinstance(node, ast.Name):
        return [node.id]
    if isinstance(node, ast.Attribute):
        return _names(node.value) + [node.attr]
    return []


def _expect_value(node: ast.AST, embedder: str, reranker: str):
    if isinstance(node, ast.Constant):
        return node.value
    if isinstance(node, ast.JoinedStr):
        names: list[str] = []
        for part in node.values:
            if isinstance(part, ast.FormattedValue):
                names.extend(_names(part.value))
        if "EMBEDDER" in names:
            return f'"{embedder}"'
        if "TINY" in names or "RERANKER" in names:
            return f'"{reranker}"'
    raise SystemExit(f"unsupported profile literal: {ast.dump(node)}")


def _dict_values(node: ast.Dict, embedder: str, reranker: str) -> dict:
    out = {}
    for key, value in zip(node.keys, node.values):
        if not isinstance(key, ast.Constant):
            raise SystemExit("profile dict key is not a constant")
        out[key.value] = _expect_value(value, embedder, reranker)
    return out


def citation_bench(embedder: str, reranker: str) -> dict:
    """Evaluate the bench literal in the pinned e0_eval.py without importing it."""
    tree = ast.parse((CITATION / "e0_eval.py").read_text())
    profiles = None
    for node in tree.body:
        if isinstance(node, ast.Assign) and any(isinstance(target, ast.Name) and target.id == "PROFILES" for target in node.targets):
            profiles = node.value
    if not isinstance(profiles, ast.Dict):
        raise SystemExit("pinned e0_eval.py has no PROFILES dict")
    bench = None
    for key, value in zip(profiles.keys, profiles.values):
        if isinstance(key, ast.Constant) and key.value == "bench":
            bench = value
    if not isinstance(bench, ast.Dict):
        raise SystemExit("pinned e0_eval.py has no bench profile")
    fields = {key.value: value for key, value in zip(bench.keys, bench.values) if isinstance(key, ast.Constant)}
    arms = []
    if not isinstance(fields.get("arms"), ast.List):
        raise SystemExit("bench arms are not a list")
    for item in fields["arms"].elts:
        if not isinstance(item, ast.Tuple) or len(item.elts) != 3:
            raise SystemExit("bench arm is not a 3-tuple")
        name, patch, knob = item.elts
        arms.append(
            (
                _expect_value(name, embedder, reranker),
                _dict_values(patch, embedder, reranker),
                _expect_value(knob, embedder, reranker),
            )
        )
    return {
        "base": _dict_values(fields["base"], embedder, reranker),
        "arms": arms,
        "mcp": _dict_values(fields["mcp"], embedder, reranker),
    }


def _render(module, patches: list[dict]) -> str:
    with tempfile.TemporaryDirectory(prefix="kimetsu-profile-") as tmp:
        return _render_in(module, Path(tmp), patches)


def _render_in(module, root: Path, patches: list[dict]) -> str:
    path = root / "project.toml"
    path.write_text(
        "\n".join(
            [
                "[retrieval]",
                'level = "deep"',
                "",
                "[embedder]",
                "enabled = true",
                'model = "placeholder"',
                'reranker = "ms-marco-tinybert-l-2-v2"',
                "",
                "[kimetsu]",
                "use_user_brain = true",
                "",
                "[broker]",
                "session_dedupe = true",
                "warm_start = true",
                "rerank_min_score = 0.3",
                "",
            ]
        )
    )
    for patch in patches:
        module.apply_config(path, patch)
    return path.read_text()


def _require(text: str, needle: str, message: str) -> None:
    if needle not in text:
        raise SystemExit(f"{message}:\n{text}")


def _forbid(text: str, needle: str, message: str) -> None:
    if needle in text:
        raise SystemExit(f"{message}:\n{text}")


def _check_runner() -> None:
    path = CITATION / "run_e0.sh"
    text = path.read_text()
    if RUNNER_BANNER not in text:
        raise SystemExit(f"run_e0.sh must document: {RUNNER_BANNER}")
    banned = (
        "/workspace/",
        "/home/box",
        "llama-server",
        "OPENSSL",
        "openssl",
        "zig",
        "407151",
        "1430040",
        "libc++",
    )
    for needle in banned:
        if needle in text:
            raise SystemExit(f"portable run_e0.sh contains {needle!r}")
    if _sha256(path) == BOX_RUNNER_SHA256:
        raise SystemExit("run_e0.sh is the box runner; commit the portable script")


def _check_summarizer() -> None:
    path = CITATION / "summarize_e0.py"
    spec = importlib.util.spec_from_file_location("summarize_e0_citation", path)
    if spec is None or spec.loader is None:
        raise SystemExit(f"could not load {path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    logged = io.StringIO()
    scored = {
        "dataset": "fixture",
        "id": "fix-wal",
        "arm": "lexical",
        "type": "single-session-user",
        "scored": True,
        "recall": {"@1": 1, "@3": 1, "@8": 1},
        "mrr": 1.0,
        "empty": 0,
        "capsules": 4,
        "labels": ["fix:t1"],
        "rank": 1,
        "excluded": None,
        "error": None,
        "skipped": False,
        "peak_rss_mib": 10,
    }
    unscored = {
        **scored,
        "id": "fix-miss",
        "scored": False,
        "recall": {"@1": 0, "@3": 0, "@8": 0},
        "mrr": 0.0,
        "empty": 1,
        "capsules": 0,
        "labels": [],
        "rank": None,
        "peak_rss_mib": 99,
    }
    with tempfile.TemporaryDirectory(prefix="kimetsu-e0-summary-") as tmp:
        root = Path(tmp)
        run1 = root / "run1"
        run2 = root / "run2"
        run1.mkdir()
        run2.mkdir()
        line = json.dumps(scored)
        other = json.dumps(unscored)
        (run1 / "fixture.jsonl").write_text(line + "\n" + other + "\n")
        louder = dict(scored)
        louder["peak_rss_mib"] = 50
        (run2 / "fixture.jsonl").write_text(json.dumps(louder) + "\n" + other + "\n")
        with redirect_stdout(logged):
            summary_rc = module.summary(run1)
            same_rc = module.diff(run1, run2, root / "same.md")
            changed = dict(scored)
            changed["labels"] = ["fix:t0"]
            (run2 / "fixture.jsonl").write_text(json.dumps(changed) + "\n" + other + "\n")
            changed_rc = module.diff(run1, run2, root / "changed.md")
        if summary_rc != 0:
            raise SystemExit(f"summarize summary failed\n{logged.getvalue()}")
        text = (run1 / "summary.md").read_text()
        if "1.000" not in text or "0.500" not in text:
            raise SystemExit(f"summary did not keep unscored rows out of recall:\n{text}")
        if same_rc != 0:
            raise SystemExit(f"peak_rss_mib should be ignored by the diff\n{logged.getvalue()}")
        if changed_rc != 1:
            raise SystemExit(f"diff missed a label change\n{logged.getvalue()}")


def _check_floors() -> None:
    mcp = {
        "budget_tokens": 16000,
        "max_capsules": 8,
        "min_score": 0.0,
        "min_lexical_coverage": 0.0,
        "abstain_evidence": 0.0,
    }
    cfg = {
        "broker.min_semantic_score": {"value": "-1"},
        "broker.min_lexical_coverage": {"value": "0.5"},
        "broker.abstain_min_score": {"value": "0"},
    }
    floors = request_floors(mcp, cfg, True, "bge-small-en-v1.5")
    if floors["min_semantic_score"]["effective"] != 0.35:
        raise SystemExit(f"bench auto floor: {floors['min_semantic_score']}")
    if "auto" not in floors["min_semantic_score"]["source"]:
        raise SystemExit(floors["min_semantic_score"]["source"])
    if request_floors(mcp, cfg, False, "bge-small-en-v1.5")["min_semantic_score"]["note"] is None:
        raise SystemExit("embedder-off floor should be marked inert")
    explicit = dict(mcp)
    explicit["min_semantic_score"] = 0.0
    pinned = request_floors(explicit, cfg, True, "bge-small-en-v1.5")
    if pinned["min_semantic_score"]["effective"] != 0.0:
        raise SystemExit(pinned["min_semantic_score"])
    if not str(pinned["min_semantic_score"]["source"]).startswith("mcp arg"):
        raise SystemExit(pinned["min_semantic_score"])


def self_test(module, fixture: Path) -> None:
    """Check the bench profile against the bundled fixture and the pinned copies."""
    import py_compile

    if module.BASE.get("retrieval.level") != '"custom"':
        raise SystemExit("script profile no longer pins retrieval.level=custom")
    if module.BASE.get("broker.min_semantic_score") != "0.0":
        raise SystemExit("script profile no longer pins broker.min_semantic_score=0")
    if module.MCP_ARGS.get("min_semantic_score") != 0.0:
        raise SystemExit("script MCP args no longer pass min_semantic_score=0")
    if module.BINARY_ARMS[0][1] != {"embedder.enabled": "false"}:
        raise SystemExit("script lexical arm changed")
    for name, digest in CITATION_SHA256.items():
        path = CITATION / name
        if _sha256(path) != digest:
            raise SystemExit(f"{path} sha256 != {digest}")
        py_compile.compile(str(path), doraise=True)
    live = bench_profile(module.EMBEDDER, module.RERANKER)
    parsed = citation_bench(module.EMBEDDER, module.RERANKER)
    if live != parsed:
        raise SystemExit(f"bench profile drifted from pinned e0_eval.py:\n{live}\n{parsed}")
    _check_floors()
    _check_runner()
    _check_summarizer()
    snap = (copy.deepcopy(module.BASE), copy.deepcopy(module.BINARY_ARMS), copy.deepcopy(module.MCP_ARGS))
    try:
        apply_to(module, "bench")
        if "retrieval.level" in module.BASE or "broker.min_semantic_score" in module.BASE:
            raise SystemExit(f"bench base pins a floor or level: {module.BASE}")
        if "min_semantic_score" in module.MCP_ARGS:
            raise SystemExit(f"bench MCP args include min_semantic_score: {module.MCP_ARGS}")
        if module.BINARY_ARMS[0][0] != "lexical" or module.BINARY_ARMS[0][1].get("embedder.reranker") != '"off"':
            raise SystemExit(f"bench lexical arm: {module.BINARY_ARMS[0]}")
        if module.BINARY_ARMS[1][1].get("embedder.reranker") != '"off"':
            raise SystemExit(f"bench fused arm: {module.BINARY_ARMS[1]}")
        questions = module.load_dataset(fixture)
        if len(questions) < 2:
            raise SystemExit("fixture has too few questions for the bench profile")
        for question in questions:
            arguments = {**module.MCP_ARGS, "query": question["query"]}
            if "min_semantic_score" in arguments:
                raise SystemExit("bench query would pass min_semantic_score")
            if arguments.get("budget_tokens") != 16000 or arguments.get("max_capsules") != 8:
                raise SystemExit(f"bench MCP args on {question['id']}: {arguments}")
            gold = [chunk["id"] for chunk in question["chunks"] if chunk.get("evidence")]
            if len(gold) != 1:
                raise SystemExit(f"fixture question {question['id']} is not one gold chunk")
        base_text = _render(module, [module.BASE])
        _require(base_text, 'level = "deep"', "bench base changed retrieval.level")
        _forbid(base_text, "min_semantic_score", "bench base pinned min_semantic_score")
        _require(base_text, 'reranker = "ms-marco-tinybert-l-2-v2"', "bench base cleared TinyBERT before the arm patch")
        _require(base_text, "use_user_brain = false", "bench base missed use_user_brain")
        _require(base_text, "session_dedupe = false", "bench base missed session_dedupe")
        _require(base_text, "warm_start = false", "bench base missed warm_start")
        _require(base_text, f'model = "{module.EMBEDDER}"', "bench base missed embedder.model")
        lexical_text = _render(module, [module.BASE, module.BINARY_ARMS[0][1]])
        _require(lexical_text, "enabled = false", "bench lexical left the embedder on")
        _require(lexical_text, 'reranker = "off"', "bench lexical left TinyBERT configured")
        _require(lexical_text, 'level = "deep"', "bench lexical changed retrieval.level")
        fused_text = _render(module, [module.BASE, module.BINARY_ARMS[1][1]])
        _require(fused_text, 'reranker = "off"', "bench fused left TinyBERT configured")
        _require(fused_text, "enabled = true", "bench fused turned the embedder off")
        floor_text = _render(module, [module.BASE, module.BINARY_ARMS[3][1]])
        _require(floor_text, f'reranker = "{module.RERANKER}"', "bench floor 0.3 missed TinyBERT")
        _require(floor_text, "rerank_min_score = 0.3", "bench floor 0.3 missed the shipped floor")
        script = capture_script(module)
        script_text = _render(module, [script["base"], script["arms"][0][1]])
        _require(script_text, 'level = "custom"', "script base missed level=custom")
        _require(script_text, "min_semantic_score = 0.0", "script base missed min_semantic_score=0")
        _require(script_text, 'reranker = "off"', "script base missed reranker=off")
        _require(script_text, "enabled = false", "script lexical missed embedder.enabled=false")
        apply_to(module, "script")
        if module.BASE.get("retrieval.level") != '"custom"' or module.MCP_ARGS.get("min_semantic_score") != 0.0:
            raise SystemExit("applying the script profile did not restore the authored defaults")
        if module.BINARY_ARMS[0][1] != {"embedder.enabled": "false"}:
            raise SystemExit("applying the script profile changed the lexical arm")
    finally:
        module.BASE, module.BINARY_ARMS, module.MCP_ARGS = snap
    if module.BASE != snap[0] or module.BINARY_ARMS != snap[1] or module.MCP_ARGS != snap[2]:
        raise SystemExit("self-test left retrieval_eval profile globals changed")
    subprocess.run(
        [sys.executable, str(SCRIPTS / "prepare_e0_data.py"), "--self-test"],
        check=True,
        stdout=subprocess.DEVNULL,
    )
    subprocess.run(
        [sys.executable, str(SCRIPTS / "e0_eval.py"), "--self-test"],
        check=True,
        stdout=subprocess.DEVNULL,
    )
