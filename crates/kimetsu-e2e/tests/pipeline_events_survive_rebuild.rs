//! W1.6 regression-lock: agent-run events land in brain.db and survive rebuild.
//!
//! Critical invariant of the durable-events-log change (W1.1–W1.3):
//! every event a pipeline run emits MUST be inserted into the `events`
//! table (via `projector::apply_events`), and `rebuild_projection` —
//! which now replays the `events` table rather than on-disk trace.jsonl
//! — must reproduce the `runs` row from those events alone.
//!
//! If any pipeline exit path bypassed `apply_events` and wrote events
//! ONLY to trace.jsonl, those events would be silently lost on rebuild.
//! This test guards that gap.
//!
//! Technique:
//!  1. Run a deterministic (no-model, dry-run, broker-disabled) coding
//!     pipeline against a temp project.
//!  2. Assert the run's events are in brain.db's `events` table.
//!  3. Assert the run's row is in brain.db's `runs` table.
//!  4. Call `rebuild_projection(root, false)` — pure events-table replay,
//!     no trace.jsonl — and assert the `runs` row survives.

use kimetsu_agent::pipeline::{CodingRunOptions, run_coding_dry_run};
use kimetsu_brain::project;
use kimetsu_brain::user_brain::with_user_brain_disabled;
use kimetsu_e2e::prelude::*;

#[test]
fn agent_run_events_survive_rebuild_from_events_table() {
    with_user_brain_disabled(|| {
        let project = TempProject::init("pipeline_events_rebuild");

        // ── 1. Run a deterministic dry-run coding pipeline ───────────────────
        //
        // dry_run=true   → skips the Implementation loop entirely.
        // disable_model=true → build_dry_run_patch_plan() is used; no API key.
        // disable_broker=true → skips all context retrieval (no embedder needed).
        //
        // In cfg!(test) builds, try_model_patch_plan also returns Ok(None)
        // (line 1150 of pipeline.rs), so the model is never contacted even
        // if disable_model were false. We set it explicitly for clarity.
        let result = run_coding_dry_run(CodingRunOptions {
            repo: project.root().to_path_buf(),
            task: "W1.6 regression-lock: dry-run smoke task".to_string(),
            dry_run: true,
            allow_high_risk: false,
            disable_model: true,
            disable_broker: true,
            model_key_override: None,
        })
        .expect("dry-run pipeline must succeed");

        let run_id = result.run_id.to_string();

        // ── 2. The run's events must be in brain.db's events table ───────────
        let conn = project.open_brain();

        let event_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1",
                rusqlite::params![run_id],
                |row| row.get(0),
            )
            .expect("COUNT(*) events query");

        assert!(
            event_count > 0,
            "run {run_id}: expected at least one event in brain.db events table, got 0; \
             this means apply_events was not called on the happy-path exit"
        );

        // ── 3. The run's row must be in brain.db's runs table ─────────────────
        let runs_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE run_id = ?1",
                rusqlite::params![run_id],
                |row| row.get(0),
            )
            .expect("COUNT(*) runs query");

        assert_eq!(
            runs_count, 1,
            "run {run_id}: expected exactly one row in runs table before rebuild, got {runs_count}"
        );

        // ── 4. Drop the derived projection + rebuild from events table only ───
        //
        // rebuild_projection(root, false) == rebuild_in_place: it reads the
        // events table, wipes derived tables (runs, memories, …), then
        // re-projects. If any pipeline event was ONLY in trace.jsonl — not in
        // the events table — the run row would disappear here.
        drop(conn); // release the connection before rebuild acquires the lock

        let events_replayed = project::rebuild_projection(project.root(), false)
            .expect("rebuild_projection must succeed");

        assert!(
            events_replayed > 0,
            "rebuild replayed 0 events; expected at least the events for run {run_id}"
        );

        // ── 5. The run must still be in the runs table after rebuild ──────────
        let conn_after = project.open_brain();

        let runs_after: i64 = conn_after
            .query_row(
                "SELECT COUNT(*) FROM runs WHERE run_id = ?1",
                rusqlite::params![run_id],
                |row| row.get(0),
            )
            .expect("COUNT(*) runs after rebuild");

        assert_eq!(
            runs_after, 1,
            "run {run_id}: expected the run to survive rebuild_projection (events → runs), \
             but got {runs_after} rows; the pipeline's apply_events call must cover all \
             exit paths so every event is in the events table before rebuild"
        );

        // ── 6. Spot-check: run.started + run.finished must both be present ────
        let terminal_count: i64 = conn_after
            .query_row(
                "SELECT COUNT(*) FROM events WHERE run_id = ?1 AND kind IN ('run.started', 'run.finished')",
                rusqlite::params![run_id],
                |row| row.get(0),
            )
            .expect("COUNT(*) terminal events");

        assert_eq!(
            terminal_count, 2,
            "run {run_id}: expected both run.started and run.finished in the events table \
             (got {terminal_count}); the dry-run pipeline happy-path must emit and persist both"
        );
    });
}
