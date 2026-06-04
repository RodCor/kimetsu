use std::borrow::Cow;
use std::str::FromStr;

use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use kimetsu_core::ids::{EventId, RunId};
use rusqlite::{Connection, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::schema;

/// Event-schema durability seam. Normalizes an event written under an older
/// `EVENT_SCHEMA_VERSION` to the current payload shape *before projection*,
/// so a future version bump is a localized addition here rather than a
/// projector rewrite. Identity today (`EVENT_SCHEMA_VERSION == 1`: every
/// stored event is already current). When the event schema first changes,
/// add `(kind, schema_version)`-keyed transforms that return `Cow::Owned`
/// with the upgraded payload.
fn upcast_event(event: &Event) -> Cow<'_, Event> {
    // No historical versions to upcast yet.
    Cow::Borrowed(event)
}

pub fn rebuild(conn: &Connection, events: &[Event]) -> KimetsuResult<()> {
    reset_projection(conn)?;
    apply_events(conn, events)
}

/// Rebuild the projection from the durable events table (in place). Reads
/// every stored event, resets the derived tables, and re-projects — WITHOUT
/// re-inserting events (so no duplication). Returns the number of events
/// replayed.
pub fn rebuild_in_place(conn: &Connection) -> KimetsuResult<usize> {
    let events = read_events_ordered(conn)?;
    let tx = conn.unchecked_transaction()?;
    reset_projection(&tx)?;
    for event in &events {
        project_event(&tx, event)?;
    }
    tx.commit()?;
    Ok(events.len())
}

/// Read all stored events from the durable `events` table, ordered by
/// (ts, event_id) so replay is deterministic and causal.
fn read_events_ordered(conn: &Connection) -> KimetsuResult<Vec<Event>> {
    let mut stmt = conn.prepare(
        "
        SELECT event_id, run_id, ts, kind, schema_version, payload_json
        FROM events
        ORDER BY ts, event_id
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        let event_id_str: String = row.get(0)?;
        let run_id_str: String = row.get(1)?;
        let ts_str: String = row.get(2)?;
        let kind: String = row.get(3)?;
        let schema_version: u32 = row.get(4)?;
        let payload_json: String = row.get(5)?;
        Ok((
            event_id_str,
            run_id_str,
            ts_str,
            kind,
            schema_version,
            payload_json,
        ))
    })?;

    let mut events = Vec::new();
    for row in rows {
        let (event_id_str, run_id_str, ts_str, kind, schema_version, payload_json) = row?;
        let event_id = EventId(
            ulid::Ulid::from_str(&event_id_str)
                .map_err(|e| format!("invalid event_id {event_id_str:?}: {e}"))?,
        );
        let run_id = RunId(
            ulid::Ulid::from_str(&run_id_str)
                .map_err(|e| format!("invalid run_id {run_id_str:?}: {e}"))?,
        );
        let ts = OffsetDateTime::parse(&ts_str, &Rfc3339)
            .map_err(|e| format!("invalid ts {ts_str:?}: {e}"))?;
        let payload: serde_json::Value = serde_json::from_str(&payload_json)?;
        events.push(Event {
            event_id,
            run_id,
            ts,
            parent_event_id: None, // not stored; never read by the projector
            kind,
            schema_version,
            payload,
        });
    }
    Ok(events)
}

pub fn apply_events(conn: &Connection, events: &[Event]) -> KimetsuResult<()> {
    let tx = conn.unchecked_transaction()?;
    for event in events {
        apply_event(&tx, event)?;
    }
    tx.commit()?;
    Ok(())
}

fn reset_projection(conn: &Connection) -> KimetsuResult<()> {
    // Wipe ONLY the derived/projected tables. The `events` table is the
    // durable log and MUST survive a rebuild (rebuild replays it).
    conn.execute_batch(
        "
        DELETE FROM runs;
        DELETE FROM sources;
        DELETE FROM memories;
        DELETE FROM memory_proposals;
        DELETE FROM memories_fts;
        DELETE FROM memory_citations;
        DELETE FROM memory_conflicts;
        ",
    )?;
    Ok(())
}

