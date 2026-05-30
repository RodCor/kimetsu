//! Embedding backfill (`kimetsu brain reindex`) for v0.4.3.
//!
//! Iterates active memories whose stored `embedding_model` doesn't
//! match the active embedder's `model_id` (including NULL — that's
//! how pre-v0.4.2 rows look) and computes + persists fresh
//! embeddings for them. Walks BOTH the per-project brain.db AND
//! `~/.kimetsu/brain.db` so the user-scope capsules introduced in
//! v0.4.1 get the same treatment.
//!
//! `--dry-run` reports how many rows would be re-embedded without
//! writing.
//! `--force` re-embeds even rows that already have the current
//! model — useful after an OS-level model update where the bytes
//! change but the model_id doesn't.
//!
//! The default Cargo build (no `embeddings` feature) ships
//! `NoopEmbedder` — `reindex_all` returns Ok with `EmbedderNoop`
//! status so the CLI can print a friendly hint rather than
//! silently doing nothing.

use std::path::Path;

use kimetsu_core::KimetsuResult;
use rusqlite::Connection;

use crate::embeddings::{self, Embedder, EmbedderError, encode_embedding};
use crate::project::load_project;
use crate::user_brain::open_user_brain;

/// Per-scope reindex result.
#[derive(Debug, Clone)]
pub struct ScopeReport {
    pub scope: &'static str,
    /// `None` if this DB wasn't opened (e.g. user brain disabled).
    pub opened: bool,
    /// Total active memory rows in this DB.
    pub total: usize,
    /// Rows that need re-embedding (NULL embedding OR
    /// embedding_model != active model OR --force was set).
    pub candidates: usize,
    /// Rows actually updated. `0` in `--dry-run`.
    pub updated: usize,
    /// Rows that failed to re-embed (kept their previous state).
    pub failed: usize,
}

impl ScopeReport {
    fn skipped(scope: &'static str) -> Self {
        Self {
            scope,
            opened: false,
            total: 0,
            candidates: 0,
            updated: 0,
            failed: 0,
        }
    }
}

/// Aggregate result over project + user scopes.
#[derive(Debug, Clone)]
pub struct ReindexReport {
    pub project: ScopeReport,
    pub user: ScopeReport,
    pub embedder_model_id: String,
    pub embedder_noop: bool,
}

impl ReindexReport {
    pub fn updated_total(&self) -> usize {
        self.project.updated + self.user.updated
    }
    pub fn candidates_total(&self) -> usize {
        self.project.candidates + self.user.candidates
    }
}

/// Reindex options. Mirrors the CLI flags 1:1.
#[derive(Debug, Clone, Copy)]
pub struct ReindexOptions {
    pub scope: ReindexScope,
    pub dry_run: bool,
    pub force: bool,
    /// Stop after this many rows. `None` means no cap.
    pub limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReindexScope {
    Project,
    User,
    All,
}

impl ReindexScope {
    pub fn parse(value: &str) -> Result<Self, String> {
        match value.trim().to_ascii_lowercase().as_str() {
            "" | "all" => Ok(Self::All),
            "project" | "repo" => Ok(Self::Project),
            "user" | "global" => Ok(Self::User),
            other => Err(format!("unknown reindex scope `{other}`")),
        }
    }
}

impl Default for ReindexOptions {
    fn default() -> Self {
        Self {
            scope: ReindexScope::All,
            dry_run: false,
            force: false,
            limit: None,
        }
    }
}

/// Walk the project DB (if `--scope` includes it) and the user DB
/// (if `--scope` includes it AND user-brain is enabled), re-embedding
/// rows whose stored `embedding_model` doesn't match the active
/// embedder.
pub fn reindex_all(repo_start: &Path, opts: ReindexOptions) -> KimetsuResult<ReindexReport> {
    let embedder = embeddings::open_default_embedder();
    let model_id = embedder.model_id().to_string();
    let noop = embedder.is_noop();

    let mut remaining = opts.limit;
    let mut project_report = ScopeReport::skipped("project");
    let mut user_report = ScopeReport::skipped("user");

    if matches!(opts.scope, ReindexScope::Project | ReindexScope::All) {
        let (_paths, _config, conn) = load_project(repo_start)?;
        project_report = reindex_one_conn(&conn, "project", embedder, &opts, &mut remaining)?;
    }

    if matches!(opts.scope, ReindexScope::User | ReindexScope::All)
        && let Some(user_conn) = open_user_brain()?
    {
        user_report = reindex_one_conn(&user_conn, "user", embedder, &opts, &mut remaining)?;
    }

    Ok(ReindexReport {
        project: project_report,
        user: user_report,
        embedder_model_id: model_id,
        embedder_noop: noop,
    })
}

fn reindex_one_conn(
    conn: &Connection,
    scope: &'static str,
    embedder: &(dyn Embedder + Send + Sync),
    opts: &ReindexOptions,
    remaining: &mut Option<usize>,
) -> KimetsuResult<ScopeReport> {
    // Total active rows (for the friendly "x of y" output).
    let total: i64 = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
        [],
        |row| row.get(0),
    )?;
    let total = total.max(0) as usize;

