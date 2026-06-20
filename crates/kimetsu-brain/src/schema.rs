use rusqlite::Connection;

use kimetsu_core::KimetsuResult;

/// Apply performance-tuning SQLite pragmas to `conn`.
///
/// Safe on both read-write AND read-only connections: pragmas that cannot
/// be set on a read-only DB (WAL mode, mmap_size) are skipped when they
/// error, so the same function is called unconditionally from every open path.
///
/// Pragmas set:
/// - `cache_size = -65536`      → 64 MiB page cache (negative = KiB)
/// - `mmap_size = 268435456`    → 256 MiB memory-mapped I/O window
/// - `synchronous = NORMAL`     → safe under WAL; avoids full fsync per commit
/// - `temp_store = MEMORY`      → keep temp tables / sort buffers in RAM
///
/// `journal_mode = WAL` and `busy_timeout` are set by `create_baseline`
/// (the read-write init path); they are NOT repeated here because
/// `PRAGMA journal_mode` is a structural change that errors on read-only
/// connections (the mode is already persisted in the DB file header).
pub fn apply_pragmas(conn: &Connection) -> KimetsuResult<()> {
    // cache_size and temp_store are safe on any connection.
    conn.pragma_update(None, "cache_size", -65536_i64)?;
    conn.pragma_update(None, "temp_store", "MEMORY")?;

    // mmap_size and synchronous may fail on a read-only connection opened
    // against a DB that's being written by another process in WAL mode.
    // Best-effort: ignore errors from these two.
    let _ = conn.pragma_update(None, "mmap_size", 268_435_456_i64);
    let _ = conn.pragma_update(None, "synchronous", "NORMAL");

    Ok(())
}

pub fn initialize(conn: &Connection) -> KimetsuResult<()> {
    apply_pragmas(conn)?;
    create_baseline(conn)?;
    crate::migrate::run_migrations(conn)?;

    // T3c: the old brute-force `memory_vec` vec0 virtual table is gone (usearch
    // supersedes it). Best-effort drop to reclaim space in upgraded brains.
    //
    // Best-effort: a vec0 vtable can't be dropped without the (now-removed)
    // sqlite-vec module loaded, so this DROP raises "no such module: vec0" on
    // upgraded brains. We deliberately ignore the Result so that error can NEVER
    // propagate and break connection-open. An orphaned, never-accessed
    // memory_vec is harmless — SQLite loads a vtable module lazily, only on
    // access, and nothing in the codebase queries memory_vec anymore. New brains
    // never create it.
    let _ = conn.execute_batch("DROP TABLE IF EXISTS memory_vec;");

    Ok(())
}

/// Test seam: exposes `create_baseline` so integration tests in sibling
/// modules can build a v1 DB without going through the full `initialize`
/// (which would run all migrations and advance to the current version).
#[cfg(test)]
pub fn create_baseline_for_test(conn: &Connection) -> KimetsuResult<()> {
    create_baseline(conn)
}

