# Structured fact answerability (v2.8.0, opt-in)

The broker can attach evidence accounting to direct configuration questions: which attributes are supported, missing, or conflicting. This is a bounded syntactic check on retrieved evidence, not a general truth or entailment model.

## Enable on a build containing this change

```sh
kimetsu config set broker.explicit_fact_guard true
```

The default is `false`. Use the same command with `false` to disable it. This feature requires v2.8.0 or a build containing these changes. If a warm embed daemon is running an older executable, restart it with the updated build before comparing daemon behavior.

An explicit statement such as `Orchid staging gateway port is 7319.` can support the port part of `What are the Orchid staging gateway port and timeout?`. The response reports timeout missing unless matching evidence is retrieved. Production evidence cannot fill a staging request. Distinct eligible values produce a conflict; equivalent durations such as `30 seconds` and `30000 ms` compare using exact rational arithmetic.

MCP context responses may contain `answerability` with `status`, `subject`, `environment`, `supported` values and source handles, `missing`, and `conflicting`. Hooks emit a short notice for incomplete or conflicting evidence. A `supported` status means the retrieved evidence supports the recognized attributes, not that the current code or outside world was independently verified.

## Storage and cost

Schema 15 adds a rebuildable SQLite fact projection. Facts bind to the memory's claim revision, source event and exact evidence excerpt; corrections refresh it transactionally and replay rebuilds it. Current lifecycle, temporal bounds and text digest checks prevent stale evidence from supporting another revision. The agent record API's tag prefix is recognized without using tags to infer subject or environment.

Projection maintenance adds write/storage work even when the guard is disabled; ordinary retrieval skips fact hydration while it is disabled. No extra model calls are required. Delivered evidence and metadata still consume agent context tokens. MCP output admission includes serialized metadata in its byte upper bound; that bound is not a measured tokenizer count.

## v2.8.0 metrics

These measurements cover the implementation included in v2.8.0. They were
recorded on September 7 using implementation `3ae8329` and harness `2c74dad`,
against the preceding answerability build with the guard enabled on both sides.
They are not a fresh tagged-release run or a v2.7.0-versus-v2.8.0 comparison.

| Metric (45 synthetic cases, two repeats) | Baseline | Candidate |
|---|---:|---:|
| Unwanted injections | 15/18 (83.3%) | 3/18 (16.7%) |
| Positive retrieval hits | 24/27 (88.9%) | 24/27 (88.9%) |
| Exact evidence metadata | Not emitted | 36/45 (80%) per repeat |
| Subsequent-query p95 | 376.6 ms | 386.6 ms |
| Mean MCP response bytes | 613.5 | 651.2 |

Unwanted injections decreased by 80% relatively, or 66.7 percentage points.
P95 increased by 10.0 ms (2.7%), and response size by 37.8 bytes (6.2%). Deltas use unrounded measurements;
the table displays rounded values.
Across the three fixtures, 688 observations covered 299 distinct scenario/query
cases without positive-hit losses. The short synthetic runs do not establish
a statistically significant performance change or general answer accuracy.

Separate release-preparation validation passed 1,500 workspace tests (six
ignored) and 132 benchmark Rust tests after dependency updates. See the
[release review](audits/2026-09-08-release-security.md) for that validation;
it does not change the original benchmark provenance.

## Measured results and limits

On the frozen 45-query synthetic fixture, unwanted injections fell from **15/18 to 3/18**, with **24/27 positive hits retained**. Exact metadata matched **36/45** cases in each of two repeats: 33/42 direct fact questions and three broad controls. P95 was **376.6 → 386.6 ms**; mean MCP result bytes were **613.5 → 651.2**. The development and prior answerability fixtures had no positive-hit losses.

These are authored evidence-delivery tests, not generated-answer accuracy or a new overall BrainBench score. Compound queries can still miss one requested attribute. Unsupported subject wording can fall back to legacy retrieval and return unrelated evidence. Conflict detection covers the bounded retrieved pool, not every memory in the corpus. The guard remains opt-in; the optional multilingual reranker is not promoted to the default.

See the [full report and reproducible artifacts](audits/2026-09-07-structured-facts.md), [canonical evaluation contract](canonical-evaluation.md), and [maintenance guidance](memory-maintenance.md).
