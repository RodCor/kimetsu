# Tier-3: HNSW retrieval via usearch — design

**Status:** approved (design), pre-implementation
**Date:** 2026-06-06
**Branch:** `release/v1.0.0` (working material — no PR, no tag)
**Supersedes:** the brute-force `sqlite-vec` (`vec0`) KNN introduced in v1.0 D1a / Tier-1.

## Problem

Stress testing at scale (commit `bd72675`, ext4, bge-small-en-v1.5) shows two
ceilings that block the 1M-memory goal, both rooted in the same cause —
**brute-force O(N) exact KNN over the `vec0` virtual table**:

| metric @ 50k | value | shape |
|---|---|---|
| ctx warm p50/p99 | 1251 / 1304 ms | **linear in N** → ~15–25 s at 1M |
| add p99 | 729 ms | climbing — conflict detection queries `vec0` ANN, also O(N) |

Tier-2 (batch embedding) lifted seed throughput 27→166 rows/s and is unrelated
to this. The remaining ceiling is the per-query vector scan, hit on **both**:

- **reads** — `context.rs::memory_ann_candidates` (`embedding MATCH … ORDER BY distance LIMIT k`)
- **writes** — `conflict.rs::find_potential_conflicts` (same `vec0` MATCH for the candidate pool)

The fix is an approximate-nearest-neighbor (HNSW) index, turning candidate
generation from O(N) into ~O(log N).

## Decisions (settled in brainstorming)

1. **Library = `usearch`** (native C++ via Rust bindings). Mature HNSW, high
   recall, incremental `add` **and** `remove`, `save`/`load`/`view`, u64 keys.
   It lives **only** under the existing `embeddings` feature, which is *already*
   native (`fastembed`→ONNX `ort`, `sqlite-vec`→bundled C). The lean/default
   build stays 100% pure-Rust, FTS-only, unchanged.
2. **Persistence = derived rebuildable sidecar.** SQLite `memories.embedding`
   BLOBs remain the source of truth. The index is a cache file
   `.kimetsu/brain.usearch` + a small manifest. On open: load + reconcile the
   delta, or rebuild from SQLite when missing/stale/corrupt.
3. **Invalidation = active-only index, remove-on-invalidate.** `remove(rowid)`
   on every invalidation, so `search()` returns live rows directly; periodic
   rebuild compacts the graph.
4. **`sqlite-vec` is dropped** once usearch lands — its only role (`vec0`) is
   fully superseded. Removed in an **isolated final commit** so it is a single
   `git revert` away if an exact brute-force fallback is ever wanted again.
5. **All three phases land**, as **separate commits** (T3a/T3b/T3c) for
   independent revertibility. Git history is the rollback mechanism.

## Architecture

New feature-gated module **`crates/kimetsu-brain/src/ann.rs`**
(`#[cfg(feature = "embeddings")]`). Public surface:

```rust
pub struct AnnIndex { /* usearch::Index, sidecar PathBuf, dim, model_id, manifest */ }

impl AnnIndex {
    /// Load the sidecar (validating the manifest) or rebuild from SQLite,
    /// then reconcile the SQLite→index delta.
    fn open_or_build(conn: &Connection, root: &Path, dim: usize, model_id: &str) -> KimetsuResult<Self>;
    fn reconcile(&mut self, conn: &Connection) -> KimetsuResult<()>;
    fn add(&self, rowid: u64, vector: &[f32]) -> KimetsuResult<()>;
    fn remove(&self, rowid: u64) -> KimetsuResult<()>;
    fn search(&self, query: &[f32], k: usize) -> KimetsuResult<Vec<(u64, f32)>>; // (rowid, distance)
    fn save(&self) -> KimetsuResult<()>;
}
```

- **Key = SQLite `rowid`** (u64). `memories` is `TEXT PRIMARY KEY` (not
  `WITHOUT ROWID`), so the implicit integer rowid is stable and usable.
  `search()` returns rowids; a single SQL hydration maps `rowid → memory_id`
  and applies the residual `embedding_model` check — reusing the candidate
  hydration the retrieval path already performs.
- **Metric:** BGE embeddings are L2-normalized, so cosine ≡ inner-product
  ranking. Use usearch cosine (or IP) — matches today's vec0 semantics.
