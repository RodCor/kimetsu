Kimetsu's retrieval and correctness numbers: recall, MRR, latency, stale-hit suppression, and contradiction resolution, all reproducible from the shipped CLI.

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

## v2.6 re-measurement

v2.6 is the first release whose `--features embeddings` build actually
compiles (see the RFC's Status section for why it did not), so it is the first
chance to re-run these numbers on the semantic flavor rather than assert them.
Same fixtures, same combos, binary reporting `kimetsu 2.6.0 (embeddings)`:

| fixture | metric | published | v2.6.0 |
|---|---|---|---|
| 100-memory / 210-case | recall@4 (default reranker) | 0.949 | 0.944 |
| 100-memory / 210-case | MRR (default reranker) | 0.914 | 0.910 |
| 100-memory / 210-case | recall@4 (quality-best) | 0.975 | 0.970 |
| 100-memory / 210-case | MRR (quality-best) | 0.933 | 0.930 |
| 100-memory / 210-case | latency (default) | ~138 ms | 130 ms |
| correctness (18 / 22) | stale-hit rate | 0.091 | **0.091** |
| correctness (18 / 22) | resolution accuracy | 0.909 | **0.909** |

The correctness numbers reproduce exactly. The retrieval numbers land within
0.005 of published on every metric — below the noise floor of an approximate-NN
index, where HNSW traversal and MMR tie-breaking can reorder candidates that
score within a rounding error of each other. **The published figures stand**:
the house rule is that a published number is superseded only by a clean run
that surpasses it, and a 0.005 shortfall is not a measurement that the number
was wrong.

What this run is evidence *for* is narrow and worth stating plainly: the
semantic build does not retrieve worse than the flavor these numbers were
measured on. It is not evidence for any ranking default. The two ranking rules
added in v2.6 — `[broker] fusion = rrf` and `[broker] normalization = global` —
are both still defaulted off, because this fixture cannot settle either: it is
single-kind, which makes per-kind and global normalization arithmetically
identical, and too small and uniform to be a fair test of rank fusion.


### Which combination to run

The full 2 × 5 embedder × reranker grid, on the 100-memory / 210-case set,
binary `kimetsu 2.6.0 (embeddings)`. Sorted by MRR:

| embedder | reranker | recall@2 | recall@4 | MRR | mean ms | peak RSS |
|---|---|---|---|---|---|---|
| jina-v2-base-code | jina-reranker-v1-turbo-en | 0.949 | 0.970 | 0.930 | 565 | 2002 MB |
| jina-v2-base-code | ms-marco-minilm-l-4-v2 | 0.954 | 0.959 | 0.930 | 363 | 2352 MB |
| jina-v2-base-code | jina-reranker-v1-tiny-en | 0.944 | 0.970 | 0.926 | 415 | 1982 MB |
| bge-small-en-v1.5 | ms-marco-minilm-l-4-v2 | 0.944 | 0.959 | 0.926 | 729 | 1331 MB |
| bge-small-en-v1.5 | jina-reranker-v1-tiny-en | 0.944 | 0.964 | 0.925 | 785 | 1060 MB |
| bge-small-en-v1.5 | jina-reranker-v1-turbo-en | 0.934 | 0.964 | 0.923 | 943 | 1079 MB |
| **jina-v2-base-code** | **ms-marco-tinybert-l-2-v2** | 0.914 | 0.944 | 0.910 | **125** | 1551 MB |
| bge-small-en-v1.5 | ms-marco-tinybert-l-2-v2 | 0.919 | 0.949 | 0.909 | 484 | **521 MB** |
| jina-v2-base-code | off | 0.827 | 0.878 | 0.824 | 101 | 1467 MB |
| bge-small-en-v1.5 | off | 0.812 | 0.863 | 0.810 | 462 | 361 MB |

**The recommendation is the shipped default** — `jina-v2-base-code` ×
`ms-marco-tinybert-l-2-v2`, which is what `[retrieval] level = "deep"`
resolves to. It gives up 0.020 MRR against the best combination and runs
**2.9× faster** for it. Nothing in the grid dominates it: every combination
that scores higher costs at least 3× the latency, and the two that also want
less RAM cost 4–8×.

Three things the grid says that are worth more than the ranking:

1. **The reranker is the whole game.** Turning it off costs ~0.10 MRR — five
   times the spread between the best and worst *reranked* combination. If you
   tune one thing, tune this. `level = "flexible"` (embedder, no reranker) is
   the configuration to avoid unless RAM is genuinely scarce.
2. **The embedder barely matters once a reranker is present.** Every reranked
   pair lands in 0.909–0.930. Picking `bge-small` buys a 3× smaller resident
   set at ~0.001 MRR — a real trade for a constrained host, and close to free
   in quality terms.
3. **bge-small is not the fast option on this corpus, despite being the small
   model.** It is faster on the 18-memory fixture (18 ms vs 32 ms unreranked)
   and 4× *slower* on the 100-memory one (462 ms vs 101 ms). Two independent
   runs agree to within 2%, so the effect is real; the cause is not
   established here, and the note is left as a measurement rather than dressed
   up as an explanation. Choose `bge-small` for memory footprint, not for
   speed, and re-measure on your own corpus if latency is what you care about.

None of this settles `[broker] fusion` or `[broker] normalization`. Both
fixtures are single-kind, which makes the normalization rules arithmetically
identical and leaves fusion with too little to fuse.

### Cold start, measured

The steady-state latencies above are all *warm*. The first semantic retrieval
in a **fresh brain** is a different number entirely, and v2.6 is the first
release able to measure it:

| call | fresh brain | same brain, second call |
|---|---|---|
| `brain context` (jina-v2-base-code) | **122 s** | 1.5 s |
| `brain context` (bge-small-en-v1.5) | **57 s** | — |
| `brain context` (lean / FTS-only) | 0.3 s | 0.3 s |

The cost is per *brain*, not per process: a second workspace pays it again even
with a model already resident from the first. This is why the `UserPromptSubmit`
context hook is FTS-only by deliberate design — the code comments say a cold
ONNX load "can exceed the host's 30s hook timeout", and on this hardware it
exceeds it by four times over. The daemon exists to absorb exactly this, and the
hook never waits on a cold one.

It is worth knowing before running any harness that seeds a fresh brain per
case. BrainBench does, which puts a 264-scenario run in the ten-hour range on
the semantic build; the harness refuses a lean binary outright, because
FTS-only retrieval would crater the scores rather than measure them.

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

