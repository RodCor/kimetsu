# Models, parameters, and memory management — 2026-09-04

Companion to the [first audit](2026-09-04-agent-memory-audit.md). Source baseline: `3ec56a8`, Kimetsu 2.7.0. The analysis concerns the memory models, not a comparison of frontier coding agents. No project configuration was applied and no paid generation benchmark was run.

## Actual configuration and evidence available

The project config selects BGE-small-en-v1.5, embeddings enabled, 8 capsules, a 6,000-token nominal budget, explicit semantic floor 0, 30-day usefulness half-life, and automatic harvesting. Missing fields inherit TinyBERT-L2-v2 reranking, lexical coverage 0.5, linear fusion, per-kind normalization, and abstention 0. New projects differ: their generated configuration enables automatic abstention. Therefore upgrading the binary does not make this project's thresholds match new-project defaults.

The `[model]` Claude Opus configuration is for Kimetsu's coding/answering model. Its temperature 0.2 and output limit 8,192 do not tune BGE embeddings or TinyBERT. The configured distiller is disabled. Memory-generation tier and semantic retrieval level are separate concepts even though both use “deep” terminology.

A read-only aggregate of the project brain found **49 active memories, all with BGE-small embeddings; 40 had never been used, none had ten uses, and none had a negative usefulness score**. These counts exclude the user brain. This is not enough evidence to fit a trustworthy personalized utility/forgetting policy or justify switching a production model automatically.

## Measured model comparison

Fresh release build, 100 memories, 197 positive queries and 13 no-answer queries; candidate pool 12, output cap 4. These are direct retrieval measurements on this machine, with default CPU allocation. Load and corpus seeding are outside query latency; the first query is included. RSS is the benchmark process's reported peak, not incremental model weight size.

| Embedder | Reranker | Positive MRR | Positive hit@4 | Mean / p95 latency | Peak RSS |
|---|---|---:|---:|---:|---:|
| BGE-small | TinyBERT-L2 | 0.9095 | 94.92% | 588 / 672 ms | 522 MiB |
| BGE-small | MiniLM-L4 | 0.9255 | 95.94% | 833 / 998 ms | 1,324 MiB |
| BGE-small | Off | 0.8105 | 86.29% | 561 / 637 ms | 361 MiB |
| Jina-code | TinyBERT-L2 | 0.8697 | 90.36% | 151 / 365 ms | 1,551 MiB |
| Jina-code | MiniLM-L4 | 0.8896 | 91.88% | 369 / 604 ms | 2,351 MiB |
| Jina-code | Off | 0.7923 | 84.26% | 126 / 333 ms | 1,468 MiB |

**Keep BGE-small + TinyBERT as the baseline.** Removing reranking loses about 0.099 MRR for only 27 ms saved in this run. MiniLM's improvement over TinyBERT is 0.0161 MRR; the exploratory paired cluster-bootstrap 95% interval is [-0.0012, 0.0327], so this dataset does not clearly establish a reliable improvement. The interval groups by 114 expected-key sets, not independent repositories. Jina is faster under these runtime defaults but costs roughly three times the process RAM and retrieves less accurately here. None of these findings establishes the best model across all agent tasks.

### Abstention matters more than a small ranking gain

All 13 no-answer cases receive four memories with BGE under this benchmark path. A post-hoc TinyBERT score floor of 0.30 reduces positive hit@4 from 94.92% to 85.79%, while still injecting on 7/13 no-answer cases. At 0.99, hit@4 falls to 69.04% and 1/13 no-answer cases still receives memory. A high sigmoid score alone does not solve this problem.

These are diagnostic filters over already returned top-four results, not a full rerun of production arbitration; they cannot recover earlier discarded candidates. Do not copy either threshold into production as an optimum. Add explicit negative examples and calibrate the complete decision to inject.

### CPU allocation experiment

Same BGE-small + TinyBERT workload, sequential runs, verified affinity masks on isolated children:

| Logical CPUs allowed | Mean latency | p95 latency | Peak RSS | MRR |
|---|---:|---:|---:|---:|
| 16, default baseline | 588 ms | 672 ms | 522 MiB | 0.9095 |
| 1 | 703 ms | 821 ms | 502 MiB | 0.9095 |
| 4 | 568 ms | 639 ms | 507 MiB | 0.9095 |
| 8 | 552 ms | 621 ms | 511 MiB | 0.9095 |

Eight logical CPUs improved mean latency by 6.0% and p95 by 7.6% in this run. That is a promising follow-up setting, not a demonstrated optimum: there is one run per allocation, fixed run order, and no simultaneous coding-agent workload. Process affinity changes scheduling as well as available CPU capacity; it is not an isolated ONNX thread-count experiment. One CPU was slower. Expose explicit inference thread/spinning controls and repeat randomized trials before making a global default change.

### Spanish stress check

On 12 manually authored Spanish queries against the same English memories, BGE-small + TinyBERT found the expected memory in the top four for **8/12** queries (MRR 0.6042); Jina-code + TinyBERT did so for **6/12** (MRR 0.5000). The fixture retains 13 existing English no-answer queries separately. This is a small exploratory cross-language test, not a matched English/Spanish experiment or a language-wide estimate.

The misses justify adding a representative multilingual evaluation set. If Spanish is a regular usage language, test a multilingual embedder **and** reranker together before choosing a replacement. Moving to the tested Jina-code model alone did not improve this stress check.

### Recommended decisions

| Item | Decision justified now |
|---|---|
| BGE-small + TinyBERT | Keep as the local-memory baseline; no paid generation required for these models. |
| MiniLM-L4 reranker | Optional quality experiment; its small measured gain does not yet justify the RAM/latency increase as default. |
| CPU allocation | Repeat the eight-logical-CPU candidate with explicit ONNX controls, randomized run order, and concurrent host load. |
| Semantic/rerank/abstention thresholds | Repair evaluator parity and collect explicit negatives before declaring an optimum. The old project's missing abstention field currently disables that gate. |
| Tuner | Correct objective units and per-candidate outcome measurement; keep automatic application off until validated. |
| Conflict resolution / consolidation | Keep detection available; disable automatic resolution pending claim-level checks, and constrain merging by scope and identity. |
| Forgetting | Keep automatic forgetting disabled until usefulness labels and recovery/archival semantics are trustworthy. |
| ANN parameters | Leave the current baseline at this corpus size; evaluate recall/latency at actual target scale later. |
| Claude temperature/output limit | No claim of optimality: these benchmarks exercise memory retrieval, not the configured generative agent. Evaluate those separately on coding-task success and cost. |

This follow-up completed **11 benchmark combinations/runs and 1,940 query evaluations**: six model combinations, three CPU-affinity runs, and two Spanish stress runs. Repeated queries are not independent new examples. All model inference in these experiments was local; no live project configuration or memory records were changed.

## The mathematical problems

### Relevance scores are not probabilities

The linear path blends lexical evidence `L` and cosine `c` as:

`R = (1 - 0.5)L + 0.5(c + 1)/2 = 0.5L + 0.25c + 0.25`.

Even `L=0, c=0` produces 0.25. The next normalization divides each score by the strongest score in its capsule kind, making the best memory's relevance 1 even in an irrelevant corpus. This is useful for relative ordering; it cannot establish answerability.

The stage score then combines `wR*R_normalized + wC*confidence + wF*freshness + wS*scope`. A final cross-encoder replaces the numerical score, while additional policy ordering still applies. Thus the configured weights are not a simple final linear ranking model.

Cross-encoder logits pass through `sigmoid(z)=1/(1+exp(-z))`. This preserves rank, but it does **not** make the output a calibrated probability of a helpful memory. BGE's authors likewise warn against transferring similarity thresholds without measuring the local score distribution. They suggest an optional query instruction for short-query retrieval and recommend evaluating it on the task. See the [BGE model card](https://huggingface.co/BAAI/bge-small-en-v1.5).

