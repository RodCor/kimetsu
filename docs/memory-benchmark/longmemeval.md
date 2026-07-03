How Kimetsu scores on LongMemEval, the public long-term-memory benchmark, with the exact setup so the number can be reproduced and compared.

[LongMemEval](https://github.com/xiaowu0162/LongMemEval) covers single-session,
multi-session, temporal-reasoning, knowledge-update, and preference questions
over long chat histories. The `kbench longmemeval` driver ingests the haystack
into a Kimetsu brain, retrieves per question, answers with an LLM, and scores
per question type. It is a chat-domain benchmark, so it tests Kimetsu's
correctness machinery (temporal validity, supersession, multi-session recall)
on a public standard.

## Results

Setup: `longmemeval_s`, a 200-question stratified slice (round-robin across the
six types), `jina-v2-base-code` for retrieval, Codex (`gpt-5.5`) as reader and
judge. Each turn is ingested as its own memory tagged with its session date;
retrieval runs through `kimetsu brain context` at a ~48k-token budget; the
reader uses the date tags for time reasoning and prefers the most recent value
when a fact changed.

| question type | accuracy |
|---------------|----------|
| knowledge-update | 34/34 (100%) |
| single-session-user | 32/34 (94.1%) |
| single-session-assistant | 32/34 (94.1%) |
| temporal-reasoning | 28/34 (82.4%) |
| single-session-preference | 20/30 (66.7%) |
| multi-session | 20/34 (58.8%) |
| **overall** | **166/200 (83.0%)** |

The slice samples types evenly, so we also report a population-weighted
overall of **~80.9%**, the better estimate of a full 500-question run (the
full set is 53% temporal + multi-session, the two hardest types). The run had
zero reader errors. Earlier runs on the same setup scored 79.5% (with three
reader timeouts counted as misses) and, on a small 60-question slice, 86.7%;
the error-free 200-question run is the number we stand behind.

What the split shows:

- **knowledge-update 100% and temporal 82.4%** validate the v2.5 correctness
  work. Both depend on the session-date tags: temporal scores near zero without
  them.
- **Single-session recall is strong** (94% / 94%).
- **Multi-session 58.8% and preference 66.7% are the weakest.** Cross-session
  counting needs every contributing turn retrieved; a preference is a small
  aside semantically far from the question. Both are the multi-hop retrieval
  work flagged for v3.0.

Reproduce with `kbench longmemeval --dataset longmemeval_s.json
--reader-backend codex --limit 200`.

## How we compare

On `longmemeval_s`, strong retrieval-based systems with a capable reader land
in roughly the **60-80% band**; ~90%+ appears only under oracle retrieval,
where the evidence is handed to the reader
([arXiv:2410.10813](https://arxiv.org/abs/2410.10813)). Kimetsu's 83.0% sits
just above that band. We do not print a head-to-head table here because
published figures vary by dataset variant, reader model, and budget; run your
system through the same `kbench` harness for a direct comparison.

## Why this is not our best measure of the brain

83.0% understates the memory, for three reasons:

1. **It bypasses the write path.** The harness ingests raw turns, skipping the
   distiller, dedup, and write-time contradiction resolution, the parts of
   Kimetsu that are actually differentiated.
2. **A strong reader does much of the work.** A frontier reader over ~100
   retrieved turns can brute-force answers a weaker memory would need to
   surface precisely.
3. **It is chat-domain, not ours.** The
   [retrieval and correctness numbers](retrieval-and-correctness) are the
   sharper signal for coding agents.

Treat LongMemEval as the comparable number and
[BrainBench](brainbench) plus the correctness metrics as the truer measure of
the brain itself.
