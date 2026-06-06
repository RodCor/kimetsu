# Tier-3 HNSW (usearch) Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace the brute-force `sqlite-vec` (`vec0`) exact KNN with a `usearch` HNSW index so context retrieval AND conflict-detection-on-write become ~O(log N), unblocking the 1M-memory goal.

**Architecture:** A new feature-gated module `crates/kimetsu-brain/src/ann.rs` wraps `usearch::Index`. The index is a derived, rebuildable cache: SQLite `memories.embedding` BLOBs stay the source of truth; a sidecar file `brain.usearch` (next to `brain.db`) plus a `.json` manifest persist the graph. The index is keyed by the brain's `rowid` (u64) and holds active rows only (`remove` on invalidate). A process-global registry caches one index per on-disk brain; in-memory test DBs rebuild transiently per query. Both call sites (retrieval, conflict) keep their existing exact cosine rerank — usearch only does candidate generation.

**Tech Stack:** Rust (edition 2024), `usearch` crate (native HNSW, optional under the `embeddings` feature), `rusqlite` (bundled SQLite, WAL), existing `embeddings::{encode_embedding, decode_embedding, cosine_similarity}`.

**Reference spec:** `docs/superpowers/specs/2026-06-06-tier3-hnsw-usearch-design.md`

**Standing rules:** branch `release/v1.0.0`; push each phase as its own commit; NO PR, NO tag. Commits end with the `Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>` trailer. Brains/memories are precious — no destructive ops outside the explicit `DROP TABLE memory_vec` migration in T3c.

---

## File Structure

- **Create** `crates/kimetsu-brain/src/ann.rs` — the usearch wrapper, manifest, registry, rebuild/reconcile. Entire file is `#[cfg(feature = "embeddings")]`.
- **Modify** `crates/kimetsu-brain/src/lib.rs` — register the module.
- **Modify** `crates/kimetsu-brain/Cargo.toml` — add `usearch` (T3a); remove `sqlite-vec` (T3c).
- **Modify** `crates/kimetsu-brain/src/context.rs` — `memory_ann_candidates` → `ann::search`; delete `ensure_vec_index` / `ensure_vec_table` / `upsert_vec_row` (T3b/T3c).
- **Modify** `crates/kimetsu-brain/src/conflict.rs` — `find_potential_conflicts_with_vec` → `ann::search` (T3b).
- **Modify** `crates/kimetsu-brain/src/embeddings.rs` — `embed_and_persist` → `ann::on_upsert` instead of vec0 upsert (T3b).
- **Modify** `crates/kimetsu-brain/src/projector.rs` — `apply_memory_invalidated` → `ann::on_invalidate` (T3b).
- **Modify** `crates/kimetsu-brain/src/conflict.rs` (resolve path) + any direct `SET invalidated_at` → `ann::on_invalidate` (T3b).
- **Modify** `crates/kimetsu-brain/src/schema.rs` — drop `ensure_vec_extension_registered` + vec0 test; add `DROP TABLE IF EXISTS memory_vec` (T3c).
- **Modify** `crates/kimetsu-brain/src/reindex.rs` — model-change invalidates the sidecar (T3c).

---

## Shared gate (run after EVERY phase before committing)

```bash
cd /e/Kimetsu
cargo fmt --all
cargo clippy --workspace --all-targets -- -D warnings
cargo clippy --workspace --all-targets --features kimetsu-cli/embeddings -- -D warnings
KIMETSU_USER_BRAIN=0 cargo test --workspace 2>&1 | grep -E "FAILED|[1-9][0-9]* failed|panicked"   # MUST be empty
KIMETSU_USER_BRAIN=0 cargo test -p kimetsu-brain --features embeddings 2>&1 | grep -E "FAILED|[1-9][0-9]* failed|panicked"  # MUST be empty
cargo tree -p kimetsu-cli -i usearch    # MUST print nothing (native dep must not reach lean CLI)
```
"counting `test result: ok` lines is NOT sufficient — the grep must be empty."

---

# PHASE T3a — `ann.rs` in isolation

Builds the index module with full unit tests. Nothing else in the crate calls it yet (it compiles as dead-but-tested code; suppress the unused warnings with `#[allow(dead_code)]` on the not-yet-wired public fns, removed in T3b).

### Task 1: Add the `usearch` dependency

**Files:**
- Modify: `crates/kimetsu-brain/Cargo.toml`

- [ ] **Step 1: Add usearch under the embeddings feature**

In `[features]`, change:
```toml
embeddings = ["dep:fastembed", "dep:sqlite-vec"]
```
to:
```toml
embeddings = ["dep:fastembed", "dep:sqlite-vec", "dep:usearch"]
```

In `[dependencies]`, after the `sqlite-vec` block, add:
```toml
# v1.0 Tier-3: usearch provides an HNSW approximate-NN index so retrieval and
# conflict-detection candidate generation are ~O(log N) instead of the vec0
# brute-force O(N) scan. Native (C++); only pulled under `embeddings`, which is
# already native (ort, sqlite-vec). The lean build never links it.
usearch = { version = "2", optional = true }
```

- [ ] **Step 2: Verify it builds (downloads + compiles the native lib)**

Run: `cargo build -p kimetsu-brain --features embeddings`
Expected: compiles clean (first build compiles the usearch C++ — may take a minute).

- [ ] **Step 3: Verify the lean build does NOT pull usearch**

Run: `cargo tree -p kimetsu-cli -i usearch`
Expected: prints nothing (empty).

- [ ] **Step 4: Commit**

```bash
git add crates/kimetsu-brain/Cargo.toml Cargo.lock
git commit -m "build(brain): add usearch dep under the embeddings feature

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 2: Create `ann.rs` with the manifest + IndexOptions skeleton

**Files:**
- Create: `crates/kimetsu-brain/src/ann.rs`
- Modify: `crates/kimetsu-brain/src/lib.rs`

- [ ] **Step 1: Register the module in lib.rs**

In `crates/kimetsu-brain/src/lib.rs`, next to the other `pub mod` lines (e.g. near `pub mod reindex;`), add:
```rust
#[cfg(feature = "embeddings")]
pub mod ann;
```

- [ ] **Step 2: Write `ann.rs` header, imports, constants, manifest, options**

Create `crates/kimetsu-brain/src/ann.rs` with:
```rust
//! Tier-3: approximate-nearest-neighbour (HNSW) index via `usearch`.
//!
//! Replaces the brute-force `vec0` KNN. The index is a *derived cache*:
//! `memories.embedding` BLOBs in SQLite are the source of truth. A sidecar
//! `brain.usearch` (next to `brain.db`) plus a `.json` manifest persist the
//! graph. Keyed by the SQLite `rowid` (u64); holds ACTIVE rows only
//! (`remove` on invalidate). Both call sites keep their exact cosine rerank,
//! so usearch only generates candidates.
//!
//! Whole file is `embeddings`-feature-only — the lean build has no vectors.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, RwLock};

use rusqlite::Connection;
use serde::{Deserialize, Serialize};
use usearch::{Index, IndexOptions, MetricKind, ScalarKind};

use kimetsu_core::KimetsuResult;

/// Bump when the on-disk sidecar format or index params change in a way that
/// makes an old sidecar unsafe to load — forces a rebuild.
const SCHEMA_VERSION: u32 = 1;

/// HNSW graph degree (M). Higher = better recall, more memory.
const CONNECTIVITY: usize = 16;
/// ef_construction: candidate list at build time.
const EXPANSION_ADD: usize = 128;
/// ef_search: candidate list at query time.
const EXPANSION_SEARCH: usize = 64;

