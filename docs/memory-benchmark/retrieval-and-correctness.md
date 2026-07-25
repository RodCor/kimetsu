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

