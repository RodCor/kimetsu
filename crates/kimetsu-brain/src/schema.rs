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

        CREATE VIRTUAL TABLE IF NOT EXISTS repo_manifests_fts
            USING fts5(repo_root UNINDEXED, manifest_path, manifest_kind, parsed_summary_json);

        CREATE VIRTUAL TABLE IF NOT EXISTS memories_fts
            USING fts5(memory_id UNINDEXED, text, kind, scope);
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

pub fn validate(conn: &Connection) -> KimetsuResult<()> {
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
