# Paired BrainBench comparison

Same harness and fixture; run order alternates. Positive delta favors the candidate.

| Dimension | Scenarios | Baseline | Candidate | Delta | Exploratory 95% interval |
|---|---:|---:|---:|---:|---|
| retrieval | 1 | 0.818 | 0.818 | +0.000 | n/a |

Errors: baseline 0, candidate 0.
Unpaired/skipped scenarios: 0.

Exploratory paired bootstrap over scenario IDs after averaging repeats; correlated task families require a separate grouped holdout.

Wall times include process/model startup, corpus seeding and queries; they are not warm inference latency.

baseline: mean complete-run time 212.01 s (1 repeats).
candidate: mean complete-run time 215.18 s (1 repeats).

Query measurements through persistent MCP (subsequent queries reuse the process):

| Build | Positive hit@4 | Positive recall@4 | False injection | Subsequent p50 / p95 ms | Mean MCP result bytes | Peak MCP working set MiB |
|---|---:|---:|---:|---:|---:|---:|
| baseline | 0.858 | 0.842 | 0.538 | 996.880 / 1111.668 | 1262.395 | 281.215 |
| candidate | 0.858 | 0.842 | 0.538 | 1011.339 / 1094.004 | 1262.395 | 281.207 |

Measured bytes include JSON escaping; reported token estimates are retained per query but may use different accounting rules across builds. Query timing excludes the separately recorded MCP initialization and corpus seeding.
