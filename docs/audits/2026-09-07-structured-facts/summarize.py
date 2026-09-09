"""Summarize saved paired observations, including corpus-based fact expectations."""
import json
from pathlib import Path

root = Path(__file__).parent
fixture = json.loads((root / "validation-frozen.json").read_text(encoding="utf-8"))
gold = {(s["id"], q["query"]): q for s in fixture["scenarios"] for q in s["queries"]}
value_gold = {(q["scenario"], q["query"]): q for q in json.loads((root / "answerability-gold.json").read_text(encoding="utf-8"))["queries"]}
assert value_gold.keys() == gold.keys()
result = {}
def semantic_answerability(row):
    value = row.get("answerability")
    if value is None:
        return None
    # Each isolated repeat creates new memory IDs. Compare evidence meaning,
    # retaining source handles in raw observations for separate provenance audit.
    return {**value, "supported": [{k: v for k, v in fact.items() if k != "sources"}
                                   for fact in value.get("supported", [])]}

for name in ("development", "answerability-regression", "validation"):
    folder = root / "results" / name
    comparison = json.loads((folder / "comparison.json").read_text(encoding="utf-8"))
    assert comparison["status"] == "complete", name
    paired = comparison["comparison"]
    assert not paired["baseline_errors"] and not paired["candidate_errors"] and not paired["unpaired_scenarios"]
    observations, counts, variation, metadata = {}, {}, {}, {}
    for side in ("baseline", "candidate"):
        repeats = []
        for path in sorted(folder.glob(f"*-{side}.json")):
            report = json.loads(path.read_text(encoding="utf-8"))
            rows = {(s["id"], q["query"]): q for s in report["scenarios"] for q in s.get("observations", [])}
            assert len(rows) == sum(len(s.get("observations", [])) for s in report["scenarios"])
            repeats.append(rows)
        rows = repeats[0]
        assert all(r.keys() == rows.keys() for r in repeats)
        observations[side] = rows
        for repeat in repeats:
            for key, row in repeat.items():
                handles = {c["expansion_handle"] for c in row.get("delivered_capsules", [])}
                for fact in (row.get("answerability") or {}).get("supported", []):
                    assert fact["sources"] and set(fact["sources"]) <= handles, (name, side, key, fact)
        variation[side] = sum(any(r[key]["ranked"] != row["ranked"] or semantic_answerability(r[key]) != semantic_answerability(row) for r in repeats) for key, row in rows.items())
        counts[side] = {
            "positive_queries": sum(q["positive_hit_at_4"] is not None for q in rows.values()),
            "positive_hits": sum(q["positive_hit_at_4"] is True for q in rows.values()),
            "negative_queries": sum(q["negative_injection"] is not None for q in rows.values()),
            "negative_injections": sum(q["negative_injection"] is True for q in rows.values()),
            "observations": sum(len(r) for r in repeats),
        }
        if name == "validation":
            assert rows.keys() == gold.keys()
            mismatches = []
            exact_per_repeat = []
            for repeat_index, repeat in enumerate(repeats, 1):
                before = len(mismatches)
                for key, row in repeat.items():
                    expected = gold[key]
                    delivered = row.get("answerability")
                    status = delivered.get("status") if delivered else None
                    missing = delivered.get("missing", []) if delivered else []
                    supported = {f["attribute"]: f["value"] for f in delivered.get("supported", [])} if delivered else None
                    conflicting = delivered.get("conflicting", []) if delivered else []
                    if (status != expected["expected_answerability"] or sorted(missing) != sorted(expected["expected_missing"])
                            or supported != value_gold[key]["supported"] or sorted(conflicting) != sorted(value_gold[key]["conflicting"])):
                        mismatches.append(dict(repeat=repeat_index, scenario=key[0], query=key[1], expected_status=expected["expected_answerability"], expected_missing=expected["expected_missing"], expected_supported=value_gold[key]["supported"], expected_conflicting=value_gold[key]["conflicting"], delivered=delivered))
                exact_per_repeat.append(len(repeat) - (len(mismatches) - before))
            metadata[side] = {"exact_per_repeat": exact_per_repeat, "exact_observations": sum(exact_per_repeat), "observations": sum(len(r) for r in repeats), "unique_queries": len(rows), "mismatches": mismatches}
    assert observations["baseline"].keys() == observations["candidate"].keys()
    losses = [list(key) for key, q in observations["baseline"].items() if q["positive_hit_at_4"] is True and observations["candidate"][key]["positive_hit_at_4"] is False]
    result[name] = dict(counts=counts, positive_losses=losses, queries_with_repeat_variation=variation, answerability=metadata, measurements=paired["measurement_summary"])
print(json.dumps(result, indent=2))
