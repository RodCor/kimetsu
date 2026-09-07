"""Development-only score sweep; excludes final byte rendering and latency."""
import json
from pathlib import Path
rows=json.loads(Path(__file__).with_name("development-scores.json").read_text(encoding="utf-8"))
results=[]
for cutoff in [0,.1,.2,.3,.4,.5,.55,.6,.7,.8,.9,.95]:
    hits=noise=pos=neg=0; recall=mrr=0.
    for row in rows:
        stage=row["stages"][0]; gold=set(row["relevant"])
        candidates=sorted(stage["candidates"],key=lambda c:c["ce"],reverse=True)
        admitted=[] if stage["pre_skipped"] else [c["key"] for c in candidates if c["ce"]>=max(cutoff,.9 if stage["top_cosine"]<row["floors"]["abstain"] else cutoff)][:4]
        if gold:
            pos+=1; hits+=bool(gold.intersection(admitted));recall+=len(gold.intersection(admitted))/len(gold)
            mrr+=next((1/(i+1) for i,key in enumerate(admitted) if key in gold),0)
        else:neg+=1;noise+=bool(admitted)
    results.append(dict(cutoff=cutoff,positive_hits=hits,positive_queries=pos,negative_injections=noise,negative_queries=neg,recall=recall/pos,mrr=mrr/pos,equal_class_quality=(hits/pos+1-noise/neg)/2))
print(json.dumps(results,indent=2))
