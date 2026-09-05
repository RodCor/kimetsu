# Kimetsu hardening and BrainBenchmark comparison

Implementation branches: `codex/brain-hardening` (Kimetsu) and `codex/brain-benchmark-hardening` (the separate benchmark repository). The original `E:/Kimetsu` source and live project/user brains were not changed. The preserved pre-change binary is from `3ec56a8`; all comparisons use the same updated harness on both binaries.

This work addresses the concrete defects in the [memory audit](2026-09-04-agent-memory-audit.md) and [model/parameter audit](2026-09-04-models-parameters-memory.md). Local retrieval and rule-based memory processing require no paid generation calls. Returned memory still consumes the receiving agent's context; zero-token information transfer is not an achievable delivery contract.

## What changed

| Area | Result |
|---|---|
| Corrections and history | Durable correction revisions, atomic projection, embedding compare-and-swap, causal exposure bindings, and rebuild-safe corrected text. Historical import/sync stages events before replaying merged history; failed replay rolls back. |
| Freshness and applicability | Monotonic corpus revisions invalidate warm ANN state after existing-row changes. Numeric validity predicates exclude future, expired and malformed dates through retrieval, graph, digest and preference paths. |
| Ranking and scaling | Usefulness/trust adjustments are bounded; freshness uses the configured half-life. Indexed FTS document-frequency lookups replace per-term text scans. Route lookup and duplicate comparison work are bounded. |
| Lifecycle | Similarity proposes conflicts rather than automatically retiring claims. Consolidation preserves scope, kind and unique text. Temporary useful facts receive validity bounds. Archival is reversible; manual rejection and supersession cannot be bypassed by restore. Restore events replicate. |
| Agent context | Final compact MCP output contains admitted capsules and expansion handles, without rejected summaries. The complete serialized MCP content is accounted for, including escaping. Strict Free hooks do not cue generation. |
| Evidence and continuity | Reliance, outcome association and verification are distinct. Feedback targets the exact delivered claim revision. Task/session/worktree resume lanes are separate; warm state is marked only after actual admission, with bounded process bookkeeping. |
| Warm caches | Delivery assembles current rule-based inputs; disk text is never substituted solely because metadata matches. Global profile opt-outs are honored. Shared repo digests do not mix task titles across lanes. |
| Repository files | Remote ingestion traverses the managed checkout but indexes files and manifests under the owning brain, matching retrieval. Replacement is transactional; the regression checks a retrieved file capsule rather than query text echoed in a response. |
| Models and evaluation | Shared serving policy, configured stdio reranker caching, explicit threshold overrides, canonical model aliases, temporal fixture seeding, final delivered-cost measurement and honest failure/latency labels. |
| Tuning | Real fractional recall differs from hit rate; known negatives differ from unknown outcomes. Stable connected fact/task-family splits reduce leakage. Fixture fallback uses isolated seeded IDs. Insufficient labels cannot authorize automatic application. |
| BrainBenchmark | Persistent production MCP, cross-process write tests, stale-injection checks, separate positive/negative denominators, equal dimension weighting, payload bytes, latency and Windows process working set. Paired runs alternate order, fingerprint inputs, validate isolation, retain failures and clean timed-out descendants. |

Details: [evaluation and objective](../canonical-evaluation.md), [warm-start contract](../warm-start-contract.md), [evidence/continuity](2026-09-04-evidence-continuity-hardening.md), [maintenance](../memory-maintenance.md), [local inference controls](../local-inference.md).

## Measurements

Measurements ran on September 5, 2026, on the Ryzen 7 3800X (8 cores/16 logical processors, 32 GiB RAM), with local cached models, one benchmark job and no concurrent compilation or agent inference. This is a desktop workload measurement, not an isolated laboratory host. Each pair uses three alternating-order repeats and fresh temporary brains; warm starts, ambient augmentation, user brain and conflict mutation are disabled. BGE-small-en-v1.5 is pinned throughout. Before/after runs retain new-project defaults: the baseline does not apply configured stdio reranking, whereas the candidate honors TinyBERT. Runtime/model contrasts explicitly select the custom retrieval level and the indicated reranker on both sides.

