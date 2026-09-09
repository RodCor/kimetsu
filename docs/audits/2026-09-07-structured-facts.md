# Structured fact evidence — 2026-09-07

Structured fact evidence reduced unwanted injections from 15/18 to 3/18 on the new synthetic fixture, retaining 24/27 positive hits. Exact answerability matched 36/45 queries in both repeats. Compound retrieval and unsupported-subject fallback remain incomplete, so the feature stays opt-in.

Kimetsu now has a local, rebuildable fact projection for explicit configuration statements. The existing `broker.explicit_fact_guard` switch controls retrieval and delivery of these facts and remains false by default. No new model or dependency is introduced.

Implementation: `3ae8329`; BrainBenchmark: `2c74dad`. No installation, merge, push or live configuration change was performed. Binary hashes and verification counts are in [provenance.json](2026-09-07-structured-facts/provenance.json).

## Measured outcome

The campaign contains 688 observations over 299 distinct scenario/query cases, with no execution errors or unpaired scenarios. There were no positive-hit losses between builds. Rankings and answerability meaning were stable across the two new-fixture repeats.

| Fixture | Positive hits, old → new | Unwanted injections, old → new | Subsequent-query p95, old → new |
|---|---:|---:|---:|
| Development, 210 queries | 169/197 → 169/197 | 7/13 → 7/13 | 1,111.7 → 1,094.0 ms |
| Prior answerability, 44 queries | 24/24 → 24/24 | 0/20 → 0/20 | 409.1 → 380.7 ms |
| New structured facts, 45 queries, two repeats | 24/27 → 24/27 | 15/18 → 3/18 | 376.6 → 386.6 ms |

On the new fixture, the unwanted-injection rate fell from 83.3% to 16.7%: a 66.7 percentage-point reduction, or 80% fewer injection failures. Exact status, supported values, missing attributes and conflicts matched on 36/45 queries per repeat (80%; 72/90 observations): 33/42 direct fact questions and all three broad-question controls. The older binary does not emit structured answerability, so its lack of that metadata is not presented as an answer-accuracy comparison.

New-fixture p95 increased by 10.0 ms (2.7%) and mean MCP result size increased from 613.5 to 651.2 bytes (6.2%). Peak MCP working set was effectively unchanged at 653.5 MiB on this optional mMARCO configuration; the development TinyBERT run remained about 281.2 MiB. These short, sequential runs are descriptive, not proof of zero overhead or a statistically established speed change. No separate large-corpus storage or ingestion-cost benchmark was run.

The nine exact-metadata failures per repeat fall into three repeated patterns:

- Three port-and-password questions retrieved no port evidence. Delivery correctly reported missing evidence but failed the corpus-based partial-answer expectation.
- Three port-and-retries questions retrieved retries but omitted port. They counted as positive hits while still lacking complete attribute coverage—an example of why hit@4 alone is insufficient.
- Three questions using the subject word `Unknown` fell outside the conservative subject grammar and used legacy retrieval. They emitted unrelated port evidence and no structured answerability. These account for all three remaining unwanted injections.

The first two findings point to retrieval per requested attribute; the third points to safer handling when a direct fact question has unsupported scope. No parameters, grammar or fixture labels were changed after these results. A subsequent fix should use these as development cases and reserve a new evaluation set.

The fixture repeats templates across only three project names; it is not 45 independent task families. It tests delivered evidence, not generated-answer correctness. See [summary.json](2026-09-07-structured-facts/summary.json) for every mismatch and [the paired result](2026-09-07-structured-facts/results/validation/comparison.md) for measurement definitions.

## Behavior

The projection stores subject, attribute, value, environment, exact evidence excerpt, memory identity, claim revision and source event. Validity comes from the memory lifecycle. Corrections replace the projection transactionally; replay rebuilds it. Revision and text digest checks prevent facts from a different revision or historical temporary view from supporting a delivered capsule.

The normal agent-facing record API prefixes lessons with `[tags: ...]`. Extraction recognizes one bounded metadata prefix while taking subject and environment only from the statement itself. An isolated MCP record-to-context probe covers this ingress path in addition to direct CLI memory writes.

Recognized questions name one subject and up to four configuration attributes. Subject and environment must match explicitly. Delivery reports supported attributes with source handles and identifies missing or conflicting attributes. It recomputes support against the final visible evidence; a conflict already observed among eligible candidates survives capsule limits and output trimming. This does not discover conflicts outside the retrieved candidate pool.