/// Sidecar manifest, stored next to `brain.usearch` as `brain.usearch.json`.
/// Validates that a loaded sidecar matches the active model/dim/schema, and
/// records how far the index has caught up to SQLite.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
struct Manifest {
    schema_version: u32,
    dim: usize,
    model_id: String,
    /// Highest `memories.rowid` already represented in the index.
    max_rowid_indexed: i64,
    /// Number of active vectors in the index (sanity check vs SQLite).
    count: usize,
}

fn index_options(dim: usize) -> IndexOptions {
    IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ScalarKind::F32,
        connectivity: CONNECTIVITY,
        expansion_add: EXPANSION_ADD,
        expansion_search: EXPANSION_SEARCH,
        multi: false,
    }
}
```

- [ ] **Step 3: Verify it compiles**

Run: `cargo build -p kimetsu-brain --features embeddings`
Expected: compiles (warnings about unused items are fine for now).

- [ ] **Step 4: Commit**

```bash
git add crates/kimetsu-brain/src/ann.rs crates/kimetsu-brain/src/lib.rs
git commit -m "feat(brain): ann.rs scaffold — manifest + usearch IndexOptions

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 3: `AnnIndex` struct + `build_from_conn` (full rebuild)

**Files:**
- Modify: `crates/kimetsu-brain/src/ann.rs`

- [ ] **Step 1: Write the failing test**

Append to `ann.rs`:
```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::encode_embedding;

    /// In-memory brain with `n` rows; vector i = a unit-ish vector pointing
    /// mostly along axis (i % dim). Deterministic, no embedder needed.
    fn seed_conn(n: usize, dim: usize, model: &str) -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("init");
        for i in 0..n {
            let mut v = vec![0.01f32; dim];
            v[i % dim] = 1.0;
            conn.execute(
                "INSERT INTO memories
                   (memory_id, scope, kind, text, normalized_text, confidence,
                    provenance_snapshot_json, created_at, use_count, usefulness_score,
                    embedding, embedding_model)
                 VALUES (?1,'project','fact',?2,?2,1.0,'{}','2026-01-01T00:00:00Z',0,0.0,?3,?4)",
                rusqlite::params![
                    format!("m-{i:06}"),
                    format!("text {i}"),
                    encode_embedding(&v),
                    model
                ],
            )
            .expect("insert");
        }
        conn
    }

    #[test]
    fn build_from_conn_indexes_all_active_rows() {
        let dim = 8;
        let conn = seed_conn(50, dim, "stub-d8");
        let idx = AnnIndex::build_from_conn(&conn, dim, "stub-d8").expect("build");
        assert_eq!(idx.len(), 50, "all 50 active rows indexed");
    }
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::build_from_conn_indexes_all_active_rows`
Expected: FAIL — `AnnIndex` / `build_from_conn` not found.

- [ ] **Step 3: Implement `AnnIndex` + `build_from_conn`**

Add to `ann.rs` (before the tests module):
```rust
/// The in-process index plus the metadata needed to persist + reconcile it.
pub struct AnnIndex {
    index: Index,
    dim: usize,
    model_id: String,
    /// `None` for in-memory / pathless DBs (no sidecar).
    sidecar: Option<PathBuf>,
    max_rowid_indexed: i64,
}

impl AnnIndex {
    /// Number of vectors currently in the index.
    pub fn len(&self) -> usize {
        self.index.size()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Build a fresh index from every active, current-model embedding in SQLite.
    pub fn build_from_conn(conn: &Connection, dim: usize, model_id: &str) -> KimetsuResult<Self> {
        let index =
            Index::new(&index_options(dim)).map_err(|e| format!("usearch new: {e}"))?;
        let mut me = Self {
            index,
            dim,
            model_id: model_id.to_string(),
            sidecar: None,
            max_rowid_indexed: 0,
        };
        me.reserve_and_load_active(conn)?;
        Ok(me)
    }

    /// Reserve capacity then add every active current-model row to the index,
    /// tracking the highest rowid seen.
    fn reserve_and_load_active(&mut self, conn: &Connection) -> KimetsuResult<()> {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories
             WHERE invalidated_at IS NULL AND embedding IS NOT NULL AND embedding_model = ?1",
            rusqlite::params![self.model_id],
            |r| r.get(0),
        )?;
        if count > 0 {
            self.index
                .reserve(count as usize)
                .map_err(|e| format!("usearch reserve: {e}"))?;
        }
        let mut stmt = conn.prepare(
            "SELECT rowid, embedding FROM memories
             WHERE invalidated_at IS NULL AND embedding IS NOT NULL AND embedding_model = ?1",
        )?;
        let rows = stmt.query_map(rusqlite::params![self.model_id], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;
        for row in rows {
            let (rowid, blob) = row?;
            if blob.len() != self.dim * 4 {
                continue; // skip malformed
            }
            let vec = crate::embeddings::decode_embedding(&blob, Some(self.dim))?;
            self.index
                .add(rowid as u64, &vec)
                .map_err(|e| format!("usearch add: {e}"))?;
            if rowid > self.max_rowid_indexed {
                self.max_rowid_indexed = rowid;
            }
        }
        Ok(())
    }
}
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::build_from_conn_indexes_all_active_rows`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-brain/src/ann.rs
git commit -m "feat(brain): AnnIndex::build_from_conn (full rebuild from SQLite)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 4: `search` returns nearest rowids

**Files:**
- Modify: `crates/kimetsu-brain/src/ann.rs`

- [ ] **Step 1: Write the failing test**

In `ann.rs` tests module, add:
```rust
#[test]
fn search_returns_nearest_rowid_first() {
    let dim = 8;
    let conn = seed_conn(dim, dim, "stub-d8"); // one row per axis
    let idx = AnnIndex::build_from_conn(&conn, dim, "stub-d8").expect("build");
    // Query strongly along axis 3 → row whose rowid maps to memory m-000003.
    let mut q = vec![0.0f32; dim];
    q[3] = 1.0;
    let hits = idx.search(&q, 3).expect("search");
    assert!(!hits.is_empty(), "got candidates");
    // The nearest must be the row with embedding peaked on axis 3.
    let (rowid, _dist) = hits[0];
    let mid: String = conn
        .query_row(
            "SELECT memory_id FROM memories WHERE rowid = ?1",
            rusqlite::params![rowid as i64],
            |r| r.get(0),
        )
        .expect("map rowid");
    assert_eq!(mid, "m-000003");
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::search_returns_nearest_rowid_first`
Expected: FAIL — `search` not found.

- [ ] **Step 3: Implement `search`**

Add to `impl AnnIndex`:
```rust
    /// Return up to `k` nearest `(rowid, distance)` pairs for `query`.
    /// Distance is usearch's metric distance (cosine: smaller = closer).
    pub fn search(&self, query: &[f32], k: usize) -> KimetsuResult<Vec<(i64, f32)>> {
        if k == 0 || self.is_empty() {
            return Ok(Vec::new());
        }
        let matches = self
            .index
            .search(query, k)
            .map_err(|e| format!("usearch search: {e}"))?;
        Ok(matches
            .keys
            .into_iter()
            .zip(matches.distances)
            .map(|(key, dist)| (key as i64, dist))
            .collect())
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::search_returns_nearest_rowid_first`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-brain/src/ann.rs
git commit -m "feat(brain): AnnIndex::search → nearest rowids

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 5: `add` (upsert-safe) and `remove`

**Files:**
- Modify: `crates/kimetsu-brain/src/ann.rs`

