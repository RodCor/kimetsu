# Structured fact answerability (unreleased, opt-in)

The broker can attach evidence accounting to direct configuration questions: which attributes are supported, missing, or conflicting. This is a bounded syntactic check on retrieved evidence, not a general truth or entailment model.

## Enable on a build containing this change

```sh
kimetsu config set broker.explicit_fact_guard true
```

The default is `false`. Use the same command with `false` to disable it. If a warm embed daemon is running an older executable, restart it with the updated build before comparing daemon behavior. This documentation describes unreleased code; installing the current published version does not guarantee this feature is available.

An explicit statement such as `Orchid staging gateway port is 7319.` can support the port part of `What are the Orchid staging gateway port and timeout?`. The response reports timeout missing unless matching evidence is retrieved. Production evidence cannot fill a staging request. Distinct eligible values produce a conflict; equivalent durations such as `30 seconds` and `30000 ms` compare using exact rational arithmetic.

MCP context responses may contain `answerability` with `status`, `subject`, `environment`, `supported` values and source handles, `missing`, and `conflicting`. Hooks emit a short notice for incomplete or conflicting evidence. A `supported` status means the retrieved evidence supports the recognized attributes, not that the current code or outside world was independently verified.

## Storage and cost

Schema 15 adds a rebuildable SQLite fact projection. Facts bind to the memory's claim revision, source event and exact evidence excerpt; corrections refresh it transactionally and replay rebuilds it. Current lifecycle, temporal bounds and text digest checks prevent stale evidence from supporting another revision. The agent record API's tag prefix is recognized without using tags to infer subject or environment.

Projection maintenance adds write/storage work even when the guard is disabled; ordinary retrieval skips fact hydration while it is disabled. No extra model calls are required. Delivered evidence and metadata still consume agent context tokens. MCP output admission includes serialized metadata in its byte upper bound; that bound is not a measured tokenizer count.

## Measured results and limits

On the frozen 45-query synthetic fixture, unwanted injections fell from **15/18 to 3/18**, with **24/27 positive hits retained**. Exact metadata matched **36/45** cases in each of two repeats: 33/42 direct fact questions and three broad controls. P95 was **376.6 → 386.6 ms**; mean MCP result bytes were **613.5 → 651.2**. The development and prior answerability fixtures had no positive-hit losses.

These are authored evidence-delivery tests, not generated-answer accuracy or a new overall BrainBench score. Compound queries can still miss one requested attribute. Unsupported subject wording can fall back to legacy retrieval and return unrelated evidence. Conflict detection covers the bounded retrieved pool, not every memory in the corpus. The guard remains opt-in; the optional multilingual reranker is not promoted to the default.

See the [full report and reproducible artifacts](audits/2026-09-07-structured-facts.md), [canonical evaluation contract](canonical-evaluation.md), and [maintenance guidance](memory-maintenance.md).
