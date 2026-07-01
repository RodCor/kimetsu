## Citations + blame

The model's biggest signal is *which memories it actually used*.
When a memory shows up in context but the model never reaches for
it, that's "silent passenger" data. When the model explicitly cites
a memory before solving the problem, that's strong evidence the
memory helped.

The flow:

1. The broker injects N capsules into the prompt (recorded as a
   `context.injected` event).
2. The model, mid-run, calls the **`cite_memory`** tool when it
   leans on a specific memory. Multiple cites per turn are fine.
3. At run end, the transport (chat REPL or harbor binary) emits one
   `memory.cited` event per citation. The projector mirrors each
   into `memory_citations`.
4. On `run.finished`, the projector applies the usefulness deltas:
   - **Cited memories**: ±1.0 (strong signal). Also bumps
     `last_useful_at` to "now" on success.
   - **Silent passengers**: ±0.1 (weak signal). Outcome-correlated,
     but small.
   - `run.failed` with `category = "Gate"` is treated as no signal:
     the agent's verifier failing isn't the memory's fault.

The split keeps the strong signal aimed at memories that actually
contributed, while still giving silent passengers a small +/- so the
broker can prune long-untouched noise.

### Inspecting a run

```bash
kimetsu brain memory blame <run-id>
```

Prints cited memories first (with rationale + turn) and silent
passengers second. `--json` for CI / hooks. The same surface is
available as the `kimetsu_brain_memory_blame` MCP tool so a host
agent can introspect its own runs.

---

## Decay

A memory that was useful 6 months ago shouldn't outrank one that
was useful yesterday. The `last_useful_at` column (bumped only on
cited + run.finished) is the reference timestamp; the broker
applies a half-life curve to attenuate stale usefulness boosts.

- Default half-life: **30 days**.
- Configurable per-project:
  ```toml
  [broker.weights]
  decay_half_life_days = 14   # faster-moving repo
  # or
  decay_half_life_days = 0    # disable decay entirely
  ```
- Decay attenuates the *deviation from neutral*, not the
  multiplier itself: a max-boosted +1.5 memory decays toward 1.0
  (neutral, like a brand-new memory), not toward 0. Losing confidence
  in old signal shouldn't penalize a memory below one with zero
  history.

---

## Semantic dedup + conflict detection at ingest

When a new capsule arrives via `kimetsu_brain_record` (the capture
tool host agents call after solving something), kimetsu first runs
**semantic dedup** through `propose_or_merge_memory`:

1. **Exact dup**: identical normalized text in the same scope/kind
   short-circuits; the existing memory's use_count bumps, nothing new
   is written.
2. **Near dup**: if an existing memory in scope is within cosine
   ≥ 0.85, the new text is *merged into* it (appended as "Also: …")
   rather than creating a near-twin. This is what stops a brain from
   filling with ten rephrasings of the same lesson over a gauntlet.
3. **High confidence** (≥ 0.7) with no near-dup → accepted directly.
4. **Lower confidence** → filed as a proposal for human review.

Dedup is embedder-gated; the lean build skips steps 1-2's semantic
match and falls through to accept/propose.

Separately, when `add_memory` writes a capsule, kimetsu scans the
active brain for nearby *contradictions*: existing memories in the
same scope whose embedding is close (cosine ≥ 0.8 by default) but
whose normalized text differs.

Hits land in `memory_conflicts`. The write itself is **not blocked**:
surfacing > blocking, because a blocked write loses user intent.
Instead the operator reviews via:

```bash
kimetsu brain memory conflicts                  # list open conflicts
kimetsu brain memory conflicts --resolve <id> kept_new
kimetsu brain memory conflicts --resolve <id> kept_existing
kimetsu brain memory conflicts --resolve <id> kept_both
```

`kept_new` invalidates the existing memory. `kept_existing`
invalidates the new one. `kept_both` is the legitimate case where
both apply (e.g., "use anyhow in CLI binaries" + "use thiserror in
library crates", same shape, different scope semantically).

Conflict detection is **embedder-gated**. The lean build silently
skips it; build with `--features embeddings` to enable.

The MCP surface (`kimetsu_brain_memory_conflicts`) is read-only by
design. Resolution is CLI-only to keep the audit trail centralized:
an agent shouldn't silently "resolve" a real contradiction it
should have surfaced.

---

## Analytics: is the brain actually helping?

A brain you can't measure is a brain you can't trust. `kimetsu brain insights`
(and the `kimetsu_brain_insights` MCP tool) compute proof-of-value metrics
over a recent-runs window (default the last 50 runs), override with
`--last-n-runs N` or an ISO-8601 `--since`, and `--top N` sizes the ranked
lists. `--json` emits the full report for CI/dashboards. The metrics:

- **Retrieval hit-rate & skip-rate**: of the retrievals served, how many
  returned at least one capsule vs. hit the zero-capsule skipped path.
- **Citation rate**: what fraction of retrieved memories the model actually
  cited.
- **Proposal acceptance rate**: accepted / (accepted + rejected).
- **Usefulness trend**: summed usefulness, average usefulness ratio, and the
  net `run.finished − run.failed(non-Gate)` outcome over the window.
- **Harvest yield**: memories created in the window, broken down by source,
  and per-run yield.
- **Corpus health**: active vs. invalidated counts, breakdowns by scope/kind,
  top-useful memories, prune candidates, open conflicts, pending proposals.
- **Token economy**: average injected tokens and capsule count per retrieval.

These are backed by two event additions: a **`context.served`** event logs
*every* retrieval (hit or miss), and **`context.injected`** now carries the
injected-token count, so the hit-rate and token-economy numbers are real
counts, not estimates.

### ROI ledger

`kimetsu brain roi [--window 7d|30d|all] [--json]` estimates how much the
brain saved you. It sums conservative per-kind token credits for every cited
memory (failure_pattern=1500, command=400, convention=300, fact=500,
preference=200 tokens), subtracts injected-token overhead, and shows a
net-positive / net-negative verdict. Dollar values are shown when the active
model is in the built-in price table or when `[model] price_per_mtok` is set.
Honest negatives are displayed as-is. The Stop hook appends a per-session
savings line when ≥1 citation occurred. Methodology: `docs/ROI-METHODOLOGY.md`.

### Consolidation and triage

`kimetsu brain consolidate` merges near-duplicate memories that accumulated
different phrasings of the same lesson. Default threshold: cosine ≥ 0.92
within the same embedding model. The survivor keeps its id and text; members
get `superseded_by` set (schema v3) and a `memory.superseded` event so
`brain rebuild` reproduces the merge. Citations are reassigned to the survivor.
`--distill` adds a second pass: loose clusters (0.75-0.85 cosine, ≥3 memories,
≥1 shared tag) are fed to the configured distiller and the result lands as a
memory proposal for review. `--dry-run` prints the plan without writing.

`kimetsu brain triage` lists memories below a usefulness + age threshold
(defaults: score < 0.2 and last-useful > 30 days) and prompts keep / prune /
skip interactively. `--prune-all --yes` for non-interactive batch pruning.

### Self-tuning

`kimetsu brain tune --status` shows how many positive eval cases the brain has
accumulated (from `context.served` + citation joins). `kimetsu brain tune`
sweeps `broker.min_lexical_coverage` × `broker.min_semantic_score` against the
production embedder and picks the combo that maximises the objective on the
training split. A holdout guardrail (deterministic 20% split) prevents
writing a config that regresses holdout quality. `--apply` writes only the
floor parameters to `project.toml`; `--revert` restores the previous entry.
Dry-run by default.

---
