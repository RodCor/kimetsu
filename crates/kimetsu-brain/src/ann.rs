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
/// makes an old sidecar unsafe to load — forces a rebuild. v2: the manifest
/// gained a `quant` field AND the default index scalar changed f32→f16, so all
/// pre-v2 (f32) sidecars must be rebuilt.
const SCHEMA_VERSION: u32 = 2;

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
    /// Scalar quantization the sidecar was built with (`f16`/`i8`/`f32`). A
    /// sidecar must NOT be loaded under a different quantization (silent
    /// corruption), so `try_load` rejects a mismatch and forces a rebuild.
    quant: String,
}

/// Index scalar quantization. f16 is the default — it ~halves the index's
/// vector RAM with negligible quality loss (final ranking is an exact f32
/// cosine rerank from SQLite, so the index only needs to surface the right
/// candidate pool). `i8` quarters it (for tight containers); `f32` is full
/// fidelity. Set via `KIMETSU_ANN_QUANTIZATION`.
fn ann_scalar_kind() -> ScalarKind {
    match std::env::var("KIMETSU_ANN_QUANTIZATION").ok().as_deref() {
        Some("f32") => ScalarKind::F32,
        Some("i8") => ScalarKind::I8,
        Some("f16") | None => ScalarKind::F16,
        Some(other) => {
            eprintln!("kimetsu-brain: unknown KIMETSU_ANN_QUANTIZATION '{other}', using f16");
            ScalarKind::F16
        }
    }
}

/// Stable string id for a ScalarKind, stored in the manifest so a sidecar built
/// with one quantization is never loaded by a process configured for another.
fn scalar_kind_id(k: ScalarKind) -> &'static str {
    match k {
        ScalarKind::F32 => "f32",
        ScalarKind::F16 => "f16",
        ScalarKind::I8 => "i8",
        _ => "other",
    }
}

fn index_options(dim: usize) -> IndexOptions {
    IndexOptions {
        dimensions: dim,
        metric: MetricKind::Cos,
        quantization: ann_scalar_kind(),
        connectivity: CONNECTIVITY,
        expansion_add: EXPANSION_ADD,
        expansion_search: EXPANSION_SEARCH,
        multi: false,
    }
}

/// Threads for parallel index construction. usearch `add` is thread-safe
/// (Index: Send+Sync, C++ locks internally), so fanning inserts across cores
/// turns the single-threaded build (the 1M bottleneck) into ~cores-x faster.
fn build_threads() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get())
        .unwrap_or(4)
        .clamp(1, 16)
}

/// Insert a batch of (rowid, vector) into `index` in parallel. The caller must
/// have reserved capacity. Returns the first add error if any thread failed.
fn parallel_add(index: &Index, rows: &[(i64, Vec<f32>)]) -> KimetsuResult<()> {
    if rows.is_empty() {
        return Ok(());
    }
    let nthreads = build_threads().min(rows.len());
    let chunk = rows.len().div_ceil(nthreads);
    let err: std::sync::Mutex<Option<String>> = std::sync::Mutex::new(None);
    std::thread::scope(|s| {
        for part in rows.chunks(chunk) {
            let err = &err;
            s.spawn(move || {
                for (rowid, vec) in part {
                    if let Err(e) = index.add(*rowid as u64, vec) {
                        let mut g = err.lock().unwrap_or_else(|p| p.into_inner());
                        if g.is_none() {
                            *g = Some(format!("usearch add: {e}"));
                        }
                        return;
                    }
                }
            });
        }
    });
    match err.into_inner().unwrap_or_else(|p| p.into_inner()) {
        Some(e) => Err(e.into()),
        None => Ok(()),
    }
}

type Handle = Arc<RwLock<AnnIndex>>;

fn registry() -> &'static Mutex<HashMap<PathBuf, Handle>> {
    static REG: OnceLock<Mutex<HashMap<PathBuf, Handle>>> = OnceLock::new();
    REG.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Per-key build lock so builds of the SAME brain serialize without blocking