- [ ] **Step 1: Write the failing test**

In tests:
```rust
#[test]
fn add_is_upsert_and_remove_drops() {
    let dim = 8;
    let conn = seed_conn(4, dim, "stub-d8");
    let mut idx = AnnIndex::build_from_conn(&conn, dim, "stub-d8").expect("build");
    assert_eq!(idx.len(), 4);

    // Upsert an existing rowid with a new vector — size unchanged.
    let mut v = vec![0.0f32; dim];
    v[0] = 1.0;
    idx.add(1, &v).expect("upsert");
    assert_eq!(idx.len(), 4, "upsert must not grow the index");

    // Add a brand-new rowid — size grows.
    idx.add(999, &v).expect("add new");
    assert_eq!(idx.len(), 5);

    // Remove it — size shrinks and it stops appearing.
    idx.remove(999).expect("remove");
    assert_eq!(idx.len(), 4);
}
```

- [ ] **Step 2: Run it to verify it fails**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::add_is_upsert_and_remove_drops`
Expected: FAIL — `add` / `remove` not found.

- [ ] **Step 3: Implement `add` (upsert) + `remove`**

Add to `impl AnnIndex`:
```rust
    /// Insert or replace the vector for `rowid`. usearch would otherwise keep a
    /// duplicate for an existing key (multi=false still appends a new slot), so
    /// remove-then-add guarantees a single current entry (in-place re-embed).
    pub fn add(&mut self, rowid: i64, vector: &[f32]) -> KimetsuResult<()> {
        if vector.len() != self.dim {
            return Err(format!(
                "ann add: dim {} != index dim {}",
                vector.len(),
                self.dim
            )
            .into());
        }
        if self.index.contains(rowid as u64) {
            self.index
                .remove(rowid as u64)
                .map_err(|e| format!("usearch remove (upsert): {e}"))?;
        }
        // Grow capacity if we're at the ceiling.
        if self.index.size() + 1 > self.index.capacity() {
            self.index
                .reserve((self.index.capacity() + 1).max(64) * 2)
                .map_err(|e| format!("usearch reserve (grow): {e}"))?;
        }
        self.index
            .add(rowid as u64, vector)
            .map_err(|e| format!("usearch add: {e}"))?;
        if rowid > self.max_rowid_indexed {
            self.max_rowid_indexed = rowid;
        }
        Ok(())
    }

    /// Remove `rowid` if present (no-op otherwise).
    pub fn remove(&mut self, rowid: i64) -> KimetsuResult<()> {
        if self.index.contains(rowid as u64) {
            self.index
                .remove(rowid as u64)
                .map_err(|e| format!("usearch remove: {e}"))?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Run the test to verify it passes**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::add_is_upsert_and_remove_drops`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-brain/src/ann.rs
git commit -m "feat(brain): AnnIndex add (upsert) + remove

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 6: persistence — `save` + `open_or_build` with manifest validation

**Files:**
- Modify: `crates/kimetsu-brain/src/ann.rs`

- [ ] **Step 1: Write the failing test**

In tests (uses a temp dir so the sidecar has a real path):
```rust
#[test]
fn save_then_open_reuses_sidecar_and_search_matches() {
    let dim = 8;
    let dir = tempfile::tempdir().expect("tmp");
    let db = dir.path().join("brain.db");
    let conn = Connection::open(&db).expect("open file db");
    crate::schema::initialize(&conn).expect("init");
    for i in 0..20usize {
        let mut v = vec![0.01f32; dim];
        v[i % dim] = 1.0;
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                embedding, embedding_model)
             VALUES (?1,'project','fact',?2,?2,1.0,'{}','2026-01-01T00:00:00Z',0,0.0,?3,'stub-d8')",
            rusqlite::params![format!("m-{i:06}"), format!("t{i}"), crate::embeddings::encode_embedding(&v)],
        ).expect("insert");
    }
    // First open: no sidecar → build → save.
    let idx = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("build");
    idx.save().expect("save");
    assert!(db.with_extension("usearch").exists(), "sidecar written");

    // Second open: sidecar present + manifest valid → load.
    let idx2 = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("load");
    assert_eq!(idx2.len(), 20);
    let mut q = vec![0.0f32; dim];
    q[2] = 1.0;
    assert!(!idx2.search(&q, 5).expect("search").is_empty());
}

#[test]
fn manifest_model_mismatch_forces_rebuild() {
    let dim = 8;
    let dir = tempfile::tempdir().expect("tmp");
    let db = dir.path().join("brain.db");
    let conn = Connection::open(&db).expect("open");
    crate::schema::initialize(&conn).expect("init");
    // Build + save under model A.
    AnnIndex::open_or_build(&conn, dim, "model-a").expect("a").save().expect("save");
    // Open under model B → manifest mismatch → rebuild (empty, no model-b rows).
    let idx = AnnIndex::open_or_build(&conn, dim, "model-b").expect("b");
    assert_eq!(idx.len(), 0, "rebuilt for model-b which has no rows");
}
```

- [ ] **Step 2: Run to verify they fail**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::save_then_open ann::tests::manifest_model`
Expected: FAIL — `open_or_build` / `save` not found.

- [ ] **Step 3: Implement sidecar paths, `save`, `open_or_build`**

Add to `impl AnnIndex`:
```rust
    /// Derive the sidecar index path from a brain.db path: sibling
    /// `<stem>.usearch`. Returns `None` for in-memory / pathless DBs.
    fn sidecar_for(conn: &Connection) -> Option<PathBuf> {
        match conn.path() {
            Some(p) if !p.is_empty() && p != ":memory:" => {
                Some(Path::new(p).with_extension("usearch"))
            }
            _ => None,
        }
    }

    fn manifest_path(sidecar: &Path) -> PathBuf {
        // brain.usearch -> brain.usearch.json
        let mut s = sidecar.as_os_str().to_owned();
        s.push(".json");
        PathBuf::from(s)
    }

    fn manifest(&self) -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION,
            dim: self.dim,
            model_id: self.model_id.clone(),
            max_rowid_indexed: self.max_rowid_indexed,
            count: self.len(),
        }
    }

    /// Serialize the index + manifest to the sidecar (no-op for in-memory DBs).
    pub fn save(&self) -> KimetsuResult<()> {
        let Some(sidecar) = &self.sidecar else {
            return Ok(());
        };
        self.index
            .save(sidecar.to_string_lossy().as_ref())
            .map_err(|e| format!("usearch save: {e}"))?;
        let manifest = serde_json::to_vec(&self.manifest())
            .map_err(|e| format!("manifest serialize: {e}"))?;
        std::fs::write(Self::manifest_path(sidecar), manifest)
            .map_err(|e| format!("manifest write: {e}"))?;
        Ok(())
    }

    /// Load a valid sidecar (manifest matches dim/model/schema) then reconcile
    /// the SQLite delta; otherwise rebuild from scratch. For in-memory DBs there
    /// is no sidecar, so this always builds fresh.
    pub fn open_or_build(conn: &Connection, dim: usize, model_id: &str) -> KimetsuResult<Self> {
        let sidecar = Self::sidecar_for(conn);
        if let Some(path) = &sidecar
            && path.exists()
            && let Some(loaded) = Self::try_load(conn, path, dim, model_id)?
        {
            let mut idx = loaded;
            idx.reconcile(conn)?;
            return Ok(idx);
        }
        // Rebuild path.
        let mut idx = Self::build_from_conn(conn, dim, model_id)?;
        idx.sidecar = sidecar;
        Ok(idx)
    }

    /// Attempt to load the sidecar; returns `None` (caller rebuilds) when the
    /// manifest is missing/unreadable or mismatches dim/model/schema.
    fn try_load(
        conn: &Connection,
        sidecar: &Path,
        dim: usize,
        model_id: &str,
    ) -> KimetsuResult<Option<Self>> {
        let manifest_bytes = match std::fs::read(Self::manifest_path(sidecar)) {
            Ok(b) => b,
            Err(_) => return Ok(None),
        };
        let manifest: Manifest = match serde_json::from_slice(&manifest_bytes) {
            Ok(m) => m,
            Err(_) => return Ok(None),
        };
        if manifest.schema_version != SCHEMA_VERSION
            || manifest.dim != dim
            || manifest.model_id != model_id
        {
            return Ok(None);
        }
        let index = Index::new(&index_options(dim)).map_err(|e| format!("usearch new: {e}"))?;
        if index
            .load(sidecar.to_string_lossy().as_ref())
            .is_err()
        {
            return Ok(None); // corrupt sidecar → rebuild
        }
        let _ = conn; // conn unused here; reconcile (caller) uses it
        Ok(Some(Self {
            index,
            dim,
            model_id: model_id.to_string(),
            sidecar: Some(sidecar.to_path_buf()),
            max_rowid_indexed: manifest.max_rowid_indexed,
        }))
    }
```

