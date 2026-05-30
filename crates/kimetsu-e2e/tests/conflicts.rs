//! v0.5.3 e2e: v0.5.2 conflict-detection surface end-to-end.
//!
//! Conflict detection at ingest is embedder-gated; the lean default
//! build uses NoopEmbedder so conflict scans are no-ops. To prove the
//! end-to-end UX (table + list_conflicts + resolve_conflict) we
//! manually seed two memories + a conflict row via the public
//! `kimetsu_brain::conflict` API, then exercise the
//! `kimetsu_brain::project` wrappers the CLI / MCP surfaces sit on.
//!
//! This catches:
//!   * Schema drift on `memory_conflicts` (column rename / type
//!     change breaks the seed insert).
//!   * Project-level `list_conflicts` / `resolve_conflict` wrappers
//!     compose correctly across the project brain (user-brain merge
//!     is exercised by unit tests in `conflict::tests`).
//!   * `kept_new` resolution invalidates the existing memory and
//!     marks the conflict row resolved atomically.

use kimetsu_brain::conflict::{self, ConflictHit};
use kimetsu_brain::user_brain::with_user_brain_disabled;
use kimetsu_e2e::prelude::*;

#[test]
fn list_and_resolve_conflict_wrappers_compose_against_a_real_project() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("conflicts_resolve");

        // Two contradictory preferences. Real prod code would hit
        // these via add_memory's detect_and_record path under an
        // embeddings build; here we seed both the memories and the
        // conflict row directly to keep the test lean+deterministic.
        let new_id = project::add_memory(
            project.root(),
            MemoryScope::Repo,
            MemoryKind::Preference,
            "Prefer anyhow for library error types.",
        )
        .expect("add new memory");
        let existing_id = project::add_memory(
            project.root(),
            MemoryScope::Repo,
            MemoryKind::Preference,
            "Prefer thiserror for library error types.",
        )
        .expect("add existing memory");

        let conn = project.open_brain();
        let hit = ConflictHit {
            existing_memory_id: existing_id.clone(),
            existing_kind: "preference".to_string(),
            existing_text: "Prefer thiserror for library error types.".to_string(),
            similarity: 0.92,
        };
        let conflict_id =
            conflict::record_conflict(&conn, &new_id, &MemoryScope::Repo, "preference", &hit)
                .expect("record conflict");
        drop(conn);

        // Smoke-check: list_conflicts surfaces the row through the
        // project wrapper (which merges project + user brains).
        let open = project::list_conflicts(project.root(), 50).expect("list_conflicts");
        assert_eq!(open.len(), 1, "exactly one open conflict expected");
        assert_eq!(
            open[0].source, "project",
            "conflict should be sourced from project brain"
        );
        assert_eq!(open[0].report.new_memory_id, new_id);
        assert_eq!(open[0].report.existing_memory_id, existing_id);
        assert!(
            (open[0].report.similarity - 0.92).abs() < 1e-3,
            "similarity should survive the round-trip; got {}",
            open[0].report.similarity
        );

        // Resolve with kept_new — existing memory should be invalidated.
        let resolved = project::resolve_conflict(project.root(), &conflict_id, "kept_new")
            .expect("resolve_conflict");
        assert!(
            resolved,
            "resolve_conflict should return true on first apply"
        );

        // Post-conditions:
        //   * list_conflicts returns empty (resolved row excluded)
        //   * existing memory's invalidated_at is set
        //   * new memory remains active
        let still_open = project::list_conflicts(project.root(), 50).expect("list after resolve");
        assert!(still_open.is_empty(), "no open conflicts after resolve");

        let conn = project.open_brain();
        let existing_invalidated: Option<String> = conn
            .query_row(
                "SELECT invalidated_at FROM memories WHERE memory_id = ?1",
                rusqlite::params![&existing_id],
                |row| row.get(0),
            )
            .expect("query existing");
        assert!(
            existing_invalidated.is_some(),
            "kept_new should invalidate the existing memory"
        );
        let new_invalidated: Option<String> = conn
            .query_row(
                "SELECT invalidated_at FROM memories WHERE memory_id = ?1",
                rusqlite::params![&new_id],
                |row| row.get(0),
            )
            .expect("query new");
        assert!(
            new_invalidated.is_none(),
            "kept_new winner should stay active"
        );
    });
}

#[test]
fn re_resolving_same_conflict_is_idempotent_through_project_wrapper() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("conflicts_idempotent");

        let new_id = project::add_memory(
            project.root(),
            MemoryScope::Repo,
            MemoryKind::Preference,
            "Use tabs for indentation.",
        )
        .expect("add new");
        let existing_id = project::add_memory(
            project.root(),
            MemoryScope::Repo,
            MemoryKind::Preference,
            "Use spaces for indentation.",
        )
        .expect("add existing");

        let conn = project.open_brain();
        let hit = ConflictHit {
            existing_memory_id: existing_id.clone(),
            existing_kind: "preference".to_string(),
            existing_text: "Use spaces for indentation.".to_string(),
            similarity: 0.88,
        };
        let conflict_id =
            conflict::record_conflict(&conn, &new_id, &MemoryScope::Repo, "preference", &hit)
                .expect("record");
        drop(conn);

        assert!(
            project::resolve_conflict(project.root(), &conflict_id, "kept_both")
                .expect("first resolve")
        );
        // Second resolve on the same id must return false (already resolved),
        // not error and not double-invalidate.
        assert!(
            !project::resolve_conflict(project.root(), &conflict_id, "kept_new")
                .expect("second resolve"),
            "resolve_conflict on an already-resolved row should return false"
        );

        // And the resolution that landed (kept_both) shouldn't have
        // touched either memory's invalidated_at.
        let conn = project.open_brain();
        let count_invalidated: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NOT NULL",
                [],
                |row| row.get(0),
            )
            .expect("query invalidated");
        assert_eq!(
            count_invalidated, 0,
            "kept_both must not invalidate either memory; got {count_invalidated}"
        );
    });
}
