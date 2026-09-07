# Paired BrainBench comparison

Same harness and fixture; run order alternates. Positive delta favors the candidate.

| Dimension | Scenarios | Baseline | Candidate | Delta | Exploratory 95% interval |
|---|---:|---:|---:|---:|---|
| retrieval | 4 | 0.736 | 1.000 | +0.264 | [+0.236, +0.278] |

Errors: baseline 0, candidate 0.
Unpaired/skipped scenarios: 0.

Exploratory paired bootstrap over scenario IDs after averaging repeats; correlated task families require a separate grouped holdout.

Wall times include process/model startup, corpus seeding and queries; they are not warm inference latency.

baseline: mean complete-run time 41.14 s (2 repeats).
candidate: mean complete-run time 38.56 s (2 repeats).

Query measurements through persistent MCP (subsequent queries reuse the process):

| Build | Positive hit@4 | Positive recall@4 | False injection | Subsequent p50 / p95 ms | Mean MCP result bytes | Peak MCP working set MiB |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 1.000 | 1.000 | 0.792 | 366.114 / 516.582 | 567.458 | 653.559 |
| candidate | 1.000 | 1.000 | 0.000 | 362.636 / 407.324 | 492.931 | 653.449 |

Measured bytes include JSON escaping; reported token estimates are retained per query but may use different accounting rules across builds. Query timing excludes the separately recorded MCP initialization and corpus seeding.