> NOTE: `reconcile` is implemented in Task 7. To compile Task 6 in isolation, add a temporary stub above the tests:
> ```rust
> impl AnnIndex { fn reconcile(&mut self, _conn: &Connection) -> KimetsuResult<()> { Ok(()) } }
> ```
> Task 7 replaces this stub with the real body. (If implementing Task 6 and 7 back-to-back, skip the stub and add the real `reconcile` now.)

Also add `tempfile` as a dev-dependency if not already present — check `crates/kimetsu-brain/Cargo.toml` `[dev-dependencies]`; the crate already uses `tempfile` elsewhere in tests, so it should be there. If absent, add `tempfile = "3"` under `[dev-dependencies]`.

- [ ] **Step 4: Run to verify they pass**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::save_then_open ann::tests::manifest_model`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-brain/src/ann.rs crates/kimetsu-brain/Cargo.toml
git commit -m "feat(brain): AnnIndex sidecar persistence (save + open_or_build + manifest)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 7: `reconcile` — apply the SQLite→index delta

**Files:**
- Modify: `crates/kimetsu-brain/src/ann.rs`

- [ ] **Step 1: Write the failing test**

In tests:
```rust
#[test]
fn reconcile_adds_new_and_removes_invalidated() {
    let dim = 8;
    let dir = tempfile::tempdir().expect("tmp");
    let db = dir.path().join("brain.db");
    let conn = Connection::open(&db).expect("open");
    crate::schema::initialize(&conn).expect("init");
    let insert = |conn: &Connection, i: usize| {
        let mut v = vec![0.01f32; dim];
        v[i % dim] = 1.0;
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                embedding, embedding_model)
             VALUES (?1,'project','fact',?2,?2,1.0,'{}','2026-01-01T00:00:00Z',0,0.0,?3,'stub-d8')",
            rusqlite::params![format!("m-{i:06}"), format!("t{i}"), crate::embeddings::encode_embedding(&v)],
        ).expect("insert");
    };
    for i in 0..10 { insert(&conn, i); }
    let idx = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("build");
    idx.save().expect("save");
    assert_eq!(idx.len(), 10);

    // Simulate another process: add 5 rows, invalidate 2 existing.
    for i in 10..15 { insert(&conn, i); }
    conn.execute("UPDATE memories SET invalidated_at='2026-02-01T00:00:00Z' WHERE memory_id IN ('m-000000','m-000001')", []).expect("invalidate");

    // Reopen → load sidecar (10) → reconcile (+5 new, -2 invalidated) = 13.
    let idx2 = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("reopen");
    assert_eq!(idx2.len(), 13);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::reconcile_adds_new_and_removes_invalidated`
Expected: FAIL — count is 10 (stub reconcile is a no-op) or compile error if you skipped the stub.

- [ ] **Step 3: Implement `reconcile` (replace the Task-6 stub)**

```rust
    /// Apply the SQLite→index delta after a sidecar load:
    ///   * add active current-model rows with `rowid > max_rowid_indexed`;
    ///   * remove rows now invalidated (rowid <= max) still in the index.
    /// Cheap: rides the `idx_memories_scope_model_active` covering index.
    pub fn reconcile(&mut self, conn: &Connection) -> KimetsuResult<()> {
        // 3a. New active rows since last index.
        let new_rows: Vec<(i64, Vec<u8>)> = {
            let mut stmt = conn.prepare(
                "SELECT rowid, embedding FROM memories
                 WHERE invalidated_at IS NULL AND embedding IS NOT NULL
                   AND embedding_model = ?1 AND rowid > ?2",
            )?;
            stmt.query_map(rusqlite::params![self.model_id, self.max_rowid_indexed], |r| {
                Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))
            })?
            .filter_map(|r| r.ok())
            .collect()
        };
        for (rowid, blob) in new_rows {
            if blob.len() != self.dim * 4 {
                continue;
            }
            let vec = crate::embeddings::decode_embedding(&blob, Some(self.dim))?;
            self.add(rowid, &vec)?;
        }

        // 3b. Remove rows now invalidated (only those <= the watermark; newer
        //     ones were never added). `remove` is a no-op if absent.
        let gone: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT rowid FROM memories
                 WHERE invalidated_at IS NOT NULL AND rowid <= ?1",
            )?;
            stmt.query_map(rusqlite::params![self.max_rowid_indexed], |r| r.get::<_, i64>(0))?
                .filter_map(|r| r.ok())
                .collect()
        };
        for rowid in gone {
            self.remove(rowid)?;
        }
        Ok(())
    }
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::reconcile_adds_new_and_removes_invalidated`
Expected: PASS.

- [ ] **Step 5: Commit**

```bash
git add crates/kimetsu-brain/src/ann.rs
git commit -m "feat(brain): AnnIndex::reconcile (SQLite delta catch-up)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 8: recall guard test (ANN vs exact brute force)

**Files:**
- Modify: `crates/kimetsu-brain/src/ann.rs`

- [ ] **Step 1: Write the test**

In tests:
```rust
#[test]
fn recall_at_10_is_at_least_0_9_vs_brute_force() {
    use crate::embeddings::{cosine_similarity, decode_embedding};
    let dim = 16;
    let n = 5000usize;
    let conn = Connection::open_in_memory().expect("open");
    crate::schema::initialize(&conn).expect("init");
    // Deterministic pseudo-random unit vectors (LCG; no Math.random/Date).
    let mut state: u64 = 0x9E3779B97F4A7C15;
    let mut next = || {
        state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
        ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0
    };
    let mut vectors: Vec<(i64, Vec<f32>)> = Vec::new();
    for i in 0..n {
        let v: Vec<f32> = (0..dim).map(|_| next()).collect();
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                embedding, embedding_model)
             VALUES (?1,'project','fact',?2,?2,1.0,'{}','2026-01-01T00:00:00Z',0,0.0,?3,'stub')",
            rusqlite::params![format!("m-{i:06}"), "t", crate::embeddings::encode_embedding(&v)],
        ).expect("insert");
    }
    // Map rowid->vec for brute force.
    let mut stmt = conn.prepare("SELECT rowid, embedding FROM memories").unwrap();
    let rows = stmt.query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?))).unwrap();
    for row in rows {
        let (rowid, blob) = row.unwrap();
        vectors.push((rowid, decode_embedding(&blob, Some(dim)).unwrap()));
    }

    let idx = AnnIndex::build_from_conn(&conn, dim, "stub").expect("build");
    let trials = 50;
    let k = 10;
    let mut hit = 0usize;
    let mut total = 0usize;
    for t in 0..trials {
        let q = &vectors[t * 7 % vectors.len()].1;
        // Exact top-k by cosine.
        let mut scored: Vec<(i64, f32)> =
            vectors.iter().map(|(id, v)| (*id, cosine_similarity(q, v))).collect();
        scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
        let exact: std::collections::HashSet<i64> =
            scored.iter().take(k).map(|(id, _)| *id).collect();
        let ann: std::collections::HashSet<i64> =
            idx.search(q, k).unwrap().into_iter().map(|(id, _)| id).collect();
        hit += exact.intersection(&ann).count();
        total += k;
    }
    let recall = hit as f32 / total as f32;
    assert!(recall >= 0.9, "recall@10 = {recall} (want >= 0.9)");
}
```

