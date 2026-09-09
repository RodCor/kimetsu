# Retrieval quality follow-up

The configuration and benchmark fixes are implemented and verified. **Keep TinyBERT as the economical default.** The optional multilingual model recovers Spanish positives, but the frozen test exposes substantially more false injections on unanswerable questions. It is an experimental recall option, not a generally better memory policy.

The earlier apparent English retrieval regression was largely a benchmark matching defect, not a demonstrated production regression. Separately, diagnostic probes identified relevant memories already in the six-candidate pool whose English TinyBERT scores failed admission; larger pools did not repair those specific cases. Spanish probes exposed a real limitation of that scorer.

## Changes

- Optional `mmarco-minilm-l12-v2-int8` reranker, pinned to revision `1427fd652930e4ba29e8149678df786c240d8825`; the alias selects only the quantized ONNX artifact, with no silent full-precision fallback.
- `broker.rerank_min_score` is validated as finite and within [0, 1], defaults to 0.30, and reaches MCP, daemon, canonical evaluation and tuning. Zero disables the final cross-encoder floor; cosine admission gates remain active.
- BrainBenchmark supports separate baseline/candidate cutoff overrides, records them and reads back effective configuration to reject unsupported settings.
- Existing project defaults and installed/live brains remain unchanged. The experimental configuration uses `retrieval.level=custom`, the new alias and cutoff 0.55.

