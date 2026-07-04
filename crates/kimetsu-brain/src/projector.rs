use std::borrow::Cow;
use std::str::FromStr;
use std::time::Duration;

use kimetsu_core::KimetsuResult;
use kimetsu_core::event::Event;
use kimetsu_core::ids::{EventId, RunId};
use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::redact;
use crate::schema;

/// Max attempts for a write transaction that loses the race to `SQLITE_BUSY`
/// after the 15s busy_timeout (rare; a fleet burst). The whole transaction is
/// retried from a clean state — safe because BUSY can only surface at `BEGIN`
/// (the IMMEDIATE write lock is held for the entire body once acquired).
const WRITE_TXN_MAX_ATTEMPTS: u32 = 5;

/// True when `err` is a SQLite busy/locked condition (downcastable through the
/// boxed `KimetsuResult` error, since `?` preserves the concrete type).
fn is_sqlite_busy(err: &(dyn std::error::Error + 'static)) -> bool {
    err.downcast_ref::<rusqlite::Error>()
        .and_then(|e| e.sqlite_error_code())
        .is_some_and(|code| {
            matches!(
                code,
                rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked
            )
        })
}

/// Run `body` inside a single `BEGIN IMMEDIATE` transaction (concurrent-write
/// safe): the write lock is taken at `BEGIN`, so two processes writing the same
/// brain.db serialize cleanly and read-modify-write projections (use_count,
/// confidence) never interleave across writers. Retries the whole transaction on
/// `SQLITE_BUSY`/`LOCKED` (which can only occur at `BEGIN`). `&Connection` can't
/// use `transaction_with_behavior`, so the transaction is driven manually.
fn with_write_txn<F>(conn: &Connection, mut body: F) -> KimetsuResult<()>
where
    F: FnMut(&Connection) -> KimetsuResult<()>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        // BEGIN IMMEDIATE — acquires the write lock now. BUSY surfaces here.
        if let Err(e) = conn.execute_batch("BEGIN IMMEDIATE") {
            let boxed: Box<dyn std::error::Error + Send + Sync> = e.into();
            if is_sqlite_busy(boxed.as_ref()) && attempt < WRITE_TXN_MAX_ATTEMPTS {
                std::thread::sleep(Duration::from_millis(20 * attempt as u64));
                continue;
            }
            return Err(boxed);
        }
        // Lock held — run the body, then COMMIT (or ROLLBACK on any error).
        match body(conn) {
            Ok(()) => match conn.execute_batch("COMMIT") {
                Ok(()) => return Ok(()),
                Err(e) => {
                    let _ = conn.execute_batch("ROLLBACK");
                    return Err(e.into());
                }
            },
            Err(e) => {
                let _ = conn.execute_batch("ROLLBACK");
                return Err(e);
            }
        }
    }
}

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
    with_write_txn(conn, |c| {
        reset_projection(c)?;
        for event in &events {
            project_event(c, event)?;
        }
        Ok(())
    })?;
    Ok(events.len())
}

/// Read all stored events from the durable `events` table, ordered by
/// (ts, rowid) so replay is deterministic AND causal.
///
/// `rowid` is the implicit, insertion-monotonic key, so within an equal `ts`
/// it preserves append order — the true causal order (e.g. a `memory.cited`
/// appended before the `memory.superseded` that reassigns it). The previous
/// `event_id` tiebreak was NOT causal: event ids are ULIDs whose ordering is
/// only random-tail-stable within the same millisecond, so equal-`ts` events
/// replayed in a platform-dependent order — non-deterministic rebuilds.
fn read_events_ordered(conn: &Connection) -> KimetsuResult<Vec<Event>> {
    // Order by HLC (Hybrid Logical Clock): a globally-deterministic, causal total
    // order. On a single brain this generalizes the old (ts, rowid) order; across
    // synced brains it makes the merged-log replay converge (same projection on
    // every brain regardless of import order). `rowid` is a stable final tiebreak.
    let mut stmt = conn.prepare(
        "
        SELECT event_id, run_id, ts, kind, schema_version, payload_json, origin, hlc
        FROM events
        ORDER BY hlc, rowid
        ",
    )?;
    let rows = stmt.query_map([], |row| {
        let event_id_str: String = row.get(0)?;
        let run_id_str: String = row.get(1)?;
        let ts_str: String = row.get(2)?;
        let kind: String = row.get(3)?;
        let schema_version: u32 = row.get(4)?;
        let payload_json: String = row.get(5)?;
        let origin: Option<String> = row.get(6)?;
        let hlc: Option<String> = row.get(7)?;
        Ok((
            event_id_str,
            run_id_str,
            ts_str,
            kind,
            schema_version,
            payload_json,
            origin,
            hlc,
        ))
    })?;

    let mut events = Vec::new();
    for row in rows {
        let (event_id_str, run_id_str, ts_str, kind, schema_version, payload_json, origin, hlc) =
            row?;
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
            origin, // preserved across rebuild (NULL for pre-v8 events)
            hlc,    // preserved across rebuild (backfilled for pre-v9 events)
        });
    }
    Ok(events)
}

pub fn apply_events(conn: &Connection, events: &[Event]) -> KimetsuResult<()> {
    with_write_txn(conn, |c| {
        for event in events {
            apply_event(c, event)?;
        }
        Ok(())
    })
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
        DELETE FROM sync_conflicts;
        DELETE FROM memory_edges;
        DELETE FROM work_episodes;
        ",
    )?;
    Ok(())
}