- [ ] **Step 2: Run it**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::recall_at_10 -- --nocapture`
Expected: PASS (recall typically ~0.97–1.0 at these params). If it fails, raise `EXPANSION_SEARCH` to 128 and re-run.

- [ ] **Step 3: Commit**

```bash
git add crates/kimetsu-brain/src/ann.rs
git commit -m "test(brain): ANN recall@10 guard vs brute-force cosine

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 9: process registry + `for_query` / `for_write` handles

**Files:**
- Modify: `crates/kimetsu-brain/src/ann.rs`

This is the integration seam T3b uses. On-disk DBs share one cached `Arc<RwLock<AnnIndex>>`; in-memory DBs rebuild transiently (tiny test DBs).

- [ ] **Step 1: Write the failing test**

In tests:
```rust
#[test]
fn registry_caches_per_ondisk_db_and_transient_for_memory() {
    let dim = 8;
    // On-disk: two handles for the same db are the same Arc.
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("brain.db");
    let conn = Connection::open(&db).unwrap();
    crate::schema::initialize(&conn).unwrap();
    let h1 = handle_for_query(&conn, dim, "stub-d8").unwrap();
    let h2 = handle_for_query(&conn, dim, "stub-d8").unwrap();
    assert!(Arc::ptr_eq(&h1, &h2), "same db → cached handle");

    // In-memory: returns a usable (transient) handle, no panic.
    let mem = Connection::open_in_memory().unwrap();
    crate::schema::initialize(&mem).unwrap();
    let hm = handle_for_query(&mem, dim, "stub-d8").unwrap();
    assert_eq!(hm.read().unwrap().len(), 0);
}
```

- [ ] **Step 2: Run to verify it fails**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::registry_caches`
Expected: FAIL — `handle_for_query` not found.

- [ ] **Step 3: Implement the registry + handles**

Add near the top of `ann.rs` (after `index_options`):
```rust
type Handle = Arc<RwLock<AnnIndex>>;

fn registry() -> &'static Mutex<HashMap<PathBuf, Handle>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Handle>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Resolve a shared index handle for read/search.
///
/// On-disk DBs: one cached handle per canonical db path (built + reconciled on
/// first use). In-memory/pathless DBs: a fresh transient handle rebuilt from the
/// current SQLite state every call (tiny test DBs — correctness over speed).
pub fn handle_for_query(conn: &Connection, dim: usize, model_id: &str) -> KimetsuResult<Handle> {
    match AnnIndex::sidecar_for(conn) {
        Some(sidecar) => {
            // canonical-ish key: the sidecar path (stable per db file).
            let key = sidecar;
            let mut reg = registry().lock().unwrap_or_else(|p| p.into_inner());
            if let Some(h) = reg.get(&key) {
                return Ok(h.clone());
            }
            let idx = AnnIndex::open_or_build(conn, dim, model_id)?;
            let handle: Handle = Arc::new(RwLock::new(idx));
            reg.insert(key, handle.clone());
            Ok(handle)
        }
        None => Ok(Arc::new(RwLock::new(AnnIndex::build_from_conn(
            conn, dim, model_id,
        )?))),
    }
}

/// Cached write handle, or `None` for in-memory DBs (their writes are picked up
/// by the rebuild-on-query path, so write hooks safely skip them).
pub fn cached_handle(conn: &Connection) -> Option<Handle> {
    let sidecar = AnnIndex::sidecar_for(conn)?;
    let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    reg.get(&sidecar).cloned()
}
```

- [ ] **Step 4: Run to verify it passes**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::registry_caches`
Expected: PASS.

- [ ] **Step 5: Full T3a gate + push**

Run the **Shared gate** (top of this doc). All greps empty, both clippy flavors clean, `cargo tree -p kimetsu-cli -i usearch` empty.

```bash
git add crates/kimetsu-brain/src/ann.rs
git commit -m "feat(brain): ANN process registry + query/write handles

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
git push origin release/v1.0.0
```

---

# PHASE T3b — wire retrieval, conflict, write/invalidate to the ANN

Now the index is used. Replace vec0 reads with `ann::handle_for_query(...).search(...)`, the vec0 upsert with a cached-handle add, and invalidation with a cached-handle remove. The vec0 *table code* may remain dead until T3c.

### Task 10: retrieval — `memory_ann_candidates` uses the ANN

**Files:**
- Modify: `crates/kimetsu-brain/src/context.rs:806-833` (the vec0 query inside `memory_ann_candidates`)

- [ ] **Step 1: Replace the vec0 MATCH block with an ANN search**

In `memory_ann_candidates`, replace the body from the `ensure_vec_index(...)?;` line (currently line 814) through the construction of `knn_ids` (currently through line 833) with:
```rust
    // Tier-3: ANN candidate generation via the usearch HNSW index.
    let handle = crate::ann::handle_for_query(conn, qe.vector.len(), &qe.model_id)?;
    let hits = handle
        .read()
        .unwrap_or_else(|p| p.into_inner())
        .search(&qe.vector, k as usize)?;
    // Map rowids back to memory_ids (active-only is enforced by the index, but
    // we still join `memories` below for the full row + the embedding_model
    // residual filter, so collect rowids here).
    let knn_rowids: Vec<i64> = hits.into_iter().map(|(rowid, _dist)| rowid).collect();
    if knn_rowids.is_empty() {
        return Ok(Vec::new());
    }
```

- [ ] **Step 2: Update the hydration query to select by rowid**

Immediately below, the existing hydration builds an `IN (...)` over `knn_ids` (memory_id strings). Change it to bind `knn_rowids` against `rowid`:
- Replace `knn_ids` with `knn_rowids` in the placeholder builder and `params_vec`.
- Change the SQL `WHERE ... AND memory_id IN ({placeholders})` to `WHERE ... AND rowid IN ({placeholders})`.
- The `params_vec` becomes `knn_rowids.iter().map(|n| n as &dyn rusqlite::ToSql).collect()`.

The resulting hydration SQL:
```rust
    let placeholders: String = knn_rowids
        .iter()
        .enumerate()
        .map(|(i, _)| format!("?{}", i + 1))
        .collect::<Vec<_>>()
        .join(", ");
    let sql = format!(
        "SELECT memory_id, scope, kind, text, confidence, created_at,
                use_count, usefulness_score, embedding, embedding_model,
                last_useful_at
         FROM   memories
         WHERE  invalidated_at IS NULL
           AND  embedding_model = ?{model_param}
           AND  rowid IN ({placeholders})",
        model_param = knn_rowids.len() + 1
    );
    let mut stmt = conn.prepare(&sql)?;
    let mut params_vec: Vec<&dyn rusqlite::ToSql> =
        knn_rowids.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
    params_vec.push(&qe.model_id);
```
(The added `embedding_model = ?` is the residual model filter the design calls for; bind `qe.model_id` last.)

