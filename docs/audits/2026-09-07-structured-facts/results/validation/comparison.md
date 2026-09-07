# Paired BrainBench comparison

Same harness and fixture; run order alternates. Positive delta favors the candidate.

| Dimension | Scenarios | Baseline | Candidate | Delta | Exploratory 95% interval |
|---|---:|---:|---:|---:|---|
| retrieval | 3 | 0.567 | 0.833 | +0.267 | [+0.267, +0.267] |

Errors: baseline 0, candidate 0.
Unpaired/skipped scenarios: 0.

Exploratory paired bootstrap over scenario IDs after averaging repeats; correlated task families require a separate grouped holdout.

Wall times include process/model startup, corpus seeding and queries; they are not warm inference latency.

baseline: mean complete-run time 27.15 s (2 repeats).
candidate: mean complete-run time 26.34 s (2 repeats).

Query measurements through persistent MCP (subsequent queries reuse the process):

| Build | Positive hit@4 | Positive recall@4 | False injection | Subsequent p50 / p95 ms | Mean MCP result bytes | Peak MCP working set MiB |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 0.889 | 0.833 | 0.833 | 349.661 / 376.583 | 613.489 | 653.500 |
| candidate | 0.889 | 0.833 | 0.167 | 352.385 / 386.575 | 651.244 | 653.508 |

Measured bytes include JSON escaping; reported token estimates are retained per query but may use different accounting rules across builds. Query timing excludes the separately recorded MCP initialization and corpus seeding.