**Recommendation:** Separate candidate relevance from the decision to inject. Fit an answerability/usefulness calibrator on labeled positive and no-answer queries, preferably using raw relevance, score margin, coverage, applicability, and evidence grade. Assess calibration on held-out data. Keep exact identifier/BM25 retrieval alongside dense retrieval. Compare RRF experimentally; it removes score-scale dependence but does not itself solve abstention.

### The tuning objective has the wrong scale for a conservative quality tradeoff

Current objective: `J = MRR - 0.005*T - 0.5*regret_rate`.

MRR lies in [0,1]. The token term subtracts an entire MRR unit for 200 tokens:

| Illustrative configuration | MRR | Tokens | Objective before regret |
|---|---:|---:|---:|
| Better retrieval | 0.95 | 300 | -0.55 |
| Worse retrieval | 0.80 | 100 | 0.30 |

That tradeoff may be intentional for an extremely expensive context, but it is not a small efficiency regularizer. If 200 additional tokens are worth sacrificing 0.01 MRR, the corresponding coefficient is `0.01/200 = 0.00005`, **100 times smaller**. This is an illustrative preference, not an empirically optimal coefficient.

More importantly, the CLI supplies the **same historical regret rate for every combination**. Subtracting a common constant cannot affect the winner. The advertised regret-aware optimization therefore does not select less regrettable configurations.

Use a constrained objective first: maximize measured task success or retrieval utility subject to a final token budget, latency limit, and no-answer false-injection limit. Once actual task costs are known, use comparable units instead of mixing a rank metric and uncalibrated token estimates.