/// Create the baseline v1 schema (pragmas + all tables/indexes/FTS as of the
/// original v1 shape). Seeds `schema_info` with version **1** so the migration
/// runner knows where to start. On an existing DB every CREATE is a no-op
/// (`IF NOT EXISTS`).
fn create_baseline(conn: &Connection) -> KimetsuResult<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 15_000)?;

    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS schema_info (
            key TEXT PRIMARY KEY,
            value INTEGER NOT NULL
        );

        INSERT OR IGNORE INTO schema_info (key, value)
        VALUES ('kimetsu_schema_version', 1);

        CREATE TABLE IF NOT EXISTS runs (
            run_id TEXT PRIMARY KEY,
            project_id TEXT NOT NULL,
            task TEXT NOT NULL,
            started_at TEXT NOT NULL,
            ended_at TEXT,
            terminal_kind TEXT,
            model TEXT,
            total_cost_usd REAL NOT NULL DEFAULT 0
        );

        CREATE TABLE IF NOT EXISTS events (
            event_id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            ts TEXT NOT NULL,
            kind TEXT NOT NULL,
            schema_version INTEGER NOT NULL,
            payload_json TEXT NOT NULL
        );

        CREATE INDEX IF NOT EXISTS idx_events_run_ts ON events (run_id, ts);
        CREATE INDEX IF NOT EXISTS idx_events_kind_ts ON events (kind, ts);

        CREATE TABLE IF NOT EXISTS sources (
            source_id TEXT PRIMARY KEY,
            kind TEXT NOT NULL,
            ref TEXT NOT NULL,
            hash TEXT,
            added_at TEXT NOT NULL
        );

        CREATE TABLE IF NOT EXISTS memories (
            memory_id TEXT PRIMARY KEY,
            scope TEXT NOT NULL,
            kind TEXT NOT NULL,
            text TEXT NOT NULL,
            normalized_text TEXT NOT NULL,
            confidence REAL NOT NULL,
            source_event_id TEXT,
            provenance_snapshot_json TEXT NOT NULL,
            created_at TEXT NOT NULL,
            last_used_at TEXT,
            use_count INTEGER NOT NULL DEFAULT 0,
            usefulness_score REAL NOT NULL DEFAULT 0.0,
            invalidated_at TEXT,
            invalidated_reason TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_memories_scope_kind_norm
            ON memories (scope, kind, normalized_text);
        CREATE TABLE IF NOT EXISTS memory_proposals (
            proposal_id TEXT PRIMARY KEY,
            run_id TEXT NOT NULL,
            scope TEXT NOT NULL,
            kind TEXT NOT NULL,
            text TEXT NOT NULL,
            rationale TEXT NOT NULL,
            proposed_confidence REAL NOT NULL,
            source_event_ids_json TEXT NOT NULL,
            status TEXT NOT NULL,
            decided_at TEXT,
            decided_by TEXT,
            decided_reason TEXT
        );

        CREATE INDEX IF NOT EXISTS idx_memory_proposals_status_run
            ON memory_proposals (status, run_id);

        CREATE TABLE IF NOT EXISTS repo_files (
            repo_root TEXT NOT NULL,
            path TEXT NOT NULL,
            hash TEXT NOT NULL,
            size INTEGER NOT NULL,
            mtime TEXT NOT NULL,
            language_guess TEXT NOT NULL,
            snippet TEXT NOT NULL,
            PRIMARY KEY (repo_root, path)
        );

        CREATE INDEX IF NOT EXISTS idx_repo_files_language
            ON repo_files (repo_root, language_guess);

        CREATE TABLE IF NOT EXISTS repo_manifests (
            repo_root TEXT NOT NULL,
            manifest_path TEXT NOT NULL,
            manifest_kind TEXT NOT NULL,
            parsed_summary_json TEXT NOT NULL,
            hash TEXT NOT NULL,
            mtime TEXT NOT NULL,
            PRIMARY KEY (repo_root, manifest_path)
        );

        CREATE VIRTUAL TABLE IF NOT EXISTS repo_files_fts
            USING fts5(repo_root, path, snippet, language_guess);

        CREATE VIRTUAL TABLE IF NOT EXISTS repo_manifests_fts
            USING fts5(repo_root UNINDEXED, manifest_path, manifest_kind, parsed_summary_json);

        CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
            USING fts5(memory_id UNINDEXED, text, kind, scope);
        ",
    )?;
    Ok(())
}

