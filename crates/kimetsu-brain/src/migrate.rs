use std::cmp::Reverse;
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use kimetsu_core::{KIMETSU_SCHEMA_VERSION, KimetsuResult};
use rusqlite::Connection;

/// Returned by `schema::validate` when a read-only connection observes a DB
/// older than the binary's target version. Read-only connections cannot run
/// DDL, so the caller must decide: the user brain treats it as "unavailable
/// this call" (`Ok(None)`) and the next read-write open migrates it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaNeedsMigration {
    pub from: i64,
    pub to: i64,
}

impl std::fmt::Display for SchemaNeedsMigration {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "brain.db schema version {} is older than this binary's {}; open it read-write once to migrate",
            self.from, self.to
        )
    }
}

impl std::error::Error for SchemaNeedsMigration {}

/// One forward-only schema migration. `version` is the value the DB is
/// stamped with AFTER `up` succeeds (i.e. `migrations()[i].version` is the
/// post-migration version). `up` MUST be idempotent (it may be re-run after
/// a crash mid-batch).
pub struct Migration {
    pub version: i64,
    pub description: &'static str,
    pub up: fn(&Connection) -> KimetsuResult<()>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MigrationOutcome {
    pub from: i64,
    pub to: i64,
    pub applied: Vec<i64>,
    /// Path of the pre-migration sidecar backup, when one was created.
    /// `None` for in-memory DBs, no-op opens, and the `current > target` error path.
    pub backup_path: Option<PathBuf>,
}

/// The ordered migration set.
///
/// Invariant (debug-asserted in `run_with`): versions strictly ascending and
/// contiguous starting at 2 (version 1 is the baseline `CREATE`, not a
/// migration step).
fn migrations() -> &'static [Migration] {
    &[
        Migration {
            version: 2,
            description: "fold additive columns, citations/conflicts tables, and FTS reshapes",
            up: crate::schema::migrate_v1_to_v2,
        },
        Migration {
            version: 3,
            description: "add superseded_by column + index for near-duplicate merge (Story 3.1)",
            up: crate::schema::migrate_v2_to_v3,
        },
        Migration {
            version: 4,
            description: "add memory_edges typed-edge projection table (S5.2 graph-lite backend)",
            up: crate::schema::migrate_v3_to_v4,
        },
    ]
}

/// Return the code's compile-time target schema version.
pub fn target_version() -> i64 {
    KIMETSU_SCHEMA_VERSION
}

/// Read the current schema version stored in `schema_info`.
pub fn current_version(conn: &Connection) -> KimetsuResult<i64> {
    Ok(conn.query_row(
        "SELECT value FROM schema_info WHERE key = 'kimetsu_schema_version'",
        [],
        |row| row.get(0),
    )?)
}

/// Public entrypoint: migrate `conn` up to the binary's target version.
pub fn run_migrations(conn: &Connection) -> KimetsuResult<MigrationOutcome> {
    run_with(conn, migrations(), target_version())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Resolve the filesystem path of `conn`'s main database file, if any.
/// Returns `None` for in-memory (`:memory:`) and anonymous temp DBs.
fn db_file_path(conn: &Connection) -> Option<PathBuf> {
    match conn.path() {
        Some(p) if !p.is_empty() && p != ":memory:" => Some(PathBuf::from(p)),
        _ => None,
    }
}

/// Return the number of rows in the `memories` table, or 0 if the table does
/// not yet exist (e.g. a synthetic / partially-initialized DB).  Defensive:
/// never panics; query errors silently map to 0.
fn durable_row_count(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get::<_, i64>(0))
        .unwrap_or(0)
}

fn unique_default_backup_path(candidate: PathBuf) -> PathBuf {
    if !candidate.exists() {
        return candidate;
    }
    let (Some(parent), Some(file_name)) = (
        candidate.parent(),
        candidate.file_name().and_then(|n| n.to_str()),
    ) else {
        return candidate;
    };
    for suffix in 1..1000 {
        let next = parent.join(format!("{file_name}-{suffix}"));
        if !next.exists() {
            return next;
        }
    }
    candidate
}

