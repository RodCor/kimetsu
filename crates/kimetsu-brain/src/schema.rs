use rusqlite::Connection;

use kimetsu_core::{KIMETSU_SCHEMA_VERSION, KimetsuResult};

pub fn initialize(conn: &Connection) -> KimetsuResult<()> {
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

        CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
            USING fts5(text, kind, scope);
        ",
    )?;

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
    add_column_if_missing(conn, "memories", "usefulness_score REAL NOT NULL DEFAULT 0.0")?;
    // MP-4d: invalidated_at is set by `kimetsu brain memory invalidate` so
    // the human reviewer can permanently retire a memory without rewriting
    // the trace. The broker excludes invalidated rows from retrieval.
    add_column_if_missing(conn, "memories", "invalidated_at TEXT")?;
    add_column_if_missing(conn, "memories", "invalidated_reason TEXT")?;

    let schema_version: i64 = conn.query_row(
        "SELECT value FROM schema_info WHERE key = 'kimetsu_schema_version'",
        [],
        |row| row.get(0),
    )?;

    if schema_version != KIMETSU_SCHEMA_VERSION {
        return Err(format!(
            "brain.db schema version {schema_version} does not match expected {KIMETSU_SCHEMA_VERSION}; run `kimetsu brain rebuild`"
        )
        .into());
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