- [ ] **Step 3: Build + test retrieval still works**

Run: `cargo test -p kimetsu-brain --features embeddings context 2>&1 | grep -E "FAILED|panicked"`
Expected: empty. (Existing context tests now exercise the ANN path on in-memory DBs via the transient rebuild.)

- [ ] **Step 4: Commit**

```bash
git add crates/kimetsu-brain/src/context.rs
git commit -m "feat(brain): retrieval ANN candidates via usearch (was vec0 MATCH)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 11: conflict detection — pool via the ANN

**Files:**
- Modify: `crates/kimetsu-brain/src/conflict.rs:197-231` (the `#[cfg(feature="embeddings")]` vec0 block in `find_potential_conflicts_with_vec`)

- [ ] **Step 1: Write a failing/guarding test first**

The existing test `exclude_id_prevents_self_conflict` (conflict.rs tests) already exercises this path on the StubEmbedder. Confirm it currently passes:
Run: `cargo test -p kimetsu-brain --features embeddings conflict::tests::exclude_id_prevents_self_conflict`
Expected: PASS (baseline before edit).

- [ ] **Step 2: Replace the vec0 pool query with an ANN search**

In the `#[cfg(feature = "embeddings")]` block, replace the `ensure_vec_table` + `memory_vec` MATCH that produces `ann_ids: Vec<String>` with an ANN search producing `ann_rowids: Vec<i64>`:
```rust
    #[cfg(feature = "embeddings")]
    {
        let handle = crate::ann::handle_for_query(conn, query_vec.len(), active_model)?;
        let ann_rowids: Vec<i64> = handle
            .read()
            .unwrap_or_else(|p| p.into_inner())
            .search(query_vec, pool_size as usize)?
            .into_iter()
            .map(|(rowid, _)| rowid)
            .collect();

        if !ann_rowids.is_empty() {
            let placeholders: String = ann_rowids
                .iter()
                .enumerate()
                .map(|(i, _)| format!("?{}", i + 1))
                .collect::<Vec<_>>()
                .join(", ");
            let sql = format!(
                "SELECT memory_id, kind, text, normalized_text, embedding, embedding_model
                 FROM   memories
                 WHERE  invalidated_at IS NULL
                   AND  scope = '{scope_label}'
                   AND  embedding_model = '{active_model}'
                   AND  rowid IN ({placeholders})"
            );
            let mut stmt = conn.prepare(&sql)?;
            let params_vec: Vec<&dyn rusqlite::ToSql> =
                ann_rowids.iter().map(|n| n as &dyn rusqlite::ToSql).collect();
            // ... existing rows_iter + cosine-threshold loop unchanged below ...
```
Keep the rest of the block (the `rows_iter` mapping, the `ConflictHit` cosine-threshold loop, the dedup/self skips) exactly as-is. Remove the now-unused `find_potential_conflicts_sql` fallback branch tied to `prepare_cached` failure (the ANN handle build returns a proper error instead). Keep `find_potential_conflicts_sql` itself only if it is still referenced elsewhere; otherwise delete it and its tests in T3c.

> If `find_potential_conflicts_sql` becomes unused, `cargo clippy` will flag it — delete it (and any test that only covered the vec0-absent fallback) as part of this task to keep the gate clean.

- [ ] **Step 3: Run conflict tests**

Run: `cargo test -p kimetsu-brain --features embeddings conflict 2>&1 | grep -E "FAILED|panicked"`
Expected: empty.

- [ ] **Step 4: Commit**

```bash
git add crates/kimetsu-brain/src/conflict.rs
git commit -m "feat(brain): conflict-detection pool via usearch (was vec0 MATCH)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 12: write path — `embed_and_persist` maintains the ANN

**Files:**
- Modify: `crates/kimetsu-brain/src/embeddings.rs:694-711` (the `#[cfg(feature="embeddings")]` vec0 upsert block)

- [ ] **Step 1: Replace the vec0 upsert with a cached-handle add**

Replace the block (currently lines 694–711) with:
```rust
    // Tier-3: keep the warm usearch index current at add time. For in-memory
    // DBs there is no cached handle — the rebuild-on-query path picks the row
    // up, so we safely skip. Best-effort: an index failure must not abort a
    // successful memory write.
    #[cfg(feature = "embeddings")]
    if let Some(handle) = crate::ann::cached_handle(conn) {
        let rowid: Option<i64> = conn
            .query_row(
                "SELECT rowid FROM memories WHERE memory_id = ?1",
                rusqlite::params![memory_id],
                |r| r.get(0),
            )
            .ok();
        if let Some(rowid) = rowid {
            let mut guard = handle.write().unwrap_or_else(|p| p.into_inner());
            if let Err(e) = guard.add(rowid, &vec) {
                eprintln!(
                    "kimetsu-brain: ann add failed for memory {memory_id}: {e} (index will reconcile on next open)"
                );
            }
        }
    }
```

- [ ] **Step 2: Build**

Run: `cargo build -p kimetsu-brain --features embeddings`
Expected: compiles. (`crate::context::ensure_vec_table`/`upsert_vec_row` are now unused — they may warn; they get deleted in T3c. If clippy in the gate fails on dead code, add `#[allow(dead_code)]` to those three fns now with a `// removed in T3c` note.)

- [ ] **Step 3: Commit**

```bash
git add crates/kimetsu-brain/src/embeddings.rs
git commit -m "feat(brain): embed_and_persist maintains usearch index (was vec0 upsert)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 13: invalidation — projector + conflict-resolve remove from the ANN

**Files:**
- Modify: `crates/kimetsu-brain/src/projector.rs:640-663` (`apply_memory_invalidated`)
- Modify: `crates/kimetsu-brain/src/conflict.rs:~585-610` (the resolve `SET invalidated_at` UPDATEs)

- [ ] **Step 1: Add a shared remove helper in ann.rs**

In `ann.rs`, add:
```rust
/// Remove a memory from the cached index by its `memory_id` (no-op for
/// in-memory DBs / cold indexes — reconcile-on-open will catch it).
pub fn on_invalidate(conn: &Connection, memory_id: &str) {
    let Some(handle) = cached_handle(conn) else {
        return;
    };
    let rowid: Option<i64> = conn
        .query_row(
            "SELECT rowid FROM memories WHERE memory_id = ?1",
            rusqlite::params![memory_id],
            |r| r.get(0),
        )
        .ok();
    if let Some(rowid) = rowid {
        let mut guard = handle.write().unwrap_or_else(|p| p.into_inner());
        let _ = guard.remove(rowid);
    }
}
```

- [ ] **Step 2: Call it from the projector**

In `apply_memory_invalidated` (projector.rs), after the `UPDATE memories SET invalidated_at ...` execute and before `Ok(())`, add:
```rust
    #[cfg(feature = "embeddings")]
    crate::ann::on_invalidate(conn, memory_id);
```

- [ ] **Step 3: Call it from conflict-resolve**

In `conflict.rs`, locate the two `SET invalidated_at = COALESCE(...)` UPDATEs (~lines 592, 602) that invalidate the losing side of a conflict. After each (or after the resolve commits), add an `on_invalidate` call for the invalidated `memory_id`:
```rust
    #[cfg(feature = "embeddings")]
    crate::ann::on_invalidate(conn, invalidated_memory_id);
