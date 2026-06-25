# Kimetsu memory benchmark

Kimetsu's house rule is that every claim ships with a measurement. This page
documents how we measure the brain and what the numbers are, so you can check
them rather than take our word for it.

We measure on two layers:

1. **In-repo correctness + retrieval bench** — runs in the shipped CLI with
   cached local models (no Docker, no downloads), gates every release, and
   covers our domain (coding-agent memory). This is the source of the numbers
   below.
2. **LongMemEval** — the public, chat-domain standard, run through a driver in
   the bench tooling so we get a number directly comparable to mem0 / Zep /
   Letta. The harness is built; see "LongMemEval" below for status.

All metrics are reproducible with `kimetsu brain bench` (semantic build). Every
result here is from `jina-v2-base-code` + the `ms-marco-tinybert-l-2-v2`
cross-encoder reranker unless noted.

## Retrieval quality

On a 100-memory / 210-case dataset seeded from real exported memories
(keyword, paraphrase, oblique, confusable, in-domain-no-answer, multi-answer):

| metric | value |
|--------|-------|
| recall@4 | **0.949** (default reranker), up to 0.975 |
| MRR | **0.914** (default), up to 0.933 |
| latency | ~138 ms per retrieval + rerank |

The default (`ms-marco-tinybert-l-2-v2`) is the fastest reranked combo; the
quality-best rerankers reach recall@4 0.975 / MRR 0.933 at higher latency. Swap
embedder and reranker with one config key each and re-judge on your own corpus.

## Memory correctness (v2.5)

v2.5 ("The best memory") added a temporal validity model, automatic
contradiction resolution, and validity-aware retrieval. We measure two things a
plain vector store cannot do, on a correctness dataset of knowledge-update,
contradiction, and temporal cases:

- **stale-hit rate** — how often a superseded / outdated memory still shows up
  in the top-k. Lower is better.
- **resolution accuracy** — on contradiction and knowledge-update cases, how
  often the *current / correct* memory outranks the stale one. Higher is better.

| metric | before (flat retrieval) | v2.5 | change |
|--------|------------------------|------|--------|
| stale-hit rate | 0.500 | **0.091** | −82% |
| resolution accuracy | 0.364 | **0.909** | +0.545 |

A plain semantic store returns both the old and new fact because cosine
similarity does not track recency or supersession — so a stale fact surfaces
about half the time, and contradictions resolve barely better than chance. With
v2.5, superseded facts are excluded from default retrieval (still queryable for
history), and a new memory that contradicts an old one is resolved
automatically by confidence × recency, with the loser invalidated-as-of
(lineage preserved, never destroyed).

### No regression

The correctness work did not cost retrieval quality. The v2.0 retrieval
baseline is unchanged in v2.5: on the 18-memory / 100-case set, recall@4 0.977 /
MRR 0.941 before and after.

## Cost

On a recorded 16-task Terminal-Bench slice, runs with the brain cost about 13×
less per win than the no-brain baseline ($0.19 vs $2.47), measured on Claude
Code at Claude pricing. See `docs/ROI-METHODOLOGY.md` for the methodology and
the `kimetsu brain roi` ledger for per-memory savings on your own work.

## How to reproduce

```bash
# retrieval quality + correctness metrics (semantic build, cached models)
kimetsu brain bench --dataset <fixture>.json \
  --embedders jina-v2-base-code --rerankers ms-marco-tinybert-l-2-v2

# the summary table reports recall@2/4, MRR, latency, and (when the fixture
# has temporal/contradiction cases) stale_hit_rate + resolution_accuracy.
```

The eval fixtures live in the bench tooling; the harness (`kimetsu brain bench`)
ships in the CLI, so you can run the same metrics against your own exported
memories.

## LongMemEval

[LongMemEval](https://github.com/xiaowu0162/LongMemEval) is the public benchmark
for long-term memory (single-session, multi-session, temporal-reasoning,
knowledge-update, preference). We built a `kbench longmemeval` driver that
ingests the LongMemEval haystack into a Kimetsu brain, retrieves per question,
answers with an LLM, and scores per question type — so the result is directly
comparable to other memory systems.

Honest framing: LongMemEval is a chat-domain benchmark, so it exercises
Kimetsu's memory-*correctness* machinery (temporal validity, supersession,
multi-session recall) on a public standard. It complements, not replaces, the
coding-domain metrics above.

**Status:** the harness is implemented and dry-run-verified. Published scores
require running it with an LLM answerer/judge and the LongMemEval dataset; the
numbers will be added here once that run completes. We will not publish a
comparison number until it is real and reproducible.

## What we do not yet claim

- Multi-hop / graph-structured retrieval (the measured ~0.93 MRR ceiling on
  oblique queries) is v3.0 work.
- The LongMemEval comparison number is pending an actual run (above).
- Output-token savings in the ROI ledger are estimated, not metered (the host
  does not expose per-session output counts).