### Before and after

The 210-query development set contains 197 positives and 13 negatives. Rates below average repeats within each query; there are 630 observations per side, not 630 independent questions.

| Metric, requested budget 6,000 | Baseline | Candidate |
|---|---:|---:|
| Positive hit@4 | 75.63% | 70.56% |
| Positive fractional recall@4 | 73.69% | 69.63% |
| Positive MRR | 0.7386 | 0.6980 |
| Negative injection | 11/13 (84.62%) | 7/13 (53.85%) |
| Subsequent query p50 / p95 | 866 / 982 ms | 1,016 / 1,154 ms |
| Mean serialized MCP result | 153,629 bytes | 1,262 bytes |
| Largest observed MCP peak working set | 210.1 MiB | 280.9 MiB |

The response shrank **99.18%**, but the combined default behavior loses ten positive hits and gains four correct abstentions. This is a delivery and safety improvement with a retrieval/latency tradeoff, not an across-the-board quality win. The query-weighted scenario score falls from 0.7008 to 0.6817; no scenario-bootstrap interval is available for this single corpus. There are no labeled stale queries in this dataset.

The contract set contains 11 positives, 11 negatives and two queries with explicit stale targets (the stale count overlaps the positive/negative classes). It includes six scenarios and a persistent-MCP workflow with writes from another process.

| Requested budget | Build | Positive hit and recall | Negative injection | Stale injection | Mean MCP bytes |
|---|---|---:|---:|---:|---:|
| 512 | Baseline | 10/11 | 7/11 | 2/2 | 3,296 |
| 512 | Candidate | 1/11 | 1/11 | 0/2 | 308 |
| 2,048 | Baseline | 10/11 | 7/11 | 2/2 | 3,303 |
| 2,048 | Candidate | 8/11 | 2/11 | 0/2 | 417 |

**Do not adopt 512 as a general default:** strict admission loses most positives at that size. At 2,048, all three missed candidate positives are the Spanish queries, each returning no capsule; the baseline retrieves two of those three. This tiny track establishes a multilingual coverage regression, not a general estimate of Spanish capability. The candidate's workflow score improves from 0.8 to 1.0 at 2,048, and the two labeled temporal cases stop injecting stale evidence. Neither result certifies behavior beyond these examples.

The equal-dimension headline changes from 0.63 to 0.54 at 512 and from 0.63 to 0.85 at 2,048. These aggregates can hide lost positives; retain the metric vector above. The exploratory retrieval-scenario bootstrap at 2,048 spans −0.16 to +0.50, crossing zero with only five retrieval scenarios.

### Runtime and model contrasts

With the candidate binary and TinyBERT fixed, explicit four-thread ONNX execution preserved every ranking across all three paired runs. It did **not** improve latency here:

| Runtime setting | Subsequent p50 / p95 | Mean complete run | Peak MCP working set |
|---|---:|---:|---:|
| Backend default | 940 / 1,052 ms | 203.4 s | 279.8 MiB |
| Four threads | 951 / 1,104 ms | 229.0 s | 276.9 MiB |

Keep the backend default on this evidence. The small working-set difference does not justify the observed slowdown; no statistically significant effect or universal optimum is claimed. The explicit thread control remains useful for host contention and future workload-specific tests. It changes the ONNX global pool, including inter-op/spinning policy, so this is a runtime-configuration contrast rather than a pure physical-core-count experiment.

With BGE-small, the candidate binary, four threads and a 6,000-unit requested budget fixed:

| Metric | TinyBERT L2 | MiniLM L4 |
|---|---:|---:|
| Positive hit@4 | 139/197 (70.56%) | 142/197 (72.08%) |
| Positive fractional recall@4 | 69.63% | 70.90% |
| Positive MRR | 0.6980 | 0.7132 |
| Negative injection | 7/13 | 6/13 |
| Subsequent p50 / p95 | 973 / 1,049 ms | 1,139 / 1,346 ms |
| Mean MCP result | 1,262 bytes | 1,206 bytes |
| Peak MCP working set | 276.1 MiB | 710.9 MiB |

MiniLM adds three positive hits and one correct abstention, at about **28% higher p95** and **2.58× peak working set**. Keep **BGE-small + TinyBERT with backend-default threads** as the economical measured starting point; MiniLM is an opt-in quality/resource tradeoff, not a demonstrated universal upgrade. This model contrast used four threads, so MiniLM on backend-default threads is unmeasured. The multilingual contract was not part of this model pair; these numbers do not establish a fix for the Spanish regression. No model/threshold setting was promoted into the user's live configuration.

### Indexed document-frequency scaling

An isolated in-memory SQLite experiment evaluates eight rare query terms over synthetic corpora. It asserts equal document counts between the old `LIKE` scans and current FTS-prefix joins, then alternates their order for five repeats. The current path includes its once-per-query population count.

| Memories | Old median | Indexed median | Old / indexed |
|---|---:|---:|---:|
| 1,000 | 4.51 ms | 0.59 ms | 7.68× |
| 10,000 | 46.94 ms | 5.87 ms | 8.00× |
| 100,000 | 558.99 ms | 71.81 ms | 7.78× |
| 1,000,000 | 5,989.83 ms | 809.66 ms | 7.40× |

This verifies reduced work for the helper under aligned token semantics. It does not benchmark production SQLite version/storage, common-term postings, end-to-end semantic retrieval or agent success. The indexed helper still has linear work, including its population count: the complete eight-term measurement took a median 810 ms at one million memories. A revision-aware population-count cache and incremental ANN maintenance are future scale work, not delivered or measured improvements in this patch.

### Reproduction and artifacts

All **five pairs completed with zero errors and zero unpaired scenarios**, totaling **4,044 query observations**. The [measurement artifacts](2026-09-05-brainbench/README.md) preserve comparison JSON, exact fixtures, a campaign runner and the SQL helper/results. Comparison JSON fingerprints both binaries, the harness, runner and dataset, and records run order/configuration. Raw per-query reports and command logs remain under `tmp-tests/paired-*` in the implementation worktree. These artifacts enable reanalysis; they are not additional held-out evidence.

## The math and parameter decisions

The previous tuning objective subtracted `0.005 × estimated_tokens`: 200 units erased an entire point of MRR. It also subtracted the same historical regret constant from every candidate, which cannot change the winner.

The default now uses:

`J = Q − 0.05 × mean_final_delivery_bound / 6000`

With both classes present, `Q = (positive MRR + negative abstention accuracy) / 2`. This explicitly gives positive retrieval and avoiding unsupported injection equal class weight. It is a policy preference, not a fitted optimum. For MRR 0.5, negative accuracy 1, and delivery bound 512, `Q=0.75` and `J=0.745733`. Spending an entire 6000-unit budget subtracts 0.05 quality units. Missing class coverage is reported, and reliance-only personal data cannot authorize all-query automatic tuning.

The measured development comparison illustrates the weighting decision. Ignoring delivery cost, equal-class `Q` rises from `(0.7386 + 2/13)/2 ≈ 0.4462` to `(0.6980 + 6/13)/2 ≈ 0.5798`. The benchmark's query-weighted fractional-recall/abstention score instead falls by `4/210 ≈ 0.0190`, because the lost positive recall mass exceeds the four newly correct abstentions. Neither weighting is mathematically mandatory: it encodes how costly unwanted memory is relative to missing useful memory. Do not optimize a headline without choosing that tradeoff.

The delivery unit is a conservative UTF-8 byte bound over the serialized MCP content envelope, not provider tokenization or billing. A tiny budget smaller than the error envelope produces a truthful `budget_too_small` response, not a fabricated compliant token count. The JSON-RPC transport framing is measured separately by the harness.