/// The v1→v2 migration: folds every historical in-place patch
/// (additive columns, citations/conflicts tables, FTS reshapes) into one
/// idempotent step. Real-world DBs were all stamped v1, so this brings
/// them — and freshly-created baselines — to the v2 shape.
///
/// NOTE: this function runs INSIDE a transaction owned by the migration
/// runner. Do NOT issue BEGIN/COMMIT here.
pub(crate) fn migrate_v1_to_v2(conn: &Connection) -> KimetsuResult<()> {
    // In-place column additions for v0.1 brain.db files predating each
    // column. Each ALTER is idempotent: we ignore the duplicate-column error
    // so an upgraded binary opens an older brain.db without forcing a
    // `kimetsu brain rebuild`.
    add_column_if_missing(conn, "memory_proposals", "decided_reason TEXT")?;
    // MP-4a: usefulness_score tracks the net outcome correlation of each
    // memory. Incremented when a memory was in the context of a run.finished
    // event; decremented for run.failed with category != "Gate". Used by the
    // broker (MP-4b) to bias retrieval and by auto-accept (MP-4c) to shadow
    // re-acceptance of low-usefulness patterns.
    add_column_if_missing(
        conn,
        "memories",
        "usefulness_score REAL NOT NULL DEFAULT 0.0",
    )?;
    // MP-4d: invalidated_at is set by `kimetsu brain memory invalidate` so
    // the human reviewer can permanently retire a memory without rewriting
    // the trace. The broker excludes invalidated rows from retrieval.
    add_column_if_missing(conn, "memories", "invalidated_at TEXT")?;
    add_column_if_missing(conn, "memories", "invalidated_reason TEXT")?;
    // v0.4.2: hybrid retrieval scaffolding.
    //   * `embedding`        — little-endian f32 BLOB, NULL on pre-v0.4.2 rows
    //   * `embedding_model`  — opaque model id ("bge-small-en-v1.5",
    //                          "stub-d8", "noop"), NULL when no embedding
    //                          was produced (e.g. NoopEmbedder).
    // Retrieval reads both: when `embedding` is non-NULL AND
    // `embedding_model` matches the active embedder's id, the cosine
    // score contributes to ranking. Otherwise the row is scored
    // lexical-only (FTS) — exact v0.4.1 behavior, no regression.
    add_column_if_missing(conn, "memories", "embedding BLOB")?;
    add_column_if_missing(conn, "memories", "embedding_model TEXT")?;
    // v0.5.1: timestamp of the most recent time this memory was
    // cited AND the citing run ended in run.finished. Used by the
    // broker's decay term: `effective = base * exp(-ln(2) *
    // age_days / half_life)` so a memory that helped 6 months ago
    // doesn't outvote one that helped yesterday.
    //
    // Distinct from `last_used_at` (bumped on every retrieval) —
    // `last_useful_at` only tracks confirmed successful uses
    // attributed via the v0.5.0 cite_memory tool.
    //
    // NULL on pre-v0.5.1 rows + on memories that have never been
    // cited successfully. Retrieval falls back to `created_at` for
    // the decay reference timestamp so brand-new memories don't
    // get penalized for never having been cited yet.
    add_column_if_missing(conn, "memories", "last_useful_at TEXT")?;
    conn.execute_batch(
        "
        CREATE INDEX IF NOT EXISTS idx_memories_active_created
            ON memories (invalidated_at, created_at);
        ",
    )?;
    // v0.5.1: per-run, per-turn memory citation log.
    //
    // The model emits a `memory.cited` event (via the `cite_memory`
    // tool) when it consciously leveraged a retrieved capsule. The
    // projector mirrors each event into this table so the
    // `kimetsu brain memory blame <run-id>` CLI + MCP tool can
    // walk attribution without re-scanning the `events` table.
    //
    // Multiple citations per turn are allowed (a turn can use
    // several memories). The PK includes `turn` so re-cites of the
    // same memory across turns don't collide.
    //
    // Usefulness scoring upgrade (v0.5.1 sibling change in
    // `projector::apply_run_finished` / `apply_run_failed`): cited
    // memories get the full +/-1 delta; retrieved-but-not-cited
    // memories get a weaker +/-0.1 — the strong signal goes to
    // memories the model actually reasoned with, the weak signal
    // stays for the silent passengers.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS memory_citations (
            run_id     TEXT NOT NULL,
            memory_id  TEXT NOT NULL,
            turn       INTEGER NOT NULL,
            cited_at   TEXT NOT NULL,
            rationale  TEXT,
            PRIMARY KEY (run_id, memory_id, turn)
        );
        CREATE INDEX IF NOT EXISTS idx_citations_run
            ON memory_citations (run_id);
        CREATE INDEX IF NOT EXISTS idx_citations_memory
            ON memory_citations (memory_id);
        ",
    )?;
    // v0.5.2: conflict-detection log. When `add_memory` (or
    // `add_user_memory`) inserts a new capsule whose embedding is
    // close to an existing capsule in the same scope but whose
    // normalized text differs, the conflict is logged here for
    // operator review via `kimetsu brain memory conflicts`.
    //
    // We use `INSERT OR IGNORE` on (new_memory_id,
    // existing_memory_id) so a re-scan over the same pair stays
    // idempotent. `resolved_at` IS NULL marks an open conflict;
    // `resolution` stores `'kept_new'`, `'kept_existing'`, or
    // `'kept_both'` after operator decision.
    //
    // Embedder-only: conflict detection runs ONLY when a real
    // embedder is available (cosine math requires it). NoopEmbedder
    // builds silently skip the scan and never write to this table,
    // so pre-v0.5.2 brain.db files opened by a lean build see no
    // new rows.
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS memory_conflicts (
            conflict_id        TEXT PRIMARY KEY,
            new_memory_id      TEXT NOT NULL,
            existing_memory_id TEXT NOT NULL,
            scope              TEXT NOT NULL,
            kind               TEXT NOT NULL,
            similarity         REAL NOT NULL,
            detected_at        TEXT NOT NULL,
            resolved_at        TEXT,
            resolution         TEXT,
            UNIQUE (new_memory_id, existing_memory_id)
        );
        CREATE INDEX IF NOT EXISTS idx_conflicts_unresolved
            ON memory_conflicts (resolved_at, detected_at);
        CREATE INDEX IF NOT EXISTS idx_conflicts_new_memory
            ON memory_conflicts (new_memory_id);
        ",
    )?;
    ensure_memories_fts_shape(conn)?;
    ensure_repo_manifests_fts_shape(conn)?;

    // v1.0 (Tier-1 perf): covering index for scope + embedding_model
    // filtering in conflict detection and ANN pool fetch. Additive — the
    // IF NOT EXISTS guard makes it idempotent on already-upgraded DBs.
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_scope_model_active
             ON memories (scope, embedding_model, invalidated_at);",
    )?;

    Ok(())
}