/// Snapshot the live DB before a version-advancing migration.  Returns the
/// sidecar path, or `None` for an in-memory DB (nothing to back up) or when
/// the DB contains zero memories (fresh install — nothing worth protecting).
///
/// Uses SQLite's online backup API for a consistent copy that respects WAL.
/// The sidecar is placed next to the source DB and named:
///   `<db-filename>.bak-<from>-<to>-<unix_nanos>`
fn backup_before_migrate(conn: &Connection, from: i64, to: i64) -> KimetsuResult<Option<PathBuf>> {
    let db_path = match db_file_path(conn) {
        Some(p) => p,
        None => return Ok(None), // in-memory or anonymous temp DB — nothing to back up
    };

    // Skip the backup when the DB is empty — a fresh/empty brain has nothing
    // to lose; an upgraded brain with real memories gets protected.
    if durable_row_count(conn) == 0 {
        return Ok(None);
    }

    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    // Sidecar next to the DB: brain.db.bak-<from>-<to>-<unix_nanos>
    let file_name = format!(
        "{}.bak-{from}-{to}-{ts}",
        db_path
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("brain.db")
    );
    let dest_path = unique_default_backup_path(db_path.with_file_name(file_name));

    // Online backup: open dest, copy main DB into it to completion.
    let mut dest = Connection::open(&dest_path)?;
    let backup = rusqlite::backup::Backup::new(conn, &mut dest)?;
    // pages_per_step must be > 0 (asserted by rusqlite); use 64.
    // pause_between_pages = 0ms since we want a fast single-shot backup.
    backup.run_to_completion(64, std::time::Duration::from_millis(0), None)?;
    drop(backup);

    Ok(Some(dest_path))
}

/// Keep the newest `keep` `<stem>.bak-*` sidecars next to `db_path`; delete
/// older ones.  Sorts candidates by the trailing `<ts>` integer parsed from
/// the filename (not mtime), which is both deterministic in tests and
/// monotonic in production since `<ts>` is the creation unix time.
///
/// Best-effort: filesystem errors while pruning are swallowed (we never fail
/// a migration over cleanup).
fn prune_backups(db_path: &Path, keep: usize) {
    let (Some(dir), Some(stem)) = (
        db_path.parent(),
        db_path.file_name().and_then(|n| n.to_str()),
    ) else {
        return;
    };

    let prefix = format!("{stem}.bak-");

    let mut backups: Vec<PathBuf> = match std::fs::read_dir(dir) {
        Ok(rd) => rd
            .filter_map(|e| e.ok().map(|e| e.path()))
            .filter(|p| {
                p.file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.starts_with(&prefix))
                    .unwrap_or(false)
            })
            .collect(),
        Err(_) => return,
    };

    if backups.len() <= keep {
        return;
    }

    // Sort newest-first by the trailing numeric `<ts>` parsed from the
    // filename.  This is deterministic in tests and monotonic in production.
    backups.sort_by_key(|p| {
        Reverse(
            p.file_name()
                .and_then(|n| n.to_str())
                .and_then(|n| n.rsplit('-').next())
                .and_then(|ts| ts.parse::<u64>().ok())
                .unwrap_or(0),
        )
    });

    for old in backups.into_iter().skip(keep) {
        let _ = std::fs::remove_file(old);
    }
}

// ---------------------------------------------------------------------------
// Core runner
// ---------------------------------------------------------------------------