fn apply_event(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    // Persist the event exactly as written (raw, original schema_version).
    insert_event(conn, event)?;
    // Project the now-stored event into the derived tables.
    project_event(conn, event)
}

/// Project a single event into the derived tables (the dispatch half of
/// `apply_event`, WITHOUT inserting into the events table). Used by both the
/// write path (after insert) and the in-place rebuild (events already stored).
fn project_event(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    // Project through the durability seam so older-schema events normalize
    // to the current shape before dispatch.
    let event = upcast_event(event);
    let event = event.as_ref();

    match event.kind.as_str() {
        "run.started" => apply_run_started(conn, event),
        "run.finished" | "run.failed" | "run.aborted" => apply_terminal_run(conn, event),
        "memory.accepted" => apply_memory_accepted(conn, event),
        "memory.proposed" => apply_memory_proposed(conn, event),
        "memory.rejected" => apply_memory_rejected(conn, event),
        "memory.invalidated" => apply_memory_invalidated(conn, event),
        // v0.5.1: per-turn memory citation. The model emits this
        // via the `cite_memory` tool when it consciously leveraged
        // a retrieved capsule. Best-effort — a missing or
        // malformed payload just no-ops.
        "memory.cited" => apply_memory_cited(conn, event),
        _ => Ok(()),
    }
}

fn apply_memory_cited(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(memory_id) = event
        .payload
        .get("memory_id")
        .and_then(|value| value.as_str())
    else {
        // No memory_id -> drop. Citations are best-effort metadata,
        // not load-bearing — silently skipping malformed payloads
        // keeps the run from breaking.
        return Ok(());
    };
    let turn = event
        .payload
        .get("turn")
        .and_then(|value| value.as_i64())
        .unwrap_or(0);
    let rationale = event
        .payload
        .get("rationale")
        .and_then(|value| value.as_str());
    let cited_at = ts_text(event)?;
    conn.execute(
        "
        INSERT OR REPLACE INTO memory_citations (
            run_id, memory_id, turn, cited_at, rationale
        )
        VALUES (?1, ?2, ?3, ?4, ?5)
        ",
        params![
            event.run_id.to_string(),
            memory_id,
            turn,
            cited_at,
            rationale,
        ],
    )?;
    Ok(())
}

pub(crate) fn insert_event(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let payload = serde_json::to_string(&event.payload)?;
    conn.execute(
        "
        INSERT OR IGNORE INTO events (
            event_id, run_id, ts, kind, schema_version, payload_json
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6)
        ",
        params![
            event.event_id.to_string(),
            event.run_id.to_string(),
            ts_text(event)?,
            event.kind,
            event.schema_version,
            payload
        ],
    )?;
    Ok(())
}

fn apply_run_started(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let project_id = event
        .payload
        .get("project_id")
        .and_then(|value| value.as_str())
        .unwrap_or("unknown");
    let task = event
        .payload
        .get("task")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let model = event
        .payload
        .get("model")
        .and_then(|value| value.as_str())
        .map(str::to_string);

    conn.execute(
        "
        INSERT OR IGNORE INTO runs (
            run_id, project_id, task, started_at, model, total_cost_usd
        )
        VALUES (?1, ?2, ?3, ?4, ?5, 0)
        ",
        params![
            event.run_id.to_string(),
            project_id,
            task,
            ts_text(event)?,
            model
        ],
    )?;
    Ok(())
}

fn apply_terminal_run(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let total_cost = event
        .payload
        .get("total_cost_usd")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0);

    conn.execute(
        "
        UPDATE runs
        SET ended_at = ?2,
            terminal_kind = ?3,
            total_cost_usd = ?4
        WHERE run_id = ?1
        ",
        params![
            event.run_id.to_string(),
            ts_text(event)?,
            event.kind,
            total_cost
        ],
    )?;

    apply_memory_usefulness_for_run(conn, event)?;
    Ok(())
}

