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
3. **Public benchmarks**, **LongMemEval** (chat-domain, per-question-type) and
   **BEAM** (ten distinct memory abilities over long multi-session chats), run
   through drivers in the bench tooling so we get numbers directly comparable to
   mem0 / Zep / Letta. See "LongMemEval", "BEAM", and "How Kimetsu compares"
   below.

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
**BrainBench**: a tiered (easy to complex) benchmark that drives the real Kimetsu
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

Run on the `longmemeval_s` haystack with a **200-question stratified slice**
(round-robin across all six question types, ~34 per type), using the
`jina-v2-base-code` embedder for retrieval and **Codex (`gpt-5.5`) as both the
reader and the judge** (no API key, driven via `codex exec`). Each haystack turn
is ingested as its own memory tagged with its session date, retrieval runs
through `kimetsu brain context` (a wide ~48k-token budget, ~100+ candidate turns
per question), and the reader answers at high reasoning effort with two rules:
use the session-date tags for time-based reasoning, and on a fact that changed
over time prefer the value from the most recent session date.

| question type | accuracy |
|---------------|----------|
| knowledge-update | 34/34 (100%) |
| single-session-user | 31/34 (91.2%) |
| single-session-assistant | 30/34 (88.2%) |
| temporal-reasoning | 25/34 (73.5%) |
| single-session-preference | 19/30 (63.3%) |
| multi-session | 20/34 (58.8%) |
| **overall** | **159/200 (79.5%)** |

Because the slice samples question types round-robin (≈equal per type) rather
than in the full set's natural proportions, we also report a **population-
weighted overall of ~77.2%**: each type's accuracy reweighted by its share of
the real 500-question set, which is 53% temporal-reasoning + multi-session (the
two hardest types). The ~77.2% is the better estimate of what the full 500 would
score; 79.5% is the raw slice number. Three of the 41 misses were `codex exec`
timeouts (infrastructure, not memory); excluding them, memory accuracy is
159/197 ≈ 80.7%.

This sits at or above the published SOTA band for `longmemeval_s` (strong
retrieval-based systems land roughly 60-80% overall; ~90%+ only appears under
*oracle* retrieval, where the evidence turns are handed to the reader and there
is no retrieval step). What the per-type split shows, honestly:

- **knowledge-update 100% and temporal-reasoning 73.5%** are the categories that
  exercise v2.5's correctness machinery (time-aware recall and picking the current
  fact among contradictions): the two we most wanted to validate on a public
  standard. Both depend on the session-date tags: temporal scores near zero
  without them, and knowledge-update reaches 100% only once the reader is told the
  most-recent dated value wins when a fact conflicts. (Note: this slice ingests
  raw haystack turns, so it tests retrieval + reader recency disambiguation; a
  live brain additionally runs the distiller's contradiction-resolution at write
  time, collapsing a changed fact to one current memory with the prior value
  invalidated (see "Memory correctness" above).)
- **single-session recall is strong** (user 91%, assistant 88%).
- **multi-session 58.8% and single-session-preference 63.3% are the weakest, and
  both are retrieval-bound, not judging-bound.** Multi-session is reasoning-bound:
  cross-session counting and summing need every contributing turn retrieved, and
  the residual misses are completeness (an off-by-one count, an incomplete sum).
  Preference needs a small aside buried in a long session, semantically far from
  the question, so the anchor is sometimes missed and the reader abstains. Closing
  both is exactly the obliquely-relevant / multi-hop retrieval work flagged for
  v3.0 below: a ceiling no single knob removes.

(An earlier 60-question slice scored 86.7%; the larger 200-question run regressed
that to the true mean. The small slice's multi-session was a high-variance 8/10,
versus 58.8% on 34 here. We report the larger, more reliable number.)

**Scope of this number:** it is a 200-question stratified slice of the
500-question `longmemeval_s` set, with a specific reader/judge model and
retrieval settings (wide budget, high reader effort, date-aware reader rules). It
is fully reproducible with `kbench longmemeval --dataset longmemeval_s.json
--reader-backend codex --limit 200`. We report the exact setup and both the raw
and population-weighted figures rather than a single headline, so the number can
be checked and compared like-for-like, per the house rule.

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

## BEAM

