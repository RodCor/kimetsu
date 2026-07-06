//! Outcome feedback surfaces: abort, telemetry, citations, regret, set-age.
//! Split out of `project.rs` (v2.5.1); re-exported by [`crate::project`].

use std::path::Path;

use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use kimetsu_core::ids::RunId;
use rusqlite::{Connection, OptionalExtension};

use crate::lock::ProjectLock;
use crate::project::*;
use crate::projector;
use crate::schema;
use crate::trace::TraceWriter;

/// D2: Abort a dangling run — cleanly finalize a run that has no terminal
/// event (e.g. the process was killed mid-way). Steps:
///
/// 1. Validate the run_id exists in `runs`.
/// 2. Error if the run already has a `terminal_kind` (already finished/failed/aborted).
/// 3. Append a `run.aborted` event to the run's trace.
/// 4. Project it (updates `runs.ended_at` + `terminal_kind`).
/// 5. Clear any stale writer lock so subsequent commands can proceed.
///
/// Returns the trace path on success. Errors if the run is unknown or already terminal.
pub fn abort_run(start: &Path, run_id_str: &str) -> KimetsuResult<()> {
    // 1. Validate the run_id exists + check terminal state (read-only query).
    {
        let (_paths, _config, ro_conn) = load_project_readonly(start)?;
        let row: Option<Option<String>> = ro_conn
            .query_row(
                "SELECT terminal_kind FROM runs WHERE run_id = ?1",
                rusqlite::params![run_id_str],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?;
        match row {
            None => {
                return Err(format!("run abort: unknown run_id `{run_id_str}`").into());
            }
            Some(Some(terminal_kind)) => {
                return Err(format!(
                    "run abort: run `{run_id_str}` is already terminal ({})",
                    terminal_kind
                )
                .into());
            }
            Some(None) => {} // dangling — proceed
        }
    }

    // 2. Parse the run_id as a RunId.
    let run_id: RunId = run_id_str
        .parse::<ulid::Ulid>()
        .map(RunId)
        .map_err(|_| format!("run abort: `{run_id_str}` is not a valid ULID run id"))?;

    // 3. Open rw, append run.aborted, project it.
    let (paths, _config, conn) = load_project(start)?;
    let lock = ProjectLock::acquire(&paths, "run abort", Some(run_id))?;

    // Open the trace in append mode (create_dirs is idempotent).
    let (mut writer, _run_paths) = TraceWriter::create(&paths, run_id)?;

    let aborted_event = Event::new(
        run_id,
        "run.aborted",
        serde_json::json!({
            "reason": "manual_abort_via_cli",
        }),
    );
    writer.append(&aborted_event, true)?;
    projector::apply_events(&conn, &[aborted_event])?;

    // 4. Release the write lock acquired above, then force-clear any
    //    additional stale lock file that may have been left by a
    //    previously killed process (clear_force is idempotent).
    lock.release()?;
    crate::lock::clear_force(&paths)?;

    Ok(())
}

/// C7: best-effort telemetry write from a hook context (no active run).
///
/// Appends a single event (e.g. `context.served`) directly to the project
/// brain's `events` table with a sentinel run_id (`"hook"` encoded as a
/// ULID-zero string). Swallows all errors — telemetry must never break
/// a hook. Opens the DB read-write so the hook can record misses without
/// holding a write lock (the DB is opened and closed immediately).
///
/// The sentinel run_id is a valid ULID-shaped string (`00000000000000000000000000`
/// padded to 26 chars). Crucially there is **no** corresponding row in the
/// `runs` table; analytics windows over `context.served` filter by `ts`, not
/// `run_id`, so this is correct.
pub fn log_telemetry_event(
    start: &Path,
    kind: &str,
    payload: serde_json::Value,
) -> KimetsuResult<()> {
    // We need a read-write connection to insert. Use a fresh Connection
    // (not load_project which also validates config) so a misconfigured
    // project.toml never prevents telemetry from writing.
    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    // Sentinel run_id: all-zero ULID (26 '0' chars), never in `runs`.
    let sentinel_run_id = RunId(ulid::Ulid::nil());
    let event = Event::new(sentinel_run_id, kind, payload);
    projector::insert_event(&conn, &event)?;
    Ok(())
}

/// v1.5: scan `events` for `memory.cited` entries and, for each cited
/// memory id, check the dropped-capsule sidecar. When a cited memory
/// was in the recent-dropped window (it was excluded by the relevance
/// floor but the model cited it anyway), emit a `retrieval.regret`
/// telemetry event and remove the entry from the sidecar.
///
/// Purely best-effort: any sidecar or telemetry error is swallowed so
/// citation recording is never disrupted. Called from the pipeline
/// after `projector::apply_events` so citations are already in the DB
/// before we check for regrets.
///
/// Cross-process note: the sidecar is written by the `brain_context_hook`
/// (CLI process) and read here by the pipeline / MCP-server process.
/// Both derive the same cache dir from the repo root, so they
/// naturally share the file without coordination.
pub fn emit_regret_for_cited_memories(start: &Path, events: &[kimetsu_core::event::Event]) {
    use crate::dropped_capsule;
    use kimetsu_core::paths::{ProjectPaths, user_cache_dir_for};

    // Derive the project cache dir; silently skip if the brain is not
    // initialised (e.g. during one-off tests that don't init a project).
    let cache_dir = match ProjectPaths::discover(start) {
        Ok(paths) => user_cache_dir_for(&paths.repo_root),
        Err(_) => return,
    };

    let cited_at = dropped_capsule::now_secs();

    for event in events {
        if event.kind != "memory.cited" {
            continue;
        }
        let Some(memory_id) = event.payload.get("memory_id").and_then(|v| v.as_str()) else {
            continue;
        };
        // Best-effort: swallow any sidecar error.
        let Some(dropped_entry) = dropped_capsule::take_if_dropped(&cache_dir, memory_id, cited_at)
        else {
            continue;
        };
        // Emit the regret event.
        let _ = log_telemetry_event(
            start,
            "retrieval.regret",
            serde_json::json!({
                "memory_id": memory_id,
                "dropped_at": dropped_entry.dropped_at,
                "cited_at": cited_at,
            }),
        );
    }
}

/// v1.5: write a `memory.cited` event from the MCP `kimetsu_brain_cite` tool.
///
/// Uses the same sentinel run_id as [`log_telemetry_event`] (all-zero ULID)
/// so no corresponding `runs` row is required. The event is inserted then
/// projected (populating `memory_citations`) in one connection, and the
/// regret sidecar is checked best-effort.
pub fn record_mcp_citation(start: &Path, memory_id: &str, note: Option<&str>) -> KimetsuResult<()> {
    record_citations(start, &[memory_id.to_string()], note, None)
}

/// v2.5.2 consolidation v1: record one or more standalone citations as a
/// GROUP. All memories share a fresh run_id, which is what makes them
/// co-cited (`brain reinforce --staple` staples pairs that answer together
/// repeatedly). `query` links the citations to the question they answered,
/// feeding the `query_routes` derived index. The `standalone: true` payload
/// flag tells the projector to apply the cited-outcome delta immediately
/// (there is no terminal run event coming), replacing the old nil-run gate
/// so grouped citations still bump usefulness.
pub fn record_citations(
    start: &Path,
    memory_ids: &[String],
    note: Option<&str>,
    query: Option<&str>,
) -> KimetsuResult<()> {
    if memory_ids.is_empty() {
        return Ok(());
    }
    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    let group_run_id = RunId::new();
    let mut events = Vec::with_capacity(memory_ids.len());
    for (turn, memory_id) in memory_ids.iter().enumerate() {
        let mut payload = serde_json::json!({
            "memory_id": memory_id,
            "turn": turn as i64,
            "standalone": true,
        });
        if let Some(n) = note {
            payload["rationale"] = serde_json::json!(n);
        }
        if let Some(q) = query {
            payload["query"] = serde_json::json!(q);
        }
        events.push(kimetsu_core::event::Event::new(
            group_run_id,
            "memory.cited",
            payload,
        ));
    }
    // apply_events calls insert_event + project_event in one transaction.
    projector::apply_events(&conn, &events)?;

    // Best-effort regret check.
    emit_regret_for_cited_memories(start, &events);

    Ok(())
}

/// Inject a `retrieval.regret` telemetry event for a memory.
///
/// The auto-path ([`emit_regret_for_cited_memories`]) only fires when a memory
/// was dropped by a retrieval floor and then cited anyway. This is the explicit
/// path (the `kimetsu brain regret` CLI / benchmarks): it records the negative
/// signal directly so lifecycle review and calibration can be exercised without
/// reproducing the full drop-then-cite dance.
pub fn record_regret(start: &Path, memory_id: &str) -> KimetsuResult<()> {
    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    // Use the sentinel run id (no active run) and PROJECT the event so the
    // outcome handler (`apply_retrieval_regret`) runs live, not just on rebuild.
    let sentinel_run_id = RunId(ulid::Ulid::nil());
    let event = kimetsu_core::event::Event::new(
        sentinel_run_id,
        "retrieval.regret",
        serde_json::json!({ "memory_id": memory_id, "source": "manual" }),
    );
    projector::apply_events(&conn, std::slice::from_ref(&event))?;
    Ok(())
}

/// Backdate a memory's `created_at` / `last_useful_at` by `days_ago` days via a
/// `memory.aged` event. A testing/benchmark affordance for exercising
/// age-sensitive policies (forgetting). The absolute target timestamp is stored
/// in the event payload, so replay on rebuild is deterministic.
pub fn record_set_age(start: &Path, memory_id: &str, days_ago: u32) -> KimetsuResult<()> {
    use time::format_description::well_known::Rfc3339;

    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    let target = time::OffsetDateTime::now_utc() - time::Duration::days(days_ago as i64);
    let ts = target.format(&Rfc3339).unwrap_or_default();

    let sentinel_run_id = RunId(ulid::Ulid::nil());
    let event = kimetsu_core::event::Event::new(
        sentinel_run_id,
        "memory.aged",
        serde_json::json!({ "memory_id": memory_id, "created_at": ts, "last_useful_at": ts }),
    );
    projector::apply_events(&conn, std::slice::from_ref(&event))?;
    Ok(())
}