Usefulness and freshness remain heuristics. Correct half-life decay is `2^(−age / half_life)`, so a 30-day half-life leaves 0.5 after 30 days and 0.125 after 90 days. A sigmoid-transformed cross-encoder score is not a calibrated probability of useful or true evidence. No calibrator, Bayesian verification channel or empirically optimal threshold is claimed.

## Verification

- Full workspace, including integration and documentation tests: **1,371 passed, zero failed, five ignored**, using `cargo test --workspace --no-fail-fast --locked --offline -j 1`.
- Embeddings-enabled CLI and remote server: compile check passed. Both focused embeddings-enabled serving tests passed, including production/evaluation arbitration and final-budget parity.
- BrainBenchmark: **130 Rust tests and 16 Python tests passed**.
- Optimized embeddings-enabled candidate built from `b9658995609e945d18ee87163d543494c1c44713`. Benchmark source: `b3e0eda1641d340781eda843a477425f629aa119`.
- Scoped independent reviews covered corrections, lifecycle, delivery, evidence, evaluation, synchronization, warm caches, repository ingestion and benchmark methodology. Regression tests exposed the identified behavior before fixes; final tests were rerun after the last source change.

Logs are preserved in `tmp-tests/final-workspace-complete.log`, `final-embeddings-check.log`, `final-embeddings-serving.log`, `final-candidate-build.log`, `benchmark-final-complete.log`, and `benchmark-final-python-complete.log`. Builds used one compiler job after an earlier parallel build exhausted available memory; that failed build is not counted as a passing verification run.

## Interpretation and remaining evidence needed

The six-scenario contract fixture and the existing 100-memory development fixture are development evidence, not a held-out certification. Repeats do not create independent examples. The paired harness averages repeats within each scenario/query; latency percentiles are descriptive. The larger fixture is one corpus/scenario, so its scenario-bootstrap uncertainty is unavailable. Thirteen no-answer examples provide little statistical power for a general abstention claim.

The comparisons exercise production MCP retrieval with warm starts and ambient augmentation explicitly disabled, because their gold labels cover capsules. Separate regressions cover warm-start identity, freshness and budget admission. These timings exclude warm-start construction and are not complete agent-task latency or success measurements. No reader model or paid generation was run.

Requested budget units do not represent equal realized response sizes across the two versions: the baseline uses the old accounting and can expose a much larger response. The before/after comparison also includes the candidate newly honoring configured stdio reranking. It measures the combined behavior change, not the causal effect of each fix or a model-controlled algorithm comparison. The same-candidate thread and reranker experiments isolate those configuration contrasts. All harness requests explicitly allow four capsules; the serving default remains three.

The performance changes do not make every operation constant-time. Population counting and ANN reload after corpus revision remain proportional to corpus size; maintaining durable history consumes storage. The SQL scale experiment isolates document-frequency work and excludes models, ANN, serialization and full-agent behavior. Whole-corpus capacity, incremental ANN maintenance and representative concurrent workloads still need their own measurements.

For capacity planning, raw dense-vector storage is `N × dimensions × bytes_per_component`. At 100,000 memories, 384-dimensional float32 vectors occupy 153.6 MB; a separate float16 ANN representation adds 76.8 MB before graph/index overhead. At one million memories those quantities become 1.536 GB and 0.768 GB. They are raw representation sizes, not measured resident RAM, and exclude text, SQLite indexes, revision history, models and caches. A 768-dimensional representation doubles the vector terms. Conservative archival protects correctness; it is not yet a measured whole-corpus capacity budget.

The next evidence needed for choosing a universal default is a held-out set of repository/task families, enough explicit no-answer and stale/conflicting cases, representative Spanish/multilingual tasks, and actual agent task success with context cost. The implemented safeguards do not turn association scores into verification or make the existing development set establish “best memory.”
