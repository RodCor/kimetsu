Citations, decay, dedup, conflict detection, and the analytics that prove the brain is helping.

## Citations + blame

The strongest signal is which memories the model actually used. The flow:

1. The broker injects N capsules (recorded as a `context.injected` event).
2. Mid-run, the model calls **`cite_memory`** when it leans on a memory.
3. At run end, one `memory.cited` event per citation lands in
   `memory_citations`.
4. On `run.finished`, usefulness updates: **cited memories** get the strong
   delta (±1.0, and `last_useful_at` bumps on success); **silent passengers**
   get a weak one (±0.1). A `run.failed` with `category = "Gate"` is no
   signal: the verifier failing is not the memory's fault.

Inspect any run with `kimetsu brain memory blame <run-id>` (cited first, then
passengers; `--json` for CI). The same surface is the
`kimetsu_brain_memory_blame` MCP tool.

---

## Decay

A memory useful six months ago should not outrank one useful yesterday. The
broker applies a half-life curve (default 30 days) to `last_useful_at`:

```toml
[broker.weights]
decay_half_life_days = 14   # faster-moving repo; 0 disables
```

Decay attenuates the deviation from neutral, not the score itself: a
max-boosted memory decays toward neutral, never below a brand-new one.

---

## Semantic dedup + conflict detection at ingest

`kimetsu_brain_record` runs semantic dedup before writing:

1. **Exact dup** (same normalized text, scope, kind): bump the existing
   memory, write nothing.
2. **Near dup** (cosine >= 0.85 in scope): merge into the existing memory
   rather than creating a near-twin.
3. **High confidence** (>= 0.7) with no near-dup: accepted directly.
4. **Lower confidence**: filed as a proposal for review.

Separately, each write scans for contradictions: memories in scope whose
embedding is close (cosine >= 0.8) but whose text differs. Hits land in
`memory_conflicts`; the write is never blocked, because a blocked write loses
user intent. Review from the CLI:

```bash
kimetsu brain memory conflicts                       # list open conflicts
kimetsu brain memory conflicts --resolve <id> kept_new | kept_existing | kept_both
```

Resolution is CLI-only so the audit trail stays centralized; the MCP conflicts
tool is read-only. Both dedup and conflict detection need the embeddings
build.

---

## Analytics: is the brain actually helping?

`kimetsu brain insights` (CLI and MCP) computes proof-of-value metrics over a
recent-runs window (`--last-n-runs`, `--since`, `--json`): retrieval hit-rate
and skip-rate, citation rate, proposal acceptance, usefulness trend, harvest
yield, corpus health (active vs invalidated, prune candidates, open
conflicts), and token economy. The numbers are real counts, backed by
`context.served` (every retrieval, hit or miss) and the injected-token count
on `context.injected`.

### ROI ledger

`kimetsu brain roi [--window 7d|30d|all]` estimates savings: conservative
per-kind token credits for each citation, minus injected-token overhead, with
a net verdict. Dollar values appear when the model is in the price table or
`[model] price_per_mtok` is set. Honest negatives are shown as-is.
Methodology: [Kimetsu Algorithm](../ROI-METHODOLOGY.md).

### Consolidation and triage

`kimetsu brain consolidate` merges near-duplicates (cosine >= 0.92); the
survivor keeps its id, members get `superseded_by`, citations reassign.
`--distill` feeds looser clusters to the distiller as proposals; `--dry-run`
prints the plan. `kimetsu brain triage` walks low-usefulness, long-unused
memories interactively (`--prune-all --yes` for batch).

### Self-tuning

`kimetsu brain tune` sweeps the retrieval floors against eval cases built from
your own query history (`context.served` + citations), with a deterministic
20% holdout guarding against regressions. `--apply` writes only the floor
parameters; `--revert` restores. Dry-run by default.
