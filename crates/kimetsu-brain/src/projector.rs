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
    Ok(())
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
        "INSERT INTO memories_fts (text, kind, scope) VALUES (?1, ?2, ?3)",
        params![text, kind, scope],
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

    conn.execute(
        "
        UPDATE memory_proposals
        SET status = 'rejected',
            decided_at = ?2,
            decided_by = 'cli'
        WHERE proposal_id = ?1
        ",
        params![proposal_id, ts_text(event)?],
    )?;
    Ok(())
}

pub fn ensure_schema(conn: &Connection) -> KimetsuResult<()> {
    schema::initialize(conn)
}

fn ts_text(event: &Event) -> KimetsuResult<String> {
    Ok(event.ts.format(&Rfc3339)?)
}
