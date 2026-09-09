# Explicit-fact answerability experiment — 2026-09-07

Status: implementation, review and paired comparisons complete. The guard is opt-in; it is not a general answerability classifier.

## Problem and implementation

A relevance reranker can give a high score to a memory about the requested subject even when it lacks the requested fact. A listener port does not supply an authentication password, and a backup schedule does not supply retention duration.

`broker.explicit_fact_guard` adds a local, deterministic admission check for selected English/Spanish configuration questions: passwords, encryption keys, versions, replicas, retention, ports, timeouts, retries, memory limits, and direct questions about literal configuration keys. It requires visible syntactic value evidence in the capsule. It makes no additional model or API call. Injected context still consumes the receiving agent's context tokens.

The shared serving path applies the check after relevance arbitration and before the final capsule cap. The context hook also applies it to lexical fallback. Compression preserves the value that justified admission; the final serving byte budget still applies. Hook prefix stripping now preserves text after a second separator. This does not extend answerability checks to general proactive tool context or warm-start project orientation.

The check returns `Unrecognized`, `ValuePresent`, or `MissingValue`. Only the last removes a capsule. Broad tasks, unsupported wording, and coordinated attribute questions retain normal retrieval. Negation and some competing-component clauses are rejected; explicit absence such as “No password is required” can be useful evidence.

## Why opt-in

Finite language rules cannot establish general entailment, entity identity, completeness, or truth. Review exposed ordinary false negatives in broad questions and literal-key explanations, which now have regression coverage. More paraphrases, languages, indirect evidence, and component relationships remain outside the grammar. Unknown or missed wording can still inject irrelevant memory. Passing synthetic tests would not establish default-on suitability.

To enable in a test project:

```powershell
kimetsu config set broker.explicit_fact_guard true
kimetsu config get broker.explicit_fact_guard
```

Set it to `false` to restore ordinary admission. No live user configuration or installed daemon was changed by this experiment.

## Evaluation protocol

BrainBenchmark compares **the same candidate binary and model**, with guard false versus true, in isolated temporary projects. Each side explicitly configures the guard and checks effective configuration readback. A binary that silently ignores the setting causes a failure. Overrides and binary hashes are recorded in each comparison.

- Existing 210-query development set: TinyBERT, raw rerank floor 0.30.
- Previously inspected 72-query missing-fact set: multilingual mMARCO, floor 0.55. This is development evidence for this phase, not held-out validation.
- Fresh frozen 44-query synthetic validation: multilingual mMARCO, floor 0.55. Includes 24 answerable and 20 missing-fact queries, broad-question controls, Spanish paraphrases, and an unsupported region request. SHA256: `ea4452872956beed1030ec071572db89435780474180b17a7bf7fe8f481e5d7e`.

Before fixture inference, a CLI preflight showed that ingestion redacts literal passwords. The two affected memories were changed to explicit password absence, and the original is retained as `validation-preflight-invalid.json`. No query results from either fixture informed this change.

Each comparison uses budget 6000, one worker, warm-start and ambient context off, and one paired repetition. Rankings are measured on delivered text. One repetition limits latency conclusions; two synthetic validation project families do not provide an independent real-agent population estimate. No policy tuning is permitted on the fresh fixture results.

Positive hit rate = answerable queries with a delivered relevant memory / answerable queries. Negative injection rate = unanswerable queries receiving any memory / unanswerable queries. Both must be reported: reducing injection by discarding useful evidence is not a free improvement.

## Verification

- Main workspace: 1,386 passed, 0 failed, 5 ignored.
- Benchmark harness: 131 Rust tests and 18 Python tests passed.
- Release build with embeddings passed (the Windows linker emitted its informational import-library message).
- Actual isolated CLI hook probes passed: suppress missing password, restore normal retrieval with guard off, and preserve an admitted port after a second text separator.
- Review regressions cover broad questions, mixed attributes, negation, competing clauses, redacted values, and compression. Final review found no blockers for opt-in evaluation.

Production source: `06b2946`. Benchmark source: `1b8fc33`. Candidate binary SHA256: `405d3483fe320e76b0ec776bf9ada3b7771852b04a73f70f3b5da377a43d31c3`.

## Results

All three comparisons completed with zero scenario errors or unpaired scenarios: 326 unique queries, 652 observations across the two conditions.

| Set | Useful hits, guard off → on | Unanswerable queries receiving memory, off → on |
| --- | --- | --- |
| Development (TinyBERT) | 169/197 → 169/197 | 7/13 → 7/13 |
| Inspected missing-fact development (mMARCO) | 48/48 → 48/48 | 19/24 → 0/24 |
| Fresh synthetic validation (mMARCO) | 24/24 → 24/24 | 12/20 → 0/20 |

There were **no per-query positive hit losses**. Development rankings were unchanged. The guard removed all 31 unwanted injections observed across the two targeted sets, while the broader development set's seven unwanted injections remained. That is evidence of a scoped improvement, not general answerability being solved. Fresh validation's injection rate fell from 60% to 0% (60 percentage points); the inspected set fell from 79.2% to 0%. Zero observed failures in a small synthetic set is not an estimated zero production failure rate.

| Initial paired timing | p50 off → on | p95 off → on | Peak working set off → on |
| --- | --- | --- | --- |
| Development | 915 → 912 ms | 980 → 989 ms | 279.6 → 282.6 MiB |
| Inspected missing-fact development | 359 → 365 ms | 382 → 662 ms | 653.4 → 653.2 MiB |
| Fresh validation | 356 → 359 ms | 379 → 381 ms | 653.2 → 653.4 MiB |

The inspected set's tail spike clustered in one project, including unrecognized questions such as a production-region request. A two-repeat follow-up with alternating run order reproduced the quality result exactly (48/48 useful hits on each side; 19/24 unwanted injections off and 0/24 on). Its pooled p50 was 366 → 363 ms and p95 was 517 → 407 ms, off → on; peak working set remained about 654 MiB. Thus the initial tail slowdown did not repeat consistently. Its cause is unproven; these runs do not establish zero overhead or a speedup. The follow-up adds 288 observations, for **940 total observations over 326 unique queries**.

Fresh validation's mean delivered model-text bytes fell from 401.7 to 340.5 (15.2%), mostly by omitting unanswerable context. This is a byte reduction, not a measured tokenizer count or generated-answer accuracy improvement.

Reproducible scripts, raw reports, verification logs and the frozen fixture are in [the artifact directory](2026-09-07-answerability/).


## Reproduction

Run `run-comparisons.ps1` with `-Binary`, `-Harness`, and a new `-OutputRoot`. Specify `-ModelCache` and `-HfHome` for cached embedding/reranker artifacts on another machine; the defaults point to this audit workspace. Add `-TimingFollowup` to include the two follow-up repetitions. Run `summarize.py` against the retained `results` directory to regenerate `summary.json`. `check-hook.py <binary>` runs the isolated hook regressions without model inference.

The guard remains disabled by default. The next evidence needed for promotion is a broader real-project set containing paraphrases, indirect facts, multiple subjects and multi-part questions; this experiment does not cover those sufficiently.
