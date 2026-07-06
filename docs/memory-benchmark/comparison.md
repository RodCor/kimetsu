How Kimetsu compares to mem0, Cognee, Zep, and Letta on the shared public benchmarks, and what we do not yet claim.

## How Kimetsu compares

mem0, Cognee, Zep, and Letta share a design: an LLM distills what to remember
at write time, and most keep an LLM in the retrieval loop too. That buys
accuracy at the cost of metered API spend on every question (mem0's own 2026
figures report ~7,000 tokens per retrieval call).

**Kimetsu's memory pipeline makes zero LLM calls.** Ingest, store, retrieve,
and rerank are FTS5 + local embeddings + a local cross-encoder. The claim is
not "more accurate"; it is **the same accuracy band, without the LLM, the
bill, or the cloud.**

| benchmark | Kimetsu (local, model-free) | mem0 (self-reported) | Cognee (self-reported) |
|-----------|-----------------------------|----------------------|------------------------|
| LongMemEval (`_s`) | **83.0%** (200-q slice) · ~80.9% weighted | 94.4% (their reader + harness) | not reported |
| LoCoMo (1,540 q) | **89.4%** | 92.5% | not reported |
| BEAM 100K | **73.3%** (400 probes) | n/a | 79% |
| BEAM **1M** | **66.0%** (300 probes) | 62% | not reported |
| BEAM 10M | future work | 48.6% | 67% |

Caveats, because the table is not apples-to-apples:

- **Only the 1M row is a matched bucket.** Our 66.0% edges mem0's 62%, but with
  a different reader and 15 of 35 conversations: read it as at least on par,
  not a decisive win.
- **Cognee leads at 100K/10M.** Our 73.3% matches the prior public state of the
  art on 100K (the 0.735 Cognee cites as the number it beat), model-free.
  Cognee needs an LLM key on both the write and read paths.
- **Vendor numbers are self-reported and often do not reproduce** (a published
  LoCoMo 91.6% re-ran closer to 58-66% in the 2026 roundups). We ship the exact
  harness and settings so ours can be checked.

Bottom line: **the same accuracy band as the leading LLM-backed systems, with
the entire memory pipeline local, free, and model-free.** For a head-to-head,
run your system through the same `kbench` harness.

Sources: mem0's [2026 benchmark roundup](https://mem0.ai/blog/ai-memory-benchmarks-in-2026),
Cognee's [BEAM figures](https://github.com/topoteretes/cognee), the
[LongMemEval](https://arxiv.org/abs/2410.10813) and
[BEAM](https://github.com/mohammadtavakoli78/BEAM) papers.

## What we do not yet claim

- Multi-hop retrieval of obliquely relevant memories is v3.0 work; the
  LongMemEval preference result (63%) is that ceiling on a public benchmark.
- The LongMemEval number is a 200-question stratified slice with a specific
  reader, not the full 500.
- BEAM covers the 100K and 1M buckets; 10M needs the write-time distiller in
  the loop and has not been run.
- BrainBench's calibration track has the fewest scenarios and is still being
  scaled; read the scores per dimension.
- Output-token savings in the ROI ledger are estimated, not metered.
