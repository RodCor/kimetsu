use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use rusqlite::{Connection, params};
use time::format_description::well_known::Rfc3339;

use crate::schema;

pub fn rebuild(conn: &Connection, events: &[Event]) -> KimetsuResult<()> {
    reset_projection(conn)?;
    apply_events(conn, events)
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
    conn.execute_batch(
        "
        DELETE FROM events;
        DELETE FROM runs;
        DELETE FROM sources;
        DELETE FROM memories;
        DELETE FROM memory_proposals;
        DELETE FROM memories_fts;
        ",
    )?;
    Ok(())
}

fn apply_event(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    insert_event(conn, event)?;

    match event.kind.as_str() {
        "run.started" => apply_run_started(conn, event),
        "run.finished" | "run.failed" | "run.aborted" => apply_terminal_run(conn, event),
        "memory.accepted" => apply_memory_accepted(conn, event),
        "memory.proposed" => apply_memory_proposed(conn, event),
        "memory.rejected" => apply_memory_rejected(conn, event),
        "memory.invalidated" => apply_memory_invalidated(conn, event),
        _ => Ok(()),
    }
}

fn insert_event(conn: &Connection, event: &Event) -> KimetsuResult<()> {
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

/// MP-4a outcome attribution: when a run terminates, look up every
/// `context.injected` event the run emitted, collect the unique memory ids,
/// and update each memory's `use_count` and `usefulness_score`.
///
/// Delta rules (per docs/MEMORY-USEFULNESS.md):
///   run.finished                 -> +1 (helped) and +1 use
///   run.failed (cat != "Gate")   -> -1 (hurt)   and +1 use
///   run.failed (cat == "Gate")   ->  no update (graceful early-exit;
///                                   the plan-create existence guard
///                                   doesn't reflect on the memory)
///   run.aborted                  ->  no update (user-initiated stop)
fn apply_memory_usefulness_for_run(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let delta: i64 = match event.kind.as_str() {
        "run.finished" => 1,
        "run.failed" => {
            let category = event
                .payload
                .get("category")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if category == "Gate" {
                return Ok(());
            }
            -1
        }
        _ => return Ok(()), // run.aborted, anything else: no update
    };

    let run_id = event.run_id.to_string();
    let memory_ids = collect_injected_memory_ids(conn, &run_id)?;
    if memory_ids.is_empty() {
        return Ok(());
    }

    for memory_id in memory_ids {
        conn.execute(
            "
            UPDATE memories
            SET use_count = use_count + 1,
                usefulness_score = usefulness_score + ?2,
                last_used_at = ?3
            WHERE memory_id = ?1
            ",
            params![memory_id, delta as f64, ts_text(event)?],
        )?;
    }
    Ok(())
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