fn apply_event(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let event = redact_memory_event(event);
    let event = event.as_ref();
    // Persist the event after memory payload redaction so durable replay tables
    // never become a second secret store.
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
    let upcasted = upcast_event(event);
    let redacted = redact_memory_event(upcasted.as_ref());
    let event = redacted.as_ref();

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
        // Story 2.4: explicit regret (negative outcome) on a memory. Only
        // manual regrets mutate stats (see apply_retrieval_regret); auto
        // telemetry regrets are projected as no-ops.
        "retrieval.regret" => apply_retrieval_regret(conn, event),
        // Testing/benchmark affordance: backdate created_at / last_useful_at so
        // age-sensitive policies (forgetting) can be exercised.
        "memory.aged" => apply_memory_aged(conn, event),
        // Story 3.1: near-duplicate merge — stamp superseded_by on merged members,
        // remove their FTS rows, and drop them from the ANN index.
        "memory.superseded" => apply_memory_superseded(conn, event),
        // #2 knowledge graph: a typed relation edge between two memories, written
        // by `kimetsu brain graph build`. Projected into `memory_edges` so the
        // graph-lite / petgraph retrieval backends can traverse it. Rebuild-safe:
        // the edge is re-derived by replaying this event.
        "memory.edge" => apply_memory_edge(conn, event),
        // Flagship 1 / Story 1.4: temporal validity — stamp valid_from / valid_to.
        "memory.temporal" => apply_memory_temporal(conn, event),
        // Flagship 1 / Story 1.3: episodic work-resume.
        "work.episode" => crate::episode::project_work_episode(conn, event),
        _ => Ok(()),
    }
}

fn redact_memory_event(event: &Event) -> Cow<'_, Event> {
    if !matches!(
        event.kind.as_str(),
        "memory.accepted" | "memory.proposed" | "memory.cited"
    ) {
        return Cow::Borrowed(event);
    }
    let (payload, changed) = redact_json_strings(&event.payload);
    if changed {
        Cow::Owned(Event {
            payload,
            ..event.clone()
        })
    } else {
        Cow::Borrowed(event)
    }
}

