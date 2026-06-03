//! User-scope brain at `~/.kimetsu/brain.db` (v0.4.1).
//!
//! Today (and through v0.3.5) `MemoryScope::GlobalUser` capsules live
//! inside the workspace's `.kimetsu/brain.db`. Open a second repo and
//! the GlobalUser memories don't follow — the "brain that follows you
//! between projects" pitch is broken.
//!
//! This module backs the user-scope DB at
//! `~/.kimetsu/brain.db` (or `$KIMETSU_USER_BRAIN_DIR/brain.db`,
//! used by tests). The user brain stores only `GlobalUser` capsules.
//! Repo-scoped memories, repo file/manifest ingest, traces, and runs
//! stay in the per-project DB where they belong.
//!
//! Retrieval merges from both DBs (see
//! [`crate::context::retrieve_context`] which now takes an
//! `extra_memory_conns` slice). Writes route by scope: a `GlobalUser`
//! capsule lands in the user DB; everything else lands in the project
//! DB as before.
//!
//! Backward compat: pre-v0.4 project brain.db files that already
//! contain `GlobalUser` rows keep working — those rows are still
//! retrieved when the user opens that specific project. Only NEW
//! `GlobalUser` writes start landing in the user DB. A future
//! `kimetsu brain migrate-user` subcommand will copy historical
//! GlobalUser rows from a project DB into the user DB (post-v0.4.1).
//!
//! Disable: `KIMETSU_USER_BRAIN=0` (or `false`/`off`/`no`,
//! case-insensitive). When disabled, GlobalUser writes stay in the
//! project DB and retrieval doesn't merge — exactly v0.3.5 behavior.

use std::fs;
use std::path::PathBuf;

use kimetsu_core::KimetsuResult;
use kimetsu_core::ids::RunId;
use kimetsu_core::memory::{MemoryKind, MemoryScope, normalize_memory_text};
use kimetsu_core::paths::{user_brain_db_path, user_brain_enabled, user_kimetsu_dir};
use rusqlite::{Connection, OpenFlags, OptionalExtension};
use time::OffsetDateTime;
use ulid::Ulid;

use crate::conflict;
use crate::embeddings;
use crate::project::MemoryRow;
use crate::redact;
use crate::schema;

/// Open (and create if missing) the user-scope brain.db. Returns
/// `Ok(None)` when the user brain is disabled or no home dir is
/// resolvable — callers treat None as "skip the user-brain path,
/// behave like v0.3.5".
pub fn open_user_brain() -> KimetsuResult<Option<Connection>> {
    if !user_brain_enabled() {
        return Ok(None);
    }
    let Some(dir) = user_kimetsu_dir() else {
        return Ok(None);
    };
    fs::create_dir_all(&dir)?;
    let db_path = dir.join("brain.db");
    schema::ensure_vec_extension_registered();
    let conn = Connection::open(&db_path)?;
    schema::initialize(&conn)?;
    Ok(Some(conn))
}

