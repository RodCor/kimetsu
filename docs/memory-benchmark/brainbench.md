BrainBench is Kimetsu's own reader-free capability benchmark: it drives the real binary across difficulty tiers and scores dedup, forgetting, importance, and calibration with no LLM in the loop.

The other pages measure parts of the brain. BrainBench measures the brain as a
whole: a tiered benchmark that drives the real Kimetsu binary against authored
fixtures, with a fresh brain per scenario and **no LLM reader in the loop**.
The score reflects what the brain does, not what a frontier model can reason
around it.

A full run of the four live dimensions over 142 scenarios scores an **Overall
Brain Quality Index of 80.0%**. By capability:

| capability | what it tests | result |
|------------|---------------|--------|
| retrieval correctness | recall / MRR / stale-suppression / contradiction resolution | strong (see [Retrieval & correctness](retrieval-and-correctness), 232 cases) |
| **dedup** | detects near-duplicates without flagging distinct memories | **77%** (98 decisions) |
| **forgetting** | forgets noise while keeping signal, scored after a real forget pass | **88%** |
| **importance** | a salient, proven memory outranks equally relevant peers | **76%** |
| **calibration** | confidence tracks proven usefulness | **82%** (newly instrumented) |

## v2.6.0 re-measurement (264 scenarios)

Re-run on the semantic build, all 264 scenarios of the comprehensive set, none
skipped:

| dimension | published | v2.6.0 | n | 95% CI |
|---|---|---|---|---|
| dedup | 77% | **77.0%** | 50 | ±7.5% |
| forgetting | 88% | **87.8%** | 49 | ±8.3% |
| importance | 76% | **92.1%** | 38 | ±8.7% |
| calibration | 82% | **99.7%** | 122 | ±0.3% |
| retrieval | — | 56.6% | 5 | ±33.6% |

**dedup and forgetting reproduce almost exactly** — 77.0 against 77, and 87.8
against 88. That is the useful part of a re-run: two independent measurements of
the same behaviour landing on the same number is evidence the harness and the
brain are both stable.

**Importance gained ~16 points**, which is outside its confidence interval and
looks real. **Calibration reads 99.7%**, and that is a *problem*, not a win. This
page's own standard is that a benchmark returning ~100% measures nothing; a
track with a ±0.3% interval at n=122 has stopped discriminating. The calibration
scenarios are synthesized from a retrieval pool rather than authored, and they
have become too easy to be informative. Authoring hard calibration cases is the
next investment there.

### The Overall Brain Quality Index is not comparable across these runs

This run reports 91.3% against the 80.0% above, and **that delta should not be
quoted as an improvement.** The index is an unweighted mean over whatever
scenarios ran, and the mix changed: 264 scenarios versus 142, with calibration
alone now 122 of them — 46% of the run, at 99.7%. A mean dominated by a
saturated dimension goes up whether or not anything got better.

Compare per dimension, where the scenarios are the same thing measured twice.
The headline 80.0% is retained above as the figure it was, over the set it was
measured on.

## Safety tracks (new in v2.6.0)

Two dimensions that did not exist before. The v2.6 RFC claimed they did; that
claim was withdrawn, and this is the real thing.

| dimension | score | n | what it checks |
|---|---|---|---|
| poisoning | 100% | 3 | quarantine holds; corroborated local outranks imported; `brain audit` surfaces a write burst |
| render-contract | 100% | 2 | prior-conclusion framing + conflict rule; ordering carries dates |

These confirm three shipped defences work: 4b import quarantine, 4a
trust-weighted ranking (the local memory *outranks* the imported one rather than
excluding it — trust is a weight, never a gate), and burst detection.

**Both scoring 100% is a weakness.** By the standard above, a track that never
breaks is not yet measuring anything; these five scenarios cover the
straightforward case and have no adversarial case that fails. Treat them as
evidence the mechanisms are wired, not as a safety score.

**`render-contract` is deliberately not called "sycophancy".** MemSyco-Bench
measures a *reader* over-deferring to memory against evidence in front of it.
BrainBench has no reader, so that property is not measurable here — what is
measurable is whether the injected text carries the preconditions for not
deferring. A perfect score means the contract is intact, not that the brain
resists sycophancy.

Neither safety dimension is included in any Overall Brain Quality Index figure
on this page.

Two things keep it honest:

1. **It discriminates.** Easy tiers pass, hard tiers break. Dedup, importance,
   and forgetting all show a clean gradient from easy to complex; a benchmark
   that returns ~100% measures nothing.
2. **It exists to surface weaknesses.** The forgetting score caught that
   pruning by usefulness alone can drop a rarely cited but still useful
   memory. Calibration is the thinnest track and the next investment. We
   publish these before claiming them solved.

Run it with `kbench brainbench`. The relationship to the public benchmarks:
**LongMemEval is comparable, BrainBench is deeper.** It scores the write path
and lifecycle behaviour a reader-driven benchmark hides.
