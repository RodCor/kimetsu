"""Summarize the paired saved observations; never runs inference."""
import json
from pathlib import Path
root = Path(__file__).parent
result = {}
for name in ["development", "missing-fact-development", "validation", "missing-fact-timing-followup"]:
    folder = root / "results" / name
    comparison = json.loads((folder / "comparison.json").read_text(encoding="utf-8"))
    assert comparison["status"] == "complete"
    paired = comparison["comparison"]
    assert not paired["baseline_errors"] and not paired["candidate_errors"] and not paired["unpaired_scenarios"]
    observations = {}
    counts = {}
    variation = {}
    for side in ["baseline", "candidate"]:
        report = json.loads((folder / f"1-{side}.json").read_text(encoding="utf-8"))
        rows = {(s["id"], q["query"]): q for s in report["scenarios"] for q in s.get("observations", [])}
        observations[side] = rows
        repeated = []
        for path in sorted(folder.glob(f"*-{side}.json")):
            report_repeat = json.loads(path.read_text(encoding="utf-8"))
            repeat_rows = {(s["id"], q["query"]): q for s in report_repeat["scenarios"] for q in s.get("observations", [])}
            assert repeat_rows.keys() == rows.keys()
            repeated.append(repeat_rows)
        variation[side] = sum(any(repeat[key]["ranked"] != row["ranked"] for repeat in repeated) for key, row in rows.items())
        counts[side] = {
            "positive_queries": sum(q["positive_hit_at_4"] is not None for q in rows.values()),
            "positive_hits": sum(q["positive_hit_at_4"] is True for q in rows.values()),
            "negative_queries": sum(q["negative_injection"] is not None for q in rows.values()),
            "negative_injections": sum(q["negative_injection"] is True for q in rows.values()),
        }
    assert observations["baseline"].keys() == observations["candidate"].keys()
    losses = [list(key) for key, q in observations["baseline"].items()
              if q["positive_hit_at_4"] is True and observations["candidate"][key]["positive_hit_at_4"] is False]
    result[name] = dict(counts=counts, positive_losses=losses, queries_with_repeat_ranking_variation=variation, measurements=paired["measurement_summary"])
print(json.dumps(result, indent=2))