/// MP-4a + v0.5.1 outcome attribution: when a run terminates, walk every
/// `context.injected` event AND every `memory.cited` event the run emitted,
/// split the unique memory ids into "cited" vs "silent passenger", and
/// update each memory's `use_count` + `usefulness_score`.
///
/// Delta rules:
///   run.finished:
///     cited memory     -> +1.0 usefulness (matches MP-4a baseline)
///     silent passenger -> +0.1 usefulness (weaker signal — it was on
///                         screen but the model didn't reach for it)
///   run.failed (cat != "Gate"):
///     cited memory     -> -1.0 usefulness (the brain pushed wrong)
///     silent passenger -> -0.1 usefulness (was retrieved, didn't help)
///   run.failed (cat == "Gate"):
///     no update (graceful early-exit; the plan-create existence guard
///     doesn't reflect on the memory)
///   run.aborted:
///     no update (user-initiated stop)
///
/// Pre-v0.5.1 behavior: cited == silent (both got the full ±1). When no
/// `memory.cited` events exist (e.g. older runs, models that never call
/// `cite_memory`), every retrieved memory is treated as a silent
/// passenger — i.e. weak ±0.1 instead of strong ±1. This is intentional:
/// without citation evidence we shouldn't claim a memory "helped." The
/// blame command surfaces the discrepancy so operators can encourage
/// citation usage where the brain is under-rewarding good capsules.
fn apply_memory_usefulness_for_run(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let (strong, weak): (f64, f64) = match event.kind.as_str() {
        "run.finished" => (1.0, 0.1),
        "run.failed" => {
            let category = event
                .payload
                .get("category")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if category == "Gate" {
                return Ok(());
            }
            (-1.0, -0.1)
        }
        _ => return Ok(()), // run.aborted, anything else: no update
    };

    let run_id = event.run_id.to_string();
    let retrieved = collect_injected_memory_ids(conn, &run_id)?;
    if retrieved.is_empty() {
        return Ok(());
    }
    let cited = collect_cited_memory_ids(conn, &run_id)?;
    let ts = ts_text(event)?;
    // v0.5.1: bump `last_useful_at` only on cited + run.finished.
    // Cited + run.failed doesn't count (the memory misled the
    // model). Silent passengers never bump regardless of outcome.
    let bump_last_useful = event.kind == "run.finished";

    for memory_id in &retrieved {
        let is_cited = cited.contains(memory_id);
        let delta = if is_cited { strong } else { weak };
        conn.execute(
            "
            UPDATE memories
            SET use_count = use_count + 1,
                usefulness_score = usefulness_score + ?2,
                last_used_at = ?3
            WHERE memory_id = ?1
            ",
            params![memory_id, delta, ts],
        )?;
        if is_cited && bump_last_useful {
            // v0.5.1: separate column for the decay reference. We
            // intentionally only touch it for confirmed successful
            // citations so the half-life curve in `usefulness_-
            // multiplier` reflects when the memory was last
            // PROVEN to help — not just when it was retrieved.
            conn.execute(
                "UPDATE memories SET last_useful_at = ?2 WHERE memory_id = ?1",
                params![memory_id, ts],
            )?;
        }
    }
    Ok(())
}

/// v0.5.1: walk this run's `memory_citations` rows and return the unique
/// memory ids that the model explicitly cited via the `cite_memory` tool.
/// Used by `apply_memory_usefulness_for_run` to give the strong delta
/// only to memories that actually contributed to the model's reasoning.
fn collect_cited_memory_ids(
    conn: &Connection,
    run_id: &str,
) -> KimetsuResult<std::collections::BTreeSet<String>> {
    let mut stmt = conn.prepare(
        "
        SELECT DISTINCT memory_id
        FROM memory_citations
        WHERE run_id = ?1
        ",
    )?;
    let rows = stmt.query_map(params![run_id], |row| row.get::<_, String>(0))?;
    let mut out = std::collections::BTreeSet::new();
    for row in rows {
        out.insert(row?);
    }
    Ok(out)
}