/// The v2→v3 migration: add `superseded_by` column to `memories`.
///
/// Superseded rows point at their survivor via this column.  Retrieval,
/// listing, and export already filter `invalidated_at IS NULL`; this
/// migration adds a companion `AND superseded_by IS NULL` guard to all
/// such queries (applied in `context.rs`, `user_brain.rs`, and
/// `project.rs`).  The FTS index and ANN index keep the same "remove on
/// supersede" semantics as invalidation.
///
/// NOTE: this function runs INSIDE a transaction owned by the migration
/// runner. Do NOT issue BEGIN/COMMIT here.
pub(crate) fn migrate_v2_to_v3(conn: &Connection) -> KimetsuResult<()> {
    add_column_if_missing(conn, "memories", "superseded_by TEXT")?;
    conn.execute_batch(
        "CREATE INDEX IF NOT EXISTS idx_memories_superseded
             ON memories (superseded_by);",
    )?;
    Ok(())
}

/// The v3→v4 migration: add the `memory_edges` typed-edge projection table.
///
/// This table is the storage substrate for the S5.2 `GraphLiteBackend`.
/// It is a **projection** (derivable from the event log) — `reset_projection`
/// clears it and `rebuild_in_place` repopulates it by replaying events.
///
/// Edge types:
/// * `supersedes`         — populated NOW from `memory.superseded` events.
///   The surviving memory acquires a directed edge toward each member it
///   absorbed.  Edge direction: `src_id` (survivor) → `dst_id` (member).
/// * `refines`            — reserved; populated by the live write path when
///   a `memory.accepted` event carries `refines_id` in the payload
///   (Flagship 1 / Story 1.7).
/// * `dead_end_of`        — reserved; populated when an episodic resume
///   event closes a task-dead-end chain.
/// * `decision_touches`   — reserved; decision memory → touched file paths.
/// * `lesson_from`        — reserved; lesson memory → source memory / run.
///
/// NOTE: this function runs INSIDE a transaction owned by the migration
/// runner.  Do NOT issue BEGIN/COMMIT here.
pub(crate) fn migrate_v3_to_v4(conn: &Connection) -> KimetsuResult<()> {
    conn.execute_batch(
        "
        CREATE TABLE IF NOT EXISTS memory_edges (
            src_id      TEXT NOT NULL,
            dst_id      TEXT NOT NULL,
            edge_type   TEXT NOT NULL,
            created_at  TEXT NOT NULL,
            PRIMARY KEY (src_id, dst_id, edge_type)
        );

        CREATE INDEX IF NOT EXISTS idx_memory_edges_src
            ON memory_edges (src_id, edge_type);

        CREATE INDEX IF NOT EXISTS idx_memory_edges_dst
            ON memory_edges (dst_id, edge_type);
        ",
    )?;
    Ok(())
}