/// other repos. The global `registry()` mutex is only held briefly (cache check
/// / insert) — never across a build.
fn build_lock_for(key: &Path) -> Arc<Mutex<()>> {
    static LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();
    let m = LOCKS.get_or_init(|| Mutex::new(HashMap::new()));
    let mut g = m.lock().unwrap_or_else(|p| p.into_inner());
    g.entry(key.to_path_buf())
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// Persist a built/updated index to its sidecar on a background thread so the
/// triggering query isn't blocked by the (potentially large) write. Best-effort.
/// usearch `save` takes `&self` and is fine concurrent with searches.
fn spawn_save(handle: Handle) {
    std::thread::spawn(move || {
        let guard = handle.read().unwrap_or_else(|p| p.into_inner());
        if let Err(e) = guard.save() {
            eprintln!("kimetsu-brain: ann background save failed: {e}");
        }
    });
}

/// Resolve a shared index handle for read/search.
///
/// On-disk DBs: one cached handle per canonical db path (built + reconciled on
/// first use). In-memory/pathless DBs: a fresh transient handle rebuilt from the
/// current SQLite state every call (tiny test DBs — correctness over speed).
pub fn handle_for_query(conn: &Connection, dim: usize, model_id: &str) -> KimetsuResult<Handle> {
    let Some(key) = AnnIndex::sidecar_for(conn) else {
        // in-memory / pathless: transient, rebuilt each call, never stale.
        return Ok(Arc::new(RwLock::new(AnnIndex::build_from_conn(
            conn, dim, model_id,
        )?)));
    };
    let handle = get_or_build_handle(&key, conn, dim, model_id)?;
    reconcile_if_stale(&handle, conn)?;
    Ok(handle)
}

/// Step 1: return the cached handle for `key`, or build it once under the
/// per-key build lock and cache + background-persist it.
///
/// LOCK ORDERING (deadlock-critical): the per-key `build_lock` is the OUTER
/// lock; the global `registry()` lock is only ever taken alone and briefly
/// (cache check + insert) — NEVER acquired while holding `build_lock` for a
/// build. We never acquire `build_lock` while holding `registry()`.
fn get_or_build_handle(
    key: &Path,
    conn: &Connection,
    dim: usize,
    model_id: &str,
) -> KimetsuResult<Handle> {
    // Fast path: already cached (brief global lock only).
    {
        let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(h) = reg.get(key) {
            return Ok(h.clone());
        }
    }
    // Serialize builds of THIS key (other keys proceed concurrently).
    let bl = build_lock_for(key);
    let _g = bl.lock().unwrap_or_else(|p| p.into_inner());
    // Double-check: someone may have built it while we waited.
    {
        let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
        if let Some(h) = reg.get(key) {
            return Ok(h.clone());
        }
    }
    // Build WITHOUT holding the global registry lock.
    let idx = AnnIndex::open_or_build(conn, dim, model_id)?;
    let handle: Handle = Arc::new(RwLock::new(idx));
    // Cache (brief global lock), then persist in the background so a restarted
    // process LOADS the sidecar instead of rebuilding from scratch.
    registry()
        .lock()
        .unwrap_or_else(|p| p.into_inner())
        .insert(key.to_path_buf(), handle.clone());
    spawn_save(handle.clone());
    Ok(handle)
}

/// Step 2: reconcile a cached index that has fallen behind SQLite (rows added
/// out-of-band of the warm add path). Cheap guard: only pay the write lock +
/// reconcile when MAX(rowid) shows new rows. Double-checked under the lock.
fn reconcile_if_stale(handle: &Handle, conn: &Connection) -> KimetsuResult<()> {
    let stale = {
        let idx = handle.read().unwrap_or_else(|p| p.into_inner());
        idx.is_stale(conn)?
    };
    if stale {
        let mut idx = handle.write().unwrap_or_else(|p| p.into_inner());
        if idx.is_stale(conn)? {
            idx.reconcile(conn)?;
        }
    }
    Ok(())
}

/// Build-or-load + cache the index for `conn`'s brain (no query). Lets a host
/// pre-warm on startup so the first real request doesn't pay the cold build.
pub fn warm(conn: &Connection, dim: usize, model_id: &str) -> KimetsuResult<()> {
    handle_for_query(conn, dim, model_id).map(|_| ())
}

/// Cached write handle, or `None` for in-memory DBs (their writes are picked up
/// by the rebuild-on-query path, so write hooks safely skip them).
pub fn cached_handle(conn: &Connection) -> Option<Handle> {
    let sidecar = AnnIndex::sidecar_for(conn)?;
    let reg = registry().lock().unwrap_or_else(|p| p.into_inner());
    reg.get(&sidecar).cloned()
}

/// Remove a superseded memory from the cached ANN index by its `memory_id`.
/// Mirrors `on_invalidate` — superseded rows are excluded from ANN retrieval
/// the same way invalidated rows are. No-op for in-memory DBs / cold indexes
/// (reconcile-on-open handles those).
pub fn on_supersede(conn: &Connection, memory_id: &str) {
    on_invalidate(conn, memory_id);
}

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

/// Drop the cached handle AND delete the sidecar for `conn`'s db, forcing a
/// rebuild on next query. Called after a reindex (model change).
pub fn invalidate_sidecar(conn: &Connection) {
    if let Some(sidecar) = AnnIndex::sidecar_for(conn) {
        registry()
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .remove(&sidecar);
        let _ = std::fs::remove_file(&sidecar);
        let _ = std::fs::remove_file(AnnIndex::manifest_path(&sidecar));
    }
}

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

    /// True when SQLite has rows beyond what the index covers (cheap: MAX(rowid)
    /// is the integer primary key, O(1)-ish). Out-of-band invalidations are NOT
    /// detected here, but that's harmless — retrieval hydration already filters
    /// `invalidated_at IS NULL`, so a stale-invalidated candidate is dropped.
    pub fn is_stale(&self, conn: &Connection) -> KimetsuResult<bool> {
        let max_rowid: i64 =
            conn.query_row("SELECT COALESCE(MAX(rowid), 0) FROM memories", [], |r| {
                r.get(0)
            })?;
        Ok(max_rowid > self.max_rowid_indexed)
    }

    /// Build a fresh index from every active, current-model embedding in SQLite.
    pub fn build_from_conn(conn: &Connection, dim: usize, model_id: &str) -> KimetsuResult<Self> {
        let index = Index::new(&index_options(dim)).map_err(|e| format!("usearch new: {e}"))?;
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
             WHERE invalidated_at IS NULL AND superseded_by IS NULL
               AND embedding IS NOT NULL AND embedding_model = ?1",
            rusqlite::params![self.model_id],
            |r| r.get(0),
        )?;
        if count > 0 {
            self.index
                .reserve(count as usize)
                .map_err(|e| format!("usearch reserve: {e}"))?;
        }
        // Stream rows in chunks (bounds memory at 1M) and parallel-add each
        // chunk across cores. usearch `add` is thread-safe (Index: Send+Sync).
        const BUILD_CHUNK: usize = 16384;
        let mut stmt = conn.prepare(
            "SELECT rowid, embedding FROM memories
             WHERE invalidated_at IS NULL AND superseded_by IS NULL
               AND embedding IS NOT NULL AND embedding_model = ?1
             ORDER BY rowid",
        )?;
        let mut rows_iter = stmt.query(rusqlite::params![self.model_id])?;
        let mut batch: Vec<(i64, Vec<f32>)> = Vec::with_capacity(BUILD_CHUNK);
        let mut max_rowid = self.max_rowid_indexed;
        loop {
            let row = rows_iter.next()?;
            let done = row.is_none();
            if let Some(row) = row {
                let rowid: i64 = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                if rowid > max_rowid {
                    max_rowid = rowid;
                }
                // Skip malformed blobs and undecodable rows rather than abort.
                // The watermark still advances so a corrupt high-rowid vector
                // does not force reconciliation on every later query.
                if blob.len() == self.dim * 4
                    && let Ok(vec) = crate::embeddings::decode_embedding(&blob, Some(self.dim))
                {
                    batch.push((rowid, vec));
                }
            }
            if batch.len() >= BUILD_CHUNK || (done && !batch.is_empty()) {
                parallel_add(&self.index, &batch)?;
                batch.clear();
            }
            if done {
                break;
            }
        }
        self.max_rowid_indexed = max_rowid;
        Ok(())
    }

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

    /// Insert or replace the vector for `rowid`. usearch would otherwise keep a
    /// duplicate for an existing key (multi=false still appends a new slot), so
    /// remove-then-add guarantees a single current entry (in-place re-embed).
    pub fn add(&mut self, rowid: i64, vector: &[f32]) -> KimetsuResult<()> {
        if vector.len() != self.dim {
            return Err(format!("ann add: dim {} != index dim {}", vector.len(), self.dim).into());
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

    /// Derive the sidecar index path from a brain.db path: sibling
    /// `<stem>.usearch`. Returns `None` for in-memory / pathless DBs.
    fn sidecar_for(conn: &Connection) -> Option<PathBuf> {
        match conn.path() {
            Some(p) if !p.is_empty() && p != ":memory:" => {
                // Canonicalize the (existing) db file so two path spellings of the
                // same brain.db resolve to ONE registry key + sidecar — otherwise
                // two live indexes could overwrite each other's sidecar.
                let db = std::fs::canonicalize(p).unwrap_or_else(|_| PathBuf::from(p));
                Some(db.with_extension("usearch"))
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

    /// A process-unique sibling temp path (`<name>.<pid>.tmp`) for an atomic
    /// write-then-rename. The pid suffix prevents two concurrent fleet processes
    /// from colliding on the same temp file.
    fn tmp_sibling(path: &Path) -> PathBuf {
        let mut name = path.file_name().map(|n| n.to_owned()).unwrap_or_default();
        name.push(format!(".{}.tmp", std::process::id()));
        path.with_file_name(name)
    }

    fn manifest(&self) -> Manifest {
        Manifest {
            schema_version: SCHEMA_VERSION,
            dim: self.dim,
            model_id: self.model_id.clone(),
            max_rowid_indexed: self.max_rowid_indexed,
            count: self.len(),
            quant: scalar_kind_id(ann_scalar_kind()).to_string(),
        }
    }

    /// Serialize the index + manifest to the sidecar (no-op for in-memory DBs).
    ///
    /// Concurrency-safe for fleet writers: each file is written to a
    /// process-unique temp path and atomically `rename`d into place, so a
    /// concurrent reader (another process opening the same brain) never observes
    /// a torn `.usearch`. The manifest is renamed LAST — a reader that sees the
    /// new manifest is guaranteed to also see the new index, and the reverse
    /// (new index + old manifest) is caught by the `size != count` check on load
    /// and degrades to a rebuild rather than serving stale hits.
    pub fn save(&self) -> KimetsuResult<()> {
        let Some(sidecar) = &self.sidecar else {
            return Ok(());
        };

        // 1. Index → temp → atomic rename.
        let index_tmp = Self::tmp_sibling(sidecar);
        self.index
            .save(index_tmp.to_string_lossy().as_ref())
            .map_err(|e| format!("usearch save: {e}"))?;
        std::fs::rename(&index_tmp, sidecar).map_err(|e| {
            let _ = std::fs::remove_file(&index_tmp);
            format!("usearch rename: {e}")
        })?;

        // 2. Manifest LAST → temp → atomic rename.
        let manifest_path = Self::manifest_path(sidecar);
        let manifest_tmp = Self::tmp_sibling(&manifest_path);
        let manifest =
            serde_json::to_vec(&self.manifest()).map_err(|e| format!("manifest serialize: {e}"))?;
        std::fs::write(&manifest_tmp, manifest).map_err(|e| format!("manifest write: {e}"))?;
        std::fs::rename(&manifest_tmp, &manifest_path).map_err(|e| {
            let _ = std::fs::remove_file(&manifest_tmp);
            format!("manifest rename: {e}")
        })?;
        Ok(())
    }

    /// Load a valid sidecar (manifest matches dim/model/schema) then reconcile
    /// the SQLite delta; otherwise rebuild from scratch. For in-memory DBs there
    /// is no sidecar, so this always builds fresh.
    pub fn open_or_build(conn: &Connection, dim: usize, model_id: &str) -> KimetsuResult<Self> {
        let sidecar = Self::sidecar_for(conn);
        if let Some(path) = &sidecar
            && path.exists()
            && let Some(loaded) = Self::try_load(path, dim, model_id)?
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
    fn try_load(sidecar: &Path, dim: usize, model_id: &str) -> KimetsuResult<Option<Self>> {
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
            // A sidecar built under one quantization must never be loaded under
            // another (the stored scalar type differs) — force a rebuild.
            || manifest.quant != scalar_kind_id(ann_scalar_kind())
        {
            return Ok(None);
        }
        let index = Index::new(&index_options(dim)).map_err(|e| format!("usearch new: {e}"))?;
        if index.load(sidecar.to_string_lossy().as_ref()).is_err() {
            return Ok(None); // corrupt sidecar → rebuild
        }
        if index.size() != manifest.count {
            return Ok(None);
        }
        Ok(Some(Self {
            index,
            dim,
            model_id: model_id.to_string(),
            sidecar: Some(sidecar.to_path_buf()),
            max_rowid_indexed: manifest.max_rowid_indexed,
        }))
    }

    /// Apply the SQLite→index delta after a sidecar load:
    ///   * add active current-model rows with `rowid > max_rowid_indexed`;
    ///   * remove rows now invalidated (rowid <= max) still in the index.
    ///
    /// Cheap: rides the `idx_memories_scope_model_active` covering index.
    pub fn reconcile(&mut self, conn: &Connection) -> KimetsuResult<()> {
        // 3a. New active rows since last index. Stream the delta in chunks so a
        // bulk load (e.g. 500k rows) never materializes its BLOBs + decoded f32
        // all at once (~1.5GB transient). COUNT once + reserve the full delta up
        // front, then decode + parallel-add each chunk and free it before the
        // next. `parallel_add` does NOT grow capacity, but the upfront reserve
        // covers the whole delta, so we do NOT re-reserve per chunk.
        const RECONCILE_CHUNK: usize = 16384;
        let delta_count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories
             WHERE invalidated_at IS NULL AND superseded_by IS NULL
               AND embedding IS NOT NULL
               AND embedding_model = ?1 AND rowid > ?2",
            rusqlite::params![self.model_id, self.max_rowid_indexed],
            |r| r.get(0),
        )?;
        if delta_count > 0 {
            self.index
                .reserve(self.index.size() + delta_count as usize)
                .map_err(|e| format!("usearch reserve: {e}"))?;
        }
        let mut stmt = conn.prepare(
            "SELECT rowid, embedding FROM memories
             WHERE invalidated_at IS NULL AND superseded_by IS NULL
               AND embedding IS NOT NULL
               AND embedding_model = ?1 AND rowid > ?2 ORDER BY rowid",
        )?;
        let mut rows_iter = stmt.query(rusqlite::params![self.model_id, self.max_rowid_indexed])?;
        let mut batch: Vec<(i64, Vec<f32>)> = Vec::with_capacity(RECONCILE_CHUNK);
        let mut max_rowid = self.max_rowid_indexed;
        loop {
            let row = rows_iter.next()?;
            let done = row.is_none();
            if let Some(row) = row {
                let rowid: i64 = row.get(0)?;
                let blob: Vec<u8> = row.get(1)?;
                if rowid > max_rowid {
                    max_rowid = rowid;
                }
                // Skip malformed blobs and undecodable rows rather than abort.
                // The watermark still advances so a corrupt high-rowid vector
                // is skipped once instead of retried on every query forever.
                if blob.len() == self.dim * 4
                    && let Ok(vec) = crate::embeddings::decode_embedding(&blob, Some(self.dim))
                {
                    batch.push((rowid, vec));
                }
            }
            if batch.len() >= RECONCILE_CHUNK || (done && !batch.is_empty()) {
                parallel_add(&self.index, &batch)?;
                batch.clear();
            }
            if done {
                break;
            }
        }
        drop(rows_iter);
        drop(stmt);
        self.max_rowid_indexed = max_rowid;

        // 3b. Remove rows now invalidated or superseded (only those <=
        //     the watermark; newer ones were never added). `remove` is a
        //     no-op if absent. Superseded rows are treated the same as
        //     invalidated: they stop appearing as ANN candidates so
        //     retrieval queries only surface the survivor.
        let gone: Vec<i64> = {
            let mut stmt = conn.prepare(
                "SELECT rowid FROM memories
                 WHERE (invalidated_at IS NOT NULL OR superseded_by IS NOT NULL)
                   AND rowid <= ?1",
            )?;
            stmt.query_map(rusqlite::params![self.max_rowid_indexed], |r| {
                r.get::<_, i64>(0)
            })?
            .filter_map(|r| r.ok())
            .collect()
        };
        for rowid in gone {
            self.remove(rowid)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::encode_embedding;

    /// Run `f` with `KIMETSU_ANN_QUANTIZATION` set to `val` (or unset when
    /// `None`), serialized via the process-wide test env lock and restored
    /// afterwards. The quantization is process-global (read from env by
    /// `ann_scalar_kind`), so env-mutating quant tests MUST go through here.
    fn with_quant<R>(val: Option<&str>, f: impl FnOnce() -> R) -> R {
        let _guard = crate::user_brain::test_env_lock()
            .lock()
            .unwrap_or_else(|p| p.into_inner());
        let prev = std::env::var("KIMETSU_ANN_QUANTIZATION").ok();
        // SAFETY: scoped via the shared mutex; no other thread races on env.
        unsafe {
            match val {
                Some(v) => std::env::set_var("KIMETSU_ANN_QUANTIZATION", v),
                None => std::env::remove_var("KIMETSU_ANN_QUANTIZATION"),
            }
        }
        let out = std::panic::catch_unwind(std::panic::AssertUnwindSafe(f));
        unsafe {
            match prev {
                Some(v) => std::env::set_var("KIMETSU_ANN_QUANTIZATION", v),
                None => std::env::remove_var("KIMETSU_ANN_QUANTIZATION"),
            }
        }
        match out {
            Ok(r) => r,
            Err(e) => std::panic::resume_unwind(e),
        }
    }

    /// Seed `n` deterministic pseudo-random unit-ish vectors into an in-memory
    /// brain and return (conn, rowid->vec map). Shared by the recall tests.
    fn seed_random(n: usize, dim: usize, model: &str) -> (Connection, Vec<(i64, Vec<f32>)>) {
        use crate::embeddings::decode_embedding;
        let conn = Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("init");
        let mut state: u64 = 0x9E3779B97F4A7C15;
        let mut next = || {
            state = state.wrapping_mul(6364136223846793005).wrapping_add(1);
            ((state >> 33) as f32 / (1u64 << 31) as f32) - 1.0
        };
        for i in 0..n {
            let v: Vec<f32> = (0..dim).map(|_| next()).collect();
            conn.execute(
                "INSERT INTO memories
                   (memory_id, scope, kind, text, normalized_text, confidence,
                    provenance_snapshot_json, created_at, use_count, usefulness_score,
                    embedding, embedding_model)
                 VALUES (?1,'project','fact',?2,?2,1.0,'{}','2026-01-01T00:00:00Z',0,0.0,?3,?4)",
                rusqlite::params![format!("m-{i:06}"), "t", encode_embedding(&v), model],
            )
            .expect("insert");
        }
        let mut stmt = conn
            .prepare("SELECT rowid, embedding FROM memories")
            .unwrap();
        let rows = stmt
            .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
            .unwrap();
        let mut vectors: Vec<(i64, Vec<f32>)> = Vec::new();
        for row in rows {
            let (rowid, blob) = row.unwrap();
            vectors.push((rowid, decode_embedding(&blob, Some(dim)).unwrap()));
        }
        drop(stmt);
        (conn, vectors)
    }

    /// Mean recall@k of the ANN index vs an exact brute-force cosine top-k.
    fn measure_recall(idx: &AnnIndex, vectors: &[(i64, Vec<f32>)], k: usize, trials: usize) -> f32 {
        use crate::embeddings::cosine_similarity;
        let mut hit = 0usize;
        let mut total = 0usize;
        for t in 0..trials {
            let q = &vectors[t * 7 % vectors.len()].1;
            let mut scored: Vec<(i64, f32)> = vectors
                .iter()
                .map(|(id, v)| (*id, cosine_similarity(q, v)))
                .collect();
            scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
            let exact: std::collections::HashSet<i64> =
                scored.iter().take(k).map(|(id, _)| *id).collect();
            let ann: std::collections::HashSet<i64> = idx
                .search(q, k)
                .unwrap()
                .into_iter()
                .map(|(id, _)| id)
                .collect();
            hit += exact.intersection(&ann).count();
            total += k;
        }
        hit as f32 / total as f32
    }

    #[test]
    fn default_quant_is_f16() {
        with_quant(None, || {
            assert!(matches!(ann_scalar_kind(), ScalarKind::F16));
            assert_eq!(scalar_kind_id(ScalarKind::F16), "f16");
            assert_eq!(scalar_kind_id(ScalarKind::F32), "f32");
            assert_eq!(scalar_kind_id(ScalarKind::I8), "i8");
        });
    }

    #[test]
    fn recall_guard_holds_under_f16() {
        with_quant(Some("f16"), || {
            let dim = 16;
            let (conn, vectors) = seed_random(5000, dim, "stub");
            let idx = AnnIndex::build_from_conn(&conn, dim, "stub").expect("build");
            let recall = measure_recall(&idx, &vectors, 10, 50);
            assert!(recall >= 0.95, "f16 recall@10 = {recall} (want >= 0.95)");
        });
    }

    #[test]
    fn i8_quant_builds_and_searches() {
        with_quant(Some("i8"), || {
            assert!(matches!(ann_scalar_kind(), ScalarKind::I8));
            let dim = 16;
            let (conn, vectors) = seed_random(5000, dim, "stub");
            let idx = AnnIndex::build_from_conn(&conn, dim, "stub").expect("build");
            assert_eq!(idx.len(), 5000, "i8 index covers all rows");
            let hits = idx.search(&vectors[0].1, 10).expect("search");
            assert_eq!(hits.len(), 10, "i8 search returns k results");
            let recall = measure_recall(&idx, &vectors, 10, 50);
            // i8 is lossier than f16; production over-fetches a pool of ~80 and
            // exact-reranks, so candidate recall is what matters. Floor at 0.85;
            // if i8 is lower, REPORT the observed number.
            assert!(recall >= 0.85, "i8 recall@10 = {recall} (want >= 0.85)");
        });
    }

    #[test]
    fn manifest_quant_mismatch_forces_rebuild() {
        let dim = 8;
        let dir = tempfile::tempdir().expect("tmp");
        let db = dir.path().join("brain.db");
        // Build + save a sidecar under f16.
        with_quant(Some("f16"), || {
            let conn = Connection::open(&db).expect("open");
            crate::schema::initialize(&conn).expect("init");
            for i in 0..10usize {
                insert_row(&conn, i, dim);
            }
            let idx = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("build");
            idx.save().expect("save");
            assert_eq!(idx.manifest().quant, "f16");
            assert!(db.with_extension("usearch").exists(), "f16 sidecar written");
        });
        // Under i8, the f16 sidecar must NOT be reused.
        with_quant(Some("i8"), || {
            let conn = Connection::open(&db).expect("open");
            let sidecar = db.with_extension("usearch");
            assert!(
                AnnIndex::try_load(&sidecar, dim, "stub-d8")
                    .expect("try_load")
                    .is_none(),
                "f16 sidecar must be rejected when i8 is active"
            );
            // open_or_build rebuilds under i8 (covers the same 10 rows).
            let idx = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("rebuild");
            assert_eq!(idx.manifest().quant, "i8");
            assert_eq!(idx.len(), 10, "rebuilt i8 index covers all rows");
        });
    }

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

    #[test]
    fn search_returns_nearest_rowid_first() {
        // Pin f16 (+ serialize via the env lock) so a concurrent quant-mutating
        // test can't flip this order-sensitive search assertion.
        with_quant(Some("f16"), || {
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
                    rusqlite::params![rowid],
                    |r| r.get(0),
                )
                .expect("map rowid");
            assert_eq!(mid, "m-000003");
        });
    }

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
        AnnIndex::open_or_build(&conn, dim, "model-a")
            .expect("a")
            .save()
            .expect("save");
        // Open under model B → manifest mismatch → rebuild (empty, no model-b rows).
        let idx = AnnIndex::open_or_build(&conn, dim, "model-b").expect("b");
        assert_eq!(idx.len(), 0, "rebuilt for model-b which has no rows");
    }

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
        for i in 0..10 {
            insert(&conn, i);
        }
        let idx = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("build");
        idx.save().expect("save");
        assert_eq!(idx.len(), 10);

        // Simulate another process: add 5 rows, invalidate 2 existing.
        for i in 10..15 {
            insert(&conn, i);
        }
        conn.execute("UPDATE memories SET invalidated_at='2026-02-01T00:00:00Z' WHERE memory_id IN ('m-000000','m-000001')", []).expect("invalidate");

        // Reopen → load sidecar (10) → reconcile (+5 new, -2 invalidated) = 13.
        let idx2 = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("reopen");
        assert_eq!(idx2.len(), 13);
    }

    #[test]
    fn recall_at_10_is_at_least_0_95_vs_brute_force() {
        with_quant(Some("f16"), || {
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
            let mut stmt = conn
                .prepare("SELECT rowid, embedding FROM memories")
                .unwrap();
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, Vec<u8>>(1)?)))
                .unwrap();
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
                let mut scored: Vec<(i64, f32)> = vectors
                    .iter()
                    .map(|(id, v)| (*id, cosine_similarity(q, v)))
                    .collect();
                scored.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap());
                let exact: std::collections::HashSet<i64> =
                    scored.iter().take(k).map(|(id, _)| *id).collect();
                let ann: std::collections::HashSet<i64> = idx
                    .search(q, k)
                    .unwrap()
                    .into_iter()
                    .map(|(id, _)| id)
                    .collect();
                hit += exact.intersection(&ann).count();
                total += k;
            }
            let recall = hit as f32 / total as f32;
            assert!(recall >= 0.95, "recall@10 = {recall} (want >= 0.95)");
        });
    }

    /// Insert one axis-peaked row directly into SQLite (bypassing the index).
    fn insert_row(conn: &Connection, i: usize, dim: usize) {
        let mut v = vec![0.01f32; dim];
        v[i % dim] = 1.0;
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                embedding, embedding_model)
             VALUES (?1,'project','fact',?2,?2,1.0,'{}','2026-01-01T00:00:00Z',0,0.0,?3,'stub-d8')",
            rusqlite::params![format!("m-{i:06}"), format!("t{i}"), encode_embedding(&v)],
        )
        .expect("insert");
    }

    #[test]
    fn parallel_build_indexes_all_rows() {
        // 2000 DISTINCT vectors exercise parallel_add's partitioning across
        // threads. We verify (a) every row is indexed (len) and (b) a sample of
        // rows self-retrieve — searching a row's own vector returns that row as
        // the nearest — which proves parallel_add stored the correct vectors.
        // Distinct (not identical) vectors are used deliberately: many byte-
        // identical points are a degenerate HNSW input that real embeddings
        // never produce. Pinned to f16 + serialized via the env lock so a
        // concurrent quant-mutating test can't perturb the search.
        with_quant(Some("f16"), || {
            let dim = 16;
            let n = 2000;
            let (conn, vectors) = seed_random(n, dim, "stub-d16");
            let idx = AnnIndex::build_from_conn(&conn, dim, "stub-d16").expect("build");
            assert_eq!(idx.len(), n, "all {n} rows indexed by parallel build");
            // Vector correctness: assert MEAN recall@10, not exact per-row
            // retrieval. HNSW is APPROXIMATE — even querying an indexed vector
            // returns it as the #1 hit only ~98-99% of the time — so an exact
            // top-k assertion would flake. A high mean recall proves parallel_add
            // stored the right vectors (mirrors `recall_guard_holds_under_f16`).
            let recall = measure_recall(&idx, &vectors, 10, 100);
            assert!(
                recall >= 0.9,
                "parallel-built index recall@10 = {recall} (want >= 0.9)"
            );
        });
    }

    #[test]
    fn is_stale_detects_new_rows() {
        let dim = 8;
        let dir = tempfile::tempdir().expect("tmp");
        let db = dir.path().join("brain.db");
        let conn = Connection::open(&db).expect("open");
        crate::schema::initialize(&conn).expect("init");
        for i in 0..10 {
            insert_row(&conn, i, dim);
        }
        let idx = AnnIndex::build_from_conn(&conn, dim, "stub-d8").expect("build");
        assert!(!idx.is_stale(&conn).expect("stale check"), "fresh index");
        // Add rows directly to SQLite, bypassing the index.
        for i in 10..15 {
            insert_row(&conn, i, dim);
        }
        assert!(
            idx.is_stale(&conn).expect("stale check"),
            "stale after out-of-band inserts"
        );
    }

    #[test]
    fn malformed_embedding_rows_do_not_keep_index_stale() {
        let dim = 8;
        let dir = tempfile::tempdir().expect("tmp");
        let db = dir.path().join("brain.db");
        let conn = Connection::open(&db).expect("open");
        crate::schema::initialize(&conn).expect("init");
        insert_row(&conn, 0, dim);
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                embedding, embedding_model)
             VALUES ('m-bad','project','fact','bad','bad',1.0,'{}',
                     '2026-01-01T00:00:00Z',0,0.0,?1,'stub-d8')",
            rusqlite::params![vec![1_u8, 2, 3]],
        )
        .expect("insert malformed embedding");

        let idx = AnnIndex::build_from_conn(&conn, dim, "stub-d8").expect("build");
        assert_eq!(idx.len(), 1, "only the valid vector is indexed");
        assert!(
            !idx.is_stale(&conn).expect("stale check"),
            "malformed high-rowid embeddings should not force repeated reconcile"
        );
    }

    #[test]
    fn reconcile_advances_watermark_past_malformed_delta() {
        let dim = 8;
        let dir = tempfile::tempdir().expect("tmp");
        let db = dir.path().join("brain.db");
        let conn = Connection::open(&db).expect("open");
        crate::schema::initialize(&conn).expect("init");
        insert_row(&conn, 0, dim);
        let mut idx = AnnIndex::build_from_conn(&conn, dim, "stub-d8").expect("build");

        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                embedding, embedding_model)
             VALUES ('m-bad-delta','project','fact','bad','bad',1.0,'{}',
                     '2026-01-01T00:00:00Z',0,0.0,?1,'stub-d8')",
            rusqlite::params![vec![1_u8, 2, 3]],
        )
        .expect("insert malformed embedding");

        idx.reconcile(&conn).expect("reconcile");
        assert_eq!(idx.len(), 1, "malformed delta is skipped");
        assert!(
            !idx.is_stale(&conn).expect("stale check"),
            "malformed delta should be skipped once, not retried forever"
        );
    }

    #[test]
    fn handle_for_query_reconciles_cached_index_on_new_rows() {
        // Regression for the bench staleness bug: a cached handle must reconcile
        // when SQLite has gained rows out-of-band of the warm add path.
        let dim = 8;
        let dir = tempfile::tempdir().expect("tmp");
        let db = dir.path().join("brain.db");
        let conn = Connection::open(&db).expect("open");
        crate::schema::initialize(&conn).expect("init");
        let n = 8usize;
        for i in 0..n {
            insert_row(&conn, i, dim);
        }
        let h1 = handle_for_query(&conn, dim, "stub-d8").expect("build");
        assert_eq!(h1.read().unwrap().len(), n, "initial build covers all rows");
        // Bulk-add M more active rows directly via SQL (no warm add path).
        let m = 5usize;
        for i in n..n + m {
            insert_row(&conn, i, dim);
        }
        let h2 = handle_for_query(&conn, dim, "stub-d8").expect("requery");
        assert!(Arc::ptr_eq(&h1, &h2), "same cached handle reused");
        assert_eq!(
            h2.read().unwrap().len(),
            n + m,
            "cached index reconciled to include bulk-added rows"
        );
    }

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
        conn.execute(
            "UPDATE memories SET invalidated_at='2026-02-01T00:00:00Z' WHERE memory_id='m-x'",
            [],
        )
        .unwrap();
        on_invalidate(&conn, "m-x");
        assert_eq!(cached_handle(&conn).unwrap().read().unwrap().len(), 0);
    }

    #[test]
    fn concurrent_same_key_builds_once() {
        // N threads racing `handle_for_query` on the SAME on-disk brain must all
        // receive ONE shared handle (built once under the per-key build lock).
        let dim = 8;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.db");
        let conn = Connection::open(&db).unwrap();
        crate::schema::initialize(&conn).unwrap();
        let n = 12usize;
        for i in 0..n {
            insert_row(&conn, i, dim);
        }
        drop(conn); // close our writer; each thread opens its own connection.
        let db_path = db.clone();
        let threads = 8usize;
        let mut handles = Vec::new();
        for _ in 0..threads {
            let p = db_path.clone();
            handles.push(std::thread::spawn(move || {
                let conn = Connection::open(&p).unwrap();
                handle_for_query(&conn, dim, "stub-d8").unwrap()
            }));
        }
        let results: Vec<Handle> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let first = results[0].clone();
        for h in &results[1..] {
            assert!(
                Arc::ptr_eq(&first, h),
                "all concurrent builds must share ONE handle (built once)"
            );
        }
        assert_eq!(
            first.read().unwrap().len(),
            n,
            "shared index covers all rows"
        );
    }

    #[test]
    fn build_persists_sidecar() {
        // A fresh build is background-saved to its sidecar so a restarted process
        // LOADS rather than rebuilds. Poll for the sidecar to appear.
        let dim = 8;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.db");
        let conn = Connection::open(&db).unwrap();
        crate::schema::initialize(&conn).unwrap();
        for i in 0..10 {
            insert_row(&conn, i, dim);
        }
        let _h = handle_for_query(&conn, dim, "stub-d8").unwrap();
        let sidecar = db.with_extension("usearch");
        let mut appeared = false;
        for _ in 0..100 {
            if sidecar.exists() {
                appeared = true;
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
        assert!(
            appeared,
            "background save must persist the sidecar within ~2s"
        );
        // A fresh open loads the persisted sidecar (manifest valid) — same count.
        let loaded = AnnIndex::open_or_build(&conn, dim, "stub-d8").expect("load sidecar");
        assert_eq!(loaded.len(), 10, "reloaded sidecar covers all rows");
    }

    #[test]
    fn warm_caches_handle() {
        let dim = 8;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.db");
        let conn = Connection::open(&db).unwrap();
        crate::schema::initialize(&conn).unwrap();
        for i in 0..6 {
            insert_row(&conn, i, dim);
        }
        assert!(cached_handle(&conn).is_none(), "cold before warm");
        warm(&conn, dim, "stub-d8").expect("warm");
        assert!(
            cached_handle(&conn).is_some(),
            "warm must build + cache the handle"
        );
    }

    #[test]
    fn invalidate_sidecar_removes_file_and_cache() {
        let dim = 8;
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("brain.db");
        let conn = Connection::open(&db).unwrap();
        crate::schema::initialize(&conn).unwrap();
        let _ = handle_for_query(&conn, dim, "stub-d8").unwrap();
        drop(
            handle_for_query(&conn, dim, "stub-d8")
                .unwrap()
                .read()
                .unwrap(),
        );
        cached_handle(&conn)
            .unwrap()
            .read()
            .unwrap()
            .save()
            .unwrap();
        assert!(db.with_extension("usearch").exists());
        invalidate_sidecar(&conn);
        assert!(!db.with_extension("usearch").exists());
        assert!(cached_handle(&conn).is_none());
    }
}
