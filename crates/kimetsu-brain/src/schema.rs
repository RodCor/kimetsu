use rusqlite::Connection;

use kimetsu_core::KimetsuResult;

/// Register the sqlite-vec extension exactly once, process-wide, so every
/// subsequently-opened connection can use `vec0` virtual tables.
///
/// Must be called BEFORE any `Connection::open` that will use `vec0`.
/// Idempotent: guarded by a `Once` so calling it from all four DB-open
/// paths is harmless.
///
/// On lean (no `embeddings` feature) builds this is a no-op — the function
/// body is compiled away and sqlite-vec is not linked.
pub(crate) fn ensure_vec_extension_registered() {
    #[cfg(feature = "embeddings")]
    {
        use std::sync::Once;
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            // SAFETY: sqlite3_auto_extension registers a C init fn pointer with
            // the rusqlite-bundled SQLite. sqlite_vec::sqlite3_vec_init is the
            // matching extension entry-point, compiled from sqlite-vec.c with
            // -DSQLITE_CORE so it links directly into the same SQLite instance.
            // The transmute converts the zero-argument C fn pointer to the
            // three-argument xEntryPoint signature expected by
            // sqlite3_auto_extension — this is the pattern from the
            // sqlite-vec crate's own tests and is safe on all supported targets.
            #[allow(clippy::missing_transmute_annotations)]
            unsafe {
                rusqlite::ffi::sqlite3_auto_extension(Some(std::mem::transmute(
                    sqlite_vec::sqlite3_vec_init as *const (),
                )));
            }
        });
    }
}

pub fn initialize(conn: &Connection) -> KimetsuResult<()> {
    create_baseline(conn)?;
    crate::migrate::run_migrations(conn)?;
    Ok(())
}

/// Create the baseline v1 schema (pragmas + all tables/indexes/FTS as of the
/// original v1 shape). Seeds `schema_info` with version **1** so the migration
/// runner knows where to start. On an existing DB every CREATE is a no-op
/// (`IF NOT EXISTS`).
fn create_baseline(conn: &Connection) -> KimetsuResult<()> {
    conn.pragma_update(None, "journal_mode", "WAL")?;
    conn.pragma_update(None, "busy_timeout", 5_000)?;

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
    Ok(())
}

pub fn validate(conn: &Connection) -> KimetsuResult<()> {
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
    // 1. Fresh init reaches v2 with full shape
    // ------------------------------------------------------------------
    #[test]
    fn fresh_init_reaches_v2_with_full_shape() {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        initialize(&conn).expect("initialize");

        // Version must be 2.
        assert_eq!(
            migrate::current_version(&conn).expect("current_version"),
            2,
            "fresh DB must be at schema version 2 after initialize"
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

        // Tables added by the migration exist.
        assert!(
            table_exists(&conn, "memory_citations"),
            "memory_citations table must exist"
        );
        assert!(
            table_exists(&conn, "memory_conflicts"),
            "memory_conflicts table must exist"
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
            2,
            "version must still be 2"
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
    // 3. Idempotent initialize: calling initialize twice succeeds, version stays 2
    // ------------------------------------------------------------------
    #[test]
    fn idempotent_initialize_twice() {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        initialize(&conn).expect("first initialize");
        initialize(&conn).expect("second initialize must not error");
        assert_eq!(
            migrate::current_version(&conn).expect("current_version"),
            2,
            "version must still be 2 after double initialize"
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
    // D1a — sqlite-vec linking + KNN proof (embeddings feature only)
    // ------------------------------------------------------------------
    #[cfg(feature = "embeddings")]
    #[test]
    fn sqlite_vec_extension_links_and_knn_runs() {
        // Register the extension before opening any connection.
        ensure_vec_extension_registered();

        let conn = Connection::open_in_memory().unwrap();

        // Prove the extension is loaded: create a vec0 virtual table with a
        // 4-dimensional float vector column.
        conn.execute_batch("CREATE VIRTUAL TABLE vt USING vec0(embedding float[4]);")
            .expect("vec0 virtual table must create — proves the extension linked and loaded");

        // Insert a few vectors. sqlite-vec accepts a JSON array literal.
        conn.execute(
            "INSERT INTO vt(rowid, embedding) VALUES (1, '[1.0, 0.0, 0.0, 0.0]')",
            [],
        )
        .unwrap();
        conn.execute(
            "INSERT INTO vt(rowid, embedding) VALUES (2, '[0.0, 1.0, 0.0, 0.0]')",
            [],
        )
        .unwrap();

        // KNN query: nearest to [0.9, 0.1, 0, 0] should be rowid 1.
        // vec0 requires `embedding MATCH <query>` + `LIMIT k` for KNN.
        let nearest: i64 = conn
            .query_row(
                "SELECT rowid FROM vt \
                 WHERE embedding MATCH '[0.9, 0.1, 0.0, 0.0]' \
                 ORDER BY distance \
                 LIMIT 1",
                [],
                |r| r.get(0),
            )
            .expect("vec0 KNN MATCH query must succeed — proves vec0 executes on this MSVC host");

        assert_eq!(
            nearest, 1,
            "nearest vector to [0.9,0.1,0,0] must be rowid 1 (the [1,0,0,0] vector)"
        );
    }
}