```
(Use whichever local variable holds the losing memory's id at that point.)

- [ ] **Step 4: Write a wiring test**

Add to `ann.rs` tests:
```rust
#[test]
fn on_invalidate_removes_from_cached_index() {
    let dim = 8;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("brain.db");
    let conn = Connection::open(&db).unwrap();
    crate::schema::initialize(&conn).unwrap();
    let mut v = vec![0.0f32; dim];
    v[0] = 1.0;
    conn.execute(
        "INSERT INTO memories
           (memory_id, scope, kind, text, normalized_text, confidence,
            provenance_snapshot_json, created_at, use_count, usefulness_score,
            embedding, embedding_model)
         VALUES ('m-x','project','fact','t','t',1.0,'{}','2026-01-01T00:00:00Z',0,0.0,?1,'stub-d8')",
        rusqlite::params![crate::embeddings::encode_embedding(&v)],
    ).unwrap();
    // Warm the cache.
    let h = handle_for_query(&conn, dim, "stub-d8").unwrap();
    assert_eq!(h.read().unwrap().len(), 1);
    // Invalidate in SQLite + notify the index.
    conn.execute("UPDATE memories SET invalidated_at='2026-02-01T00:00:00Z' WHERE memory_id='m-x'", []).unwrap();
    on_invalidate(&conn, "m-x");
    assert_eq!(cached_handle(&conn).unwrap().read().unwrap().len(), 0);
}
```

- [ ] **Step 5: Run + integration check**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::on_invalidate 2>&1 | grep -E "FAILED|panicked"` → empty.
Run: `cargo test -p kimetsu-brain --features embeddings projector 2>&1 | grep -E "FAILED|panicked"` → empty.

- [ ] **Step 6: Commit**

```bash
git add crates/kimetsu-brain/src/ann.rs crates/kimetsu-brain/src/projector.rs crates/kimetsu-brain/src/conflict.rs
git commit -m "feat(brain): remove from usearch index on invalidation (projector + conflict-resolve)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 14: integration test — retrieval parity + invalidate-drops via the public API

**Files:**
- Modify: `crates/kimetsu-brain/src/project.rs` (tests module)

- [ ] **Step 1: Write the integration test**

Add to the `project.rs` `#[cfg(test)] mod tests`, guarded by the feature:
```rust
#[cfg(feature = "embeddings")]
#[test]
fn ann_retrieval_round_trips_and_invalidate_drops() {
    use crate::user_brain::with_user_brain_disabled;
    with_user_brain_disabled(|| {
        // Use the StubEmbedder so this is deterministic and offline.
        unsafe { std::env::set_var("KIMETSU_BRAIN_EMBEDDER", "stub-d8"); }
        let root = test_root();
        init_project(&root, false).expect("init");
        add_memory(&root, MemoryScope::Project, MemoryKind::Fact,
            "ripgrep is the fast recursive search tool").expect("add a");
        let id = add_memory(&root, MemoryScope::Project, MemoryKind::Fact,
            "use fd to find files quickly").expect("add b");

        // Retrieval surfaces the relevant memory via the ANN path.
        let ctx = retrieve_context(&root, "recall", "find files fast", 1024).expect("ctx");
        assert!(format!("{ctx:?}").contains("fd to find files"), "expected the fd memory in context");

        // Invalidate it → it disappears from retrieval.
        invalidate_memory(&root, &id, Some("test")).expect("invalidate");
        let ctx2 = retrieve_context(&root, "recall", "find files fast", 1024).expect("ctx2");
        assert!(!format!("{ctx2:?}").contains("fd to find files"), "invalidated memory must not return");
    });
}
```
(Match the exact `add_memory` return type / `retrieve_context` signature in this file; adapt the assertion to the real capsule shape — search the existing `retrieve_context` tests in this module for the established assertion pattern and mirror it.)

- [ ] **Step 2: Run it**

Run: `KIMETSU_USER_BRAIN=0 cargo test -p kimetsu-brain --features embeddings ann_retrieval_round_trips -- --test-threads=1 2>&1 | grep -E "FAILED|panicked"`
Expected: empty.

- [ ] **Step 3: Full T3b gate + push**

Run the **Shared gate**. Fix any dead-code clippy warnings by `#[allow(dead_code)]` on the soon-to-be-deleted vec0 fns (with a `// removed in T3c` comment) if needed.

```bash
git add crates/kimetsu-brain/src/project.rs
git commit -m "test(brain): ANN retrieval round-trip + invalidate-drops integration

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
git push origin release/v1.0.0
```

---

# PHASE T3c — lifecycle polish + drop sqlite-vec + bench

### Task 15: reindex invalidates the sidecar (model change → rebuild)

**Files:**
- Modify: `crates/kimetsu-brain/src/reindex.rs` (`reindex_all_with_embedder` or `reindex_one_conn`)

- [ ] **Step 1: Add sidecar invalidation after a reindex run**

A reindex changes `embedding_model` on rows, so the cached/persisted index for the old model is stale. After a successful (non-dry-run) reindex of the project conn, delete the sidecar + drop the cached handle so the next query rebuilds under the new model. Add a helper in `ann.rs`:
```rust
/// Drop the cached handle AND delete the sidecar for `conn`'s db, forcing a
/// rebuild on next query. Called after a reindex (model change).
pub fn invalidate_sidecar(conn: &Connection) {
    if let Some(sidecar) = AnnIndex::sidecar_for(conn) {
        registry().lock().unwrap_or_else(|p| p.into_inner()).remove(&sidecar);
        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_file(AnnIndex::manifest_path(&sidecar));
    }
}
```
In `reindex.rs`, after a scope's rows are updated (non-dry-run, `updated > 0`), call:
```rust
#[cfg(feature = "embeddings")]
crate::ann::invalidate_sidecar(conn);
```

- [ ] **Step 2: Test**

Add an `ann.rs` test: build+save a sidecar, call `invalidate_sidecar`, assert the file is gone and `cached_handle` is `None`.
```rust
#[test]
fn invalidate_sidecar_removes_file_and_cache() {
    let dim = 8;
    let dir = tempfile::tempdir().unwrap();
    let db = dir.path().join("brain.db");
    let conn = Connection::open(&db).unwrap();
    crate::schema::initialize(&conn).unwrap();
    let _ = handle_for_query(&conn, dim, "stub-d8").unwrap();
    handle_for_query(&conn, dim, "stub-d8").unwrap().read().unwrap();
    cached_handle(&conn).unwrap().read().unwrap().save().unwrap();
    assert!(db.with_extension("usearch").exists());
    invalidate_sidecar(&conn);
    assert!(!db.with_extension("usearch").exists());
    assert!(cached_handle(&conn).is_none());
}
```

- [ ] **Step 3: Run + commit**