/// Walk this run's `context.injected` events and return the unique memory
/// ids that were surfaced into any stage's broker bundle. Per-run counting:
/// a memory injected into Localization AND PatchPlan in the same run counts
/// once.
fn collect_injected_memory_ids(conn: &Connection, run_id: &str) -> KimetsuResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "
        SELECT payload_json
        FROM events
        WHERE run_id = ?1 AND kind = 'context.injected'
        ",
    )?;
    let rows = stmt.query_map(params![run_id], |row| row.get::<_, String>(0))?;

    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        let payload_json = row?;
        let payload: serde_json::Value = serde_json::from_str(&payload_json)?;
        if let Some(ids) = payload.get("memory_ids").and_then(|v| v.as_array()) {
            for id in ids {
                if let Some(id_str) = id.as_str()
                    && !id_str.is_empty()
                {
                    seen.insert(id_str.to_string());
                }
            }
        }
    }
    Ok(seen.into_iter().collect())
}

fn apply_memory_accepted(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(memory_id) = event
        .payload
        .get("memory_id")
        .and_then(|value| value.as_str())
    else {
        return Ok(());
    };
    let scope = event
        .payload
        .get("scope")
        .and_then(|value| value.as_str())
        .unwrap_or("global_user");
    let kind = event
        .payload
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or("fact");
    let text = event
        .payload
        .get("text")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let normalized_text = event
        .payload
        .get("normalized_text")
        .and_then(|value| value.as_str())
        .unwrap_or(text);
    let confidence = event
        .payload
        .get("confidence")
        .and_then(|value| value.as_f64())
        .unwrap_or(1.0);
    let provenance_snapshot = event
        .payload
        .get("provenance_snapshot")
        .cloned()
        .unwrap_or_else(
            || serde_json::json!({ "source": "event", "event_id": event.event_id.to_string() }),
        );

    conn.execute(
        "
        INSERT OR REPLACE INTO memories (
            memory_id, scope, kind, text, normalized_text, confidence,
            source_event_id, provenance_snapshot_json, created_at, use_count
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0)
        ",
        params![
            memory_id,
            scope,
            kind,
            text,
            normalized_text,
            confidence,
            event.event_id.to_string(),
            serde_json::to_string(&provenance_snapshot)?,
            ts_text(event)?
        ],
    )?;

    conn.execute(
        "DELETE FROM memories_fts WHERE memory_id = ?1",
        params![memory_id],
    )?;
    conn.execute(
        "INSERT INTO memories_fts (memory_id, text, kind, scope) VALUES (?1, ?2, ?3, ?4)",
        params![memory_id, text, kind, scope],
    )?;
    Ok(())
}

fn apply_memory_proposed(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(proposal_id) = event
        .payload
        .get("proposal_id")
        .and_then(|value| value.as_str())
    else {
        return Ok(());
    };
    let scope = event
        .payload
        .get("scope")
        .and_then(|value| value.as_str())
        .unwrap_or("run");
    let kind = event
        .payload
        .get("kind")
        .and_then(|value| value.as_str())
        .unwrap_or("fact");
    let text = event
        .payload
        .get("text")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let rationale = event
        .payload
        .get("rationale")
        .and_then(|value| value.as_str())
        .unwrap_or("");
    let confidence = event
        .payload
        .get("proposed_confidence")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.5);
    let source_event_ids = event
        .payload
        .get("source_event_ids")
        .cloned()
        .unwrap_or_else(|| serde_json::json!([]));

    conn.execute(
        "
        INSERT OR REPLACE INTO memory_proposals (
            proposal_id, run_id, scope, kind, text, rationale,
            proposed_confidence, source_event_ids_json, status
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, 'pending')
        ",
        params![
            proposal_id,
            event.run_id.to_string(),
            scope,
            kind,
            text,
            rationale,
            confidence,
            serde_json::to_string(&source_event_ids)?
        ],
    )?;
    Ok(())
}

