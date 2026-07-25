How the broker scores, selects, and budgets the memories injected into each run.

When a run starts, the **broker** assembles a context bundle: it walks both
brains, scores candidates, and returns the top-N inside a token budget.

**Candidate generation.** Lexical FTS5 always provides candidates. On the
embeddings build the broker also queries a **usearch HNSW** index (a
`brain.usearch` sidecar, f16-quantized, O(log N)) and unions those hits with
the FTS set, so a memory whose meaning matches can surface without sharing a
word with the query.

The default backend is **graph-lite**: on top of that flat set it walks up to
two hops over `memory_edges`, so a memory one edge from the best hit can be
pulled in even when the query never mentions it. Graph-reached candidates enter
at a hop-decayed share of the seed's relevance (`×0.6` per hop), so they rank
below the hit that surfaced them. The candidate set is a strict superset of
flat's, which is why the default is safe: broader recall, never a displaced
result. Edges are written as each memory lands, from the entities it shares
with the existing corpus — no `kimetsu brain graph build` required, though that
command still rebuilds the whole graph if you want it re-derived.

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

**Abstention.** A bundle carries `evidence_coverage`: the IDF-weighted share
of the query's discriminating terms the returned capsules cover *collectively*.
Below 0.5 the injection is followed by an explicit line naming what none of them
mention — "nothing above covers kubernetes, rollout. Treat the rest as unknown
rather than inferring it." Kimetsu already abstained at the bundle level (top
score below `min_score` → empty bundle, zero tokens); this is the signal for a
bundle it *does* return, so a reader handed three capsules that touch half the
question can tell that from three that answer it. The same fields are exposed on
`kimetsu_brain_context`.

The weighting is deliberately not the one the per-memory floor uses: there a
query term absent from the whole corpus is zeroed (it would sink every
candidate), while here it is the *strongest* evidence of a gap, and weighs most.

**Budgeting.** Capsules are filled greedily in score order against **half** of
the requested `budget_tokens` — the other half is headroom for the rest of the
bundle (repo files, manifests, the render-time framing) and for the chars/4
token estimate being an approximation rather than a count. So a request for
2000 tokens fills capsules up to ~1000. Overflow lands in `excluded` rather
than being dropped silently, and `max_capsules` caps the count before the
token check.

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

For the autonomous agent pipeline (`kimetsu run`), an adaptive layer sits on
top of retrieval. The tools named below (`expand_capsule`, `cite_memory`) are
that pipeline's own tools, not part of the MCP surface a host agent sees:

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