    // Find rows that need re-embedding. With NoopEmbedder there's
    // nothing meaningful to do, so we return zeros and let the CLI
    // print a hint.
    if embedder.is_noop() {
        return Ok(ScopeReport {
            scope,
            opened: true,
            total,
            candidates: 0,
            updated: 0,
            failed: 0,
        });
    }

    // Candidate predicate:
    //   force          -> every active row
    //   default        -> rows where embedding_model is NULL OR != active model
    // NULL captures both "never embedded" and "embedded with a model
    // that didn't bother to record an id".
    let model_id = embedder.model_id().to_string();
    let mut stmt = if opts.force {
        conn.prepare(
            "
            SELECT memory_id, text
            FROM memories
            WHERE invalidated_at IS NULL
            ORDER BY created_at ASC
            ",
        )?
    } else {
        conn.prepare(
            "
            SELECT memory_id, text
            FROM memories
            WHERE invalidated_at IS NULL
              AND (embedding_model IS NULL OR embedding_model != ?1)
            ORDER BY created_at ASC
            ",
        )?
    };

    // SQLite prepared statements bind by index; build the row
    // iterator with the matching params signature.
    let mut rows = if opts.force {
        stmt.query([])?
    } else {
        stmt.query(rusqlite::params![model_id])?
    };

    let mut candidates = 0usize;
    let mut updated = 0usize;
    let mut failed = 0usize;
    while let Some(row) = rows.next()? {
        if remaining.map(|r| r == 0).unwrap_or(false) {
            break;
        }
        candidates += 1;
        let memory_id: String = row.get(0)?;
        let text: String = row.get(1)?;
        if opts.dry_run {
            continue;
        }
        match embedder.embed(&text) {
            Ok(vec) if vec.len() == embedder.dim() => {
                conn.execute(
                    "UPDATE memories SET embedding = ?1, embedding_model = ?2 WHERE memory_id = ?3",
                    rusqlite::params![encode_embedding(&vec), embedder.model_id(), memory_id],
                )?;
                updated += 1;
                if let Some(r) = remaining {
                    *r = r.saturating_sub(1);
                }
            }
            Ok(_) => {
                failed += 1;
            }
            Err(EmbedderError::NotImplemented) => {
                // Embedder degraded to noop mid-run (unusual but
                // possible if the model unloaded). Record as failed
                // so the caller can surface the partial-progress.
                failed += 1;
            }
            Err(_) => {
                failed += 1;
            }
        }
    }