/// Injectable core (test seam): apply `migs` to advance `conn` to `target`.
///
/// Each migration runs inside its own transaction; the `schema_info` version
/// bump is committed in the SAME transaction as the migration DDL, so a
/// crash between migrations leaves the DB at a cleanly-stamped intermediate
/// version rather than an ambiguous half-applied state.
pub(crate) fn run_with(
    conn: &Connection,
    migs: &[Migration],
    target: i64,
) -> KimetsuResult<MigrationOutcome> {
    // Invariant: each step advances exactly one version and every step is ≤ target.
    debug_assert!(
        migs.windows(2).all(|w| w[1].version == w[0].version + 1),
        "migrations must be strictly ascending and contiguous"
    );
    debug_assert!(
        migs.iter().all(|m| m.version <= target),
        "no migration may exceed the target version"
    );

    let current = current_version(conn)?;

    if current == target {
        return Ok(MigrationOutcome {
            from: current,
            to: current,
            applied: Vec::new(),
            backup_path: None,
        });
    }

    if current > target {
        return Err(format!(
            "brain.db schema version {current} was written by a newer Kimetsu \
             (this binary expects {target}); upgrade Kimetsu"
        )
        .into());
    }

    // current < target — snapshot before we touch anything.
    let backup_path = backup_before_migrate(conn, current, target)?;

    let mut applied = Vec::new();

    for m in migs
        .iter()
        .filter(|m| m.version > current && m.version <= target)
    {
        // Run the migration DDL and the version bump inside one transaction so
        // a crash mid-step is fully rolled back on the next open.
        conn.execute_batch("BEGIN")?;

        let result = (|| -> KimetsuResult<()> {
            (m.up)(conn)?;
            conn.execute(
                "UPDATE schema_info SET value = ?1 WHERE key = 'kimetsu_schema_version'",
                [m.version],
            )?;
            Ok(())
        })();

        match result {
            Ok(()) => {
                conn.execute_batch("COMMIT")?;
                applied.push(m.version);
            }
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }

    // Prune old backups (best-effort; swallows errors).
    if let Some(ref bp) = backup_path {
        if let Some(parent) = bp.parent() {
            let db_ref = db_file_path(conn).unwrap_or_else(|| parent.join("brain.db"));
            prune_backups(&db_ref, 3);
        }
    }

    // Emit a structured trace event so operators using RUST_LOG can see
    // when a migration ran without spamming every fresh-install stdout.
    if !applied.is_empty() {
        tracing::info!(
            from = current,
            to = target,
            backup = ?backup_path,
            "migrated brain.db schema"
        );
    }

    Ok(MigrationOutcome {
        from: current,
        to: target,
        applied,
        backup_path,
    })
}

// ---------------------------------------------------------------------------
// Public convenience: kimetsu brain backup
// ---------------------------------------------------------------------------

/// Write a consistent full-DB snapshot of the brain at `brain_db_path` to
/// `dest`.  When `dest` is `None`, the snapshot is placed next to the source
/// DB and named `<brain.db>.backup-<unix_nanos>`.
///
/// Uses the SQLite online backup API (same as `backup_before_migrate`) so the
/// copy is WAL-aware and consistent even if another writer is active.
///
/// Returns the absolute path of the snapshot and its size in bytes.
///
/// # Errors
/// Propagates IO and SQLite errors.  Does **not** swallow errors — callers
/// should surface them to the user.
pub fn backup_brain(
    brain_db_path: &std::path::Path,
    dest: Option<&std::path::Path>,
) -> KimetsuResult<(std::path::PathBuf, u64)> {
    let ts = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);

    let dest_path = match dest {
        Some(p) => p.to_path_buf(),
        None => {
            let file_name = format!(
                "{}.backup-{ts}",
                brain_db_path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or("brain.db")
            );
            unique_default_backup_path(brain_db_path.with_file_name(file_name))
        }
    };

    // Open the source in read-only mode so we don't disturb a running brain.
    let src = Connection::open_with_flags(
        brain_db_path,
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX,
    )?;

    // Online backup to the destination (created or overwritten).
    let mut dst = Connection::open(&dest_path)?;
    let backup = rusqlite::backup::Backup::new(&src, &mut dst)?;
    backup.run_to_completion(64, std::time::Duration::from_millis(0), None)?;
    drop(backup);
    drop(dst);
    drop(src);

    let size = std::fs::metadata(&dest_path).map(|m| m.len()).unwrap_or(0);

    Ok((dest_path, size))
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::Connection;

    /// Create an in-memory SQLite DB seeded with `schema_info` at `version`.
    /// Deliberately does NOT call `schema::initialize` — the runner must work
    /// against just the `schema_info` table.
    fn make_db(version: i64) -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        conn.execute_batch(&format!(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             INSERT INTO schema_info VALUES ('kimetsu_schema_version', {version});"
        ))
        .expect("seed schema_info");
        conn
    }

    /// Seed a file-based DB at `path` with `schema_info` at `version`.
    fn make_file_db(path: &Path, version: i64) -> Connection {
        let conn = Connection::open(path).expect("open file db");
        conn.execute_batch(&format!(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             INSERT INTO schema_info VALUES ('kimetsu_schema_version', {version});"
        ))
        .expect("seed schema_info");
        conn
    }

    /// Seed a file-based DB with schema_info at `version` AND one row in a
    /// minimal `memories` table, so `durable_row_count` returns 1 and the
    /// backup guard fires.
    fn make_file_db_with_memory(path: &Path, version: i64) -> Connection {
        let conn = make_file_db(path, version);
        conn.execute_batch(
            "CREATE TABLE memories (
                 memory_id TEXT PRIMARY KEY,
                 scope TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 text TEXT NOT NULL
             );
             INSERT INTO memories VALUES ('test-mem-id', 'repo', 'preference', 'test memory');",
        )
        .expect("seed memories table");
        conn
    }

    /// Check whether a table exists in `sqlite_master`.
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
    // Test helpers: plain `fn` pointers (not closures) to satisfy
    // `up: fn(&Connection) -> KimetsuResult<()>`.
    // ------------------------------------------------------------------

    fn up_create_m2(conn: &Connection) -> KimetsuResult<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS m2 (x INTEGER);")?;
        Ok(())
    }

    fn up_create_m3(conn: &Connection) -> KimetsuResult<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS m3 (x INTEGER);")?;
        Ok(())
    }

    fn up_fail_partial(conn: &Connection) -> KimetsuResult<()> {
        // Creates a table then returns an error — the table creation must be
        // rolled back together with the version bump.
        conn.execute_batch("CREATE TABLE IF NOT EXISTS partial_table (x INTEGER);")?;
        Err("intentional migration failure".into())
    }

    fn up_create_t(conn: &Connection) -> KimetsuResult<()> {
        conn.execute_batch("CREATE TABLE IF NOT EXISTS t (x INTEGER);")?;
        Ok(())
    }

    // ------------------------------------------------------------------
    // 1. No-op at target
    // ------------------------------------------------------------------
    #[test]
    fn noop_when_at_target() {
        let conn = make_db(5);
        let outcome = run_with(&conn, &[], 5).expect("run_with");
        assert_eq!(
            outcome,
            MigrationOutcome {
                from: 5,
                to: 5,
                applied: vec![],
                backup_path: None,
            }
        );
        // Version unchanged.
        assert_eq!(current_version(&conn).unwrap(), 5);
    }

    // ------------------------------------------------------------------
    // 2. Forward-only guard: stored > target → Err, version unchanged
    // ------------------------------------------------------------------
    #[test]
    fn rejects_newer_db() {
        let conn = make_db(999);
        let err = run_with(&conn, &[], 1).expect_err("should error on newer DB");
        let msg = err.to_string();
        assert!(
            msg.contains("newer"),
            "error message should mention 'newer', got: {msg}"
        );
        // DB version must be untouched.
        assert_eq!(current_version(&conn).unwrap(), 999);
    }

    // ------------------------------------------------------------------
    // 3. Apply migration: advances version, runs DDL in-txn
    // ------------------------------------------------------------------
    #[test]
    fn applies_single_migration() {
        let conn = make_db(1);
        let migs = [Migration {
            version: 2,
            description: "create m2",
            up: up_create_m2,
        }];
        let outcome = run_with(&conn, &migs, 2).expect("run_with");
        assert_eq!(outcome.from, 1);
        assert_eq!(outcome.to, 2);
        assert_eq!(outcome.applied, vec![2]);
        // in-memory — no backup
        assert!(outcome.backup_path.is_none());
        // Version bumped in DB.
        assert_eq!(current_version(&conn).unwrap(), 2);
        // DDL applied.
        assert!(table_exists(&conn, "m2"), "m2 table should exist");
    }

    // ------------------------------------------------------------------
    // 4. Idempotent re-run (current == target → no-op)
    // ------------------------------------------------------------------
    #[test]
    fn idempotent_rerun() {
        let conn = make_db(1);
        let migs = [Migration {
            version: 2,
            description: "create m2",
            up: up_create_m2,
        }];
        // First run.
        run_with(&conn, &migs, 2).expect("first run");
        // Second run — must be a no-op.
        let outcome = run_with(&conn, &migs, 2).expect("second run");
        assert_eq!(
            outcome.applied,
            Vec::<i64>::new(),
            "second run must apply nothing"
        );
        assert_eq!(current_version(&conn).unwrap(), 2);
    }

    // ------------------------------------------------------------------
    // 5. Rollback on failing up: version and DDL both rolled back
    // ------------------------------------------------------------------
    #[test]
    fn rollback_on_failing_migration() {
        let conn = make_db(1);
        let migs = [Migration {
            version: 2,
            description: "fail",
            up: up_fail_partial,
        }];
        let err = run_with(&conn, &migs, 2).expect_err("should propagate migration error");
        assert!(
            err.to_string().contains("intentional"),
            "propagated error should contain original message, got: {err}"
        );
        // Version must still be 1.
        assert_eq!(
            current_version(&conn).unwrap(),
            1,
            "version must be unchanged after rollback"
        );
        // The partial DDL (partial_table) must NOT exist — the txn was rolled back.
        assert!(
            !table_exists(&conn, "partial_table"),
            "partial_table must not exist after rollback"
        );
    }

    // ------------------------------------------------------------------
    // 6. Multi-step chain: applies all steps in order
    // ------------------------------------------------------------------
    #[test]
    fn multi_step_chain() {
        let conn = make_db(1);
        let migs = [
            Migration {
                version: 2,
                description: "create m2",
                up: up_create_m2,
            },
            Migration {
                version: 3,
                description: "create m3",
                up: up_create_m3,
            },
        ];
        let outcome = run_with(&conn, &migs, 3).expect("run_with");
        assert_eq!(outcome.from, 1);
        assert_eq!(outcome.to, 3);
        assert_eq!(outcome.applied, vec![2, 3]);
        assert_eq!(current_version(&conn).unwrap(), 3);
        assert!(table_exists(&conn, "m2"), "m2 should exist");
        assert!(table_exists(&conn, "m3"), "m3 should exist");
    }

    // ------------------------------------------------------------------
    // A4-1. Backup created + stamped at pre-migration version (file DB)
    // ------------------------------------------------------------------
    #[test]
    fn backup_created_for_file_db() {
        let tmp_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_dir = std::env::temp_dir().join(format!("kimetsu-test-backup-{tmp_id}"));
        std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");

        let db_path = tmp_dir.join("brain.db");
        {
            // Seed a memories row so durable_row_count > 0 and the backup fires.
            let conn = make_file_db_with_memory(&db_path, 1);

            let migs = [Migration {
                version: 2,
                description: "create t",
                up: up_create_t,
            }];

            let outcome = run_with(&conn, &migs, 2).expect("run_with");

            // Backup must be Some and the file must exist on disk.
            let bak_path = outcome
                .backup_path
                .expect("backup_path should be Some for file DB");
            assert!(
                bak_path.exists(),
                "backup file should exist at {bak_path:?}"
            );

            // Filename must match pattern brain.db.bak-1-2-*
            let bak_name = bak_path
                .file_name()
                .and_then(|n| n.to_str())
                .expect("backup has a filename");
            assert!(
                bak_name.starts_with("brain.db.bak-1-2-"),
                "backup name should be brain.db.bak-1-2-<ts>, got: {bak_name}"
            );

            // The backup must reflect PRE-migration state (version = 1).
            let bak_conn = Connection::open(&bak_path).expect("open backup db");
            let bak_version: i64 = bak_conn
                .query_row(
                    "SELECT value FROM schema_info WHERE key = 'kimetsu_schema_version'",
                    [],
                    |r| r.get(0),
                )
                .expect("read backup version");
            assert_eq!(
                bak_version, 1,
                "backup should capture pre-migration version 1"
            );

            // Live DB must now be at version 2.
            assert_eq!(current_version(&conn).unwrap(), 2);
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // ------------------------------------------------------------------
    // A4-2. In-memory DB → no backup
    // ------------------------------------------------------------------
    #[test]
    fn no_backup_for_in_memory_db() {
        let conn = make_db(1);
        let migs = [Migration {
            version: 2,
            description: "create t",
            up: up_create_t,
        }];
        let outcome = run_with(&conn, &migs, 2).expect("run_with");
        assert!(
            outcome.backup_path.is_none(),
            "in-memory DB must not produce a backup"
        );
        // Migration must still have been applied.
        assert_eq!(current_version(&conn).unwrap(), 2);
        assert!(table_exists(&conn, "t"), "table t should exist");
    }

    // ------------------------------------------------------------------
    // A4-3. No-op at target → no backup created
    // ------------------------------------------------------------------
    #[test]
    fn no_backup_for_noop() {
        let tmp_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_dir = std::env::temp_dir().join(format!("kimetsu-test-noop-{tmp_id}"));
        std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");

        let db_path = tmp_dir.join("brain.db");
        {
            let conn = make_file_db(&db_path, 2);
            let outcome = run_with(&conn, &[], 2).expect("run_with");

            assert!(
                outcome.backup_path.is_none(),
                "no-op run must not produce a backup"
            );

            // No .bak-* files should exist in the directory.
            let bak_files: Vec<_> = std::fs::read_dir(&tmp_dir)
                .expect("read_dir")
                .filter_map(|e| e.ok())
                .filter(|e| {
                    e.file_name()
                        .to_str()
                        .map(|n| n.contains(".bak-"))
                        .unwrap_or(false)
                })
                .collect();
            assert!(
                bak_files.is_empty(),
                "no backup files should exist after no-op, found: {bak_files:?}"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // ------------------------------------------------------------------
    // A4-4. Retention keep-3: prune_backups removes oldest, keeps 3 newest
    // ------------------------------------------------------------------
    #[test]
    fn retention_keep_3() {
        let tmp_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_dir = std::env::temp_dir().join(format!("kimetsu-test-retention-{tmp_id}"));
        std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");

        // Create 4 fake sidecar files with distinct trailing timestamps.
        // prune_backups sorts by the trailing <ts> integer, so the timestamps
        // in the filenames drive the ordering — mtime is irrelevant.
        let sidecar_names = [
            "brain.db.bak-1-2-1000",
            "brain.db.bak-1-2-2000",
            "brain.db.bak-1-2-3000",
            "brain.db.bak-1-2-4000",
        ];
        for name in &sidecar_names {
            let p = tmp_dir.join(name);
            std::fs::write(&p, b"fake backup").expect("write fake sidecar");
        }

        let db_path = tmp_dir.join("brain.db");
        prune_backups(&db_path, 3);

        // Count surviving .bak-* files.
        let remaining: Vec<_> = std::fs::read_dir(&tmp_dir)
            .expect("read_dir")
            .filter_map(|e| e.ok())
            .filter(|e| {
                e.file_name()
                    .to_str()
                    .map(|n| n.starts_with("brain.db.bak-"))
                    .unwrap_or(false)
            })
            .map(|e| e.file_name().to_str().unwrap_or("").to_owned())
            .collect();

        assert_eq!(
            remaining.len(),
            3,
            "exactly 3 backups should remain after pruning, found: {remaining:?}"
        );

        // The oldest one (ts=1000) must have been deleted.
        assert!(
            !tmp_dir.join("brain.db.bak-1-2-1000").exists(),
            "oldest backup (ts=1000) should have been pruned"
        );
        // The 3 newest must survive.
        assert!(
            tmp_dir.join("brain.db.bak-1-2-2000").exists(),
            "backup ts=2000 should survive"
        );
        assert!(
            tmp_dir.join("brain.db.bak-1-2-3000").exists(),
            "backup ts=3000 should survive"
        );
        assert!(
            tmp_dir.join("brain.db.bak-1-2-4000").exists(),
            "backup ts=4000 should survive"
        );

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // ------------------------------------------------------------------
    // backup_brain tests
    // ------------------------------------------------------------------

    /// Seed a minimal fully-initialized brain DB (schema_info + memories table
    /// with one row) at `path` and return its connection.
    fn make_full_brain_db(path: &Path) -> Connection {
        let conn = Connection::open(path).expect("open brain db");
        // Minimal schema enough for backup_brain to copy.
        conn.execute_batch(&format!(
            "CREATE TABLE schema_info (key TEXT PRIMARY KEY, value INTEGER NOT NULL);
             INSERT INTO schema_info VALUES ('kimetsu_schema_version', {});
             CREATE TABLE memories (
                 memory_id TEXT PRIMARY KEY,
                 scope TEXT NOT NULL,
                 kind TEXT NOT NULL,
                 text TEXT NOT NULL
             );
             INSERT INTO memories VALUES ('bk-mem-1', 'repo', 'fact', 'backup test memory');",
            target_version(),
        ))
        .expect("seed brain db");
        conn
    }

    #[test]
    fn backup_brain_default_path_exists_and_valid() {
        let tmp_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_dir = std::env::temp_dir().join(format!("kimetsu-test-backup-brain-{tmp_id}"));
        std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");

        let db_path = tmp_dir.join("brain.db");
        {
            let _conn = make_full_brain_db(&db_path);
        } // close connection before backup_brain opens it read-only

        let (dest, size) = backup_brain(&db_path, None).expect("backup_brain");

        // Path must exist.
        assert!(dest.exists(), "backup file should exist at {dest:?}");
        // Must be non-empty.
        assert!(size > 0, "backup size should be > 0, got {size}");
        // Name must follow the pattern brain.db.backup-<ts>.
        let name = dest
            .file_name()
            .and_then(|n| n.to_str())
            .expect("backup has a filename");
        assert!(
            name.starts_with("brain.db.backup-"),
            "backup name should start with 'brain.db.backup-', got: {name}"
        );

        // Must be a valid SQLite DB with the expected memory count.
        let bak_conn = Connection::open(&dest).expect("open backup");
        let count: i64 = bak_conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .expect("count memories in backup");
        assert_eq!(count, 1, "backup should contain 1 memory row");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn backup_brain_default_path_does_not_overwrite_existing_backup() {
        let tmp_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_dir =
            std::env::temp_dir().join(format!("kimetsu-test-backup-brain-unique-{tmp_id}"));
        std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");

        let db_path = tmp_dir.join("brain.db");
        {
            let _conn = make_full_brain_db(&db_path);
        }

        let (first, _) = backup_brain(&db_path, None).expect("first backup");
        let (second, _) = backup_brain(&db_path, None).expect("second backup");

        assert_ne!(first, second, "default backups must not overwrite");
        assert!(first.exists(), "first backup should still exist");
        assert!(second.exists(), "second backup should exist");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn backup_brain_custom_path() {
        let tmp_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_dir = std::env::temp_dir().join(format!("kimetsu-test-backup-brain2-{tmp_id}"));
        std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");

        let db_path = tmp_dir.join("brain.db");
        let custom = tmp_dir.join("my-custom-backup.db");
        {
            let _conn = make_full_brain_db(&db_path);
        }

        let (dest, size) = backup_brain(&db_path, Some(&custom)).expect("backup_brain custom");

        assert_eq!(dest, custom, "dest should be the custom path");
        assert!(custom.exists(), "custom backup file should exist");
        assert!(size > 0);

        // Valid SQLite with the expected row.
        let bak_conn = Connection::open(&custom).expect("open custom backup");
        let count: i64 = bak_conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .expect("count memories in custom backup");
        assert_eq!(count, 1, "custom backup should contain 1 memory row");

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }
}
