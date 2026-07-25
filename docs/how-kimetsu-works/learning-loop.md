Citations, decay, dedup, conflict detection, and the analytics that prove the brain is helping.

## Citations + blame

The strongest signal is which memories the model actually used. The flow:

1. The broker injects N capsules (recorded as a `context.injected` event).
2. Mid-run, the model cites a memory it leaned on: **`cite_memory`** inside the
   autonomous agent pipeline (`kimetsu run`), or the **`kimetsu_brain_cite`**
   MCP tool from a host agent. Both land in the same place.
3. At run end, one `memory.cited` event per citation lands in
   `memory_citations`.
4. On `run.finished`, usefulness updates: **cited memories** get the strong
   delta (+1.0, and `last_useful_at` bumps); **silent passengers** get a weak
   one (±0.1). On `run.failed`, the cited penalty is **scaled by the memory's
   citation history** (`-1.0 / (1 + prior citations / 3)`), so a proven memory
   absorbs an unlucky flaky run while an unproven one takes the full hit. A
   `run.failed` with `category = "Gate"` (the plan-stage existence guard) is
   no signal at all.

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

### Trust and audit

A poisoned memory is not a prompt injection. Prompt injection is session-scoped
and resets; a memory written into the brain persists and influences every
future session until someone notices. MINJA reports >95% injection success
against memory-backed agents through ordinary, unprivileged interaction.

Kimetsu's exposure is narrower than a hosted service's — the brain is a local
file — but it grows with exactly the features that make the product good:
`brain import` from a URL, `brain sync`, and Remote's shared org brain. So
retrieval discounts a memory by where it came from:

| origin | multiplier (uncorroborated) |
|--------|------------------------------|
| local, derived | 1.00 |
| distilled (model-written) | 0.95 |
| remote / synced | 0.90 |
| pack (imported) | 0.85 |

**Corroboration erases the discount entirely.** Once a memory has been cited in
a successful run on this machine — which is exactly what `last_useful_at`
records — it has been tested here, and where it was written stops being the
most informative thing about it. Otherwise an imported pack, whose whole point
is to share knowledge, would stay second-class forever.

Trust is a weight, never a gate: a bad import must not be able to silently
delete your working knowledge. Memories written before provenance existed read
as local, so upgrading does not make an existing brain untrusted overnight.

**Quarantine is the gate the weight is not.** Discounting a poisoned pack ranks
it lower; it still influences sessions. So `kimetsu brain import` holds an
imported pack in the review queue — the same one `brain memory proposals`
already drives — and nothing in it can reach a session until you accept it. On
by default for `http(s)://` sources, since content authored elsewhere and
fetched over the network is the widest attack surface here; off for a local file
or stdin, which you chose and can open. `--quarantine` / `--no-quarantine`
override either way, and `--mode replace` is refused alongside quarantine —
superseding memories you have in favour of content you have not read is the
worst of both. Entries matching a memory you already hold are deduped rather
than proposed: a review queue nobody can face is not a safety mechanism.

Nothing reaches back. A pack imported before quarantine existed stays where it
is, discounted by origin — pulling working memories out of retrieval on an
upgrade is a worse failure than the one quarantine prevents.

`kimetsu brain audit` groups the active corpus by origin, shows how much of it
has never been corroborated, and flags write bursts — clusters that arrived
faster than anyone types, which is the shape both a bulk import and induced
poisoning leave. Read-only on purpose: an automated purge keyed on that
heuristic would delete a legitimate import.

---

### Self-tuning

`kimetsu brain tune` sweeps the retrieval floors against eval cases built from
your own query history (`context.served` + citations), with a deterministic
20% holdout guarding against regressions. `--apply` writes only the floor
parameters; `--revert` restores. Dry-run by default.
