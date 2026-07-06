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