Evidence: [tune.rs](../../crates/kimetsu-brain/src/tune.rs#L347); [tuner CLI](../../crates/kimetsu-cli/src/commands/brain.rs#L2838).

### The self-tuner's experimental design needs repair before automatic application

- Its evaluator uses **pool 8 / cap 4 / reranker floor 0.30**. The model-grid benchmark uses **pool 12 / cap 4 / floor 0**. Production adds evidence-band arbitration and host-specific delivery. These are different experiments.
- A semantic value of `0` in an injected request means “inherit configuration”; `-1` is passed through and fails the positive-floor condition instead of resolving the documented automatic threshold. The sweep labels do not consistently mean what users expect.
- The train/holdout split is every fifth array position. Personal examples originate from a `HashMap`; it is not a stable split by query identity, session, repository, or fact family. Paraphrases can leak across the split.
- Personal examples without citations are counted but excluded from the returned evaluation cases. Absence of a citation is not necessarily a negative label, but removing all such cases also means the dataset cannot measure abstention.
- Citation matching can fall back to a time window across sessions. That introduces mislabeled examples.

**Recommendation:** One canonical evaluator shared by tuning, benchmarking, and serving. Use explicit `Option`/enum semantics for inherit/off/automatic thresholds. Split by stable fact/task family and time; preserve explicit no-answer labels and retain unknown outcomes separately. Include a final untouched test set and report uncertainty. Do not use `brain tune --apply` as an automatic authority yet.

Evidence: [request resolution](../../crates/kimetsu-brain/src/project.rs#L604), [tuner evaluation](../../crates/kimetsu-cli/src/commands/brain.rs#L2778), [split](../../crates/kimetsu-brain/src/tune.rs#L418), [personal labels](../../crates/kimetsu-brain/src/tuneset.rs#L75).

### Decide to inject using expected utility

A simple model is:

`U = p*B - (1-p)*H - C`.

Here `p` is calibrated helpfulness probability, `B` is expected benefit when helpful, `H` is expected harm when misleading, and `C` is delivery/attention cost, all in the same units. Inject only if:

`p > (H+C)/(B+H)`.

For illustrative equivalent-work costs `B=500, H=1500, C=200`, the threshold is **0.85**. Changing the risk changes the right threshold. A remembered formatter command and a migration instruction should not share identical error costs. A raw cosine of 0.85 is not this probability.

For multiple memories, select their **marginal** benefit under a shared budget: maximize total utility minus redundant evidence costs, subject to total serialized tokens <= budget. Do not count two paraphrases from one source as two independent confirmations. MMR is a useful heuristic, but evidence needed for multi-hop answers can look redundant and must remain available.

### Confidence and decay currently mean several different things

The confidence EMA is `c_next = 0.95*c + 0.05*y`. Starting at 0.5, three successes produce **0.5713**, while the usefulness multiplier already reaches its full envelope after three uses. These two confidence mechanisms react at very different speeds. An EMA with alpha 0.05 has an asymptotic effective sample size of roughly `(2-alpha)/alpha = 39` under independent stationary observations; it is not automatically Bayesian calibration.

A more interpretable starting point for verified binary outcomes is a Beta prior:

`p ~ Beta(a+s, b+f)`, with posterior mean `(a+s)/(a+b+s+f)`.

With a uniform prior and three successes, the mean is 0.8, but the equal-tail 95% interval's lower endpoint is only **0.398**. Three successes are weak evidence. Real outcomes are correlated, so even this model requires session/source grouping and careful definitions of success and failure.

There are also three different clocks:

- Configured usefulness decay: `2^(-age/30)`; 30-day half-life.
- Freshness score: `exp(-age/30)`; **20.79-day half-life**, not 30.
- Conflict resolution: confidence times a hardcoded 30-day half-life, independent of the broker's configured one.

The verification-stage freshness weight is 0.4. After 90 days, freshness is only 0.0498, so a new memory can gain about 0.38 score from recency alone. That is a substantial bias against stable verified procedures.

**Recommendation:** Use explicit clocks for factual validity, recent usefulness, and retrieval activity. Stable preferences/conventions should persist until corrected; code facts should depend on source revision; task episodes should depend on task lifetime. Decay uncertainty or priority, not truth indiscriminately.

Evidence: [scoring constants](../../crates/kimetsu-brain/src/scoring.rs), [usefulness](../../crates/kimetsu-brain/src/context.rs#L1763), [freshness](../../crates/kimetsu-brain/src/context.rs#L2333), [conflict score](../../crates/kimetsu-brain/src/conflict.rs#L155).

## Memory management beyond the earlier audit

### Separate duplicate, related, contradictory, and superseding claims

High cosine means related meaning, not equivalence or contradiction. Current conflict detection can treat similar-but-different text as a conflict, and resolution picks a confidence-times-recency winner. A confidence-0.95 fact aged 90 days scores 0.11875; a confidence-0.55 new statement scores 0.55 and clears the 0.15 winner gap. Recency can therefore retire stronger old evidence without proving a contradiction.

The distiller novelty gate can also reject a correction because its cosine to the old claim exceeds 0.9. Phrases like “temporarily” are dropped even when they describe a useful version-scoped workaround.

Use similarity to propose relationships. Use an explicit claim key—entity, attribute, scope, condition, validity—and incompatible values or correction evidence to resolve them. Route temporary facts to expiring episodic memory. Until that exists, automatic conflict resolution should be disabled for reliable-memory operation, while conflict detection remains available for review.

Evidence: [conflict.rs](../../crates/kimetsu-brain/src/conflict.rs#L104), [quality gate](../../crates/kimetsu-cli/src/distiller.rs#L171).

### Consolidation needs semantic and scope constraints

The merge loop checks embedding model identity but **not scope or memory kind**. It joins all connected pairs above cosine 0.92, then keeps the survivor's text and combines evidence counters.

Connected similarity is not transitive equivalence. Let three unit vectors lie at 0°, 20°, and 40°. Adjacent similarities are **0.9397**, above 0.92, but the endpoints have similarity **0.7660**. Union-find still merges all three. A unique detail can disappear, and cross-scope material can inherit the survivor's scope.

Require matching scope and claim identity; constrain each member against the representative, not just any neighbor. Preserve unique clauses and source lineage. Do not add support counts as if dependent copies were independent observations. Use ANN to propose bounded candidate pairs instead of exhaustive O(N²) comparisons; at 100K entries the latter is roughly five billion pairs.

Evidence: [consolidate.rs](../../crates/kimetsu-brain/src/consolidate.rs#L332).

### Popularity must not make memories immortal

Forgetting is disabled by default, which is appropriate with the current evidence. Its optional policy protects any memory with ten uses regardless of later usefulness. Retrieval activity also refreshes the age reference. A repeatedly surfaced bad memory can therefore remain protected precisely because it keeps being surfaced. `COALESCE(last_used_at,last_useful_at,created_at)` is additionally first-non-null selection, not the “most recent” timestamp described in the comment.

Keep separate hot, archived, invalid, and superseded states. Archive low expected future utility under a capacity budget; reserve invalidation for false/inapplicable claims. Retain explicit durable preferences and rare critical recovery procedures. A verified contradiction must override popularity. Provide a recovery route for archived evidence.

Evidence: [forget policy](../../crates/kimetsu-brain/src/lifecycle.rs#L243), [defaults](../../crates/kimetsu-core/src/config.rs#L1382).

### Bounded usefulness should remain bounded after reranking

Before reranking, usefulness gains are capped at 0.10. After reranking, a positive usefulness tier sorts before all neutral tiers. For example, a cited capsule at cross-encoder score 0.31 can outrank a neutral capsule at 0.99 when both clear a 0.30 floor. This reintroduces unbounded priority in a different form.

Usefulness should reorder similarly relevant, currently applicable candidates, not override relevance globally. Preserve evidence thresholds and cap the final effect. Evaluate repeated-success incumbents against new correct facts explicitly.

Evidence: [policy ordering](../../crates/kimetsu-brain/src/context.rs#L3278).

## Model/runtime parameters worth experimenting with

- **CPU threads:** FastEmbed 5.13.4 initializes ONNX intra-op threads from all available logical processors for both embedding and reranking. This machine has a Ryzen 7 3800X, 8 cores/16 logical processors, and about 32 GiB RAM. More threads are not automatically faster for short, sequential inference. Measure 1/2/4/8 threads with spinning on/off and realistic host contention; expose explicit thread budgets rather than consuming all available CPU.
- **Input length:** Kimetsu does not override FastEmbed's 512-token maximum. Buying an 8K-capable model does not automatically provide an 8K embedding window in this integration. Chunk complete atomic facts/procedures, retain source handles, and test evidence near the truncation boundary before increasing the maximum.
- **Query encoding:** Compare a query-specific BGE instruction against no instruction while leaving document embeddings unchanged. The current adapter uses the same `embed(text)` operation for both.
- **Candidate pool:** Compare 8/12/24/32 under the same retrieval and final-token budgets. Increasing pool alone may not help when pre-rerank budgeting already removed evidence. Measure candidate recall before blaming the reranker.
- **ANN:** Current M=16, construction expansion=128, search expansion=64, f16 are reasonable starting points, not measured optima. Tune ANN recall against exact cosine at target scale separately from answer quality. At only 49 memories, ANN tuning is unlikely to be the primary lever.
- **Memory footprint:** Dense vector storage is `N*d*bytes`. At 100K entries, BGE's 384-dimensional float32 blobs require 153.6 MB of raw storage; 768-dimensional Jina doubles that. The f16 ANN vectors add a separate representation, plus graph and model overhead. Disk blob size is not the same as resident memory.

The daemon already shares loaded models and uses a bounded connection queue, which is a useful foundation. Each embedding/reranking engine is protected by a mutex, so adding connection workers does not create independent inference capacity. Benchmark concurrent clients and report queue wait separately from model inference; avoid multiplying model copies just to improve a single-request latency chart. Evidence: [model engines](../../crates/kimetsu-brain/src/embeddings.rs#L705), [daemon queue](../../crates/kimetsu-cli/src/embed_daemon/server.rs#L155).

For the nominal 6,000-token context budget, run a separate end-to-end sweep at 256/512/1,024 tokens after the previously identified final-payload budget issue is fixed. Select the smallest budget preserving task success and critical constraints. Those values are experiment points, not a claim that every task fits 1,024 tokens. Measure all memory-related host tokens, including tool calls, metadata, harvesting, and injected content.

“Zero tokens” is achievable for **paid LLM calls used by the memory subsystem** when extraction and retrieval remain local/deterministic. A hosted agent still consumes context tokens when it reads a memory. Optimize net task tokens saved and useful decisions per injected token; zero additional context tokens cannot convey new textual knowledge to the model.

For Spanish or mixed natural languages, BGE-M3 and a multilingual reranker are candidates, not proven replacements. Kimetsu already exposes these model families. Their resources and thresholds require a separate local benchmark. [BGE-M3 model card](https://huggingface.co/BAAI/bge-m3). Jina-v2-code targets English and programming languages, so its label alone does not establish Spanish quality. [Jina model card](https://huggingface.co/jinaai/jina-embeddings-v2-base-code). TinyBERT's published speed figures are GPU measurements, not a promise about this CPU. [TinyBERT model card](https://huggingface.co/cross-encoder/ms-marco-TinyBERT-L2-v2).

## Reproduction and scope of measurements

Current-source embeddings release build: `cargo build -p kimetsu-cli --release --features embeddings --locked --offline`.

Model grid: `kimetsu brain bench --dataset bench/local/dataset-100.json --embedders bge-small-en-v1.5,jina-v2-base-code --rerankers off,ms-marco-tinybert-l-2-v2,ms-marco-minilm-l-4-v2 --pool 12 --cap 4 --out tmp-tests/model-audit-grid`.

Child environment disables the user brain and conflict detection/resolution, keeping the corpus contents fixed. The benchmark initializes a fresh project, so its inherited thresholds differ from the existing project configuration. It calls the broker/reranker directly, not the full host/MCP delivery path. Timestamps are generated during seeding; exact replay with fixed timestamps is still needed before interpreting small model differences as causal.

The dataset has 100 memories, 197 positive queries and 13 no-answer queries. It is an existing development dataset, not a newly held-out test. Some positives have multiple relevant keys. The CLI's displayed “recall@4” is actually an any-relevant **hit rate**; the analysis script additionally calculates true fraction-of-relevant recall.

The [analysis script](../../tmp-tests/model_audit_analysis.py) produces [metrics and mathematical examples](../../tmp-tests/model-audit-analysis.json), including exploratory score-floor sweeps and a paired bootstrap grouped by expected-key set. These groups do not establish repository-level independence. The 13 no-answer cases are too few to certify a low error rate: even zero failures would leave a **20.6% one-sided 95% upper bound** under an independent-binomial model.

The [Spanish fixture builder](../../tmp-tests/model_audit_stress_fixture.py) adds 12 manually authored positive queries and reuses the 13 existing no-answer queries. It is an exploratory stress check, not a comprehensive language benchmark. The [affinity runner](../../tmp-tests/model_audit_affinity.py) changes affinity only for its own process and benchmark children.

### What the existing tests establish

The earlier audit ran the lean test suites: 1,060 passed, four ignored, none failed. This follow-up rebuilt the embeddings release and exercised real local models. It did not rerun the entire suite because implementation code was unchanged.

Review of the inline tests found coverage for merge identity/model separation, event replay, forgetting, abstention, decay curves, and tuner arithmetic. Those checks are valuable but do not establish useful policy: the reranking test explicitly expects a historically useful item to outrank a higher relevance score, and objective tests verify the chosen formula rather than the desirability of its tradeoff.

Missing evaluation priorities are cross-scope/non-transitive merge safety, conflicting near-duplicates, corrections after repeated prior success, rare critical memory retention, calibrated no-answer behavior, and stable task-family holdouts. Add end-to-end agent tasks measuring success, wrong-memory harm, and final context tokens; this retrieval fixture contains no labeled stale-memory correctness cases, so its zero stale-hit rate is not evidence of temporal correctness.