/// Open the user-scope brain.db read-only. Returns `Ok(None)` if the
/// file doesn't exist yet OR the user brain is disabled. Used by
/// retrieval-only paths (broker, MCP read tools) so we don't
/// accidentally create an empty DB on first hot-path call.
pub fn open_user_brain_readonly() -> KimetsuResult<Option<Connection>> {
    if !user_brain_enabled() {
        return Ok(None);
    }
    let Some(db_path) = user_brain_db_path() else {
        return Ok(None);
    };
    if !db_path.exists() {
        return Ok(None);
    }
    schema::ensure_vec_extension_registered();
    let conn = Connection::open_with_flags(&db_path, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    match schema::validate(&conn) {
        Ok(()) => {}
        // A stale user brain must not break an unrelated read-only project op;
        // skip it this call. The next read-write open migrates it.
        Err(e)
            if e.downcast_ref::<crate::migrate::SchemaNeedsMigration>()
                .is_some() =>
        {
            return Ok(None);
        }
        Err(e) => return Err(e),
    }
    Ok(Some(conn))
}

/// Return the path to the user brain.db, if a home dir resolves
/// (regardless of whether the file actually exists yet). Used by
/// `kimetsu brain status`-style diagnostics.
pub fn user_brain_path() -> Option<PathBuf> {
    user_brain_db_path()
}

/// Write a GlobalUser memory to the user brain.
///
/// Differs from `project::add_memory` deliberately: we do NOT emit
/// trace events, run rows, or take the project lock — the user brain
/// has no project to attribute those to. We DO honor the same
/// dedup-by-normalized-text rule so a user who imports the same
/// reusable preference twice doesn't end up with duplicate rows.
///
/// Returns the memory_id (either freshly minted or the existing
/// duplicate's id).
pub fn add_user_memory(
    conn: &Connection,
    kind: MemoryKind,
    text: &str,
    confidence: f32,
) -> KimetsuResult<String> {
    // v0.4.5: defense-in-depth redaction for external callers who
    // bypass `project::add_memory` and write to the user brain
    // directly. Redacting twice is idempotent — `[REDACTED:foo]`
    // doesn't match any pattern — so doubling-up with
    // `add_memory`'s upstream call is safe + cheap.
    let redaction = redact::redact_secrets(text);
    if redaction.was_redacted() {
        eprintln!("kimetsu-brain (user): {}", redaction.summary());
    }
    let text = redaction.text.as_str();
    let normalized = normalize_memory_text(text);
    // Same dedup rule as project add_memory: active row with same
    // (scope, kind, normalized_text) collapses.
    let existing: Option<String> = conn
        .query_row(
            "
            SELECT memory_id FROM memories
            WHERE scope = ?1 AND kind = ?2 AND normalized_text = ?3
              AND invalidated_at IS NULL
            LIMIT 1
            ",
            rusqlite::params!["global_user".to_string(), kind.to_string(), &normalized],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(existing_id) = existing {
        return Ok(existing_id);
    }

    let memory_id = Ulid::new().to_string();
    let created_at = OffsetDateTime::now_utc()
        .format(&time::format_description::well_known::Rfc3339)
        .map_err(|e| format!("timestamp format: {e}"))?;
    // The provenance snapshot mirrors what `add_memory` writes for a
    // manual_cli source. We use a synthesized RunId because user-brain
    // writes don't live inside a run.
    let provenance = serde_json::json!({
        "source": "user_brain",
        "run_id": RunId::new().to_string(),
        "text": text,
    })
    .to_string();
    conn.execute(
        "
        INSERT INTO memories (
            memory_id, scope, kind, text, normalized_text,
            confidence, provenance_snapshot_json, created_at,
            use_count, usefulness_score
        )
        VALUES (?1, 'global_user', ?2, ?3, ?4, ?5, ?6, ?7, 0, 0.0)
        ",
        rusqlite::params![
            memory_id,
            kind.to_string(),
            text,
            normalized,
            confidence,
            provenance,
            created_at,
        ],
    )?;
    conn.execute(
        "
        INSERT INTO memories_fts (memory_id, text, kind, scope)
        VALUES (?1, ?2, ?3, 'global_user')
        ",
        rusqlite::params![memory_id, text, kind.to_string()],
    )?;

    // v0.4.2: post-insert embedding update. v0.4.3 swapped the
    // default behind the `embeddings` feature flag — same Noop
    // behavior on the default build, fastembed-rs BGE-small when
    // the feature is on.
    let embedder = embeddings::open_default_embedder();
    embeddings::embed_and_persist(conn, &memory_id, text, embedder)?;

    // v0.5.2: conflict detection for user-brain writes too. The
    // user brain ships the same `memory_conflicts` schema (shared
    // `schema::initialize`), so an operator's `kimetsu brain
    // memory conflicts` walks the project AND user brains via the
    // existing multi-brain plumbing. Best-effort: NoopEmbedder
    // returns 0 hits; failures are logged, not raised.
    let conflicts = conflict::detect_and_record(
        conn,
        &memory_id,
        &MemoryScope::GlobalUser,
        &kind.to_string(),
        text,
        embedder,
    );
    if conflicts > 0 {
        eprintln!(
            "kimetsu-brain (user): memory {memory_id} conflicts with {conflicts} existing memor{} (run `kimetsu brain memory conflicts` to review)",
            if conflicts == 1 { "y" } else { "ies" }
        );
    }

    Ok(memory_id)
}

/// List user-brain memories. Mirrors `project::list_memories`
/// behavior: most-recent first, capped at 100. Returns the same
/// `MemoryRow` shape so callers can merge with project memories
/// without reshaping.
pub fn list_user_memories(conn: &Connection) -> KimetsuResult<Vec<MemoryRow>> {
    let mut stmt = conn.prepare(
        "
        SELECT memory_id, scope, kind, text, confidence, use_count, usefulness_score
        FROM memories
        WHERE invalidated_at IS NULL
        ORDER BY created_at DESC
        LIMIT 100
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok(MemoryRow {
            memory_id: row.get(0)?,
            scope: row.get(1)?,
            kind: row.get(2)?,
            text: row.get(3)?,
            confidence: row.get(4)?,
            use_count: row.get(5)?,
            usefulness_score: row.get::<_, f64>(6)? as f32,
        })
    })?;
    let mut memories = Vec::new();
    for row in rows {
        memories.push(row?);
    }
    Ok(memories)
}

// ----- v0.4.1 test-env helpers -----
//
// Shared between this module's tests, project.rs's tests, AND
// downstream crate tests (kimetsu-chat, kimetsu-cli) so that any
// unit test that does NOT explicitly want the user brain ON gets it
// OFF by default. Without this, parallel tests across crates writing
// `MemoryScope::GlobalUser` memories would all land in the same real
// `~/.kimetsu/brain.db` on the developer's machine and stomp each
// other's assertions.
//
// Why these are regular `pub fn` (not `#[cfg(test)]`-gated): the
// `#[cfg(test)]` flag is set only when the SAME crate is being
// compiled as a test, so downstream crates can't see test-gated
// items in their dependencies. The functions are tiny, harmless in
// non-test builds, and clearly labeled "testing" — production code
// has no reason to call them.
//
// Pattern:
//   * `test_env_lock()` returns a process-wide mutex; any test that
//     mutates `KIMETSU_USER_BRAIN_DIR` / `KIMETSU_USER_BRAIN` should
//     acquire it.
//   * `with_user_brain_disabled` wraps a closure with the brain
//     forced off — used by tests that predate v0.4.1 and assume
//     GlobalUser writes land in the project DB.
//   * `with_user_brain_at` (in `tests` submodule) wraps a closure
//     with the brain pointed at a per-test dir — used by user-brain
//     tests that actually want the user-brain path exercised.

/// Process-wide mutex used to serialize env-mutating tests that
/// touch `KIMETSU_USER_BRAIN` or `KIMETSU_USER_BRAIN_DIR`. Returns
/// the same `&'static Mutex` regardless of caller, so cross-crate
/// tests share the same serialization order.
#[doc(hidden)]
pub fn test_env_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());
    &LOCK
}