For these recognized requests, a bounded candidate pool reaches arbitration before token-budget admission. The final MCP renderer enforces its serialized byte budget. The lexical hook records conflicts first and then reapplies its original capsule budget and cap.

For example, a port statement can support the port part of a port-and-timeout question while reporting timeout missing. A production value cannot fill a staging request. Two distinct current timeout values produce a conflict instead of selecting one as an answer.

## Arithmetic

Numeric equivalence uses bounded, checked integer fractions rather than floating-point tolerances. A decimal is represented as an integer numerator over a power of ten, reduced by the greatest common divisor and scaled to seconds or bytes. Cross-cancellation limits intermediate overflow. Thus `30 seconds` and `30000 ms` share a comparison key, while `512 MB` and `512 MiB` remain distinct. Evidence excerpts retain the original text; displayed numbers and units remain unconverted, although casing and whitespace may be normalized. Unsupported units, malformed values and overflow use exact comparison instead of guessed conversions.

## Evaluation protocol

The prior answerability binary is the baseline, with its guard enabled. The candidate also has the guard enabled. Model, score floor, threads and token budget are fixed on both sides. The 210-query development corpus and the prior 44-query answerability fixture are regression sets. The new 45-query synthetic fixture was frozen before candidate inference; its SHA-256 is `6b90f328e989fa0addf7c9a6d2468114fa1b233800f6e979a83830250dd4e469`.

The new fixture has 27 positive and 18 negative queries across three template families, including scope, environment, explicit absence, partial evidence and opposing values. Its expected answerability states are corpus-based: a retrieval omission can therefore fail the expectation even if delivery accurately describes its limited evidence. Two paired repeats check stability. This is assistant-authored synthetic evidence, not independent validation of arbitrary agent tasks.

BrainBenchmark retains delivered answerability alongside capsule text, quality and latency observations. The summary checks exact status and missing attributes, while retaining individual mismatches for inspection. It does not measure generated-answer accuracy or use an LLM judge.

A separate value expectation file was also frozen before inference (`d4d8b3f84282ea52515a1e9dc4fe7fe20f1e26ef45c4905cefcd984db0540445`). Every repeat is checked for status, values, missing attributes and conflicts. Supported source handles must refer to final delivered capsules. Repeat comparisons ignore newly generated memory IDs while retaining them in raw evidence.

The embeddings-enabled workspace run exposed a separate remote model-policy bug: with no server reranker, requests fell through to the stdio handler and loaded the repository's local reranker. Remote context dispatch now uses the server's optional reranker directly, including its disabled state, and avoids the stdio warm-start cache.

## Limits

Extraction deliberately accepts a small grammar. Comma-qualified statements and provisional examples are rejected rather than assigned an inferred scope. Unsupported prose retains the prior retrieval behavior. Environment aliases, arbitrary relations, broad paraphrases and general entailment are not solved by this implementation.

Fact maintenance adds local write and storage work. Hydration is skipped when the guard is disabled. Local extraction and comparison require no model calls; evidence injected into an agent still consumes context tokens. Performance measurements below must determine overhead rather than assuming it is zero.

## Verification and measured results

The final embeddings-enabled workspace run passed 1,470 tests with six ignored. BrainBenchmark passed 132 Rust tests and 18 Python tests. Focused failures were observed before fixes for the budget, daemon transport, remote dispatch and tagged agent-ingress cases. Raw logs are retained under `2026-09-07-structured-facts/checks`.

The optimized CLI and harness builds succeeded. Six isolated release-binary probes passed: tagged MCP record-to-context, partial answers, conflict at cap one, equivalent units, conflict across an intermediate budget, and wrong-environment rejection. Probes use Noop embeddings with reranking and daemon autostart disabled. The campaign then used the cached models listed above, with no builds or tests running alongside inference.

To reproduce the paired measurements, run [run-comparisons.ps1](2026-09-07-structured-facts/run-comparisons.ps1) with preserved baseline/candidate binaries, the harness and a new output directory. The wrapper verifies both frozen fixture hashes. [summarize.py](2026-09-07-structured-facts/summarize.py) reads saved results without inference; the manifest records exact artifact hashes.