fn redact_json_strings(value: &serde_json::Value) -> (serde_json::Value, bool) {
    match value {
        serde_json::Value::String(text) => {
            let redaction = redact::redact_secrets(text);
            let changed = redaction.was_redacted();
            (serde_json::Value::String(redaction.text), changed)
        }
        serde_json::Value::Array(values) => {
            let mut changed = false;
            let values = values
                .iter()
                .map(|value| {
                    let (value, did_change) = redact_json_strings(value);
                    changed |= did_change;
                    value
                })
                .collect();
            (serde_json::Value::Array(values), changed)
        }
        serde_json::Value::Object(map) => {
            let mut changed = false;
            let map = map
                .iter()
                .map(|(key, value)| {
                    let (value, did_change) = redact_json_strings(value);
                    changed |= did_change;
                    (key.clone(), value)
                })
                .collect();
            (serde_json::Value::Object(map), changed)
        }
        other => (other.clone(), false),
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

    // Flagship 2 / Story 2.4: a STANDALONE citation (sentinel/nil run_id, i.e.
    // the `record_mcp_citation` / `brain cite` path) is an explicit outcome
    // signal with no run finalization behind it, so apply the cited-memory
    // delta here. Citations tied to a REAL run keep metadata-only here and are
    // bumped by `apply_memory_usefulness_for_run` on the terminal run event —
    // gating on the sentinel avoids double-counting.
    if event.run_id.0 == ulid::Ulid::nil() {
        apply_cited_outcome(conn, memory_id, 1.0, 1.0, &cited_at, true)?;
    }
    Ok(())
}

/// Story 2.4: a memory the model flagged as unhelpful/misleading. Mirrors the
/// `run.failed` cited delta. Only EXPLICIT manual regrets (`payload.source ==
/// "manual"`, set by `record_regret` / `brain regret`) mutate stats; the
/// auto-emitted regret telemetry (no `source`) stays a no-op so existing
/// behavior is unchanged.
fn apply_retrieval_regret(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let is_manual = event
        .payload
        .get("source")
        .and_then(|v| v.as_str())
        .map(|s| s == "manual")
        .unwrap_or(false);
    if !is_manual {
        return Ok(());
    }
    let Some(memory_id) = event.payload.get("memory_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let ts = ts_text(event)?;
    apply_cited_outcome(conn, memory_id, -1.0, 0.0, &ts, false)?;
    Ok(())
}

/// Backdate a memory's `created_at` / `last_useful_at` from a `memory.aged`
/// event (absolute timestamps in the payload → rebuild-deterministic).
fn apply_memory_aged(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(memory_id) = event.payload.get("memory_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    if let Some(created) = event.payload.get("created_at").and_then(|v| v.as_str()) {
        conn.execute(
            "UPDATE memories SET created_at = ?2 WHERE memory_id = ?1",
            params![memory_id, created],
        )?;
    }
    if let Some(last_useful) = event.payload.get("last_useful_at").and_then(|v| v.as_str()) {
        conn.execute(
            "UPDATE memories SET last_useful_at = ?2 WHERE memory_id = ?1",
            params![memory_id, last_useful],
        )?;
    }
    Ok(())
}

/// Confidence calibration smoothing factor (Bayesian-ish nudge per outcome).
use crate::scoring::{CITED_DELTA, CONF_ALPHA, FAILURE_PENALTY_CITES_DIVISOR, PASSENGER_DELTA};

/// Apply a single cited-memory OUTCOME to one memory row, shared by the run
/// attribution path and the standalone cite/regret path: bump `use_count`,
/// add `usefulness_delta`, stamp `last_used_at` (and `last_useful_at` when
/// `bump_last_useful`), and nudge `confidence` toward `conf_target`
/// (`new = old + 0.05*(target-old)`, clamped to [0.1, 0.99]). Read-modify-write
/// on the deterministic event order → rebuild-safe.
fn apply_cited_outcome(
    conn: &Connection,
    memory_id: &str,
    usefulness_delta: f64,
    conf_target: f64,
    ts: &str,
    bump_last_useful: bool,
) -> KimetsuResult<()> {
    conn.execute(
        "UPDATE memories
         SET use_count = use_count + 1,
             usefulness_score = usefulness_score + ?2,
             last_used_at = ?3
         WHERE memory_id = ?1",
        params![memory_id, usefulness_delta, ts],
    )?;
    if bump_last_useful {
        conn.execute(
            "UPDATE memories SET last_useful_at = ?2 WHERE memory_id = ?1",
            params![memory_id, ts],
        )?;
    }
    let old_conf: f64 = conn
        .query_row(
            "SELECT confidence FROM memories WHERE memory_id = ?1",
            params![memory_id],
            |row| row.get::<_, f64>(0),
        )
        .unwrap_or(1.0);
    let new_conf = (old_conf + CONF_ALPHA * (conf_target - old_conf)).clamp(0.1, 0.99);
    conn.execute(
        "UPDATE memories SET confidence = ?2 WHERE memory_id = ?1",
        params![memory_id, new_conf],
    )?;
    Ok(())
}

pub(crate) fn insert_event(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let payload = serde_json::to_string(&event.payload)?;
    conn.execute(
        "
        INSERT OR IGNORE INTO events (
            event_id, run_id, ts, kind, schema_version, payload_json, origin, hlc
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)
        ",
        params![
            event.event_id.to_string(),
            event.run_id.to_string(),
            ts_text(event)?,
            event.kind,
            event.schema_version,
            payload,
            event.origin,
            event.hlc,
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
        "run.finished" => (CITED_DELTA, PASSENGER_DELTA),
        "run.failed" => {
            let category = event
                .payload
                .get("category")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            if category == "Gate" {
                return Ok(());
            }
            (-CITED_DELTA, -PASSENGER_DELTA)
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

    // Flagship 2 / Story 2.4: confidence calibration target.
    // run.finished → target 1.0 (success), run.failed → target 0.0 (failure).
    // alpha = 0.05: conservative Bayesian-ish smoothing.
    let conf_target: Option<f64> = match event.kind.as_str() {
        "run.finished" => Some(1.0),
        "run.failed" => Some(0.0),
        _ => None,
    };

    for memory_id in &retrieved {
        let is_cited = cited.contains(memory_id);
        let delta = if is_cited {
            if strong < 0.0 {
                // v2.5.1: citation-aware failure penalty. A memory cited in a
                // run that fails for unrelated reasons (flaky verification, an
                // environment hiccup categorized non-Gate) used to eat the flat
                // -1.0; two or three unlucky runs made a genuinely proven
                // memory a prune candidate. Scale the penalty down by the
                // memory's citation history: a long positive track record
                // absorbs occasional cited-failures, an unproven memory takes
                // proportionally more of the hit.
                //   effective = -1.0 / (1 + prior_citations / 3)
                // (0 priors -> -1.0, 3 -> -0.5, 9 -> -0.25). Successes are
                // never scaled; the Gate carve-out above still applies.
                let prior_cites: i64 = conn
                    .query_row(
                        "SELECT COUNT(*) FROM memory_citations
                         WHERE memory_id = ?1 AND run_id != ?2",
                        params![memory_id, run_id],
                        |row| row.get(0),
                    )
                    .unwrap_or(0);
                strong / (1.0 + prior_cites as f64 / FAILURE_PENALTY_CITES_DIVISOR)
            } else {
                strong
            }
        } else {
            weak
        };
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
        // Flagship 2 / Story 2.4: update confidence only for cited memories.
        // Silent passengers do not get a confidence update — only explicitly
        // cited memories affect the calibration.
        if is_cited {
            if let Some(target) = conf_target {
                // Read current confidence, apply Bayesian-ish posterior, clamp.
                let old_conf: f64 = conn
                    .query_row(
                        "SELECT confidence FROM memories WHERE memory_id = ?1",
                        params![memory_id],
                        |row| row.get::<_, f64>(0),
                    )
                    .unwrap_or(1.0);
                let new_conf = (old_conf + CONF_ALPHA * (target - old_conf)).clamp(0.1, 0.99);
                conn.execute(
                    "UPDATE memories SET confidence = ?2 WHERE memory_id = ?1",
                    params![memory_id, new_conf],
                )?;
            }
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
    // Flagship 2 / Story 2.1: initial usefulness seed.
    // Pre-Flagship-2 events don't carry this field → default 0.0 (backward compat).
    let initial_usefulness = event
        .payload
        .get("initial_usefulness")
        .and_then(|value| value.as_f64())
        .unwrap_or(0.0) as f32;
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
            source_event_id, provenance_snapshot_json, created_at, use_count,
            usefulness_score
        )
        VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, 0, ?10)
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
            ts_text(event)?,
            initial_usefulness
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
    #[cfg(feature = "embeddings")]
    crate::ann::on_invalidate(conn, memory_id);
    Ok(())
}

/// Story 3.1: project a `memory.superseded` event.
///
/// Payload fields:
///   `memory_id`       — the member being superseded (merged into survivor)
///   `survivor_id`     — the memory that absorbs the cluster
///   `use_count_delta` — member's use_count contribution (optional, default 0)
///   `score_delta`     — member's usefulness_score contribution (optional, default 0)
///
/// Projection:
///   1. Stamp `superseded_by = survivor_id` on the member row.
///   2. Add member's use_count_delta / score_delta to the survivor row.
///   3. Reassign the member's citations to the survivor.
///   4. Delete the member's FTS row so it stops appearing in lexical retrieval.
///   5. Remove the member from the ANN index (embeddings feature only).
///
/// The member row is intentionally NOT invalidated — `blame` can still see
/// it and trace it to its survivor via `superseded_by`.
///
/// This is the single canonical projection path used by BOTH the live
/// consolidation path (via `apply_events`) and `rebuild_in_place` (replay),
/// so the two can never drift.
fn apply_memory_superseded(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(memory_id) = event.payload.get("memory_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let Some(survivor_id) = event.payload.get("survivor_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let use_count_delta = event
        .payload
        .get("use_count_delta")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let score_delta = event
        .payload
        .get("score_delta")
        .and_then(|v| v.as_f64())
        .unwrap_or(0.0);

    // Slice B: detect a concurrent-supersede conflict. If this member is already
    // superseded by a DIFFERENT survivor, two edits (typically from different
    // brains' consolidations) disagree. HLC-order replay still picks a
    // deterministic winner (the supersede applied last in HLC order — see below),
    // so brains converge; we record the collision for human review. Replay-safe:
    // sync_conflicts is a projection cleared by reset_projection and the pair is
    // canonicalized + INSERT OR IGNORE, so it records once.
    let prior_survivor: Option<String> = conn
        .query_row(
            "SELECT superseded_by FROM memories WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get::<_, Option<String>>(0),
        )
        .optional()?
        .flatten();
    if let Some(prev) = prior_survivor {
        if prev != survivor_id {
            let (a, b) = if prev.as_str() < survivor_id {
                (prev.as_str(), survivor_id)
            } else {
                (survivor_id, prev.as_str())
            };
            let detected_at = ts_text(event)?;
            conn.execute(
                "INSERT OR IGNORE INTO sync_conflicts
                     (member_id, survivor_a, survivor_b, detected_at)
                 VALUES (?1, ?2, ?3, ?4)",
                params![memory_id, a, b, detected_at],
            )?;
        }
    }

    // 1. Stamp superseded_by on the member (last supersede in HLC replay order
    //    wins → deterministic survivor on every brain).
    conn.execute(
        "UPDATE memories SET superseded_by = ?2 WHERE memory_id = ?1",
        params![memory_id, survivor_id],
    )?;

    // 2. Accumulate the member's stats onto the survivor.
    if use_count_delta != 0 || score_delta != 0.0 {
        conn.execute(
            "UPDATE memories
             SET use_count       = use_count       + ?2,
                 usefulness_score = usefulness_score + ?3
             WHERE memory_id = ?1",
            params![survivor_id, use_count_delta, score_delta],
        )?;
    }

    // 3. Reassign citations from member to survivor (shared helper).
    reassign_citations_projection(conn, memory_id, survivor_id)?;

    // 4. Remove from FTS index.
    conn.execute(
        "DELETE FROM memories_fts WHERE memory_id = ?1",
        params![memory_id],
    )?;

    // 5. Remove from ANN index (embeddings feature only).
    #[cfg(feature = "embeddings")]
    crate::ann::on_supersede(conn, memory_id);

    // 6. S5.2: insert a `supersedes` edge from survivor → member into the
    //    typed-edge projection table so graph-lite traversal can follow it.
    let edge_ts = ts_text(event)?;
    insert_memory_edge(conn, survivor_id, memory_id, "supersedes", &edge_ts)?;

    Ok(())
}

/// Flagship 1 / Story 1.4: project a `memory.temporal` event.
///
/// Payload fields:
///   `memory_id`  — the memory whose validity window is being stamped.
///   `valid_from` — optional RFC 3339 lower bound (inclusive). NULL = "since creation".
///   `valid_to`   — optional RFC 3339 upper bound (exclusive). NULL = "never expires".
///                  When set to a past timestamp the memory is "expired" and the
///                  default retrieval path (`valid_to IS NULL OR valid_to > now`)
///                  will exclude it.
///
/// The update is additive: only the fields present in the payload are written.
/// A `memory.temporal` event with only `valid_to` leaves `valid_from` unchanged.
fn apply_memory_temporal(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(memory_id) = event.payload.get("memory_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let valid_from = event
        .payload
        .get("valid_from")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());
    let valid_to = event
        .payload
        .get("valid_to")
        .and_then(|v| v.as_str())
        .map(|s| s.to_string());

    // Build a partial update: only stamp the fields that are present in the payload.
    // Both absent → no-op (caller sent an empty event — treat gracefully).
    match (valid_from, valid_to) {
        (Some(vf), Some(vt)) => {
            conn.execute(
                "UPDATE memories SET valid_from = ?2, valid_to = ?3 WHERE memory_id = ?1",
                params![memory_id, vf, vt],
            )?;
        }
        (Some(vf), None) => {
            conn.execute(
                "UPDATE memories SET valid_from = ?2 WHERE memory_id = ?1",
                params![memory_id, vf],
            )?;
        }
        (None, Some(vt)) => {
            conn.execute(
                "UPDATE memories SET valid_to = ?2 WHERE memory_id = ?1",
                params![memory_id, vt],
            )?;
        }
        (None, None) => {} // no-op
    }
    Ok(())
}

/// Flagship 1 / Story 1.4: programmatic API for stamping a memory's temporal
/// validity window.
///
/// Emits a `memory.temporal` event into the event log (so the action is
/// rebuild-safe and replay-correct) and applies it immediately by projecting
/// it into the `memories` table.
///
/// Used by the bench seeder (`brain_bench_single`) and will be used by
/// Flagship 1 Pass B (resolution) once it is implemented.
///
/// `valid_from` and `valid_to` are RFC 3339 / ISO-8601 strings. Pass `None`
/// to leave a bound unchanged.
pub fn mark_memory_temporal(
    conn: &Connection,
    memory_id: &str,
    valid_from: Option<&str>,
    valid_to: Option<&str>,
) -> KimetsuResult<()> {
    // Build a synthetic event to go through the standard projection path.
    // We use a throwaway RunId (zero ULID) since this is an out-of-band
    // operation (not part of a live agent run).
    use kimetsu_core::ids::RunId;
    let run_id = RunId::new();
    let mut payload = serde_json::json!({ "memory_id": memory_id });
    if let Some(vf) = valid_from {
        payload["valid_from"] = serde_json::Value::String(vf.to_string());
    }
    if let Some(vt) = valid_to {
        payload["valid_to"] = serde_json::Value::String(vt.to_string());
    }
    let event = kimetsu_core::event::Event::new(run_id, "memory.temporal", payload);
    // Use apply_event so the event is persisted AND projected in one step.
    apply_event(conn, &event)
}

/// #2 knowledge graph: project a `memory.edge` event into `memory_edges`.
///
/// Payload fields:
///   `src_id`    — source memory id.
///   `dst_id`    — destination memory id.
///   `edge_type` — relation kind (e.g. `"relates_to"`, `"refines"`).
///
/// A missing/malformed payload no-ops (best-effort, matching the other memory
/// projectors). The `OR IGNORE` insert makes replay idempotent.
fn apply_memory_edge(conn: &Connection, event: &Event) -> KimetsuResult<()> {
    let Some(src_id) = event.payload.get("src_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let Some(dst_id) = event.payload.get("dst_id").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let Some(edge_type) = event.payload.get("edge_type").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    // Never self-loop.
    if src_id == dst_id {
        return Ok(());
    }
    let edge_ts = ts_text(event)?;
    insert_memory_edge(conn, src_id, dst_id, edge_type, &edge_ts)
}

/// #2 knowledge graph: programmatic API for writing a batch of typed relation
/// edges. Each `(src_id, dst_id, edge_type)` is emitted as a `memory.edge` event
/// (so the action is rebuild-safe — replay reconstructs the edges) and projected
/// into `memory_edges` in a single transaction via `apply_events`.
///
/// Self-loops (`src == dst`) are skipped. Returns the number of edges written.
/// Used by `kimetsu brain graph build`.
pub fn add_memory_edges(
    conn: &Connection,
    edges: &[(String, String, String)],
) -> KimetsuResult<usize> {
    use kimetsu_core::ids::RunId;
    let run_id = RunId::new();
    let mut events = Vec::with_capacity(edges.len());
    let mut written = 0usize;
    for (src_id, dst_id, edge_type) in edges {
        if src_id == dst_id {
            continue;
        }
        let payload = serde_json::json!({
            "src_id": src_id,
            "dst_id": dst_id,
            "edge_type": edge_type,
        });
        events.push(kimetsu_core::event::Event::new(
            run_id,
            "memory.edge",
            payload,
        ));
        written += 1;
    }
    apply_events(conn, &events)?;
    Ok(written)
}

/// S5.2: insert a typed edge into `memory_edges`.
///
/// This is the **single canonical path** for writing to `memory_edges`.
/// Call it from any projector that wants to populate an edge type.
///
/// Currently populated edge types:
///   * `"supersedes"` — populated here by `apply_memory_superseded`.
///
/// Reserved edge types (populated by Flagship 1 / Story 1.7):
///   * `"refines"`          — memory A refines / narrows memory B.
///   * `"dead_end_of"`      — task outcome closes a dead-end chain.
///   * `"decision_touches"` — decision memory touches a file path.
///   * `"lesson_from"`      — lesson memory derived from a source memory.
///
/// The INSERT is `OR IGNORE` so replaying the same event twice is safe.
pub(crate) fn insert_memory_edge(
    conn: &Connection,
    src_id: &str,
    dst_id: &str,
    edge_type: &str,
    created_at: &str,
) -> KimetsuResult<()> {
    conn.execute(
        "INSERT OR IGNORE INTO memory_edges (src_id, dst_id, edge_type, created_at)
         VALUES (?1, ?2, ?3, ?4)",
        params![src_id, dst_id, edge_type, created_at],
    )?;
    Ok(())
}

/// Shared citation-reassignment helper used by both the live consolidation
/// path and the replay path (`apply_memory_superseded`).  Keeping a single
/// implementation prevents the two paths from drifting.
///
/// Copies every `memory_citations` row from `from_id` to `to_id`
/// (INSERT OR IGNORE — skip conflicts), then deletes the originals.
pub(crate) fn reassign_citations_projection(
    conn: &Connection,
    from_id: &str,
    to_id: &str,
) -> KimetsuResult<()> {
    // Collect existing citations for `from_id`.
    let rows: Vec<(String, i64, String, Option<String>)> = {
        let mut stmt = conn.prepare(
            "SELECT run_id, turn, cited_at, rationale
             FROM memory_citations WHERE memory_id = ?1",
        )?;
        stmt.query_map(params![from_id], |r| {
            Ok((
                r.get::<_, String>(0)?,
                r.get::<_, i64>(1)?,
                r.get::<_, String>(2)?,
                r.get::<_, Option<String>>(3)?,
            ))
        })?
        .collect::<Result<_, _>>()?
    };

    for (run_id, turn, cited_at, rationale) in &rows {
        conn.execute(
            "INSERT OR IGNORE INTO memory_citations
             (run_id, memory_id, turn, cited_at, rationale)
             VALUES (?1, ?2, ?3, ?4, ?5)",
            params![run_id, to_id, turn, cited_at, rationale],
        )?;
    }

    conn.execute(
        "DELETE FROM memory_citations WHERE memory_id = ?1",
        params![from_id],
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
    use rusqlite::{Connection, params};
    use serde_json::json;

    use super::{apply_events, rebuild_in_place, upcast_event};
    use crate::schema;

    fn make_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        schema::initialize(&conn).expect("schema::initialize");
        conn
    }

    fn make_event(run_id: RunId, kind: &str, payload: serde_json::Value) -> Event {
        Event::new(run_id, kind, payload)
    }

    /// The nil-ULID sentinel run id: a STANDALONE `memory.cited` (this run id)
    /// applies a real outcome delta (+use_count) via `apply_cited_outcome`.
    fn sentinel_run() -> RunId {
        RunId(ulid::Ulid::nil())
    }

    // ------------------------------------------------------------------
    // v3.0 #3: concurrent writers to ONE on-disk brain.db must not lose
    // updates. Independent Connections behave like independent processes for
    // SQLite locking, so this exercises the IMMEDIATE-transaction + busy-retry
    // write path under real contention.
    // ------------------------------------------------------------------
    #[test]
    fn concurrent_cites_lose_no_updates() {
        use std::sync::atomic::{AtomicU64, Ordering};
        use std::sync::{Arc, Barrier};

        static CTR: AtomicU64 = AtomicU64::new(0);
        let n = CTR.fetch_add(1, Ordering::Relaxed);
        let db_path =
            std::env::temp_dir().join(format!("kimetsu-concurrency-{}-{n}.db", std::process::id()));
        let _ = std::fs::remove_file(&db_path);

        // Seed one accepted memory (use_count starts at 0).
        let mem_id = "mem-concurrency";
        {
            let conn = Connection::open(&db_path).expect("open seed");
            schema::initialize(&conn).expect("init seed");
            let accepted = Event::new(
                sentinel_run(),
                "memory.accepted",
                json!({
                    "memory_id": mem_id,
                    "text": "hammer me",
                    "scope": "global_user",
                    "kind": "fact"
                }),
            );
            apply_events(&conn, std::slice::from_ref(&accepted)).expect("seed accepted");
        }

        const THREADS: usize = 6;
        const CITES_PER_THREAD: usize = 25;
        let barrier = Arc::new(Barrier::new(THREADS));
        let path = Arc::new(db_path.clone());

        let mut handles = Vec::new();
        for _ in 0..THREADS {
            let b = Arc::clone(&barrier);
            let p = Arc::clone(&path);
            handles.push(std::thread::spawn(move || {
                // Each thread = its own connection (≈ its own process).
                let conn = Connection::open(&*p).expect("open writer");
                schema::initialize(&conn).expect("init writer");
                b.wait(); // maximize contention
                for _ in 0..CITES_PER_THREAD {
                    let cited = Event::new(
                        sentinel_run(),
                        "memory.cited",
                        json!({ "memory_id": mem_id, "turn": 0 }),
                    );
                    // Must not error under contention (busy-retry + IMMEDIATE).
                    apply_events(&conn, std::slice::from_ref(&cited))
                        .expect("concurrent cite must succeed");
                }
            }));
        }
        for h in handles {
            h.join().expect("thread join");
        }

        let expected = (THREADS * CITES_PER_THREAD) as i64;

        let conn = Connection::open(&db_path).expect("open verify");
        schema::initialize(&conn).expect("init verify");

        // No lost increments: every concurrent cite landed.
        let use_count: i64 = conn
            .query_row(
                "SELECT use_count FROM memories WHERE memory_id = ?1",
                params![mem_id],
                |r| r.get(0),
            )
            .expect("read use_count");
        assert_eq!(
            use_count, expected,
            "lost updates under concurrency: got {use_count}, expected {expected}"
        );

        // All events durably appended (1 accepted + N*M cited).
        let event_count: i64 = conn
            .query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))
            .expect("count events");
        assert_eq!(event_count, expected + 1, "missing durable events");

        // Rebuild is deterministic: replay reproduces the same use_count.
        rebuild_in_place(&conn).expect("rebuild");
        let after: i64 = conn
            .query_row(
                "SELECT use_count FROM memories WHERE memory_id = ?1",
                params![mem_id],
                |r| r.get(0),
            )
            .expect("read use_count after rebuild");
        assert_eq!(after, expected, "rebuild changed the projected use_count");

        drop(conn);
        let _ = std::fs::remove_file(&db_path);
        // WAL sidecars.
        let _ = std::fs::remove_file(db_path.with_extension("db-wal"));
        let _ = std::fs::remove_file(db_path.with_extension("db-shm"));
    }

    #[test]
    fn event_carries_and_roundtrips_origin() {
        use super::{insert_event, read_events_ordered};

        let conn = make_conn();
        kimetsu_core::event::set_process_origin("test-machine/unit");

        let ev = Event::new(
            sentinel_run(),
            "memory.accepted",
            json!({
                "memory_id": "m-origin",
                "text": "with origin",
                "scope": "global_user",
                "kind": "fact"
            }),
        );
        // process_origin() is a OnceLock — first setter wins; assert the event
        // carries SOME origin and that it round-trips through the events table.
        let stamped = ev.origin.clone();
        insert_event(&conn, &ev).expect("insert");
        let read_back = read_events_ordered(&conn).expect("read");
        assert_eq!(read_back.len(), 1);
        assert_eq!(read_back[0].origin, stamped, "origin must round-trip");
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

    // F1: empty payload work.episode must not panic/error.
    #[test]
    fn empty_payload_work_episode() {
        assert_empty_payload_ok("work.episode");
    }

    // F1A: empty payload memory.temporal must not panic/error.
    #[test]
    fn empty_payload_memory_temporal() {
        assert_empty_payload_ok("memory.temporal");
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

        // work_episodes must also be cleared.
        let episodes_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM work_episodes", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            episodes_after, 0,
            "work_episodes must be cleared by reset_projection"
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

    #[test]
    fn add_memory_edges_writes_and_survives_rebuild() {
        use super::{add_memory_edges, rebuild_in_place};

        let conn = make_conn();
        let run_id = RunId::new();
        let m1 = "mem-edge-a";
        let m2 = "mem-edge-b";

        let events = vec![
            make_event(
                run_id,
                "memory.accepted",
                json!({"memory_id": m1, "text": "alpha", "scope": "global_user", "kind": "fact"}),
            ),
            make_event(
                run_id,
                "memory.accepted",
                json!({"memory_id": m2, "text": "beta", "scope": "global_user", "kind": "fact"}),
            ),
        ];
        apply_events(&conn, &events).expect("apply_events");

        // Self-loop is skipped; a real edge is written.
        let written = add_memory_edges(
            &conn,
            &[
                (m1.to_string(), m1.to_string(), "relates_to".to_string()),
                (m1.to_string(), m2.to_string(), "relates_to".to_string()),
            ],
        )
        .expect("add_memory_edges");
        assert_eq!(
            written, 1,
            "self-loop must be skipped, one real edge written"
        );

        let edge_count = |c: &Connection| -> i64 {
            c.query_row(
                "SELECT COUNT(*) FROM memory_edges WHERE src_id=?1 AND dst_id=?2 AND edge_type='relates_to'",
                params![m1, m2],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert_eq!(edge_count(&conn), 1, "edge present after write");

        // Rebuild from the durable log: the edge is re-derived (replayed event).
        let total_edges_before: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_edges", [], |r| r.get(0))
            .unwrap();
        rebuild_in_place(&conn).expect("rebuild_in_place");
        let total_edges_after: i64 = conn
            .query_row("SELECT COUNT(*) FROM memory_edges", [], |r| r.get(0))
            .unwrap();
        assert_eq!(
            total_edges_before, total_edges_after,
            "rebuild must reproduce exactly the same edge set"
        );
        assert_eq!(edge_count(&conn), 1, "edge survives rebuild_in_place");
    }

    // ------------------------------------------------------------------
    // W1.2c: Event reconstruction fidelity — after rebuild_in_place the
    // projected memory's text/scope/kind match the original.
    // ------------------------------------------------------------------
    #[test]
    fn memory_proposed_redacts_event_and_projection_payloads() {
        let conn = make_conn();
        let run_id = RunId::new();
        let secret = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf";
        let event = make_event(
            run_id,
            "memory.proposed",
            json!({
                "proposal_id": "prop-redact",
                "scope": "project",
                "kind": "fact",
                "text": format!("lesson uses {secret}"),
                "rationale": format!("model repeated {secret}"),
                "proposed_confidence": 0.5,
                "source_event_ids": [],
            }),
        );
        apply_events(&conn, &[event]).expect("apply_events");

        let payload: String = conn
            .query_row(
                "SELECT payload_json FROM events WHERE kind = 'memory.proposed'",
                [],
                |r| r.get(0),
            )
            .expect("event payload");
        assert!(!payload.contains(secret), "event leaked secret: {payload}");
        assert!(payload.contains("[REDACTED:anthropic_oauth]"));

        let row: (String, String) = conn
            .query_row(
                "SELECT text, rationale FROM memory_proposals WHERE proposal_id = 'prop-redact'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("proposal row");
        assert!(!row.0.contains(secret), "proposal text leaked: {}", row.0);
        assert!(
            !row.1.contains(secret),
            "proposal rationale leaked: {}",
            row.1
        );
    }

    #[test]
    fn memory_cited_redacts_event_and_projection_rationale() {
        let conn = make_conn();
        let run_id = RunId::new();
        let secret = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUv0123456789AbCdEf";
        let event = make_event(
            run_id,
            "memory.cited",
            json!({
                "memory_id": "mem-redact",
                "turn": 1,
                "rationale": format!("used because output showed {secret}"),
            }),
        );
        apply_events(&conn, &[event]).expect("apply_events");

        let payload: String = conn
            .query_row(
                "SELECT payload_json FROM events WHERE kind = 'memory.cited'",
                [],
                |r| r.get(0),
            )
            .expect("event payload");
        assert!(!payload.contains(secret), "event leaked secret: {payload}");
        assert!(payload.contains("[REDACTED:anthropic_oauth]"));

        let rationale: String = conn
            .query_row(
                "SELECT rationale FROM memory_citations WHERE memory_id = 'mem-redact'",
                [],
                |r| r.get(0),
            )
            .expect("citation rationale");
        assert!(
            !rationale.contains(secret),
            "citation rationale leaked: {rationale}"
        );
        assert!(rationale.contains("[REDACTED:anthropic_oauth]"));
    }

    // ------------------------------------------------------------------
    // F1A: memory.temporal event stamps valid_from/valid_to and survives
    // rebuild_in_place (rebuild-safe).
    // ------------------------------------------------------------------
    #[test]
    fn memory_temporal_stamps_validity_and_survives_rebuild() {
        use super::{mark_memory_temporal, rebuild_in_place};

        let conn = make_conn();
        let run_id = RunId::new();
        let mem_id = "mem-temporal-test";

        let events = vec![make_event(
            run_id,
            "memory.accepted",
            json!({
                "memory_id": mem_id,
                "text": "old fact that expired",
                "scope": "project",
                "kind": "fact",
                "confidence": 0.9
            }),
        )];
        apply_events(&conn, &events).expect("apply_events");

        // Stamp valid_to to a past timestamp (expired).
        mark_memory_temporal(
            &conn,
            mem_id,
            Some("2020-01-01T00:00:00Z"),
            Some("2025-01-01T00:00:00Z"),
        )
        .expect("mark_memory_temporal");

        // Verify both columns are set.
        let (vf, vt): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT valid_from, valid_to FROM memories WHERE memory_id = ?1",
                [mem_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query valid_from/valid_to");
        assert_eq!(
            vf.as_deref(),
            Some("2020-01-01T00:00:00Z"),
            "valid_from must be set"
        );
        assert_eq!(
            vt.as_deref(),
            Some("2025-01-01T00:00:00Z"),
            "valid_to must be set"
        );

        // Rebuild in-place: temporal state must be restored from the event log.
        rebuild_in_place(&conn).expect("rebuild_in_place");

        let (vf2, vt2): (Option<String>, Option<String>) = conn
            .query_row(
                "SELECT valid_from, valid_to FROM memories WHERE memory_id = ?1",
                [mem_id],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query valid_from/valid_to after rebuild");
        assert_eq!(
            vf2.as_deref(),
            Some("2020-01-01T00:00:00Z"),
            "valid_from must survive rebuild_in_place"
        );
        assert_eq!(
            vt2.as_deref(),
            Some("2025-01-01T00:00:00Z"),
            "valid_to must survive rebuild_in_place"
        );
    }

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

    // ------------------------------------------------------------------
    // Flagship 2 / Story 2.1: importance scoring at write time
    // ------------------------------------------------------------------

    /// Story 2.1: a memory.accepted event carrying `initial_usefulness` seeds
    /// the memory's usefulness_score (rebuild-safe), so a salient new memory
    /// outranks a freshly-added neutral one with score 0.
    #[test]
    fn initial_usefulness_seeds_score_and_survives_rebuild() {
        use super::rebuild_in_place;

        let conn = make_conn();
        let run_id = RunId::new();

        let events = vec![
            // Salient: failure_pattern seeded at 0.3.
            make_event(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": "salient",
                    "text": "rm -rf node_modules then reinstall fixes the EBUSY lock",
                    "scope": "project",
                    "kind": "failure_pattern",
                    "confidence": 1.0,
                    "initial_usefulness": 0.3
                }),
            ),
            // Neutral: no initial_usefulness field → default 0.0 (back-compat).
            make_event(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": "neutral",
                    "text": "the readme mentions a port number",
                    "scope": "project",
                    "kind": "fact",
                    "confidence": 1.0
                }),
            ),
        ];
        apply_events(&conn, &events).expect("apply_events");

        let read = |id: &str| -> f64 {
            conn.query_row(
                "SELECT usefulness_score FROM memories WHERE memory_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        assert!(
            (read("salient") - 0.3).abs() < 1e-6,
            "salient memory must be seeded to 0.3"
        );
        assert!(
            read("neutral").abs() < 1e-6,
            "memory without initial_usefulness must default to 0.0"
        );
        assert!(
            read("salient") > read("neutral"),
            "salient new memory must outrank a neutral one from day one"
        );

        // Rebuild-safe: the seed is in the event payload, so it survives replay.
        conn.execute_batch("DELETE FROM memories; DELETE FROM memories_fts;")
            .unwrap();
        rebuild_in_place(&conn).expect("rebuild_in_place");
        assert!(
            (read("salient") - 0.3).abs() < 1e-6,
            "initial_usefulness seed must survive rebuild"
        );
    }

    // ------------------------------------------------------------------
    // Flagship 2 / Story 2.4: confidence calibration from outcomes
    // ------------------------------------------------------------------

    /// Run a full cycle that injects + cites `mem_id`, then terminates with
    /// `terminal_kind` ("run.finished" or "run.failed"). Returns the memory's
    /// confidence afterward.
    fn cite_and_terminate_confidence(terminal_kind: &str) -> (Connection, f64) {
        let conn = make_conn();
        let run_id = RunId::new();
        let mem_id = "cal-mem";

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
                    "text": "use lld linker on windows",
                    "scope": "project",
                    "kind": "convention",
                    "confidence": 0.7
                }),
            ),
            // Mark it as retrieved so usefulness/confidence attribution fires.
            make_event(
                run_id,
                "context.injected",
                json!({"stage": "loc", "memory_ids": [mem_id], "used_tokens": 100}),
            ),
            // Explicitly cited so it earns the strong (cited) confidence update.
            make_event(
                run_id,
                "memory.cited",
                json!({"memory_id": mem_id, "turn": 1}),
            ),
            make_event(run_id, terminal_kind, json!({"total_cost_usd": 0.0})),
        ];
        apply_events(&conn, &events).expect("apply_events");

        let conf: f64 = conn
            .query_row(
                "SELECT confidence FROM memories WHERE memory_id = ?1",
                [mem_id],
                |r| r.get(0),
            )
            .unwrap();
        (conn, conf)
    }

    /// v2.5.1: the cited-in-failure penalty is scaled by citation history.
    /// A memory with a positive track record absorbs an unlucky failed run
    /// (flaky verification etc.); an unproven memory takes the full hit.
    #[test]
    fn failure_penalty_scales_with_prior_citations() {
        let conn = make_conn();

        // Two memories: "proven" accumulates 3 successful cited runs first.
        for (id, text) in [
            ("proven", "use lld on windows"),
            ("rookie", "try the new flag"),
        ] {
            apply_events(
                &conn,
                &[make_event(
                    RunId::new(),
                    "memory.accepted",
                    json!({"memory_id": id, "text": text, "scope": "project", "kind": "convention", "confidence": 0.7}),
                )],
            )
            .unwrap();
        }
        for _ in 0..3 {
            let run = RunId::new();
            apply_events(
                &conn,
                &[
                    make_event(run, "run.started", json!({"project_id": "p", "task": "t"})),
                    make_event(
                        run,
                        "context.injected",
                        json!({"stage": "loc", "memory_ids": ["proven"], "used_tokens": 50}),
                    ),
                    make_event(
                        run,
                        "memory.cited",
                        json!({"memory_id": "proven", "turn": 1}),
                    ),
                    make_event(run, "run.finished", json!({"outcome": "ok"})),
                ],
            )
            .unwrap();
        }

        let score = |id: &str| -> f64 {
            conn.query_row(
                "SELECT usefulness_score FROM memories WHERE memory_id = ?1",
                [id],
                |r| r.get(0),
            )
            .unwrap()
        };
        let proven_before = score("proven");
        let rookie_before = score("rookie");

        // One failing (non-Gate) run where BOTH are injected and cited.
        let fail_run = RunId::new();
        apply_events(
            &conn,
            &[
                make_event(
                    fail_run,
                    "run.started",
                    json!({"project_id": "p", "task": "t"}),
                ),
                make_event(
                    fail_run,
                    "context.injected",
                    json!({"stage": "loc", "memory_ids": ["proven", "rookie"], "used_tokens": 80}),
                ),
                make_event(
                    fail_run,
                    "memory.cited",
                    json!({"memory_id": "proven", "turn": 1}),
                ),
                make_event(
                    fail_run,
                    "memory.cited",
                    json!({"memory_id": "rookie", "turn": 1}),
                ),
                make_event(
                    fail_run,
                    "run.failed",
                    json!({"category": "Verification", "message": "flaky test"}),
                ),
            ],
        )
        .unwrap();

        let proven_drop = proven_before - score("proven");
        let rookie_drop = rookie_before - score("rookie");
        assert!(
            (rookie_drop - 1.0).abs() < 1e-6,
            "unproven memory takes the full -1.0, got -{rookie_drop}"
        );
        assert!(
            proven_drop < rookie_drop,
            "3 prior citations must shrink the penalty: proven -{proven_drop} vs rookie -{rookie_drop}"
        );
        // effective = 1.0 / (1 + 3/3) = 0.5
        assert!(
            (proven_drop - 0.5).abs() < 1e-6,
            "penalty with 3 priors must be -0.5, got -{proven_drop}"
        );
    }

    /// Story 2.4 (headline): a cited memory in a successful run ends with
    /// HIGHER confidence than one in a failed run, and the calibrated value
    /// is reproduced exactly after rebuild_in_place.
    #[test]
    fn confidence_calibration_rewards_success_and_survives_rebuild() {
        use super::rebuild_in_place;

        let (success_conn, success_conf) = cite_and_terminate_confidence("run.finished");
        let (_fail_conn, fail_conf) = cite_and_terminate_confidence("run.failed");

        // Started at 0.7. Success nudges toward 1.0; failure toward 0.0.
        assert!(
            success_conf > 0.7,
            "successful citation must raise confidence above 0.7, got {success_conf}"
        );
        assert!(
            fail_conf < 0.7,
            "failed citation must lower confidence below 0.7, got {fail_conf}"
        );
        assert!(
            success_conf > fail_conf,
            "cited-in-success must beat cited-in-failure: {success_conf} vs {fail_conf}"
        );

        // Rebuild-safe: the calibration is derived purely from replayed events.
        let mem_id = "cal-mem";
        success_conn
            .execute_batch("DELETE FROM memories; DELETE FROM memories_fts;")
            .unwrap();
        rebuild_in_place(&success_conn).expect("rebuild_in_place");
        let post: f64 = success_conn
            .query_row(
                "SELECT confidence FROM memories WHERE memory_id = ?1",
                [mem_id],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            (post - success_conf).abs() < 1e-9,
            "calibrated confidence must reproduce after rebuild: {post} vs {success_conf}"
        );
    }
}