/// Run `f` with the user brain forcibly disabled. Restores the
/// previous values of `KIMETSU_USER_BRAIN` and
/// `KIMETSU_USER_BRAIN_DIR` after the closure returns (or unwinds).
///
/// Tests use this to opt into the pre-v0.4.1 routing where
/// `MemoryScope::GlobalUser` writes land in the project DB.
#[doc(hidden)]
pub fn with_user_brain_disabled<R>(f: impl FnOnce() -> R) -> R {
    let _guard = test_env_lock().lock().unwrap_or_else(|p| p.into_inner());
    let prev_enabled = std::env::var("KIMETSU_USER_BRAIN").ok();
    let prev_dir = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
    // SAFETY: scoped via the shared mutex; no other thread races on
    // env mutation while we hold it.
    unsafe {
        std::env::set_var("KIMETSU_USER_BRAIN", "0");
        std::env::remove_var("KIMETSU_USER_BRAIN_DIR");
    }
    let out = f();
    unsafe {
        match prev_enabled {
            Some(v) => std::env::set_var("KIMETSU_USER_BRAIN", v),
            None => std::env::remove_var("KIMETSU_USER_BRAIN"),
        }
        match prev_dir {
            Some(v) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", v),
            None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn with_user_brain_at(dir: &std::path::Path, f: impl FnOnce()) {
        let _guard = test_env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let prev_dir = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
        let prev_enabled = std::env::var("KIMETSU_USER_BRAIN").ok();
        // SAFETY: scoped via the shared mutex.
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN_DIR", dir);
            std::env::remove_var("KIMETSU_USER_BRAIN");
        }
        f();
        unsafe {
            match prev_dir {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
            }
            match prev_enabled {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN"),
            }
        }
    }

    #[test]
    fn open_user_brain_creates_db_on_first_call() {
        let tmp = tempdir_in_test("kimetsu-user-brain-1");
        with_user_brain_at(&tmp, || {
            let conn = open_user_brain()
                .expect("open ok")
                .expect("user brain enabled");
            // Run a no-op query to confirm the schema initialized.
            let count: i64 = conn
                .query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))
                .expect("query memories");
            assert_eq!(count, 0);
            assert!(tmp.join("brain.db").exists());
        });
    }

    #[test]
    fn open_user_brain_returns_none_when_disabled() {
        let tmp = tempdir_in_test("kimetsu-user-brain-2");
        let _guard = test_env_lock().lock().unwrap_or_else(|p| p.into_inner());
        let prev_enabled = std::env::var("KIMETSU_USER_BRAIN").ok();
        let prev_dir = std::env::var("KIMETSU_USER_BRAIN_DIR").ok();
        unsafe {
            std::env::set_var("KIMETSU_USER_BRAIN", "0");
            std::env::set_var("KIMETSU_USER_BRAIN_DIR", &tmp);
        }
        let result = open_user_brain().expect("open ok");
        assert!(result.is_none(), "disabled should short-circuit to None");
        // Confirm we did NOT create the dir as a side effect.
        assert!(!tmp.join("brain.db").exists());
        unsafe {
            match prev_dir {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN_DIR", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN_DIR"),
            }
            match prev_enabled {
                Some(v) => std::env::set_var("KIMETSU_USER_BRAIN", v),
                None => std::env::remove_var("KIMETSU_USER_BRAIN"),
            }
        }
    }

    #[test]
    fn open_user_brain_readonly_returns_none_before_first_write() {
        let tmp = tempdir_in_test("kimetsu-user-brain-3");
        with_user_brain_at(&tmp, || {
            let result = open_user_brain_readonly().expect("open ok");
            assert!(result.is_none(), "missing file -> None for readonly path");
        });
    }

    #[test]
    fn add_user_memory_persists_and_dedups() {
        let tmp = tempdir_in_test("kimetsu-user-brain-4");
        with_user_brain_at(&tmp, || {
            let conn = open_user_brain().expect("open").expect("enabled");
            let first =
                add_user_memory(&conn, MemoryKind::Preference, "use thiserror", 1.0).expect("add");
            let second = add_user_memory(&conn, MemoryKind::Preference, "  use   thiserror  ", 1.0)
                .expect("add normalized dup");
            assert_eq!(first, second, "normalized-text dedup must hit");
            // List back what we wrote.
            let rows = list_user_memories(&conn).expect("list");
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0].text, "use thiserror");
            assert_eq!(rows[0].scope, "global_user");
        });
    }

    #[test]
    fn user_brain_path_resolves_from_override_env() {
        let tmp = tempdir_in_test("kimetsu-user-brain-5");
        with_user_brain_at(&tmp, || {
            let path = user_brain_path().expect("path");
            assert!(path.starts_with(&tmp));
            assert!(path.ends_with("brain.db"));
        });
    }

    // ------------------------------------------------------------------
    // A5-4. open_user_brain_readonly degrades to Ok(None) on a stale user brain
    // ------------------------------------------------------------------
    #[test]
    fn readonly_degrades_to_none_on_stale_schema() {
        let tmp = tempdir_in_test("kimetsu-user-brain-stale");
        with_user_brain_at(&tmp, || {
            // Write a v1 stub directly — only schema_info, no full schema.
            // schema::validate reads only schema_info, so this is sufficient
            // to trigger SchemaNeedsMigration without any other tables.
            let db_path = tmp.join("brain.db");
            {
                let conn = rusqlite::Connection::open(&db_path).expect("open stub db");
                conn.execute_batch(
                    "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
                     INSERT INTO schema_info VALUES ('kimetsu_schema_version', 1);",
                )
                .expect("seed v1 stub");
            }
            // The file exists but is at v1; open_user_brain_readonly must degrade
            // to Ok(None) instead of propagating the SchemaNeedsMigration error.
            let result = open_user_brain_readonly()
                .expect("open_user_brain_readonly must not error on stale user brain");
            assert!(
                result.is_none(),
                "stale user brain (v1 < target) must yield Ok(None), not an error"
            );
        });
    }

    // ------------------------------------------------------------------
    // A7: user-brain v1→v2 migration — backup sidecar + data preserved
    // ------------------------------------------------------------------
    #[test]
    fn migration_upgrades_user_brain_creates_backup_and_preserves_data() {
        let tmp = tempdir_in_test("kimetsu-user-brain-migrate");
        with_user_brain_at(&tmp, || {
            // (a) Create the user brain and seed a GlobalUser memory.
            //     open_user_brain() calls schema::initialize() which runs
            //     run_migrations, leaving the DB at v2.
            let mem_id = {
                let conn = open_user_brain().expect("open ok").expect("enabled");
                add_user_memory(
                    &conn,
                    MemoryKind::Preference,
                    "A7 user-brain migration test",
                    1.0,
                )
                .expect("add_user_memory")
            };

            // (b) Stamp the schema_info version back to 1 to simulate a
            //     pre-upgrade DB.  The DDL is already v2-shaped; the
            //     migration is idempotent, so re-running is safe.
            let db_path = tmp.join("brain.db");
            {
                let conn = rusqlite::Connection::open(&db_path).expect("open for stamp-down");
                conn.execute(
                    "UPDATE schema_info SET value = 1 WHERE key = 'kimetsu_schema_version'",
                    [],
                )
                .expect("stamp version back to 1");
                let stamped: i64 = conn
                    .query_row(
                        "SELECT value FROM schema_info WHERE key = 'kimetsu_schema_version'",
                        [],
                        |r| r.get(0),
                    )
                    .expect("read stamped version");
                assert_eq!(stamped, 1, "version should be 1 after stamp-down");
            }

            // (c) Re-open through open_user_brain() — calls schema::initialize
            //     → run_migrations → migrates v1→v2 with a backup.
            let conn = open_user_brain().expect("re-open ok").expect("enabled");

            // Assert 1: version is back at 2.
            let ver =
                crate::migrate::current_version(&conn).expect("current_version after re-open");
            assert_eq!(ver, 2, "user brain must be at v2 after re-open");

            // Assert 2: backup sidecar brain.db.bak-1-2-* exists next to brain.db.
            let bak_files: Vec<_> = std::fs::read_dir(&tmp)
                .expect("read tmp dir")
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.starts_with("brain.db.bak-1-2-"))
                        .unwrap_or(false)
                })
                .collect();
            assert_eq!(
                bak_files.len(),
                1,
                "exactly one user-brain backup sidecar brain.db.bak-1-2-* must exist; found: {:?}",
                bak_files.iter().map(|e| e.file_name()).collect::<Vec<_>>()
            );

            // Assert 3: the seeded memory survives the migration.
            let rows = list_user_memories(&conn).expect("list_user_memories");
            assert!(
                rows.iter().any(|r| r.memory_id == mem_id),
                "seeded memory must survive v1→v2 migration; mem_id={mem_id}"
            );
        });
    }

    fn tempdir_in_test(prefix: &str) -> std::path::PathBuf {
        // Don't pull in `tempfile` — the workspace doesn't use it
        // elsewhere in this crate. Roll a small helper.
        let dir = std::env::temp_dir().join(format!("{prefix}-{}", Ulid::new()));
        std::fs::create_dir_all(&dir).expect("mkdir");
        dir
    }
}
