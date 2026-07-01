
When a run starts (chat REPL, MCP `kimetsu_brain_context` call, or
the agent loop's pre-stage hook), the **broker** assembles a
context bundle. It walks both brains, scores candidates, and returns
the top-N inside a token budget.

**Candidate generation.** Lexical FTS5 always provides candidates. On the
embeddings build the broker *also* runs an approximate-nearest-neighbour query
against a **usearch HNSW** index (persisted as a `brain.usearch` sidecar next to
brain.db, f16-quantized by default, O(log N) per query) and **unions** those hits with the FTS
set, so a memory whose *meaning* matches the query can surface even when it
shares no words with it. Lean builds use the FTS candidate set alone.

The score is a weighted sum of four signals, plus two multipliers:

```
raw_relevance  = (1 - α) * lexical_match + α * cosine_similarity
                                                 (where α = 0.5 default,
                                                  cosine only fires when
                                                  --features embeddings is on)

multiplier     = usefulness_multiplier(usefulness_score, use_count)
                 ∈ [0.5, 1.5]  blended by Bayesian smoothing

decay          = exp(-ln 2 · age_days / half_life_days)   ∈ [0, 1]
                 age measured from coalesce(last_useful_at, created_at)
                 half_life_days default = 30, set to 0 to disable

effective      = 1.0 + (multiplier - 1.0) · decay
                 (decay attenuates the *deviation* from neutral, not
                  the multiplier itself; a year-old +1.5 memory
                  slides toward 1.0, NOT toward 0)

final_score    = weights.relevance   · raw_relevance
               + weights.confidence  · confidence
               + weights.freshness   · freshness
               + weights.scope       · scope_weight
                 (all per-stage tunable via [broker.weights.<stage>])
```

Stages: `localization`, `patch_plan`, `verification`, `review`. Each
has its own weight profile in `project.toml`.

**Selection (sharpened in v1.0).** On the embeddings build the broker runs
**embedding-MMR** (lambda=0.7): diversity is measured by cosine distance, so it
collapses true paraphrase near-duplicates that share no surface words (with
Jaccard token overlap as the lean-build / fallback path). An **absolute
semantic-relevance floor** (`min_semantic_score`, embeddings-only) then drops
candidates whose cosine to the query is below the threshold *before* budgeting,
so a genuinely off-topic query hits the zero-capsule "skipped" path and returns
nothing rather than padding the prompt with weak hits. Lean (FTS-only)
selection is unchanged.

Tunable knobs in `[broker]`:

- `max_capsules` (default **8**): hard cap on capsules rendered into a prompt.
- `min_semantic_score` (default `-1.0` = AUTO: 0.35 on bge-family, off otherwise): the embeddings-only relevance
  floor described above.
- `budget_floor_tokens` (default `1500`) and `budget_run_cap_tokens`
  (default `8000`): bounds for the adaptive per-run budget (see the agent
  brain section below).

## Embeddings vs lean builds

- **Embeddings** (default for the CLI): `cargo install kimetsu-cli`
  ships with `--features embeddings` on. Pulls fastembed-rs + ONNX
  runtime; needs the VS2022 C++ runtime on Windows (ort prebuilts).
  Default model is BGE-small-en-v1.5. Cosine retrieval, semantic
  dedup, and conflict detection all light up. The ~24 MB model
  downloads to `~/.cache/huggingface/` on first embed call, then
  caches.
- **Choosing the model.** Three built-ins are curated:
  `bge-small-en-v1.5` (384d, default), `bge-m3` (1024d, multilingual),
  and `jina-v2-base-code` (768d, code-tuned). Resolution precedence is
  `KIMETSU_BRAIN_EMBEDDER` env > the `[embedder]` table in
  `project.toml` > default. Inspect/switch with `kimetsu brain model
  list` / `kimetsu brain model set <id>` (or the `kimetsu_brain_model_list`
  / `kimetsu_brain_model_set` MCP tools). Switching changes the vector
  dimension, so `model set` re-embeds the corpus with the new model;
  cross-model rows fall back to FTS until reindexed, so retrieval never
  breaks mid-migration.
- **Lean**: `cargo install kimetsu-cli --no-default-features`. No
  embedder binary, no model download. Retrieval is FTS-only via the
  `α=0` effective behavior. Semantic dedup and conflict detection at
  ingest become silent no-ops. The library crates
  (`kimetsu-brain`, `kimetsu-chat`) default to lean so downstream
  consumers stay slim; only the `kimetsu-cli` binary opts embeddings
  in by default.

---

## The agent brain (proactive + cost-shrinking)

The broker above describes *retrieval*. For the autonomous agent pipeline,
v1.0 layers an adaptive, task-aware recall strategy on top, so the brain is
proactive, and its token overhead grows far slower than the task does.

- **Task-kind routing.** Each task is classified once by a cheap deterministic
  keyword classifier into one of `Debug` / `Feature` / `Refactor` / `Docs` /
  `Investigation` (priority order on a tie: Debug > Investigation > Refactor >
  Docs > Feature). A task-kind weight layer composes over the per-stage weights
  (then renormalizes) to bias which *kinds* of memory get recalled: Debug leans
  on recent `failure_pattern`s, Refactor on `convention`/scope, Investigation
  on broad `fact`/`preference` recall. `Feature` is the neutral default: it
  leaves the stage weights untouched.
- **Proactive "Known pitfalls".** Before the first implementation attempt, a
  tight `failure_pattern`/`convention` retrieval surfaces known pitfalls,
  proactively, not only after a failure. It costs ~zero tokens when nothing
  matches, and a per-run recall ledger stops it re-surfacing the same pitfall
  on retries.
- **Cross-stage capsule dedup.** A capsule rendered in an earlier stage is
  back-referenced (not re-rendered) in later stages and counted once via the
  run's recall ledger, so brain overhead *shrinks* in relative terms as a
  task spans more stages.
- **Lazy capsule expansion.** Top-confidence capsules are injected in full; the
  long tail is injected as ~1-line headlines that the agent expands on demand
  via a new **`expand_capsule`** tool (it resolves `memory:` / `file:`
  handles). The agent only pays for the detail it actually opens.
- **Adaptive budget.** The flat 6000-tokens-per-stage budget is replaced by one
  that scales *sublinearly* with task size (`floor + k·√task_size`), floored by
  `budget_floor_tokens` so small tasks aren't starved and capped per-run by
  `budget_run_cap_tokens` via the ledger. Doubling task size grows the budget
  by only ~41%; a 5× task by ~124%. (When the size signal is unavailable the
  broker falls back to the flat `default_budget_tokens`.)

---
