# Kimetsu memory benchmark

Kimetsu's house rule is that every claim ships with a measurement. This page
documents how we measure the brain and what the numbers are, so you can check
them rather than take our word for it.

We measure on three layers:

1. **In-repo correctness + retrieval bench**: runs in the shipped CLI with
   cached local models (no Docker, no downloads), gates every release, and
   covers our domain (coding-agent memory). This is the source of the retrieval
   and correctness numbers below.
2. **BrainBench**, our own deep, reader-free capability benchmark: it drives the
   real brain across difficulty tiers and scores dedup, forgetting, importance,
   and calibration, the write-path and lifecycle behaviour a reader-driven test
   can't see. See "Brain capability benchmark" below.
3. **LongMemEval**, the public, chat-domain standard, run through a driver in
   the bench tooling so we get a number directly comparable to mem0 / Zep /
   Letta. See "LongMemEval" below for the per-question-type results.

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

- **stale-hit rate**: how often a superseded / outdated memory still shows up
  in the top-k. Lower is better.
- **resolution accuracy**: on contradiction and knowledge-update cases, how
  often the *current / correct* memory outranks the stale one. Higher is better.

| metric | before (flat retrieval) | v2.5 | change |
|--------|------------------------|------|--------|
| stale-hit rate | 0.500 | **0.091** | −82% |
| resolution accuracy | 0.364 | **0.909** | +0.545 |

A plain semantic store returns both the old and new fact because cosine
similarity does not track recency or supersession, so a stale fact surfaces
about half the time, and contradictions resolve barely better than chance. With
v2.5, superseded facts are excluded from default retrieval (still queryable for
history), and a new memory that contradicts an old one is resolved
automatically by confidence × recency, with the loser invalidated-as-of
(lineage preserved, never destroyed).

### No regression

The correctness work did not cost retrieval quality. The v2.0 retrieval
baseline is unchanged in v2.5: on the 18-memory / 100-case set, recall@4 0.977 /
MRR 0.941 before and after.

## Brain capability benchmark (BrainBench)

The numbers above measure *parts* of the brain. To measure the brain as a
whole, and the way a memory system actually should be judged, we built
**BrainBench**: a tiered (easy → complex) benchmark that drives the real Kimetsu
binary against authored fixtures and scores five capabilities a plain vector
store can't even attempt. Crucially, **no LLM reader is in the loop**, so the
score reflects what the brain *does*, not what a frontier model can reason around
it. (That reader confound is why the public LongMemEval number below, while
comparable, isn't our truest measure.)

Across ~150 scenarios spread over difficulty tiers:

| capability | what it tests | result |
|------------|---------------|--------|
| retrieval correctness | recall / MRR / stale-suppression / contradiction resolution | strong (see *Retrieval quality* above, 232 cases) |
| **dedup** | detects near-duplicates **and** does not flag distinct memories | **77%** (98 decisions) |
| **forgetting** | forgets noise while keeping signal, scored by recall *retained* after a real forget pass | **88%** |
| **importance** | a salient / proven memory outranks equally-relevant peers | **76%** |
| **calibration** | confidence tracks proven usefulness (citations raise it, regrets lower it) | **82%** (newly instrumented) |

Two things make this an honest instrument rather than a vanity score:

1. **It discriminates.** Easy tiers pass; hard and complex tiers break, exactly
   what a measuring tool should do. A benchmark that returns ~100% isn't measuring
   difficulty, it's measuring nothing. Dedup, importance, and forgetting all show
   a clean gradient from easy to complex.
2. **It is designed to surface our own weaknesses.** Kimetsu is self-tuning, so
   the benchmark exists to tell us what to fix next, not to flatter us. The
   forgetting score, for instance, is measured by whether retrieval still works
   *after* a real forget pass, which caught that pruning by usefulness alone can
   drop a rarely-cited but still-useful memory. The calibration track (new) is the
   thinnest and is where we're investing next. We publish these before we claim
   them solved: that is the house rule.

BrainBench is reader-free, runs through the bench tooling (`kbench brainbench`),
drives the shipped binary, and isolates a fresh brain per scenario. It complements
LongMemEval: **LongMemEval is *comparable*, BrainBench is *deeper***. It scores
the write-path, dedup, forgetting, and confidence behaviour that a reader-driven
benchmark hides.

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
answers with an LLM, and scores per question type, so the result is directly
comparable to other memory systems.

Honest framing: LongMemEval is a chat-domain benchmark, so it exercises
Kimetsu's memory-*correctness* machinery (temporal validity, supersession,
multi-session recall) on a public standard. It complements, not replaces, the
coding-domain metrics above.

### Results

Run on the `longmemeval_s` haystack with a **60-question stratified slice (10
per question type)**, using the `jina-v2-base-code` embedder for retrieval and
**Codex (`gpt-5.5`) as both the reader and the judge** (no API key, driven via
`codex exec`). Each haystack turn is ingested as its own memory tagged with its
session date, retrieval runs through `kimetsu brain context` (a wide ~48k-token
budget, ~100+ candidate turns per question), and the reader answers at high
reasoning effort with two rules: use the session-date tags for time-based
reasoning, and on a fact that changed over time prefer the value from the most
recent session date.

| question type | accuracy |
|---------------|----------|
| single-session-user | 10/10 (100%) |
| knowledge-update | 10/10 (100%) |
| temporal-reasoning | 9/10 (90%) |
| single-session-assistant | 9/10 (90%) |
| multi-session | 8/10 (80%) |
| single-session-preference | 6/10 (60%) |
| **overall** | **52/60 (86.7%)** |

