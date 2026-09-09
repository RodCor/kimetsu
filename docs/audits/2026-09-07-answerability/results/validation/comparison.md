# Paired BrainBench comparison

Same harness and fixture; run order alternates. Positive delta favors the candidate.

| Dimension | Scenarios | Baseline | Candidate | Delta | Exploratory 95% interval |
|---|---:|---:|---:|---:|---|
| retrieval | 2 | 0.727 | 1.000 | +0.273 | [+0.273, +0.273] |

Errors: baseline 0, candidate 0.
Unpaired/skipped scenarios: 0.

Exploratory paired bootstrap over scenario IDs after averaging repeats; correlated task families require a separate grouped holdout.

Wall times include process/model startup, corpus seeding and queries; they are not warm inference latency.

baseline: mean complete-run time 22.24 s (1 repeats).
candidate: mean complete-run time 22.29 s (1 repeats).

Query measurements through persistent MCP (subsequent queries reuse the process):

| Build | Positive hit@4 | Positive recall@4 | False injection | Subsequent p50 / p95 ms | Mean MCP result bytes | Peak MCP working set MiB |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 1.000 | 1.000 | 0.600 | 355.854 / 379.055 | 479.795 | 653.207 |
| candidate | 1.000 | 1.000 | 0.000 | 359.001 / 380.882 | 413.318 | 653.379 |

Measured bytes include JSON escaping; reported token estimates are retained per query but may use different accounting rules across builds. Query timing excludes the separately recorded MCP initialization and corpus seeding.
