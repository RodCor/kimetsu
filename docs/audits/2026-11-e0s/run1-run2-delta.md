# E0 run1 vs run2 all-split metric deltas

Generated from the committed `run{1,2}/summary.json` `all` split. Retrieval-only numbers, not an official judge. R@8 and MRR are over scored questions; empty count and median capsules are over all rows.

- run1: `run1/summary.json`
- run2: `run2/summary.json`
- regenerate: `python3 scripts/experiments/e0s/summary_delta.py --run1 docs/audits/2026-11-e0s/run1/summary.json --run2 docs/audits/2026-11-e0s/run2/summary.json --out-dir docs/audits/2026-11-e0s`

## maxima (R@8)

- `locomo-conv26` `rrf_k60`: 0.487310 → 0.461929 (Δ -0.025381)
- `lme-oracle` `rrf_k60`: 0.624217 → 0.638831 (Δ +0.014614)
- every arm other than `rrf_k60`: |Δ R@8| at most 0.004175 (`lme-oracle` `fused_defaults`, `lme-oracle` `rerank_off`, `lme-oracle` `tinybert_floor_0_fill_to_cap`, `lme-oracle` `tinybert_floor_0.1`, `lme-oracle` `semantic_floor_0`)
- overall max |Δ R@8|: 0.025381 on `locomo-conv26` `rrf_k60`

## all-split table

| dataset | arm | n | n_scored | R@8 run1 | R@8 run2 | Δ R@8 | MRR run1 | MRR run2 | Δ MRR | empty run1 | empty run2 | Δ empty | median caps run1 | median caps run2 | Δ median caps |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|---:|
| locomo-conv26 | lexical | 199 | 197 | 0.568528 | 0.568528 | +0.000000 | 0.294906 | 0.294906 | +0.000000 | 0 | 0 | +0 | 8 | 8 | +0 |
| locomo-conv26 | vector | 199 | 197 | 0.670051 | 0.670051 | +0.000000 | 0.435551 | 0.435551 | +0.000000 | 0 | 0 | +0 | 8 | 8 | +0 |
| locomo-conv26 | fused_defaults | 199 | 197 | 0.446701 | 0.446701 | +0.000000 | 0.387817 | 0.380203 | -0.007614 | 0 | 0 | +0 | 2 | 2 | +0 |
| locomo-conv26 | rerank_off | 199 | 197 | 0.482234 | 0.482234 | +0.000000 | 0.271906 | 0.272075 | +0.000169 | 0 | 0 | +0 | 8 | 8 | +0 |
| locomo-conv26 | tinybert_floor_0_fill_to_cap | 199 | 197 | 0.482234 | 0.482234 | +0.000000 | 0.397172 | 0.394634 | -0.002538 | 0 | 0 | +0 | 8 | 8 | +0 |
| locomo-conv26 | tinybert_floor_0.1 | 199 | 197 | 0.446701 | 0.446701 | +0.000000 | 0.387817 | 0.382741 | -0.005076 | 0 | 0 | +0 | 2 | 2 | +0 |
| locomo-conv26 | tinybert_floor_0.2 | 199 | 197 | 0.441624 | 0.441624 | +0.000000 | 0.385279 | 0.380203 | -0.005076 | 0 | 0 | +0 | 2 | 2 | +0 |
| locomo-conv26 | semantic_floor_0 | 199 | 197 | 0.446701 | 0.446701 | +0.000000 | 0.387817 | 0.382741 | -0.005076 | 0 | 0 | +0 | 2 | 2 | +0 |
| locomo-conv26 | rrf_k60 | 199 | 197 | 0.487310 | 0.461929 | -0.025381 | 0.400302 | 0.383503 | -0.016800 | 0 | 0 | +0 | 2 | 2 | +0 |
| lme-oracle | lexical | 500 | 479 | 0.881002 | 0.881002 | +0.000000 | 0.591413 | 0.591413 | +0.000000 | 0 | 0 | +0 | 8 | 8 | +0 |
| lme-oracle | vector | 500 | 479 | 0.951983 | 0.951983 | +0.000000 | 0.589045 | 0.589045 | +0.000000 | 0 | 0 | +0 | 8 | 8 | +0 |
| lme-oracle | fused_defaults | 500 | 479 | 0.818372 | 0.822547 | +0.004175 | 0.631236 | 0.632836 | +0.001601 | 0 | 0 | +0 | 6 | 6 | +0 |
| lme-oracle | rerank_off | 500 | 479 | 0.874739 | 0.878914 | +0.004175 | 0.582933 | 0.585786 | +0.002853 | 0 | 0 | +0 | 8 | 8 | +0 |
| lme-oracle | tinybert_floor_0_fill_to_cap | 500 | 479 | 0.874739 | 0.878914 | +0.004175 | 0.650268 | 0.651625 | +0.001357 | 0 | 0 | +0 | 8 | 8 | +0 |
| lme-oracle | tinybert_floor_0.1 | 500 | 479 | 0.807933 | 0.812109 | +0.004175 | 0.630590 | 0.632190 | +0.001601 | 0 | 0 | +0 | 4 | 4 | +0 |
| lme-oracle | tinybert_floor_0.2 | 500 | 479 | 0.816284 | 0.818372 | +0.002088 | 0.632106 | 0.633010 | +0.000905 | 0 | 0 | +0 | 5 | 5 | +0 |
| lme-oracle | semantic_floor_0 | 500 | 479 | 0.818372 | 0.822547 | +0.004175 | 0.632627 | 0.634228 | +0.001601 | 0 | 0 | +0 | 6 | 6 | +0 |
| lme-oracle | rrf_k60 | 500 | 479 | 0.624217 | 0.638831 | +0.014614 | 0.494878 | 0.504918 | +0.010041 | 0 | 0 | +0 | 8 | 8 | +0 |