pub fn validate(conn: &Connection) -> KimetsuResult<()> {
    // Apply performance pragmas on read-only connections too. The helper
    // skips pragmas that error (journal_mode/mmap_size on some read-only
    // opens), so this is always safe to call here.
    apply_pragmas(conn)?;
    use kimetsu_core::KIMETSU_SCHEMA_VERSION;
    let current: i64 = conn.query_row(
        "SELECT value FROM schema_info WHERE key = 'kimetsu_schema_version'",
        [],
        |row| row.get(0),
    )?;
    let target = KIMETSU_SCHEMA_VERSION;
    if current > target {
        return Err(format!(
            "brain.db schema version {current} was written by a newer Kimetsu (this binary expects {target}); upgrade Kimetsu"
        )
        .into());
    }
    if current < target {
        return Err(Box::new(crate::migrate::SchemaNeedsMigration {
            from: current,
            to: target,
        }));
    }
    Ok(())
}

fn add_column_if_missing(conn: &Connection, table: &str, column_def: &str) -> KimetsuResult<()> {
    let column_name = column_def
        .split_whitespace()
        .next()
        .ok_or("empty column definition")?;
    let exists: bool = {
        let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
        let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
        let mut found = false;
        for row in rows {
            if row? == column_name {
                found = true;
                break;
            }
        }
        found
    };
    if !exists {
        conn.execute_batch(&format!("ALTER TABLE {table} ADD COLUMN {column_def};"))?;
    }
    Ok(())
}

fn ensure_memories_fts_shape(conn: &Connection) -> KimetsuResult<()> {
    if table_has_column(conn, "memories_fts", "memory_id")? {
        return Ok(());
    }
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS memories_fts;
        CREATE VIRTUAL TABLE memories_fts
            USING fts5(memory_id UNINDEXED, text, kind, scope);
        INSERT INTO memories_fts (memory_id, text, kind, scope)
            SELECT memory_id, text, kind, scope FROM memories;
        ",
    )?;
    Ok(())
}

fn ensure_repo_manifests_fts_shape(conn: &Connection) -> KimetsuResult<()> {
    if table_has_column(conn, "repo_manifests_fts", "parsed_summary_json")? {
        return Ok(());
    }
    conn.execute_batch(
        "
        DROP TABLE IF EXISTS repo_manifests_fts;
        CREATE VIRTUAL TABLE repo_manifests_fts
            USING fts5(repo_root UNINDEXED, manifest_path, manifest_kind, parsed_summary_json);
        INSERT INTO repo_manifests_fts (
            repo_root, manifest_path, manifest_kind, parsed_summary_json
        )
            SELECT repo_root, manifest_path, manifest_kind, parsed_summary_json
            FROM repo_manifests;
        ",
    )?;
    Ok(())
}