- **Lifecycle = process-global registry**, mirroring the existing embedder
  `OnceLock`: `OnceLock<Mutex<HashMap<CanonicalRoot, Arc<RwLock<AnnIndex>>>>>`.
  `search` takes the read lock (usearch supports concurrent search); `add` /
  `remove` / `reconcile` take the write lock. Long-running hosts (chat REPL,
  `kimetsu-remote`) keep the index warm; one-shot CLI processes load the
  sidecar (fast) and reconcile the small delta before serving.

## Data flow

### Manifest (sidecar metadata)
A tiny JSON/struct stored next to `brain.usearch`:
`{ dim, model_id, schema_version, max_rowid_indexed, count }`.

### Open
1. If `brain.usearch` is absent, or manifest `dim`/`model_id`/`schema_version`
   mismatches the active embedder, or load fails → **rebuild**:
   `SELECT rowid, embedding FROM memories WHERE invalidated_at IS NULL AND embedding IS NOT NULL AND embedding_model = ?1`,
   `reserve(count)` then `add` each. Multi-threaded; minutes at 1M.
2. Else **load** + **reconcile the delta**:
   - add active rows with `rowid > manifest.max_rowid_indexed`;
   - `remove()` rowids now invalidated (`SELECT rowid FROM memories WHERE invalidated_at IS NOT NULL`; `remove` is a no-op if absent).
   The delta scan rides the Tier-1 covering index
   `idx_memories_scope_model_active`.

### Write (warm)
- `embeddings::embed_and_persist` — after inserting the embedding BLOB,
  `ann.add(rowid, &vector)` (replaces the Tier-1 incremental `vec0` upsert).
  `add` is **upsert-safe**: for an existing key it removes-then-adds, so an
  in-place re-embed (merge/edit via `propose_or_merge_memory`) refreshes the
  vector rather than leaving a stale or duplicate entry.
- **Invalidation choke point:** a single helper `ann_remove_for(conn, memory_id)`
  called from every invalidation site — `project.rs::invalidate_memory`,
  conflict-resolve (`conflict.rs:~592/602`), `projector.rs:~656`. It resolves
  `memory_id → rowid` and `ann.remove(rowid)`.
- `save()` periodically and on graceful shutdown (chat/remote already have
  shutdown hooks; CLI one-shots persist via the next reconcile, see below).

### Write (one-shot CLI)
A one-shot `kimetsu brain memory add` writes SQLite and may skip touching the
sidecar; the next process that opens the index reconciles the delta. Correctness
is preserved because SQLite is authoritative; worst case is a single slightly
stale retrieval that self-heals on reconcile.

### Read
- `context.rs::memory_ann_candidates` → `ann.search(query, k')` → hydrate rows →
  **existing** exact cosine blend + recency scoring (unchanged).
- `conflict.rs::find_potential_conflicts` → `ann.search(query, pool)` →
  **existing** exact cosine-threshold filter (unchanged).
- ANN is candidate-generation only; the exact rerank stays, so final ordering is
  exact over the returned candidates.

## Filtering & recall

- The index is **active-only**, so `search()` never returns invalidated rows —
  the only residual post-filter is `embedding_model`, which matters solely
  mid-reindex and is applied during SQL hydration.
- Project vs user/global memories already live in **separate brain.db files**
  (separate indexes), so cross-scope filtering is structural; within one index
  the population is scope-homogeneous.
- **Recall:** usearch with a tuned `ef_search` yields ~99% recall@K. A missed
  near-duplicate in conflict detection degrades to today's behavior (an
  unflagged duplicate) — never data loss. `ef_construction`/`ef_search` and `M`
  are set with sane defaults (e.g. `M=16`, `ef_construction=128`,
  `ef_search≈64`), tunable, and documented.

## Migration & dependency change

- Stop creating/maintaining `vec0` (`memory_vec`); replace its upsert with
  `ann.add` and its `MATCH` queries with `ann.search`.
- Existing brains: first open finds no sidecar → one rebuild from the BLOBs.
- Add `usearch` (optional, `embeddings` feature) to `kimetsu-brain/Cargo.toml`;
  **remove `sqlite-vec`** in the isolated T3c commit; a migration runs
  `DROP TABLE IF EXISTS memory_vec`.
- Guardrail: `cargo tree -p kimetsu-cli -i usearch` must be **empty** (native
  ANN dep must not reach the lean CLI), mirroring the existing axum guard.

## Phasing

Each phase is a self-contained, separately-revertible commit on
`release/v1.0.0`.