Run: `cargo test -p kimetsu-brain --features embeddings ann::tests::invalidate_sidecar 2>&1 | grep -E "FAILED|panicked"` → empty.
```bash
git add crates/kimetsu-brain/src/ann.rs crates/kimetsu-brain/src/reindex.rs
git commit -m "feat(brain): reindex invalidates the ANN sidecar (model change → rebuild)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 16: persist on shutdown (warm hosts) + periodic save

**Files:**
- Modify: `crates/kimetsu-brain/src/ann.rs` (add `save_all`)
- Modify: the host shutdown paths — `crates/kimetsu-chat` REPL exit and `crates/kimetsu-remote` graceful shutdown.

- [ ] **Step 1: Add `save_all` to flush every cached index**

In `ann.rs`:
```rust
/// Save every cached on-disk index (called on graceful host shutdown).
pub fn save_all() {
    let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    for handle in reg.values() {
        let guard = handle.read().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = guard.save() {
            eprintln!("kimetsu-brain: ann save_all failed: {e}");
        }
    }
}
```

- [ ] **Step 2: Call `save_all` on host shutdown**

- In `kimetsu-remote`'s graceful-shutdown branch (after `axum::serve(...).with_graceful_shutdown(...)` returns), add (guarded so the lean binary still compiles — kimetsu-remote builds with embeddings by default):
  ```rust
  #[cfg(feature = "embeddings")]
  kimetsu_brain::ann::save_all();
  ```
  Search `crates/kimetsu-remote/src/main.rs` for where the server future completes; place it there.
- In `kimetsu-chat`'s REPL exit path (where the process is about to return from the interactive loop), add the same guarded call. Search for the REPL teardown in `crates/kimetsu-chat/src/`.

> If wiring a host shutdown hook proves invasive, it is acceptable to rely on per-add warm maintenance + reconcile-on-open instead, and SKIP this task — the index stays correct without it (just colder after a restart). Decide based on how clean the shutdown seam is; document the choice in the commit message.

- [ ] **Step 3: Build both hosts**

Run: `cargo build -p kimetsu-remote --features embeddings` and `cargo build -p kimetsu-chat`.
Expected: compile.

- [ ] **Step 4: Commit**

```bash
git add -A
git commit -m "feat(brain): persist ANN indexes on host shutdown (save_all)

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
```

---

### Task 17: delete vec0 code + drop sqlite-vec + DROP TABLE memory_vec

**Files:**
- Modify: `crates/kimetsu-brain/src/context.rs` — delete `ensure_vec_index`, `ensure_vec_table`, `upsert_vec_row`.
- Modify: `crates/kimetsu-brain/src/schema.rs` — delete `ensure_vec_extension_registered` + its caller(s) + the `sqlite_vec_extension_links_and_knn_runs` test; add the migration.
- Modify: `crates/kimetsu-brain/Cargo.toml` — remove `sqlite-vec`.
- Modify: any caller of `ensure_vec_extension_registered` (grep first).

- [ ] **Step 1: Grep for every vec0 / sqlite-vec reference**

Run: `rg -n "memory_vec|ensure_vec_|sqlite_vec|sqlite-vec|vec0" crates/`
Make a checklist of every hit; each must be deleted or (for `memory_vec` in the migration) intentionally kept.

- [ ] **Step 2: Delete the three vec0 functions in context.rs**

Remove `ensure_vec_index`, `ensure_vec_table`, `upsert_vec_row` (and the `vec_to_json` helper if any remnant remains) and their `#[cfg(feature="embeddings")]` attributes. Remove any now-dead `use` imports.

- [ ] **Step 3: Remove sqlite-vec registration in schema.rs**

Delete `ensure_vec_extension_registered` and remove every call to it (grep). Delete the `sqlite_vec_extension_links_and_knn_runs` test.

- [ ] **Step 4: Add the migration to drop the table**

In `schema.rs::initialize` (or the migrations module `migrate.rs` if that's where idempotent DDL lives — follow the existing pattern), add an idempotent:
```rust
conn.execute_batch("DROP TABLE IF EXISTS memory_vec;")?;
```
Place it so it runs on every open (idempotent). This reclaims space in upgraded brains.

- [ ] **Step 5: Remove the dependency**

In `crates/kimetsu-brain/Cargo.toml`:
- In `[features]`: `embeddings = ["dep:fastembed", "dep:usearch"]` (drop `"dep:sqlite-vec"`).
- In `[dependencies]`: delete the `sqlite-vec = { ... }` line and its comment.

- [ ] **Step 6: Build + full gate**

Run: `cargo build -p kimetsu-brain --features embeddings` (must compile with no sqlite-vec).
Run the **Shared gate**. Additionally:
```bash
cargo tree -p kimetsu-brain -i sqlite-vec   # MUST be empty
rg -n "sqlite_vec|sqlite-vec|vec0|memory_vec" crates/   # only the DROP TABLE migration should remain
```

- [ ] **Step 7: Commit (isolated — easy revert)**

```bash
git add crates/kimetsu-brain/
git commit -m "refactor(brain): drop sqlite-vec + vec0; usearch fully supersedes it

Removes the brute-force vec0 KNN path entirely (ensure_vec_index/table,
upsert_vec_row, ensure_vec_extension_registered) and the sqlite-vec dep.
Adds DROP TABLE IF EXISTS memory_vec to reclaim space in upgraded brains.
Isolated commit: one git revert restores the exact brute-force fallback.

Co-Authored-By: Claude Opus 4.8 <noreply@anthropic.com>"
git push origin release/v1.0.0
```

---

### Task 18: bench re-measure (manual, by the user)

**Files:** none (measurement only).

- [ ] **Step 1: Rebuild the stress harness with embeddings**

In `bench/` (separate repo, ext4 work dir already default after Tier-2):
Run: `make stress`

- [ ] **Step 2: Read the local-emb report**

Open `bench/runs/stress/<newest>/local-emb/report.md`. Expect, vs the `bd72675` run:
- **ctx warm p99** roughly **flat** across 100→50k (was 122→1304 ms, linear) — the headline Tier-3 win.
- **add p99** flat (conflict-detection no longer O(N)).
- seed rows/s and lean numbers unchanged.

- [ ] **Step 3: Report results to the user** with the warm-ctx and add-p99 curves; if warm is still rising, raise `EXPANSION_SEARCH` (recall) or check the index is being reused (not rebuilt per query). No commit (bench is a separate repo and gitignored here).

---

## Self-Review (done while writing)

- **Spec coverage:** usearch (T3a deps/wrapper) ✓; derived sidecar + manifest + rebuild/reconcile (Tasks 3,6,7) ✓; active-only + remove-on-invalidate (Tasks 5,13) ✓; rowid key (Tasks 3,4,10,11) ✓; exact rerank preserved (Tasks 10,11 keep the cosine loops) ✓; recall guard (Task 8) ✓; drop sqlite-vec isolated (Task 17) ✓; three separately-revertible phase pushes (Tasks 9,14,17) ✓; bench re-measure (Task 18) ✓; in-place re-embed upsert (Task 5) ✓; cross-process staleness handled by reconcile (Task 7) ✓; lean build untouched (every ann path is `#[cfg(feature="embeddings")]`; `cargo tree` guard in the gate) ✓.
- **Type consistency:** `AnnIndex::{build_from_conn, search, add, remove, save, open_or_build, reconcile, len, is_empty}`; module fns `handle_for_query`, `cached_handle`, `on_invalidate`, `invalidate_sidecar`, `save_all`. `search` returns `Vec<(i64, f32)>` (rowid, distance) — consumed as rowids in Tasks 10/11. Manifest fields used consistently in save/try_load/reconcile.
- **Open verification risks flagged inline:** exact `usearch` 2.x API names (`Index::new`, `IndexOptions{...}`, `add/search/remove/contains/reserve/capacity/size/save/load`, `Matches{keys,distances}`, `MetricKind::Cos`, `ScalarKind::F32`) must be confirmed against the resolved crate version at Task 1 — adjust call sites if the crate's signatures differ (e.g. `search` returning a `Matches` vs tuple). This is the single most likely source of churn; verify before Task 3.
```