fn table_has_column(conn: &Connection, table: &str, column: &str) -> KimetsuResult<bool> {
    let mut stmt = conn.prepare(&format!("PRAGMA table_info({table})"))?;
    let rows = stmt.query_map([], |row| row.get::<_, String>(1))?;
    for row in rows {
        if row? == column {
            return Ok(true);
        }
    }
    Ok(false)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::migrate;
    use rusqlite::Connection;

    fn column_names(conn: &Connection, table: &str) -> Vec<String> {
        let mut stmt = conn
            .prepare(&format!("PRAGMA table_info({table})"))
            .expect("prepare table_info");
        stmt.query_map([], |row| row.get::<_, String>(1))
            .expect("query_map")
            .map(|r| r.expect("row"))
            .collect()
    }

    fn table_exists(conn: &Connection, name: &str) -> bool {
        let count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='table' AND name=?1",
                [name],
                |r| r.get(0),
            )
            .unwrap_or(0);
        count > 0
    }

    // ------------------------------------------------------------------
    // 1. Fresh init reaches v4 with full shape
    // ------------------------------------------------------------------
    #[test]
    fn fresh_init_reaches_v4_with_full_shape() {
        use kimetsu_core::KIMETSU_SCHEMA_VERSION;
        let conn = Connection::open_in_memory().expect("open_in_memory");
        initialize(&conn).expect("initialize");

        // Version must be at target (currently 4).
        assert_eq!(
            migrate::current_version(&conn).expect("current_version"),
            KIMETSU_SCHEMA_VERSION,
            "fresh DB must be at current schema version after initialize"
        );

        // Post-migration columns exist on `memories`.
        let mem_cols = column_names(&conn, "memories");
        assert!(
            mem_cols.contains(&"embedding".to_string()),
            "memories must have `embedding` column"
        );
        assert!(
            mem_cols.contains(&"embedding_model".to_string()),
            "memories must have `embedding_model` column"
        );
        assert!(
            mem_cols.contains(&"last_useful_at".to_string()),
            "memories must have `last_useful_at` column"
        );
        // v3: superseded_by column
        assert!(
            mem_cols.contains(&"superseded_by".to_string()),
            "memories must have `superseded_by` column after v3 migration"
        );

        // Tables added by the migrations exist.
        assert!(
            table_exists(&conn, "memory_citations"),
            "memory_citations table must exist"
        );
        assert!(
            table_exists(&conn, "memory_conflicts"),
            "memory_conflicts table must exist"
        );
        // v4: typed-edge projection table
        assert!(
            table_exists(&conn, "memory_edges"),
            "memory_edges table must exist after v4 migration"
        );
    }

    // ------------------------------------------------------------------
    // 2. Idempotent re-run: run_migrations again after initialize is a no-op
    // ------------------------------------------------------------------
    #[test]
    fn idempotent_rerun_preserves_data() {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        initialize(&conn).expect("initialize");

        // Insert a memories row.
        conn.execute_batch(
            "INSERT INTO memories (
                memory_id, scope, kind, text, normalized_text,
                confidence, provenance_snapshot_json, created_at,
                use_count, usefulness_score
             ) VALUES (
                'mem-1', 'test', 'fact', 'hello world', 'hello world',
                0.9, '{}', '2024-01-01T00:00:00Z',
                0, 0.0
             );",
        )
        .expect("insert row");

        // Re-run migrations — must be a no-op at target.
        let outcome = migrate::run_migrations(&conn).expect("second run_migrations");
        assert_eq!(
            outcome.applied,
            Vec::<i64>::new(),
            "second run_migrations must apply nothing"
        );
        assert_eq!(
            migrate::current_version(&conn).expect("current_version"),
            4,
            "version must still be 4"
        );

        // Data must be intact.
        let text: String = conn
            .query_row(
                "SELECT text FROM memories WHERE memory_id = 'mem-1'",
                [],
                |r| r.get(0),
            )
            .expect("row must survive");
        assert_eq!(text, "hello world");
    }

    // ------------------------------------------------------------------
    // 3. Idempotent initialize: calling initialize twice succeeds, version stays at target
    // ------------------------------------------------------------------
    #[test]
    fn idempotent_initialize_twice() {
        use kimetsu_core::KIMETSU_SCHEMA_VERSION;
        let conn = Connection::open_in_memory().expect("open_in_memory");
        initialize(&conn).expect("first initialize");
        initialize(&conn).expect("second initialize must not error");
        assert_eq!(
            migrate::current_version(&conn).expect("current_version"),
            KIMETSU_SCHEMA_VERSION,
            "version must still be at target after double initialize"
        );
    }

    // ------------------------------------------------------------------
    // Fix 1: apply_pragmas sets the tuned cache_size on both RW and RO
    // ------------------------------------------------------------------
    #[test]
    fn apply_pragmas_sets_cache_size_on_rw_connection() {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        initialize(&conn).expect("initialize");
        // After initialize (which calls apply_pragmas), cache_size must be -65536
        // (the negative-KiB form we set). SQLite may return it as a page count
        // (positive) or keep the -KiB form; we just assert it's not the default
        // -2000 pages, which is what SQLite uses without any pragma_update.
        let cache_size: i64 = conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .expect("cache_size query");
        assert_ne!(
            cache_size, -2000,
            "cache_size must have been updated from the 2 MiB default, got {cache_size}"
        );
        // The tuned value should be a large negative number (KiB) or a large
        // positive page count — either way not the stock default.
        assert!(
            !(-2000..=2000).contains(&cache_size),
            "cache_size should reflect the 64 MiB tuning (not default -2000), got {cache_size}"
        );
    }

    /// Fix 1: validate() (the read-only open path) also calls apply_pragmas.
    /// We can't open a true read-only connection to an in-memory DB via OpenFlags,
    /// so we exercise the helper directly and verify it doesn't error.
    #[test]
    fn apply_pragmas_does_not_error_on_in_memory_conn() {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        apply_pragmas(&conn).expect("apply_pragmas must not error on a fresh in-memory conn");
        let cache_size: i64 = conn
            .pragma_query_value(None, "cache_size", |row| row.get(0))
            .expect("cache_size");
        assert!(
            !(-2000..=2000).contains(&cache_size),
            "apply_pragmas must update cache_size from the default, got {cache_size}"
        );
    }

    // Helper: seed an in-memory conn with only schema_info at the given version.
    fn seed_schema_info(version: i64) -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        conn.execute_batch(&format!(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             INSERT INTO schema_info VALUES ('kimetsu_schema_version', {version});"
        ))
        .expect("seed schema_info");
        conn
    }

    // ------------------------------------------------------------------
    // A5-1. validate Ok at target version
    // ------------------------------------------------------------------
    #[test]
    fn validate_ok_at_target() {
        use kimetsu_core::KIMETSU_SCHEMA_VERSION;
        let conn = seed_schema_info(KIMETSU_SCHEMA_VERSION);
        validate(&conn).expect("validate at target must return Ok(())");
    }

    // ------------------------------------------------------------------
    // A5-2. validate returns SchemaNeedsMigration for an older DB
    // ------------------------------------------------------------------
    #[test]
    fn validate_returns_needs_migration_for_older_db() {
        use kimetsu_core::KIMETSU_SCHEMA_VERSION;
        let conn = seed_schema_info(1);
        let err = validate(&conn).expect_err("validate on v1 DB must return Err");
        let snm = err
            .downcast_ref::<migrate::SchemaNeedsMigration>()
            .expect("error must downcast to SchemaNeedsMigration");
        assert_eq!(
            snm,
            &migrate::SchemaNeedsMigration {
                from: 1,
                to: KIMETSU_SCHEMA_VERSION,
            },
            "SchemaNeedsMigration must carry the correct from/to versions"
        );
    }

    // ------------------------------------------------------------------
    // A5-v3. v2→v3 migration adds superseded_by + index
    // ------------------------------------------------------------------
    #[test]
    fn v2_to_v3_migration_adds_superseded_by() {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        // Seed a v2 DB manually (baseline + v1→v2 migration, no v2→v3).
        create_baseline(&conn).expect("create_baseline");
        migrate_v1_to_v2(&conn).expect("migrate_v1_to_v2");
        conn.execute(
            "UPDATE schema_info SET value = 2 WHERE key = 'kimetsu_schema_version'",
            [],
        )
        .expect("set v2");

        // superseded_by must NOT exist yet.
        let cols_before = column_names(&conn, "memories");
        assert!(
            !cols_before.contains(&"superseded_by".to_string()),
            "superseded_by must not exist before v3 migration"
        );

        // Run v2→v3.
        migrate_v2_to_v3(&conn).expect("migrate_v2_to_v3");

        // Now it must exist.
        let cols_after = column_names(&conn, "memories");
        assert!(
            cols_after.contains(&"superseded_by".to_string()),
            "superseded_by must exist after v3 migration"
        );

        // Index must also exist.
        let idx_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_memories_superseded'",
                [],
                |r| r.get(0),
            )
            .expect("query index");
        assert_eq!(
            idx_count, 1,
            "idx_memories_superseded must exist after v3 migration"
        );
    }

    // ------------------------------------------------------------------
    // A5-3. validate hard-errors (non-SchemaNeedsMigration) for a newer DB
    // ------------------------------------------------------------------
    #[test]
    fn validate_hard_errors_for_newer_db() {
        let conn = seed_schema_info(999);
        let err = validate(&conn).expect_err("validate on v999 DB must return Err");
        assert!(
            err.downcast_ref::<migrate::SchemaNeedsMigration>()
                .is_none(),
            "error for a newer DB must NOT downcast to SchemaNeedsMigration"
        );
        let msg = err.to_string();
        assert!(
            msg.contains("newer"),
            "error message must contain 'newer', got: {msg}"
        );
    }

    // ------------------------------------------------------------------
    // S5.2-v4. v3→v4 migration adds memory_edges table + indexes
    // ------------------------------------------------------------------
    #[test]
    fn v3_to_v4_migration_adds_memory_edges() {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        // Build a v3 DB (baseline + v1→v2 + v2→v3, no v3→v4).
        create_baseline(&conn).expect("create_baseline");
        migrate_v1_to_v2(&conn).expect("migrate_v1_to_v2");
        migrate_v2_to_v3(&conn).expect("migrate_v2_to_v3");
        conn.execute(
            "UPDATE schema_info SET value = 3 WHERE key = 'kimetsu_schema_version'",
            [],
        )
        .expect("set v3");

        // memory_edges must NOT exist yet.
        assert!(
            !table_exists(&conn, "memory_edges"),
            "memory_edges must not exist before v4 migration"
        );

        // Run v3→v4.
        migrate_v3_to_v4(&conn).expect("migrate_v3_to_v4");

        // Table must now exist.
        assert!(
            table_exists(&conn, "memory_edges"),
            "memory_edges must exist after v4 migration"
        );

        // Indexes must exist.
        let src_idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_memory_edges_src'",
                [],
                |r| r.get(0),
            )
            .expect("query idx_memory_edges_src");
        assert_eq!(src_idx, 1, "idx_memory_edges_src must exist");

        let dst_idx: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type='index' AND name='idx_memory_edges_dst'",
                [],
                |r| r.get(0),
            )
            .expect("query idx_memory_edges_dst");
        assert_eq!(dst_idx, 1, "idx_memory_edges_dst must exist");
    }
}