This is at or above the published SOTA band for `longmemeval_s` (strong retrieval-
based systems land roughly 60-80% overall; ~90%+ only appears under *oracle*
retrieval, where the evidence turns are handed to the reader and there is no
retrieval step). What the per-type split shows, honestly:

- **knowledge-update 100% and temporal-reasoning 90%** are the categories that
  exercise v2.5's correctness machinery (time-aware recall and picking the current
  fact among contradictions): the two we most wanted to validate on a public
  standard. Both depend on the session-date tags: temporal scores near zero (1/10)
  without them, and knowledge-update reaches 100% only once the reader is told the
  most-recent dated value wins when a fact conflicts. (Note: this slice ingests
  raw haystack turns, so it tests retrieval + reader recency disambiguation; a
  live brain additionally runs the distiller's contradiction-resolution at write
  time, collapsing a changed fact to one current memory with the prior value
  invalidated (see "Memory correctness" above).)
- **multi-session 80%** is reasoning-bound: cross-session counting and summing
  need both wide retrieval (every contributing turn) and a reader that actually
  reasons. It climbs from 50% to 80% when the reader runs at high effort. The
  residual misses are completeness (an off-by-one count, an incomplete sum).
- **single-session recall is strong** (user 100%, assistant 90%).
- **single-session-preference (60%) is the weakest category, and the cause is
  retrieval, not judging.** The preference signal is often a small aside buried
  in a long session, semantically far from the question, so even with ~100
  candidate turns retrieved the anchor is sometimes missed and the reader
  abstains. Closing this is exactly the obliquely-relevant retrieval work flagged
  for v3.0 below: it is a ceiling no single knob removes.

**Scope of this number:** it is a 60-question stratified slice, not the full
500-question `longmemeval_s` set, and it uses a specific reader/judge model and
retrieval settings (wide budget, high reader effort, date-aware reader rules). It
is fully reproducible with `kbench longmemeval --dataset longmemeval_s.json
--reader-backend codex --limit 60`. A full-set run is future work. We report the
exact setup rather than a single headline figure so the number can be checked and
compared like-for-like, per the house rule.

### How we compare

LongMemEval is the field's shared yardstick (mem0, Zep, Letta and others report on
it), which is exactly why we run it. On `longmemeval_s`, strong retrieval-based memory
systems with a capable reader land in roughly the **60-80% overall** band; scores above
~90% appear only under *oracle* retrieval (the evidence turns are handed to the reader,
so there is no retrieval problem left to solve). See the LongMemEval paper for the task
and baseline methodology ([arXiv:2410.10813](https://arxiv.org/abs/2410.10813)).

**Kimetsu's 86.7% sits at or above that band.** We deliberately do not print a
head-to-head table of competitor numbers here: published figures vary by dataset
variant, reader model, and retrieval budget, and a row-by-row table implies an
apples-to-apples comparison we cannot guarantee across those differences. The honest,
checkable claim is the one above (at/above the public SOTA band), plus the exact setup
to reproduce ours. If you want a direct comparison, run your system through the same
`kbench longmemeval` harness and settings.

### Why this is not our best measure of the brain

Counter-intuitively, 86.7% *understates* Kimetsu's memory, for three reasons:

1. **It bypasses our write path.** This harness ingests raw chat turns as independent
   memories. That skips the part of Kimetsu that is actually hard and actually
   differentiated: the distiller deciding *what* to remember, novelty/dedup, and
   write-time contradiction-resolution + supersession. A live brain would collapse a
   changed fact into one current memory with the old value invalidated; here we stand in
   for that with a reader rule. We score the retrieval, not the remembering.
2. **A strong reader does much of the work.** A frontier reader at high reasoning effort
   over ~100 retrieved turns can brute-force answers a weaker memory would have to
   *surface precisely*. The benchmark rewards the reader's reasoning as much as the
   brain's retrieval.
3. **It is chat-domain, not ours.** Kimetsu is built for coding agents; LongMemEval is
   general chat. Our domain-specific retrieval + correctness numbers (above) are the
   sharper signal for the use case we actually target.

So treat LongMemEval as the **comparable** number, and the in-repo correctness metrics
(stale-hit rate, resolution accuracy) plus the reader-free **BrainBench** capability
benchmark (above), which scores the write path, dedup, forgetting, and calibration
directly, as the **truer** measure of whether the brain itself is getting better.

## What we do not yet claim

- Multi-hop / graph-structured retrieval (the measured ~0.93 MRR ceiling on
  oblique queries) is v3.0 work. The LongMemEval single-session-preference
  result (60%, above) is the same ceiling showing up on a public benchmark:
  surfacing an obliquely-relevant memory the question does not lexically or
  semantically resemble. Wider retrieval and a stronger reader lift it (from 30%
  to 60%) but do not remove it.
- The LongMemEval number above is a 60-question stratified slice with a specific
  reader/judge model, not the full 500-question set (see "Scope of this number").
- BrainBench's **calibration** track is the thinnest (fewest scenarios) and is
  the one we trust least so far: we are scaling it before leaning on it. Its
  per-capability scores are not a single headline figure; read them per
  dimension. Outcome-driven confidence is new in v2.5 and still being tuned.
- Output-token savings in the ROI ledger are estimated, not metered (the host
  does not expose per-session output counts).
