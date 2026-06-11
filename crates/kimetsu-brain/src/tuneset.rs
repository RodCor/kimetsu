//! v1.5: personal eval-set builder for `kimetsu brain tune`.
//!
//! Walks `context.served` events that carry a raw `query` field (present when
//! `store_queries = true` in `project.toml`). Joins to `memory_citations` via
//! `session_id` (exact match) or, when session_id is absent, a ±30-minute
//! time window. A served event becomes a POSITIVE eval case when ≥1 citation
//! occurred in that window. Zero-citation served events are counted as noise
//! (used only for cost statistics; they do NOT appear in the `cases` vec).
//!
//! Deduplication: when the same query text appears multiple times (the same
//! task is worked on across sessions), only the latest served event is kept.

use rusqlite::Connection;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::eval::EvalCase;
use kimetsu_core::KimetsuResult;

/// Output of [`build_personal_eval`].
#[derive(Debug, Clone, Default)]
pub struct PersonalEval {
    /// Positive eval cases (query + relevant memory ids).
    pub cases: Vec<EvalCase>,
    /// Number of served events with zero subsequent citations (noise pool).
    pub noise_count: usize,
    /// RFC-3339 timestamp of the oldest positive served event, if any.
    pub oldest: Option<String>,
    /// RFC-3339 timestamp of the newest positive served event, if any.
    pub newest: Option<String>,
}

/// Build a personal eval set from the events already in `conn`.
///
/// Parameters:
/// - `window_secs`: maximum seconds between a served event and a citation for
///   them to be considered linked when no `session_id` is available (default 1800 = 30 min).
pub fn build_personal_eval(conn: &Connection, window_secs: i64) -> KimetsuResult<PersonalEval> {
    // 1. Collect served events that carry a query.
    let mut stmt = conn.prepare(
        "SELECT payload_json, ts FROM events
         WHERE kind = 'context.served'
         ORDER BY ts DESC",
    )?;
    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;

    // Dedup by query text: keep latest ts for each unique query.
    let mut seen_queries: std::collections::HashMap<String, (String, serde_json::Value)> =
        std::collections::HashMap::new();
    for row in rows {
        let (payload_json, ts) = row?;
        let payload: serde_json::Value =
            serde_json::from_str(&payload_json).unwrap_or(serde_json::Value::Null);
        let Some(query) = payload.get("query").and_then(|v| v.as_str()) else {
            continue; // no raw query stored → skip
        };
        if !seen_queries.contains_key(query) {
            seen_queries.insert(query.to_string(), (ts, payload));
        }
    }

    if seen_queries.is_empty() {
        return Ok(PersonalEval::default());
    }

    // 2. For each unique served event, find citations in-window.
    let mut cases: Vec<EvalCase> = Vec::new();
    let mut noise_count = 0usize;
    let mut oldest: Option<String> = None;
    let mut newest: Option<String> = None;

    for (query, (ts, payload)) in &seen_queries {
        let session_id = payload.get("session_id").and_then(|v| v.as_str());

        // Find citation memory_ids linked to this served event.
        let relevant = citations_for_served(conn, ts, session_id, window_secs)?;

        if relevant.is_empty() {
            noise_count += 1;
        } else {
            // Track oldest/newest timestamps.
            if oldest.as_deref().map(|o| ts.as_str() < o).unwrap_or(true) {
                oldest = Some(ts.clone());
            }
            if newest.as_deref().map(|n| ts.as_str() > n).unwrap_or(true) {
                newest = Some(ts.clone());
            }
            cases.push(EvalCase {
                query: query.clone(),
                relevant,
            });
        }
    }

    Ok(PersonalEval {
        cases,
        noise_count,
        oldest,
        newest,
    })
}

/// Collect distinct memory_ids cited in-window relative to a served event.
///
/// Strategy:
///   1. If session_id is present: find all `memory.cited` events whose
///      payload `session_id` matches. (Citation events emitted by the MCP
///      `kimetsu_brain_cite` tool carry `session_id` in their payload.)
///      → NOT currently stored there; fall through to time-window.
///   2. Time-window fallback: find `memory_citations` rows whose `cited_at`
///      falls within [ts − window_secs, ts + window_secs].
///
/// Note on design: `memory_citations` has `cited_at` (RFC-3339 text) and
/// `run_id`. We cannot reliably join on `run_id` for MCP cites (sentinel
/// run_id shared by all). So the primary join key is `session_id` from the
/// event payload when available; otherwise the time window is used.
fn citations_for_served(
    conn: &Connection,
    served_ts: &str,
    session_id: Option<&str>,
    window_secs: i64,
) -> KimetsuResult<Vec<String>> {
    // Try session_id join first (citations from the same Claude Code session).
    if let Some(sid) = session_id {
        let mut stmt = conn.prepare(
            "SELECT DISTINCT mc.memory_id
             FROM memory_citations mc
             JOIN events e ON e.run_id = mc.run_id
                          AND e.kind = 'memory.cited'
             WHERE json_extract(e.payload_json, '$.session_id') = ?1",
        )?;
        let ids: Vec<String> = stmt
            .query_map([sid], |row| row.get(0))?
            .filter_map(|r| r.ok())
            .collect();
        if !ids.is_empty() {
            return Ok(ids);
        }
        // Fall through: session_id join found nothing (MCP cites without session_id
        // in payload, or old agent cites) — use time window below.
    }

    // Time-window fallback: parse the served ts, compute bounds.
    let served_dt = OffsetDateTime::parse(served_ts, &Rfc3339)
        .map_err(|e| format!("parse served_ts {served_ts:?}: {e}"))?;
    let lo = (served_dt - time::Duration::seconds(window_secs))
        .format(&Rfc3339)
        .map_err(|e| format!("format lo: {e}"))?;
    let hi = (served_dt + time::Duration::seconds(window_secs))
        .format(&Rfc3339)
        .map_err(|e| format!("format hi: {e}"))?;

    let mut stmt = conn.prepare(
        "SELECT DISTINCT memory_id FROM memory_citations
         WHERE cited_at >= ?1 AND cited_at <= ?2",
    )?;
    let ids: Vec<String> = stmt
        .query_map([&lo, &hi], |row| row.get(0))?
        .filter_map(|r| r.ok())
        .collect();
    Ok(ids)
}

