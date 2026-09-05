//! Personal weak reliance labels from exact delivered exposure + claim revision.
//! Uncited observations are unknown, never negative gold. Raw queries exist only
//! after learning.store_queries opt-in at the producer. Legacy time joins are ignored.
use crate::eval::EvalCase;
use kimetsu_core::KimetsuResult;
use rusqlite::Connection;
#[cfg(test)]
use time::{OffsetDateTime, format_description::well_known::Rfc3339};

#[derive(Debug, Clone, Default)]
pub struct PersonalEval {
    /// Weak reliance labels, not verified relevance.
    pub cases: Vec<EvalCase>,
    /// Compatibility name: count of unknown uncited/unusable observations, NOT noise gold.
    pub noise_count: usize,
    pub oldest: Option<String>,
    pub newest: Option<String>,
}

/// window_secs is retained for source compatibility and intentionally ignored.
pub fn build_personal_eval(conn: &Connection, _window_secs: i64) -> KimetsuResult<PersonalEval> {
    let mut stmt = conn.prepare("SELECT event_id,run_id,payload_json,ts FROM events WHERE kind='context.injected' ORDER BY ts DESC,event_id DESC")?;
    let rows = stmt.query_map([], |r| {
        Ok((
            r.get::<_, String>(0)?,
            r.get::<_, String>(1)?,
            r.get::<_, String>(2)?,
            r.get::<_, String>(3)?,
        ))
    })?;
    let mut seen = std::collections::HashSet::new();
    let mut result = PersonalEval::default();
    for row in rows {
        let (exposure, run, payload, ts) = row?;
        let payload: serde_json::Value = serde_json::from_str(&payload)?;
        let Some(query) = payload["query"].as_str().filter(|q| !q.trim().is_empty()) else {
            continue;
        };
        if !seen.insert(query.to_string()) {
            continue;
        }
        let mut citations = conn.prepare("SELECT payload_json FROM events WHERE kind='memory.cited' AND run_id=?1 AND json_extract(payload_json,'$.exposure_id')=?2 AND json_extract(payload_json,'$.evidence_kind')='reliance'")?;
        let cited =
            citations.query_map(rusqlite::params![run, exposure], |r| r.get::<_, String>(0))?;
        let mut relevant = std::collections::BTreeSet::new();
        for cited in cited {
            let cited: serde_json::Value = serde_json::from_str(&cited?)?;
            let Some(id) = cited["memory_id"].as_str() else {
                continue;
            };
            let Some(revision) = cited["revision_event_id"].as_str() else {
                continue;
            };
            if payload["memory_revisions"][id].as_str() != Some(revision)
                || !payload["memory_ids"]
                    .as_array()
                    .is_some_and(|ids| ids.iter().any(|x| x.as_str() == Some(id)))
                || crate::projector::claim_revision_at(conn, id, None)? != revision
            {
                continue;
            }
            relevant.insert(id.to_string());
        }
        if relevant.is_empty() {
            result.noise_count += 1;
            continue;
        }
        if result.oldest.as_ref().is_none_or(|old| &ts < old) {
            result.oldest = Some(ts.clone())
        }
        if result.newest.as_ref().is_none_or(|new| &ts > new) {
            result.newest = Some(ts.clone())
        }
        result.cases.push(EvalCase {
            query: query.into(),
            relevant: relevant.into_iter().collect(),
            family: payload["task_id"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or("")
                .into(),
            kind: Default::default(),
            stale: Vec::new(),
        });
    }
    result.cases.sort_by(|a, b| a.query.cmp(&b.query));
    Ok(result)
}

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
            origin: None,
            hlc: None,
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
            origin: None,
            hlc: None,
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
            assert!(
                eval.cases.is_empty(),
                "nearby legacy citations are not linked evidence"
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
            assert_eq!(
                eval.cases.len(),
                0,
                "out-of-window citation → no positive case"
            );
            assert_eq!(
                eval.noise_count, 0,
                "legacy retrieval is not a canonical exposure"
            );
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
            assert_eq!(
                eval.cases.len(),
                0,
                "legacy time joins cannot produce labels"
            );
            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn exact_exposure_citation_labels_only_its_query_and_current_claim() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).unwrap();
            let mid = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "the durable label",
            )
            .unwrap();
            let (_, _, conn) = crate::project::load_project(&root).unwrap();
            let revision = projector::claim_revision_at(&conn, &mid, None).unwrap();
            let exposed = |query: &str| {
                Event::new(
                    RunId::new(),
                    "context.injected",
                    serde_json::json!({
                        "query":query,"memory_ids":[mid],"memory_revisions":{mid.clone():revision},
                        "session_id":"shared-session","task_id":"shared-task"
                    }),
                )
            };
            let a = exposed("cited query");
            let b = exposed("uncited query");
            crate::project::record_context_exposure(&root, &a).unwrap();
            crate::project::record_context_exposure(&root, &b).unwrap();
            crate::project::record_exposure_citation(
                &root,
                &a.event_id.to_string(),
                &mid,
                Some("used"),
            )
            .unwrap();
            let eval = build_personal_eval(&conn, 1800).unwrap();
            assert_eq!(eval.cases.len(), 1);
            assert_eq!(eval.cases[0].query, "cited query");
            assert_eq!(eval.cases[0].relevant, vec![mid.clone()]);
            assert_eq!(
                eval.noise_count, 1,
                "uncited is unknown, never a negative gold label"
            );
            crate::project::edit_memory(&root, &mid, Some("a corrected proposition"), None)
                .unwrap();
            let revised = build_personal_eval(&conn, 1800).unwrap();
            assert!(
                revised.cases.is_empty(),
                "old reliance cannot label a corrected proposition"
            );
            assert_eq!(revised.noise_count, 2);
        });
    }
}
