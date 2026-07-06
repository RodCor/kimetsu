//! Reusable brain-state assertions for e2e tests.
//!
//! Each helper opens its own connection so a test can interleave loop
//! runs (which hold their own write connection) with assertions
//! without deadlocking on the WAL writer.

use rusqlite::Connection;

/// Count active (non-invalidated) memories in the brain.
pub fn count_memories(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
        [],
        |row| row.get(0),
    )
    .expect("count memories")
}

/// Count `memory_citations` rows. Bumped by the projector each time a
/// `memory.cited` event is applied. One row per (run_id, memory_id,
/// turn).
pub fn count_citations(conn: &Connection) -> i64 {
    conn.query_row("SELECT COUNT(*) FROM memory_citations", [], |row| {
        row.get(0)
    })
    .expect("count citations")
}

/// Count open (unresolved) conflict-detection hits. Resolved rows are
/// excluded so this number tracks "things the operator still needs to
/// look at."
pub fn count_conflicts(conn: &Connection) -> i64 {
    conn.query_row(
        "SELECT COUNT(*) FROM memory_conflicts WHERE resolved_at IS NULL",
        [],
        |row| row.get(0),
    )
    .expect("count conflicts")
}

/// Assert a specific memory row exists (matches by `memory_id`) and is
/// not invalidated. Panics with a descriptive message on miss so the
/// test failure points at the right line.
pub fn assert_memory_present(conn: &Connection, memory_id: &str) {
    let found: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memories
             WHERE memory_id = ?1 AND invalidated_at IS NULL",
            rusqlite::params![memory_id],
            |row| row.get(0),
        )
        .expect("query memories");
    assert_eq!(
        found, 1,
        "expected active memory {memory_id} in brain, got {found} matching rows"
    );
}

/// Assert a `memory_citations` row exists for the given `(run_id,
/// memory_id, turn)` triple.
pub fn assert_citation_recorded(conn: &Connection, run_id: &str, memory_id: &str, turn: i64) {
    let found: i64 = conn
        .query_row(
            "SELECT COUNT(*) FROM memory_citations
             WHERE run_id = ?1 AND memory_id = ?2 AND turn = ?3",
            rusqlite::params![run_id, memory_id, turn],
            |row| row.get(0),
        )
        .expect("query memory_citations");
    assert_eq!(
        found, 1,
        "expected memory_citations row for run={run_id} memory={memory_id} turn={turn}, got {found}"
    );
}