The [official model card](https://huggingface.co/cross-encoder/mmarco-mMiniLMv2-L12-H384-v1) describes multilingual mMARCO training and Apache-2.0 licensing. The [official ONNX repository](https://huggingface.co/cross-encoder/mmarco-mMiniLMv2-L12-H384-v1/tree/1427fd652930e4ba29e8149678df786c240d8825/onnx) supplies the selected artifact. File sizes and hashes are in `2026-09-07-retrieval/model-manifest.json`; weights were downloaded into an isolated scratch cache.

## Benchmark defect discovered during production verification

Chronological rendering adds `[YYYY-MM-DD]` before memory text. When compression also removes the tail, neither the decorated rendered body nor the full fixture text contains the other. The old matcher called these delivered memories `__unmatched__`, understating hit/recall. A regression test reproduced this and the harness now ignores the date decoration when matching visible text, preserving ambiguity as unmatched. Reports also retain delivered capsules for independent audit. Earlier hardening report retrieval comparisons using the affected matcher must not be treated as evidence of a retrieval regression. The interrupted campaign is preserved in scratch artifacts and superseded by fresh runs with the corrected harness.

## Development selection and mathematics

The diagnostic uses actual embeddings and a six-document reranking batch, but its threshold sweep is post-processing: it does not re-render every threshold or measure production latency. At 0.90 it predicted 154/197 positive hits and 1/13 negative injections. After the production run exposed the matcher defect, that threshold was rejected before validation: the corrected TinyBERT baseline was much stronger than the old 139/197 count suggested. The existing development sweep at 0.55 predicted 170/197 hits and 5/13 injections, which production MCP confirmed. No labels were changed.

Hit@4 = positive queries with at least one delivered gold memory / positive queries. Recall@4 averages the fraction of each query's gold memories delivered. MRR averages the reciprocal rank of the first delivered gold memory. Negative injection rate = negative queries receiving any memory / negative queries. A threshold is a model-specific score, not a calibrated probability.

The optional equal-class selection score is `(Hit@4 + 1 - negative_injection_rate) / 2`; it gives equal weight to the two query classes despite the development set's 197:13 imbalance. It does not express real deployment costs. More generally choose a threshold against `C_miss * P(positive) * miss_rate + C_noise * P(negative) * injection_rate`, with costs and class frequencies from deployment. Thirteen negative queries are too few for a precise false-injection estimate. Repeating deterministic queries measures runtime variation, not additional independent accuracy evidence.

The historical negative labels were preserved, including ambiguous questions about benchmark quality and tokenizer customization. Do not relabel them to improve scores.

## Frozen validation protocol

`validation-frozen.json` SHA256 `1a09270e09a9521c38f9dca14dfebe4349fed0e10b1537a92857eb18d3f6bb6f` was frozen before production policy changes and before inference on it. Four assistant-authored synthetic project families contain 48 positive queries (24 English, 24 Spanish), 24 missing-attribute negatives, and expired/future facts. This is held-out synthetic coverage, not independent real-repository or agent-task certification. The initial development choice 0.90 was rejected before validation after production testing exposed a benchmark text-matching defect. The corrected candidate cutoff 0.55 was chosen from the same development sweep to preserve more positive recall. Frozen validation was first run after cutoff 0.55 and the corrected harness were fixed.

Completed paired production runs used the previously hardened binary versus this candidate, identical BGE-small embeddings, six rerank candidates, cap four, warm-start/ambient/global brain disabled, default thread count, and three alternating repetitions. Contract budget is 2048 tokens; development and frozen validation budgets are 6000. No builds or other owned inference ran during timing. Each comparison fingerprints binaries, runner, harness and fixture inputs.

## Applying the optional configuration

For an explicit experiment requiring multilingual recall and accepting the measured false-injection risk, use an embeddings-enabled candidate binary in the intended project:

```powershell
kimetsu config set retrieval.level custom
kimetsu config set embedder.reranker mmarco-minilm-l12-v2-int8
kimetsu config set broker.rerank_min_score 0.55
```

The first model load requires the pinned local weights or their download. The selected ONNX file is 118,620,016 bytes; runtime memory is measured separately. These commands have not been applied to the user's live project.

## Results and remaining limits

Three alternating repetitions per campaign produced **1,824 query observations over 304 distinct fixture queries**. All comparisons completed with zero scenario errors and no unpaired scenarios. Rankings were identical across repetitions; repeats do not increase the independent accuracy sample size.

| Dataset | Positive hits: Tiny → multilingual | False injections: Tiny → multilingual | Stale injections |
|---|---:|---:|---:|
| Contract, 2048 budget | 8/11 → 11/11 | 2/11 → 1/11 | 0/2 → 0/2 |
| Development, 6000 budget | 169/197 → 170/197 | 7/13 → 5/13 | Not tested |
| Frozen synthetic validation, 6000 budget | 24/48 → 48/48 | 12/24 → 19/24 | 0/8 → 0/8 |

Frozen validation English positives were 24/24 for both models; Spanish positives were 0/24 → 24/24. English missing-fact injections were 12/12 → 8/12; Spanish missing-fact injections were 0/12 → 11/12. TinyBERT's Spanish abstention also discarded every answerable Spanish query: it does not establish language-aware answerability. The multilingual model recognizes related topics but too often injects facts that do not answer the requested attribute. Neither scorer alone solves that problem.

| Dataset | Subsequent-query p50, ms | Subsequent-query p95, ms | Peak MCP MiB |
|---|---:|---:|---:|
| Contract | 429 → 445 | 802 → 470 | 237 → 653 |
| Development | 970 → 1336 | 1107 → 1768 | 281 → 955 |
| Frozen validation | 324 → 369 | 355 → 401 | 237 → 653 |

The contract p95 varies strongly across small runs; its reversed ordering is not evidence that the larger model is generally faster. On the larger development workload the candidate's p95 was about 60% higher and peak memory about 3.4 times higher. Development mean MCP result size fell from 1262 to 1112 bytes. First-query mean latency, including model loading where applicable, rose from 1200 to 2615 ms. These are measurements on this Windows machine, not portable service-level guarantees or million-memory capacity results.

Development positive changes were six gains and five losses. That modest net gain, observed on data used to choose the threshold, is not independent evidence of general superiority. On the frozen fixture, ignoring runtime and using its query frequencies, the miss/noise costs are `24*C_miss + 12*C_noise` for TinyBERT and `19*C_noise` for the candidate. The candidate wins that specific cost model only when `C_noise < (24/7)*C_miss`; this is not a calibrated threshold rule or an estimate of real production frequencies. Four related synthetic project templates are not 72 independent real-world tasks.

**Decision:** retain current defaults; expose the tested model and threshold as explicit experimental controls. Do not present the multilingual option as an abstention fix. The remaining substantive gap is an answerability check that distinguishes a requested fact from a merely related topic, supported by independently evaluated structured claims or another validated verifier. The frozen results are reported as a failed abstention criterion, not tuned away.

No zero-context-token claim: local retrieval needs no paid generation calls, but memories supplied to the agent still occupy its context. Large-corpus capacity, independent agent-task success, and richer claim extraction remain unverified by this follow-up.

## Verification and provenance

- Production source `41ff583`: full workspace **1373 passed, 0 failed, 5 ignored**; embeddings-enabled release built successfully. Core config and actual MCP cutoff tests were observed failing before the fix, then passing. The new model alias failed on the old binary and loaded successfully on the candidate from the pinned isolated cache.
- Benchmark source `3c275f6` (includes `e6cfc3c`): **131 Rust and 17 Python tests passed**, release built. The dated/compressed matcher test was observed failing before the fix. An actual old binary was rejected when it could not report the requested effective cutoff. Independent code review found no remaining blocking issues.
- Candidate SHA256: `2445e8adc06c4a6b59d5ee51be46a6f5aa6e777514dfbb60ac4b136b69ba0ba6`. Baseline SHA256: `5c87e542907a47917f23fad50ddff1e46789eaae14328ccc662ca748b66b5477`.
- Full input/harness/runner fingerprints, per-query delivered capsules and runtime measurements are under [the artifact directory](2026-09-07-retrieval/README.md). `summarize.py` reproduces the saved cohort summary without inference; `run-comparisons.ps1` reruns the campaign with explicit binaries and caches.
- Changes are committed in the isolated audit worktrees. The original source checkout, installed daemon, live configuration and live brains were not changed. No push, merge or installation was performed.