// ─── Unit tests ───────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        project::{add_memory, init_project},
        projector,
        user_brain::with_user_brain_disabled,
    };
    use kimetsu_core::{
        event::Event,
        ids::RunId,
        memory::{MemoryKind, MemoryScope},
    };
    use ulid::Ulid;

    fn test_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("kimetsu-tuneset-test-{}", Ulid::new()));
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }

    fn seed_context_served_with_query(
        conn: &Connection,
        query: &str,
        session_id: Option<&str>,
        ts_offset_secs: i64,
    ) -> String {
        // Build a timestamp slightly offset from now for ordering tests.
        let now = OffsetDateTime::now_utc() + time::Duration::seconds(ts_offset_secs);
        let ts = now.format(&Rfc3339).unwrap();

        let run_id = RunId::new();
        let mut payload = serde_json::json!({
            "query_hash": format!("{:016x}", query.len()),
            "query": query,
            "capsule_count": 2,
            "top_score": 0.8,
            "skipped": false,
            "stage": "localization",
            "retrieval_path": "fts",
        });
        if let Some(sid) = session_id {
            payload["session_id"] = serde_json::json!(sid);
        }
        // Insert directly into events with the given ts.
        let event = Event {
            event_id: kimetsu_core::ids::EventId(Ulid::new()),
            run_id,
            ts: now,
            parent_event_id: None,
            kind: "context.served".to_string(),
            schema_version: 1,
            payload,
        };
        projector::apply_events(conn, &[event]).expect("seed served");
        ts
    }

    fn seed_memory_cited(conn: &Connection, memory_id: &str, ts_offset_secs: i64) {
        let now = OffsetDateTime::now_utc() + time::Duration::seconds(ts_offset_secs);
        let run_id = RunId::new();
        let event = Event {
            event_id: kimetsu_core::ids::EventId(Ulid::new()),
            run_id,
            ts: now,
            parent_event_id: None,
            kind: "memory.cited".to_string(),
            schema_version: 1,
            payload: serde_json::json!({
                "memory_id": memory_id,
                "turn": 1,
            }),
        };
        projector::apply_events(conn, &[event]).expect("seed cited");
    }

    #[test]
    fn build_personal_eval_empty_when_no_queries_stored() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create");
            init_project(&root, false).expect("init");
            let (_, _, conn) = crate::project::load_project(&root).expect("load");
            let eval = build_personal_eval(&conn, 1800).expect("build");
            assert!(eval.cases.is_empty(), "no served events → empty cases");
            assert_eq!(eval.noise_count, 0);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn build_personal_eval_positive_case_from_time_window() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create");
            init_project(&root, false).expect("init");

            let mid = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "tuneset window test memory",
            )
            .expect("add memory");

            let (_, _, conn) = crate::project::load_project(&root).expect("load");

            // Served event at t=0, citation at t=+5min → within 30min window.
            seed_context_served_with_query(&conn, "find files fast", None, 0);
            seed_memory_cited(&conn, &mid, 5 * 60);

            let eval = build_personal_eval(&conn, 1800).expect("build");
            assert_eq!(eval.cases.len(), 1, "should have 1 positive case");
            assert_eq!(eval.cases[0].query, "find files fast");
            assert!(
                eval.cases[0].relevant.contains(&mid),
                "memory_id must be in relevant"
            );
            assert_eq!(eval.noise_count, 0);
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn build_personal_eval_noise_when_no_citation_in_window() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create");
            init_project(&root, false).expect("init");
            let (_, _, conn) = crate::project::load_project(&root).expect("load");

            // Served event but citation far outside window (3 hours later).
            let mid = add_memory(&root, MemoryScope::Project, MemoryKind::Fact, "noise test")
                .expect("add memory");
            seed_context_served_with_query(&conn, "some query", None, 0);
            seed_memory_cited(&conn, &mid, 3 * 3600);

            let eval = build_personal_eval(&conn, 1800).expect("build");
            assert_eq!(eval.cases.len(), 0, "out-of-window citation → no positive case");
            assert_eq!(eval.noise_count, 1, "should count 1 noise entry");
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn build_personal_eval_deduplicates_same_query() {
        with_user_brain_disabled(|| {
            let root = test_root();
            std::fs::create_dir_all(&root).expect("create");
            init_project(&root, false).expect("init");

            let mid = add_memory(&root, MemoryScope::Project, MemoryKind::Fact, "dedup test")
                .expect("add");
            let (_, _, conn) = crate::project::load_project(&root).expect("load");

            // Same query twice (different sessions/times), both with a citation.
            seed_context_served_with_query(&conn, "duplicate query", None, -100);
            seed_context_served_with_query(&conn, "duplicate query", None, 0);
            seed_memory_cited(&conn, &mid, 60);

            let eval = build_personal_eval(&conn, 1800).expect("build");
            // Dedup: both map to same query string → one case (the latest ts kept).
            assert_eq!(eval.cases.len(), 1, "dedup must produce exactly 1 case");
            std::fs::remove_dir_all(&root).ok();
        });
    }
}
