How the broker scores, selects, and budgets the memories injected into each run.

When a run starts, the **broker** assembles a context bundle: it walks both
brains, scores candidates, and returns the top-N inside a token budget.

**Candidate generation.** Lexical FTS5 always provides candidates. On the
embeddings build the broker also queries a **usearch HNSW** index (a
`brain.usearch` sidecar, f16-quantized, O(log N)) and unions those hits with
the FTS set, so a memory whose meaning matches can surface without sharing a
word with the query.

The score is a weighted sum plus two multipliers:

```
raw_relevance  = (1 - α) * lexical_match + α * cosine_similarity   (α = 0.5)
multiplier     = usefulness_multiplier(score, use_count)  ∈ [0.5, 1.5]
decay          = exp(-ln 2 · age_days / half_life_days)   (default 30d)
effective      = 1.0 + (multiplier - 1.0) · decay
final_score    = w.relevance · raw_relevance + w.confidence · confidence
               + w.freshness · freshness + w.scope · scope_weight
```

Weights are tunable per stage (`localization`, `patch_plan`, `verification`,
`review`) via `[broker.weights.<stage>]`.

**Selection.** Embedding-MMR (lambda 0.7) collapses paraphrase
near-duplicates; an absolute semantic floor (`min_semantic_score`, AUTO by
default) drops off-topic candidates before budgeting, so an irrelevant query
returns nothing rather than padding the prompt. Key knobs in `[broker]`:
`max_capsules` (8), `budget_floor_tokens` (1500), `budget_run_cap_tokens`
(8000).

## Embeddings vs lean builds

- **Embeddings** (the CLI default): ships fastembed + ONNX. Cosine retrieval,
  semantic dedup, and conflict detection all light up. Three curated models:
  `bge-small-en-v1.5` (384d, default), `bge-m3` (1024d, multilingual),
  `jina-v2-base-code` (768d, code-tuned). Precedence:
  `KIMETSU_BRAIN_EMBEDDER` env > `[embedder]` config > default. `kimetsu
  brain model set <id>` re-embeds the corpus; cross-model rows fall back to
  FTS until reindexed, so retrieval never breaks mid-migration.
- **Lean** (`--no-default-features`): no embedder, no model download.
  Retrieval is FTS-only; semantic dedup and conflict detection become silent
  no-ops. Library crates default to lean so downstream consumers stay slim.

---

## The agent brain (proactive + cost-shrinking)

For the autonomous agent pipeline, an adaptive layer sits on top of
retrieval:

- **Task-kind routing.** A cheap deterministic classifier sorts each task
  into Debug / Feature / Refactor / Docs / Investigation, and a weight layer
  biases recall accordingly: Debug leans on recent `failure_pattern`s,
  Refactor on conventions, Investigation on broad facts.
- **Proactive "Known pitfalls".** Before the first attempt, a tight
  `failure_pattern` retrieval surfaces known mistakes, at ~zero tokens when
  nothing matches. A per-run ledger stops re-surfacing on retries.
- **Cross-stage dedup.** A capsule rendered once is back-referenced in later
  stages, so brain overhead shrinks as a task spans more stages.
- **Lazy expansion.** Top capsules inject in full; the tail injects as
  one-line headlines the agent expands on demand via `expand_capsule`.
- **Adaptive budget.** The per-stage budget scales sublinearly with task size
  (`floor + k·√task_size`), floored and capped per run. Doubling task size
  grows the budget ~41%.
