How Kimetsu scores on BEAM, the ten-ability long-term-memory benchmark, at the 100K and 1M token buckets.

[BEAM](https://github.com/mohammadtavakoli78/BEAM) probes ten memory abilities
(information extraction, multi-session reasoning, knowledge update, temporal
reasoning, abstention, contradiction resolution, event ordering, instruction
following, preference following, summarization) over conversations from 128K
to 10M tokens, graded by an LLM judge against per-question rubrics. The
`kbench beam` driver ingests each conversation into a fresh brain, retrieves
per probe, answers with the Codex reader, and judges against the rubric.

## Results: 100K bucket

The full 100K bucket (20 conversations, 400 probes), v2.5 graph-lite backend,
96k retrieval budget.

| ability | accuracy |
|---------|----------|
| contradiction resolution | 40/40 (100%) |
| preference following | 38/40 (95%) |
| information extraction | 36/40 (90%) |
| summarization | 34/40 (85%) |
| temporal reasoning | 31/40 (77.5%) |
| multi-session reasoning | 30/40 (75%) |
| instruction following | 29/40 (72.5%) |
| knowledge update | 24/40 (60%) |
| abstention | 18/40 (45%) |
| event ordering | 13/40 (32.5%) |
| **overall** | **293/400 (73.3%)** |

Three notes on what drives the result:

- **Graph ranking moved the number, model-free.** The flat baseline scored
  62.3% on the same set. Graph-lite lets a probe's top hits pull in their
  edge-connected neighbours with hop-decayed relevance, lifting the abilities
  that stitch facts across sessions (multi-session 35 → 75%, preference
  72.5 → 95%).
- **Global abilities need budget.** Summarization, contradiction resolution,
  and temporal reasoning need the whole arc. At 48k tokens they scored near
  zero; at 96k they reach 100% / 85% / 77.5%. The fix is a knob, not a
  redesign.
- **The reader and judge are LLMs; the memory is not.** The pipeline feeding
  the reader makes zero model calls. The two laggards, abstention (45%) and
  event ordering (32.5%), are reader-behavior limits, not retrieval gaps.

Reproduce with `kbench beam --dataset beam-100k.json --reader-backend codex`;
the converter that builds the dataset from the BEAM repo ships in the bench
tooling.

## Results: 1M bucket

A 1M-token conversation exceeds any context window, so this bucket tests
retrieval ranking: the 96k budget surfaces ~10% of the conversation. Run on 15
of the 35 conversations (300 probes), uniform 96k budget.

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

- **66.0% edges mem0's self-reported 62% at the matched bucket.** Caveats in
  [How Kimetsu compares](comparison).
- **Global and temporal abilities degrade with scale, as expected.** When the
  conversation is ~10x the budget, whole-arc tasks lose ground that local-fact
  tasks keep.
- **The buckets are not a controlled A/B**: different conversations, and the
  100K run used a smaller budget for the localized abilities. Each overall is a
  standalone, reproducible figure.

Reproduce with `kbench beam --dataset beam-1m.json --limit 15
--reader-backend codex`. The 10M bucket is future work: a faithful run needs
the write-time distiller in the loop rather than raw per-turn ingest. mem0
reports 48.6% there; Cognee reports 67%.
