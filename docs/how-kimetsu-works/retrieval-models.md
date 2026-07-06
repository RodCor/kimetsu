Which embedder and reranker Kimetsu ships, why, and how to swap or re-benchmark them.

The local retrieval stack is an embedder plus a cross-encoder reranker, both
warm inside the embed daemon. Defaults were chosen with `kimetsu brain bench`,
a benchmark seeded from real exported memories (100 memories in confusable
topic clusters, 210 cases: keyword, paraphrase, oblique, confusable,
no-answer, multi-answer):

| embedder          | reranker     | recall@2 | recall@4 |  MRR  | mean ms | peak RSS |
|-------------------|--------------|----------|----------|-------|---------|----------|
| jina-v2-base-code | jina-turbo   | 0.954    | 0.975    | 0.933 | 552     | 2.0 GB   |
| jina-v2-base-code | jina-tiny    | 0.949    | 0.975    | 0.931 | 414     | 2.0 GB   |
| jina-v2-base-code | minilm-l-4   | 0.949    | 0.959    | 0.927 | 372     | 2.3 GB   |
| **jina-v2-base-code** | **tinybert-l-2** | 0.914 | 0.949 | 0.914 | **132** | 1.5 GB |
| jina-v2-base-code | off          | 0.929    | 0.939    | 0.915 | 106     | 1.5 GB   |
| bge-small-en-v1.5 | off          | 0.931    | 0.966    | 0.911 | 446     | 359 MB   |

The default (**jina-v2-base-code + ms-marco-tinybert-l-2-v2**) is the fastest
reranked combo, within ~2% MRR of the grid best, and fits the hook's 300ms
budget. jina-v2 beats bge-small across every reranker on this corpus; any
reranker beats none. The lean-RAM option is `bge-small-en-v1.5` (~360-525 MB
at ~1-3% lower MRR).

**Swapping models** (takes effect after a daemon restart):

```bash
kimetsu config set embedder.model bge-small-en-v1.5
kimetsu config set embedder.reranker jina-reranker-v1-tiny-en   # or off, any HF ONNX id
kimetsu brain reindex          # REQUIRED after an embedder change
kimetsu brain daemon stop      # next prompt spawns a daemon with the new models
```

`KIMETSU_BRAIN_EMBEDDER` overrides per process.

**Re-judging as your brain grows**:

```bash
kimetsu brain export bench/memories-export.json   # refresh the dataset source
kimetsu brain bench                               # full grid -> summary.md
kimetsu brain eval                                # fixture-based quick check
```

Watch-item: the semantic floor (`broker.min_semantic_score`, 0.35) was
calibrated on bge-family cosine distributions; re-tune it against
`kimetsu brain eval` after an embedder change. The remote server runs its own
operator-configured reranker; see [Kimetsu Remote](../remote).