fn apply_memory_rejected(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(proposal_id) = event
        .payload
        .get("proposal_id")
        .and_then(|value| value.as_str())
    else {
        return Ok(());
    };
    let reason = event
        .payload
        .get("reason")
        .and_then(|value| value.as_str())
        .map(|s| s.to_string());

    conn.execute(
        "
        UPDATE memory_proposals
        SET status = 'rejected',
            decided_at = ?2,
            decided_by = 'cli',
            decided_reason = ?3
        WHERE proposal_id = ?1
        ",
        params![proposal_id, ts_text(event)?, reason],
    )?;
    Ok(())
}

/// MP-4d: human-invalidated memories are flagged so the broker excludes
/// them from retrieval and `kimetsu brain memory list` can render the
/// reason. The canonical trace still holds the original memory.accepted
/// event; invalidation is additive metadata, not a delete.
fn apply_memory_invalidated(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(memory_id) = event
        .payload
        .get("memory_id")
        .and_then(|value| value.as_str())
    else {
        return Ok(());
    };
    let reason = event
        .payload
        .get("reason")
        .and_then(|value| value.as_str())
        .map(|s| s.to_string());
    conn.execute(
        "
        UPDATE memories
        SET invalidated_at = ?2,
            invalidated_reason = ?3
        WHERE memory_id = ?1
        ",
        params![memory_id, ts_text(event)?, reason],
    )?;
    Ok(())
}

pub fn ensure_schema(conn: &Connection) -> KimetsuResult<()> {
    schema::initialize(conn)
}

