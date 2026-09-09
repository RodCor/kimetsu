"""Summarize saved paired reports without running inference."""
import json
from pathlib import Path
ROOT=Path(__file__).parent
out={}
for name in ["contract","development","validation"]:
    folder=ROOT/name
    if not (folder/"comparison.json").exists():
        continue
    comparison=json.loads((folder/"comparison.json").read_text(encoding="utf-8"))
    assert comparison["status"]=="complete"
    info=comparison["comparison"]
    assert not info["baseline_errors"] and not info["candidate_errors"] and not info["unpaired_scenarios"]
    rows={}
    variation={}
    for side in ["baseline","candidate"]:
        repeats=[]
        for i in [1,2,3]:
            report=json.loads((folder/f"{i}-{side}.json").read_text(encoding="utf-8"))
            repeats.append({(s["id"],q["query"]):q for s in report["scenarios"] for q in s.get("observations",[])})
        rows[side]=repeats[0]
        variation[side]=sum(any(r[k]["ranked"]!=repeats[0][k]["ranked"] for r in repeats[1:]) for k in repeats[0])
    assert rows["baseline"].keys()==rows["candidate"].keys()
    gains=sum(not b["positive_hit_at_4"] and rows["candidate"][k]["positive_hit_at_4"] for k,b in rows["baseline"].items() if b["positive_hit_at_4"] is not None)
    losses=sum(b["positive_hit_at_4"] and not rows["candidate"][k]["positive_hit_at_4"] for k,b in rows["baseline"].items() if b["positive_hit_at_4"] is not None)
    out[name]=dict(measurements=info["measurement_summary"],positive_gains=gains,positive_losses=losses,queries_with_ranking_variation=variation)
    if name=="validation":
        fixture=json.loads((ROOT/"validation-frozen.json").read_text(encoding="utf-8"))
        metadata={(s["id"],q["query"]):q for s in fixture["scenarios"] for q in s["queries"]}
        cohorts={}
        for side,observations in rows.items():
            cohorts[side]={}
            for lang in ["en","es"]:
                selected=[q for k,q in observations.items() if metadata[k]["language"]==lang]
                cohorts[side][lang]=dict(positive_hits=sum(q["positive_hit_at_4"] is True for q in selected),positive_queries=sum(q["positive_hit_at_4"] is not None for q in selected),negative_injections=sum(q["negative_injection"] is True for q in selected),negative_queries=sum(q["negative_injection"] is not None for q in selected))
        out[name]["language_cohorts"]=cohorts
print(json.dumps(out,indent=2))