- **T3a — `ann.rs` in isolation.** usearch wrapper + process registry +
  manifest/stamp + `open_or_build` / `reconcile` / `add` / `remove` / `search`
  / `save`. Add `usearch` dep. Unit-tested; not yet wired into retrieval.
- **T3b — wire retrieval + conflict + write/invalidate.** Replace the `vec0`
  read paths with `ann.search`; replace the incremental `vec0` upsert with
  `ann.add`; add the invalidation choke point `ann.remove`. Remove vec0
  *maintenance* (table may remain until T3c). Integration tests.
- **T3c — lifecycle polish + cleanup.** reindex interaction (model change →
  rebuild), periodic/shutdown `save`, drop `sqlite-vec`, `DROP TABLE memory_vec`,
  bench re-measure.

## Testing

**Unit (`ann.rs`):**
- add/search/remove round-trip; `search` returns the inserted nearest.
- persistence round-trip: `save` → reopen → `search` identical.
- `open_or_build` rebuild path matches a from-scratch build.
- `reconcile` adds `rowid > max_rowid_indexed` deltas and removes invalidated.
- manifest mismatch (dim/model/schema) forces rebuild.
- **recall guard:** 5–10k random vectors, assert ANN top-10 vs brute-force
  (plain-Rust cosine) top-10 recall ≥ 0.95.

**Integration (`kimetsu-brain`):**
- retrieval parity: a seeded brain returns the expected memory in top-K via the
  ANN path (semantic assert behind `#[cfg(feature="embeddings")]`).
- conflict detection still flags a near-duplicate add.
- invalidate removes a memory from subsequent retrieval immediately (warm) and
  after reopen (reconcile).

**Gate per phase (non-negotiable):**
- `cargo fmt --all --check`
- `cargo clippy --workspace --all-targets -- -D warnings` (lean)
- `cargo clippy --workspace --all-targets --features kimetsu-cli/embeddings -- -D warnings`
- failure audit empty:
  `KIMETSU_USER_BRAIN=0 cargo test --workspace 2>&1 | grep -E "FAILED|[1-9][0-9]* failed|panicked"`
- `cargo tree -p kimetsu-cli -i usearch` empty.

**Bench (after T3c):** re-run `kstress` emb on ext4; expect ctx warm p99 and
add p99 to go **flat (not linear)** across 100→50k, and seed/read unaffected.

## Risks

- **usearch build on Windows MSVC.** Mitigated: the embeddings build already
  compiles native C/C++ (`ort`, `sqlite-vec`, `onig`); usearch joins that set.
  Verify the `usearch` crate builds on the target before T3b wiring.
- **Recall cliff for conflict detection.** Mitigated by exact rerank on the pool
  + tuned `ef_search` + the recall-guard test; worst case = today's behavior.
- **Sidecar/SQLite divergence across processes.** Mitigated by reconcile-on-open
  + SQLite-authoritative rebuild; per-repo write serialization (`ProjectLock`)
  bounds concurrent mutation.
- **Whole-index `save` cost at 1M.** Avoided on the hot path: warm hosts save
  periodically/on shutdown; one-shot writers rely on reconcile, not per-add save.
- **Graph degradation after many removes.** Mitigated by periodic rebuild
  (compaction) — manifest count vs active count can trigger it.
- **Cross-process in-place re-embed staleness.** A merge/edit that re-embeds an
  existing rowid in another process is not caught by the `rowid > max` delta
  (the warm writer upserts correctly; reindex forces a full rebuild). This rare
  case self-heals on the next periodic rebuild. Acceptable: retrieval is
  best-effort and SQLite stays authoritative.

## Critical files

- `crates/kimetsu-brain/src/ann.rs` — new module (the index + registry).
- `crates/kimetsu-brain/src/context.rs` — `memory_ann_candidates` → `ann.search`.
- `crates/kimetsu-brain/src/conflict.rs` — `find_potential_conflicts` → `ann.search`.
- `crates/kimetsu-brain/src/embeddings.rs` — `embed_and_persist` → `ann.add`.
- `crates/kimetsu-brain/src/project.rs`, `projector.rs` — invalidation → `ann.remove`.
- `crates/kimetsu-brain/src/schema.rs` / `migrate.rs` — `DROP TABLE memory_vec`.
- `crates/kimetsu-brain/Cargo.toml` — `+usearch`, `−sqlite-vec` (T3c).