fn ts_text(event: &Event) -> KimetsuResult<String> {
    Ok(event.ts.format(&Rfc3339)?)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::borrow::Cow;

    use kimetsu_core::event::Event;
    use kimetsu_core::ids::RunId;
    use rusqlite::Connection;
    use serde_json::json;

    use super::{apply_events, upcast_event};
    use crate::schema;

    fn make_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        schema::initialize(&conn).expect("schema::initialize");
        conn
    }

    fn make_event(run_id: RunId, kind: &str, payload: serde_json::Value) -> Event {
        Event::new(run_id, kind, payload)
    }

    // ------------------------------------------------------------------
    // A6-1. upcast_event is identity (Cow::Borrowed) at schema_version 1
    // ------------------------------------------------------------------
    #[test]
    fn upcast_is_identity_at_v1() {
        let run_id = RunId::new();
        let event = make_event(
            run_id,
            "run.started",
            json!({"project_id": "p1", "task": "t"}),
        );
        assert_eq!(
            event.schema_version, 1,
            "Event::new must stamp schema_version=1"
        );

        let cow = upcast_event(&event);
        // Must be a Borrowed reference, not an owned clone.
        assert!(
            matches!(cow, Cow::Borrowed(_)),
            "upcast_event must return Cow::Borrowed for current schema_version"
        );
        // The payload fields must be unchanged.
        let out = cow.as_ref();
        assert_eq!(out.kind, event.kind);
        assert_eq!(out.schema_version, event.schema_version);
        assert_eq!(out.payload, event.payload);
    }

    // ------------------------------------------------------------------
    // A6-2. Per-kind missing-field durability: every dispatched kind with
    // an empty payload replays without panic/error.
    // ------------------------------------------------------------------

    fn assert_empty_payload_ok(kind: &str) {
        let conn = make_conn();
        let run_id = RunId::new();
        let event = make_event(run_id, kind, json!({}));
        let result = apply_events(&conn, &[event]);
        assert!(
            result.is_ok(),
            "apply_events with empty payload for kind={kind:?} must return Ok(()), got: {result:?}"
        );
    }

    #[test]
    fn empty_payload_run_started() {
        assert_empty_payload_ok("run.started");
    }

    #[test]
    fn empty_payload_run_finished() {
        assert_empty_payload_ok("run.finished");
    }

    #[test]
    fn empty_payload_run_failed() {
        assert_empty_payload_ok("run.failed");
    }

    #[test]
    fn empty_payload_run_aborted() {
        assert_empty_payload_ok("run.aborted");
    }

    #[test]
    fn empty_payload_memory_accepted() {
        assert_empty_payload_ok("memory.accepted");
    }

    #[test]
    fn empty_payload_memory_proposed() {
        assert_empty_payload_ok("memory.proposed");
    }

    #[test]
    fn empty_payload_memory_rejected() {
        assert_empty_payload_ok("memory.rejected");
    }

    #[test]
    fn empty_payload_memory_invalidated() {
        assert_empty_payload_ok("memory.invalidated");
    }

    #[test]
    fn empty_payload_memory_cited() {
        assert_empty_payload_ok("memory.cited");
    }

    // ------------------------------------------------------------------
    // A6-3. A well-formed run.started event still projects correctly
    // after routing through the upcast seam.
    // ------------------------------------------------------------------
    #[test]
    fn well_formed_run_started_projects_correctly() {
        let conn = make_conn();
        let run_id = RunId::new();
        let event = make_event(
            run_id,
            "run.started",
            json!({
                "project_id": "proj-abc",
                "task": "fix the bug",
                "model": "claude-sonnet-4-6"
            }),
        );
        apply_events(&conn, &[event])
            .expect("apply_events must succeed for well-formed run.started");

        let row: (String, String, String) = conn
            .query_row(
                "SELECT run_id, project_id, task FROM runs WHERE run_id = ?1",
                [run_id.to_string()],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("runs row must exist after apply_events");

        assert_eq!(row.0, run_id.to_string());
        assert_eq!(row.1, "proj-abc");
        assert_eq!(row.2, "fix the bug");
    }

    // ------------------------------------------------------------------
    // W1.1: reset_projection keeps the events table intact while wiping
    // all derived/projected tables.
    // ------------------------------------------------------------------
    #[test]
    fn reset_projection_keeps_events() {
        use super::reset_projection;

        let conn = make_conn();
        let run_id = RunId::new();
        let mem_id = "mem-reset-test";

        let events = vec![
            make_event(
                run_id,
                "run.started",
                json!({"project_id": "p", "task": "t"}),
            ),
            make_event(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": mem_id,
                    "text": "hello",
                    "scope": "global_user",
                    "kind": "fact"
                }),
            ),
        ];
        apply_events(&conn, &events).expect("apply_events");

        // Preconditions: both events stored, memory projected.
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert!(event_count > 0, "events must be stored before reset");
        let mem_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mem_count, 1, "memory must be projected before reset");

        reset_projection(&conn).expect("reset_projection");

        // Events MUST survive.
        let event_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            event_count_after, event_count,
            "reset_projection must NOT delete from events"
        );

        // All derived tables must be empty.
        let memories_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            memories_after, 0,
            "memories must be cleared by reset_projection"
        );

        let runs_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM runs", [], |r| r.get(0))
            .unwrap();
        assert_eq!(runs_after, 0, "runs must be cleared by reset_projection");

        let citations_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_citations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            citations_after, 0,
            "memory_citations must be cleared by reset_projection"
        );

        let conflicts_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_conflicts", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            conflicts_after, 0,
            "memory_conflicts must be cleared by reset_projection"
        );
    }

    // ------------------------------------------------------------------
    // W1.2a: rebuild_in_place round-trips without duplicating events.
    // ------------------------------------------------------------------
    #[test]
    fn rebuild_in_place_no_dup_events() {
        use super::rebuild_in_place;

        let conn = make_conn();
        let run_id = RunId::new();
        let mem_id = "mem-dup-test";

        let events = vec![
            make_event(
                run_id,
                "run.started",
                json!({"project_id": "p", "task": "t"}),
            ),
            make_event(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": mem_id,
                    "text": "no dup",
                    "scope": "global_user",
                    "kind": "fact"
                }),
            ),
            make_event(run_id, "run.finished", json!({"total_cost_usd": 0.01})),
        ];
        apply_events(&conn, &events).expect("apply_events");

        let event_count_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(event_count_before, 3, "expected 3 events seeded");

        // Manually wipe derived tables to simulate a corrupted projection.
        conn.execute_batch("DELETE FROM memories; DELETE FROM memories_fts;")
            .unwrap();
        let mem_count_wiped: i64 = conn
            .query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))
            .unwrap();
        assert_eq!(mem_count_wiped, 0, "memories wiped before rebuild_in_place");

        let replayed = rebuild_in_place(&conn).expect("rebuild_in_place");

        // Correct replay count.
        assert_eq!(
            replayed, 3,
            "rebuild_in_place must return the number of events replayed"
        );

        // Memory is back.
        let mem_exists: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memories WHERE memory_id = ?1",
                [mem_id],
                |r| r.get(0),
            )
            .unwrap();
        assert_eq!(
            mem_exists, 1,
            "memory must be re-projected after rebuild_in_place"
        );

        // NO duplicate events inserted.
        let event_count_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            event_count_after, event_count_before,
            "rebuild_in_place must NOT insert duplicate events"
        );
    }

    // ------------------------------------------------------------------
    // W1.2b: rebuild_in_place reconstructs memory_citations (proves
    // project_event runs the full dispatch including memory.cited).
    // ------------------------------------------------------------------
    #[test]
    fn rebuild_in_place_reconstructs_citations() {
        use super::rebuild_in_place;

        let conn = make_conn();
        let run_id = RunId::new();
        let mem_id = "mem-cite-test";

        let events = vec![
            make_event(
                run_id,
                "run.started",
                json!({"project_id": "p", "task": "t"}),
            ),
            make_event(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": mem_id,
                    "text": "cite me",
                    "scope": "global_user",
                    "kind": "fact"
                }),
            ),
            make_event(
                run_id,
                "memory.cited",
                json!({
                    "memory_id": mem_id,
                    "turn": 2,
                    "rationale": "relevant context"
                }),
            ),
            make_event(run_id, "run.finished", json!({"total_cost_usd": 0.0})),
        ];
        apply_events(&conn, &events).expect("apply_events");

        let citations_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_citations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            citations_before, 1,
            "citation must exist after apply_events"
        );

        let replayed = rebuild_in_place(&conn).expect("rebuild_in_place");
        assert_eq!(replayed, 4, "expected 4 events replayed");

        let citations_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_citations", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            citations_after, 1,
            "memory_citations must be repopulated by rebuild_in_place"
        );
    }

    // ------------------------------------------------------------------
    // W1.2c: Event reconstruction fidelity — after rebuild_in_place the
    // projected memory's text/scope/kind match the original.
    // ------------------------------------------------------------------
    #[test]
    fn rebuild_in_place_payload_fidelity() {
        use super::rebuild_in_place;

        let conn = make_conn();
        let run_id = RunId::new();
        let mem_id = "mem-fidelity-test";
        let expected_text = "Rust edition 2024 requires explicit use of `use` for trait impls";
        let expected_scope = "project";
        let expected_kind = "guideline";

        let events = vec![
            make_event(
                run_id,
                "run.started",
                json!({"project_id": "p", "task": "t"}),
            ),
            make_event(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": mem_id,
                    "text": expected_text,
                    "scope": expected_scope,
                    "kind": expected_kind,
                    "confidence": 0.9
                }),
            ),
        ];
        apply_events(&conn, &events).expect("apply_events");

        // Wipe derived tables to force a full rebuild.
        conn.execute_batch("DELETE FROM memories; DELETE FROM memories_fts; DELETE FROM runs;")
            .unwrap();

        rebuild_in_place(&conn).expect("rebuild_in_place");

        let row: (String, String, String) = conn
            .query_row(
                "SELECT text, scope, kind FROM memories WHERE memory_id = ?1",
                [mem_id],
                |r| Ok((r.get(0)?, r.get(1)?, r.get(2)?)),
            )
            .expect("memory must exist after rebuild_in_place");

        assert_eq!(row.0, expected_text, "text must round-trip through rebuild");
        assert_eq!(
            row.1, expected_scope,
            "scope must round-trip through rebuild"
        );
        assert_eq!(row.2, expected_kind, "kind must round-trip through rebuild");
    }
}
