Kimetsu's house rule is that every claim ships with a measurement. This section
documents how we measure the brain and what the numbers are, so you can check
them rather than take our word for it.

## The headline numbers

| benchmark | result |
|-----------|--------|
| BEAM 100K | **73.3%**, matching the prior public state of the art, model-free |
| BEAM 1M | **66.0%**, ahead of mem0's self-reported 62% |
| LongMemEval (`_s`) | **83.0%** (200-question stratified slice) |
| BrainBench quality index | **80.0%** (142 scenarios, reader-free) |
| retrieval quality | recall@4 **0.949**, MRR **0.914** at ~138 ms |
| stale-hit rate | **0.091** (was 0.500 on flat retrieval) |
| cost per solved task | **~13x cheaper** than a no-brain baseline |

Every result uses `jina-v2-base-code` + the `ms-marco-tinybert-l-2-v2`
cross-encoder reranker unless noted, and the memory pipeline makes zero LLM
calls: the reader in the public benchmarks answers questions, it never stores
or retrieves.

## How this section is organized

We measure on three layers, one page each plus the comparison:

1. **[Retrieval and correctness](retrieval-and-correctness)**: the in-repo
   bench that gates every release. Recall, MRR, latency, stale-hit rate, and
   contradiction resolution, runnable with `kimetsu brain bench`.
2. **[BrainBench](brainbench)**: our own reader-free capability benchmark.
   It drives the real binary across difficulty tiers and scores dedup,
   forgetting, importance, and calibration, the write-path behaviour a
   reader-driven test can't see.
3. Public benchmarks, directly comparable to other memory systems:
   **[LongMemEval](longmemeval)** (chat-domain, per-question-type) and
   **[BEAM](beam)** (ten memory abilities over long multi-session chats).
4. **[How Kimetsu compares](comparison)**: the honest side-by-side against
   mem0, Cognee, Zep, and Letta, including where they lead.