    Ok(ScopeReport {
        scope,
        opened: true,
        total,
        candidates,
        updated,
        failed,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::embeddings::{StubEmbedder, encode_embedding};
    use crate::user_brain::with_user_brain_disabled;

    #[test]
    fn reindex_scope_parser_accepts_aliases() {
        assert_eq!(
            ReindexScope::parse("project").unwrap(),
            ReindexScope::Project
        );
        assert_eq!(ReindexScope::parse("repo").unwrap(), ReindexScope::Project);
        assert_eq!(ReindexScope::parse("user").unwrap(), ReindexScope::User);
        assert_eq!(ReindexScope::parse("global").unwrap(), ReindexScope::User);
        assert_eq!(ReindexScope::parse("all").unwrap(), ReindexScope::All);
        assert_eq!(ReindexScope::parse("").unwrap(), ReindexScope::All);
        assert!(ReindexScope::parse("nope").is_err());
    }

    /// v0.4.3: `reindex_one_conn` with an explicit StubEmbedder
    /// finds NULL-embedding rows and back-fills them. Tests the
    /// SQL + walker logic directly without going through the
    /// process-static `open_default_embedder` cache.
    #[test]
    fn reindex_one_conn_backfills_null_embeddings() {
        with_user_brain_disabled(|| {
            let conn = rusqlite::Connection::open_in_memory().expect("open");
            crate::schema::initialize(&conn).expect("init");

            // Insert two memories: one without embedding, one with
            // a stale model id. Both should be candidates.
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score
                )
                VALUES ('m_a', 'repo', 'fact', 'use rg', 'use rg', 1.0,
                        NULL, '{}', '2026-05-01T00:00:00Z', 0, 0.0)
                ",
                [],
            )
            .expect("insert m_a");
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score, embedding, embedding_model
                )
                VALUES ('m_b', 'repo', 'fact', 'use ripgrep', 'use ripgrep', 1.0,
                        NULL, '{}', '2026-05-02T00:00:00Z', 0, 0.0,
                        ?1, 'old-model-id')
                ",
                rusqlite::params![encode_embedding(&[0.0f32; 4])],
            )
            .expect("insert m_b");

            let stub = StubEmbedder::new();
            let mut remaining = None;
            let report = reindex_one_conn(
                &conn,
                "project",
                &stub,
                &ReindexOptions::default(),
                &mut remaining,
            )
            .expect("reindex");

            assert_eq!(report.total, 2);
            assert_eq!(report.candidates, 2, "both rows should be candidates");
            assert_eq!(report.updated, 2, "both should be updated");
            assert_eq!(report.failed, 0);

            // Confirm rows now carry the stub's model id and a
            // properly-sized blob.
            for memory_id in ["m_a", "m_b"] {
                let model: String = conn
                    .query_row(
                        "SELECT embedding_model FROM memories WHERE memory_id = ?1",
                        rusqlite::params![memory_id],
                        |row| row.get(0),
                    )
                    .expect("fetch model");
                assert_eq!(model, stub.model_id());
                let blob: Vec<u8> = conn
                    .query_row(
                        "SELECT embedding FROM memories WHERE memory_id = ?1",
                        rusqlite::params![memory_id],
                        |row| row.get(0),
                    )
                    .expect("fetch blob");
                assert_eq!(
                    blob.len(),
                    stub.dim() * 4,
                    "stub-d8 -> 8 floats -> 32 bytes"
                );
            }
        });
    }

    /// `--dry-run` reports candidates without writing.
    #[test]
    fn reindex_one_conn_dry_run_does_not_mutate() {
        with_user_brain_disabled(|| {
            let conn = rusqlite::Connection::open_in_memory().expect("open");
            crate::schema::initialize(&conn).expect("init");
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score
                )
                VALUES ('m_a', 'repo', 'fact', 'use rg', 'use rg', 1.0,
                        NULL, '{}', '2026-05-01T00:00:00Z', 0, 0.0)
                ",
                [],
            )
            .expect("insert");

            let stub = StubEmbedder::new();
            let mut remaining = None;
            let report = reindex_one_conn(
                &conn,
                "project",
                &stub,
                &ReindexOptions {
                    dry_run: true,
                    ..ReindexOptions::default()
                },
                &mut remaining,
            )
            .expect("dry-run");

            assert_eq!(report.candidates, 1);
            assert_eq!(report.updated, 0, "dry-run must not write");
            let model: Option<String> = conn
                .query_row(
                    "SELECT embedding_model FROM memories WHERE memory_id = 'm_a'",
                    [],
                    |row| row.get(0),
                )
                .expect("fetch");
            assert!(model.is_none(), "embedding_model should still be NULL");
        });
    }

    /// With a NoopEmbedder the walker returns candidates=0 + a clear
    /// note via the `embedder_noop` flag — lets the CLI print a
    /// hint instead of silently doing nothing.
    #[test]
    fn reindex_one_conn_with_noop_embedder_returns_zero_candidates() {
        with_user_brain_disabled(|| {
            let conn = rusqlite::Connection::open_in_memory().expect("open");
            crate::schema::initialize(&conn).expect("init");
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score
                )
                VALUES ('m_a', 'repo', 'fact', 'use rg', 'use rg', 1.0,
                        NULL, '{}', '2026-05-01T00:00:00Z', 0, 0.0)
                ",
                [],
            )
            .expect("insert");

            let noop = embeddings::NoopEmbedder;
            let mut remaining = None;
            let report = reindex_one_conn(
                &conn,
                "project",
                &noop,
                &ReindexOptions::default(),
                &mut remaining,
            )
            .expect("noop reindex");

            assert_eq!(report.total, 1);
            assert_eq!(report.candidates, 0, "noop should walk zero candidates");
            assert_eq!(report.updated, 0);
        });
    }

    /// `--force` re-embeds even rows that already carry the active
    /// model id.
    #[test]
    fn reindex_one_conn_force_reembeds_current_model_rows() {
        with_user_brain_disabled(|| {
            let conn = rusqlite::Connection::open_in_memory().expect("open");
            crate::schema::initialize(&conn).expect("init");
            let stub = StubEmbedder::new();
            // Pre-populate a row with the stub's current id AND a
            // bogus zero-vector embedding so we can verify `--force`
            // overwrites it.
            conn.execute(
                "
                INSERT INTO memories (
                    memory_id, scope, kind, text, normalized_text, confidence,
                    source_event_id, provenance_snapshot_json, created_at,
                    use_count, usefulness_score, embedding, embedding_model
                )
                VALUES ('m_a', 'repo', 'fact', 'use rg', 'use rg', 1.0,
                        NULL, '{}', '2026-05-01T00:00:00Z', 0, 0.0,
                        ?1, ?2)
                ",
                rusqlite::params![encode_embedding(&[0.0f32; 8]), stub.model_id()],
            )
            .expect("insert pre-stamped row");

            let mut remaining = None;
            // Without --force: zero candidates (already on current model).
            let plain = reindex_one_conn(
                &conn,
                "project",
                &stub,
                &ReindexOptions::default(),
                &mut remaining,
            )
            .expect("plain reindex");
            assert_eq!(plain.candidates, 0);

            // With --force: one candidate, gets re-embedded.
            let forced = reindex_one_conn(
                &conn,
                "project",
                &stub,
                &ReindexOptions {
                    force: true,
                    ..ReindexOptions::default()
                },
                &mut remaining,
            )
            .expect("forced reindex");
            assert_eq!(forced.candidates, 1);
            assert_eq!(forced.updated, 1);

            // The embedding must be non-zero now (stub produces a
            // hash-bucket vector for "use rg").
            let blob: Vec<u8> = conn
                .query_row(
                    "SELECT embedding FROM memories WHERE memory_id = 'm_a'",
                    [],
                    |row| row.get(0),
                )
                .expect("fetch blob");
            assert!(
                blob.iter().any(|&b| b != 0),
                "force should have overwritten the zero embedding"
            );
        });
    }
}
