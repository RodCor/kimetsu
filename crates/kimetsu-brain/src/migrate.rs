use std::path::PathBuf;

use kimetsu_core::{KIMETSU_SCHEMA_VERSION, KimetsuResult};
use rusqlite::Connection;

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
    /// Populated by A4 (backup-before-migrate). Always `None` until then.
    pub backup_path: Option<PathBuf>,
}

/// The ordered migration set.
///
/// Invariant (debug-asserted in `run_with`): versions strictly ascending and
/// contiguous starting at 2 (version 1 is the baseline `CREATE`, not a
/// migration step).
fn migrations() -> &'static [Migration] {
    &[Migration {
        version: 2,
        description: "fold additive columns, citations/conflicts tables, and FTS reshapes",
        up: crate::schema::migrate_v1_to_v2,
    }]
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

    // current < target
    // A4 will insert backup_before_migrate(...) here.

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

    Ok(MigrationOutcome {
        from: current,
        to: target,
        applied,
        backup_path: None,
    })
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
}