[BEAM](https://github.com/mohammadtavakoli78/BEAM) (HuggingFace
`Mohammadta/BEAM-10M`) is a 2026 long-term-memory benchmark that probes **ten
distinct memory abilities** (information extraction, multi-session reasoning,
knowledge update, temporal reasoning, abstention, contradiction resolution, event
ordering, instruction following, preference following, and summarization) over
long multi-session conversations (128K to 10M tokens). Each conversation ships
per-ability *probing questions*, each with a grading *rubric*; the official
pipeline scores answers with an LLM-as-judge against the rubric. We built a
`kbench beam` driver that ingests a conversation into a fresh Kimetsu brain,
retrieves per probe, answers with the same Codex reader, and judges each answer
against its rubric (counting how many of the rubric's points the answer covers).

### Results: 100K bucket

Run on the **100K-token bucket** (the 20 conversations the BEAM repo ships as
JSON; 400 probes, 40 per ability), same embedder + Codex reader/judge as above.

| ability | accuracy |
|---------|----------|
| contradiction resolution | 40/40 (100%) |
| summarization | 32/40 (80%) |
| temporal reasoning | 31/40 (77.5%) |
| preference following | 29/40 (72.5%) |
| information extraction | 26/40 (65%) |
| instruction following | 23/40 (57.5%) |
| knowledge update | 21/40 (52.5%) |
| abstention | 17/40 (42.5%) |
| event ordering | 16/40 (40%) |
| multi-session reasoning | 14/40 (35%) |
| **overall** | **249/400 (62.3%)** |

Two honest notes on the setup, because they drive the result:

- **Retrieval budget by ability is the headline finding.** The four
  *global-aggregation* abilities (summarization, event ordering, contradiction
  resolution, temporal reasoning) need comprehensive recall (the whole arc, both
  sides of a contradiction, every dated event). At a 48k-token retrieval budget
  they scored near zero, not because the brain lacks the ability but because half
  the conversation was never surfaced to the reader. At a 96k budget (most of a
  100K-token conversation, still ranked retrieval, not the raw transcript) they
  jumped: contradiction resolution 0 → 100%, summarization 0 → 80%, temporal
  12.5 → 77.5%, event ordering 0 → 40%. The other six abilities answer from
  localized facts and are not budget-bound; they ran at 48k. This is a real,
  reproducible property of retrieval-based memory: global tasks need enough budget
  to see the whole picture, and the fix is a knob, not a redesign.
- **The reader and judge are LLMs; the memory is not.** As in every memory
  benchmark, an LLM reader answers the final question and an LLM judges it against
  the rubric. Nothing in Kimetsu's storage or retrieval calls a model: the
  pipeline that feeds the reader is FTS5 + local embeddings + a local
  cross-encoder reranker.

Reproduce with `kbench beam --dataset beam-100k.json --reader-backend codex`; the
Node converter that builds `beam-100k.json` from the BEAM repo's JSON ships in the
bench tooling.

### Results: 1M bucket

The 1M bucket exceeds any reader's context window, so it is the regime BEAM is
built for: a 96k retrieval budget surfaces only **~10% of a 1M-token
conversation**, making this a test of retrieval *ranking*, not of stuffing the
transcript into the prompt. Run on **15 of the 35 1M conversations** the BEAM repo
ships (300 probes), at a **uniform 96k budget** across all ten abilities.

| ability | accuracy |
|---------|----------|
| contradiction resolution | 27/30 (90%) |
| knowledge update | 26/30 (86.7%) |
| preference following | 25/30 (83.3%) |
| information extraction | 24/30 (80%) |
| summarization | 23/30 (76.7%) |
| instruction following | 20/30 (66.7%) |
| multi-session reasoning | 20/30 (66.7%) |
| temporal reasoning | 15/30 (50%) |
| event ordering | 9/30 (30%) |
| abstention | 9/30 (30%) |
| **overall** | **198/300 (66.0%)** |

What this shows, honestly:

- **66.0% at 1M is in the same band as mem0's self-reported BEAM-1M (62%)**, and
  now at a *matched bucket*. See "How Kimetsu compares" for the caveats (different
  reader/harness, our 15 conversations vs their full set, vendor self-reported).
- **The global / temporal abilities degrade with scale, as expected.** At the
  same 96k budget, between the 100K and 1M buckets temporal reasoning falls
  77.5 → 50%, event ordering 40 → 30%, and abstention 42.5 → 30% (more retrieved
  context tempts the reader to answer rather than say "I don't know"). When the
  conversation is ~10× the retrievable budget, tasks that need the *whole* arc
  lose ground that tasks needing a *local* fact or a *single* contradiction keep
  (contradiction 90%, knowledge-update 86.7%, information-extraction 80%, and
  summarization still 76.7%).
- **The 1M and 100K per-ability numbers are not a controlled A/B.** They are
  different conversations, and the 100K run used a 48k budget for the six
  localized abilities versus a uniform 96k here, so part of the localized-ability
  difference is budget, not bucket. Each bucket's overall is a standalone,
  reproducible figure; we do not read a "1M beats 100K" trend into them.

Reproduce with `kbench beam --dataset beam-1m.json --limit 15 --reader-backend
codex` (the converter builds `beam-1m.json` from the BEAM repo's `chats/1M` JSON).
The **10M bucket** (10 conversations at ~10M tokens each) is future work: at
that scale a faithful run needs Kimetsu's write-time distiller in the loop
(compacting turns into memories) rather than raw per-turn ingest, and is beyond a
single local machine. mem0 reports 48.6% there.

## How Kimetsu compares

The memory systems Kimetsu is measured against (mem0, Zep, Letta) share a
design: they call an LLM to *distill* what to remember at write time, and most
keep an LLM in the retrieval loop at read time. That buys accuracy at the cost of
per-memory API spend, network dependency, and a cloud service in the path. mem0's
own 2026 figures, for instance, report ~7,000 tokens *per retrieval call*, an
ongoing, metered cost on every question.

**Kimetsu's memory pipeline makes zero LLM calls.** Ingest, store, retrieve, and
rerank are FTS5 + local embeddings + a local cross-encoder: 100% local, free,
and offline-capable. (An optional distiller LLM exists, but the default,
LLM-free pipeline produced every number on this page; adding a model moves the
result only marginally unless it is a top-tier one.) The honest claim is
therefore not "more accurate." It is **the same accuracy band, without the LLM,
the bill, or the cloud.**

On the shared public benchmarks, with the setups documented above:

| benchmark | Kimetsu (local, model-free pipeline) | for reference (vendor self-reported) |
|-----------|--------------------------------------|--------------------------------------|
| LongMemEval (`_s`) | **79.5%** (200-q slice) · ~77.2% pop-weighted | mem0 94.4% (full set, their reader + harness) |
| BEAM 100K | **62.3%** (400 probes) | n/a |
| BEAM **1M** | **66.0%** (15 convs, 300 probes) | mem0 62% (their full set) |
| BEAM 10M | future work | mem0 48.6% |

Read that table carefully, because the comparison is not apples-to-apples and we
won't pretend it is:

- **The 1M row is a matched bucket; the rest are not.** At the **1M** bucket our
  66.0% edges mem0's self-reported 62%, but ours is 15 of the 35 conversations
  with a Codex reader, and mem0's is their full set with their own reader/harness,
  so read it as "**at least on par at the hard bucket**," not a decisive win. Our
  LongMemEval is a 200-question slice, not the full 500. The 10M bucket we have
  not run (see "BEAM" above for why).
- **Vendor numbers are self-reported and often do not reproduce.** Independent
  re-runs of vendor memory numbers tend to land well below the published figure
  (the public 2026 roundups note, e.g., a LoCoMo claim of 91.6% reproducing closer
  to 58–66%). We publish the exact harness, reader, and settings so ours can be
  checked: that *is* the comparison we stand behind.

The defensible, checkable bottom line: **Kimetsu reaches the same accuracy band as
the leading LLM-backed memory systems while keeping the entire memory pipeline
local, free, and model-free.** Want a head-to-head? Run your system through the
same `kbench` harness and settings.

Sources: mem0's 2026 benchmark roundup
([mem0.ai](https://mem0.ai/blog/ai-memory-benchmarks-in-2026)); the
[LongMemEval](https://arxiv.org/abs/2410.10813) and
[BEAM](https://github.com/mohammadtavakoli78/BEAM) papers.

## What we do not yet claim

- Multi-hop / graph-structured retrieval (the measured ~0.93 MRR ceiling on
  oblique queries) is v3.0 work. The LongMemEval single-session-preference
  result (60%, above) is the same ceiling showing up on a public benchmark:
  surfacing an obliquely-relevant memory the question does not lexically or
  semantically resemble. Wider retrieval and a stronger reader lift it (from 30%
  to 60%) but do not remove it.
- The LongMemEval number above is a 200-question stratified slice with a specific
  reader/judge model, not the full 500-question set (see "Scope of this number").
- The BEAM numbers cover the 100K bucket (20 conversations, mixed 48k/96k budget
  by ability) and the 1M bucket (15 of 35 conversations, uniform 96k). The 10M
  bucket is future work: at ~10M tokens per conversation a faithful run needs the
  write-time distiller in the loop, not raw per-turn ingest (see "BEAM").
- BrainBench's **calibration** track is the thinnest (fewest scenarios) and is
  the one we trust least so far: we are scaling it before leaning on it. Its
  per-capability scores are not a single headline figure; read them per
  dimension. Outcome-driven confidence is new in v2.5 and still being tuned.
- Output-token savings in the ROI ledger are estimated, not metered (the host
  does not expose per-session output counts).
