//! v2.6: has this session wandered off the thing it set out to do?
//!
//! Nautilus Compass reaches ROC AUC 0.83 detecting behavioural drift on real
//! Claude Code traces using nothing but cosine similarity against a behavioural
//! anchor — no model in the loop, no labels, no training. That is a Free-tier
//! signal by construction, and Kimetsu already keeps a warm embedder for
//! retrieval, so the marginal cost of computing it is one embedding per turn.
//!
//! ## What Kimetsu can and cannot see
//!
//! Being precise about this matters, because the paper's setting is not
//! Kimetsu's. Compass reads agent *traces* — the actions the agent took. A
//! memory sidecar sees no such thing. What Kimetsu has is the sequence of user
//! prompts, recorded per session in `context.served`.
//!
//! So this measures how far a session has moved from the question it opened
//! with, not whether an agent has drifted from its instructions. That is a
//! weaker claim than the paper's, and it is the honest one: a session that
//! opened on "fix the failing migration test" and is now three turns into
//! Kubernetes networking has drifted, whatever the agent did in between.
//!
//! ## Why a memory system cares
//!
//! Because retrieval anchors on the session. When Kimetsu augments a query with
//! ambient session context, a drifted session's opening turns stop being
//! context and start being noise — the retrieval is being steered by a task
//! nobody is working on any more. Knowing *where* the session turned is what
//! makes it possible to stop doing that.
//!
//! Report-only for now, like `brain prune` and `brain audit` before it. A
//! signal that silently re-anchors retrieval is a signal whose false positives
//! are invisible, and this one has never been measured on a Kimetsu corpus.
//!
//! ## The shape of the signal
//!
//! Sustained, never instantaneous. A single tangential question is not drift —
//! it is a question. [`detect`] requires the similarity to stay below the
//! threshold for [`SUSTAINED_TURNS`] consecutive turns before it will say the
//! session turned, which is the difference between an aside and a new topic.

use rusqlite::Connection;

use kimetsu_core::KimetsuResult;

/// Cosine below which a turn is considered off-anchor.
///
/// Cosine between short unrelated English texts under the default embedder
/// sits well under this; two phrasings of the same task sit well above it. It
/// is a wide gap, chosen deliberately: the cost of a false positive here is
/// telling a user their session drifted when it did not, and the signal has no
/// measured operating point on a Kimetsu corpus to tune against.
pub const OFF_ANCHOR_COSINE: f32 = 0.35;

/// Consecutive off-anchor turns required before the session is called drifted.
///
/// One tangential question is a question. Three in a row is a different task.
pub const SUSTAINED_TURNS: usize = 3;

/// A session's prompts, oldest first.
#[derive(Debug, Clone, PartialEq)]
pub struct SessionQueries {
    pub session_id: String,
    pub queries: Vec<String>,
}

/// What [`analyze`] found in one session.
#[derive(Debug, Clone, PartialEq)]
pub struct DriftReport {
    pub session_id: String,
    /// Cosine of each turn against the anchor, in turn order. The anchor's own
    /// entry is 1.0 and is included so indices line up with the prompts.
    pub similarity: Vec<f32>,
    /// Index of the first turn of the sustained run that turned the session,
    /// or `None` when the session held its topic.
    pub drifted_at: Option<usize>,
}

impl DriftReport {
    pub fn drifted(&self) -> bool {
        self.drifted_at.is_some()
    }

    /// How far the session got from its anchor at worst, in `[0, 1]` where 1.0
    /// means it never left.
    pub fn min_similarity(&self) -> f32 {
        self.similarity
            .iter()
            .copied()
            .fold(1.0f32, |acc, s| acc.min(s))
    }
}

/// The first turn of the first sustained off-anchor run, if any.
///
/// `similarity[0]` is the anchor against itself and is skipped: a session
/// cannot drift at the moment it starts.
pub fn detect(similarity: &[f32], threshold: f32, sustained: usize) -> Option<usize> {
    if sustained == 0 {
        return None;
    }
    let mut run_start: Option<usize> = None;
    for (idx, &sim) in similarity.iter().enumerate().skip(1) {
        if sim < threshold {
            let start = *run_start.get_or_insert(idx);
            if idx + 1 - start >= sustained {
                return Some(start);
            }
        } else {
            run_start = None;
        }
    }
    None
}

/// Score one session's turns against its opening turn.
///
/// The anchor is the session's first prompt — what it set out to do. Anchoring
/// on a rolling window instead would let the session walk anywhere one step at
/// a time without ever registering, which is exactly the failure being looked
/// for.
pub fn analyze(session_id: &str, embeddings: &[Vec<f32>]) -> DriftReport {
    let Some(anchor) = embeddings.first() else {
        return DriftReport {
            session_id: session_id.to_string(),
            similarity: Vec::new(),
            drifted_at: None,
        };
    };
    let similarity: Vec<f32> = embeddings
        .iter()
        .map(|e| crate::embeddings::cosine_similarity(anchor, e))
        .collect();
    let drifted_at = detect(&similarity, OFF_ANCHOR_COSINE, SUSTAINED_TURNS);
    DriftReport {
        session_id: session_id.to_string(),
        similarity,
        drifted_at,
    }
}

/// Reconstruct recent sessions' prompts from the `context.served` log.
///
/// Sessions are returned newest-first by their last turn; the prompts within
/// each are oldest-first, which is the order drift is measured in.
///
/// Only sessions whose queries were actually stored are returned. `[learning]
/// store_queries = false` keeps a `query_hash` and drops the text, which is a
/// deliberate privacy choice and leaves nothing to embed — such sessions are
/// absent rather than reported as un-drifted.
pub fn recent_sessions(conn: &Connection, limit: usize) -> KimetsuResult<Vec<SessionQueries>> {
    let mut stmt = conn.prepare(
        "SELECT payload_json, ts
         FROM events
         WHERE kind = 'context.served'
         ORDER BY ts",
    )?;
    let rows = stmt
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    // Insertion order is turn order because the query is sorted by ts; the map
    // preserves which session was seen last so the newest can be returned
    // first without a second sort key.
    let mut order: Vec<String> = Vec::new();
    let mut by_session: std::collections::HashMap<String, Vec<String>> =
        std::collections::HashMap::new();
    for (payload_json, _ts) in rows {
        let Ok(payload) = serde_json::from_str::<serde_json::Value>(&payload_json) else {
            continue;
        };
        let Some(session_id) = payload
            .get("session_id")
            .and_then(serde_json::Value::as_str)
        else {
            continue; // a host with no session id has no session to measure
        };
        let Some(query) = payload
            .get("query")
            .and_then(serde_json::Value::as_str)
            .filter(|q| !q.trim().is_empty())
        else {
            continue; // store_queries = false: nothing to embed
        };
        let entry = by_session.entry(session_id.to_string()).or_insert_with(|| {
            order.push(session_id.to_string());
            Vec::new()
        });
        entry.push(query.to_string());
    }

    let mut sessions: Vec<SessionQueries> = order
        .into_iter()
        .rev()
        .filter_map(|session_id| {
            by_session
                .remove(&session_id)
                .map(|queries| SessionQueries {
                    session_id,
                    queries,
                })
        })
        .take(limit)
        .collect();
    // A one-turn session has an anchor and nothing to compare it against.
    sessions.retain(|s| s.queries.len() > 1);
    Ok(sessions)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A session that stays on topic must not be flagged. False positives here
    /// cost more than misses: the signal has no measured operating point yet.
    #[test]
    fn a_session_that_holds_its_topic_does_not_drift() {
        let report = analyze("s", &[vec![1.0, 0.0], vec![0.95, 0.05], vec![0.9, 0.1]]);
        assert!(!report.drifted(), "got: {report:?}");
        assert!(report.min_similarity() > OFF_ANCHOR_COSINE);
    }

    /// A single tangential question is a question, not a new task.
    #[test]
    fn one_off_anchor_turn_is_not_drift() {
        let similarity = [1.0, 0.9, 0.05, 0.9, 0.95];
        assert_eq!(
            detect(&similarity, OFF_ANCHOR_COSINE, SUSTAINED_TURNS),
            None
        );
    }

    /// Sustained is the whole definition: three in a row is a different task.
    #[test]
    fn a_sustained_run_marks_where_the_session_turned() {
        let similarity = [1.0, 0.9, 0.05, 0.02, 0.01, 0.03];
        assert_eq!(
            detect(&similarity, OFF_ANCHOR_COSINE, SUSTAINED_TURNS),
            Some(2),
            "the run starts where it started, not where it was confirmed"
        );
    }

    /// A run interrupted by a return to topic starts over, or a session that
    /// dips in and out would eventually accumulate into a false positive.
    #[test]
    fn a_return_to_topic_resets_the_run() {
        let similarity = [1.0, 0.05, 0.02, 0.9, 0.05, 0.02];
        assert_eq!(
            detect(&similarity, OFF_ANCHOR_COSINE, SUSTAINED_TURNS),
            None
        );
    }

    /// The anchor cannot drift from itself, and a session cannot drift at the
    /// moment it opens.
    #[test]
    fn the_anchor_turn_is_never_the_drift_point() {
        let similarity = [0.0, 0.0, 0.0, 0.0];
        assert_eq!(
            detect(&similarity, OFF_ANCHOR_COSINE, SUSTAINED_TURNS),
            Some(1)
        );
    }

    #[test]
    fn an_empty_session_reports_nothing() {
        let report = analyze("s", &[]);
        assert!(!report.drifted());
        assert!(report.similarity.is_empty());
        assert_eq!(report.min_similarity(), 1.0);
    }

    /// Anchoring on the opening turn, not on a rolling window: a session that
    /// walks away one small step at a time is exactly the case a rolling anchor
    /// would never catch.
    #[test]
    fn a_slow_walk_away_from_the_opening_turn_is_still_drift() {
        let steps = [
            vec![1.0f32, 0.0],
            vec![0.8, 0.6],
            vec![0.3, 0.95],
            vec![0.1, 0.99],
            vec![0.0, 1.0],
        ];
        let report = analyze("s", &steps);
        assert!(report.drifted(), "got: {report:?}");
    }

    // ── Reconstructing sessions from the log ─────────────────────────────

    fn served(conn: &Connection, session_id: Option<&str>, query: Option<&str>, ts: &str) {
        let mut payload = serde_json::Map::new();
        if let Some(sid) = session_id {
            payload.insert("session_id".into(), serde_json::json!(sid));
        }
        if let Some(q) = query {
            payload.insert("query".into(), serde_json::json!(q));
        }
        conn.execute(
            "INSERT INTO events (event_id, run_id, ts, kind, schema_version, payload_json)
             VALUES (?1, 'r', ?2, 'context.served', 1, ?3)",
            rusqlite::params![
                kimetsu_core::ids::new_id().to_string(),
                ts,
                serde_json::Value::Object(payload).to_string()
            ],
        )
        .expect("insert event");
    }

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("schema");
        conn
    }

    #[test]
    fn turns_come_back_in_order_grouped_by_session() {
        let c = conn();
        served(&c, Some("a"), Some("first"), "2026-01-01T00:00:00Z");
        served(&c, Some("b"), Some("other"), "2026-01-01T00:00:01Z");
        served(&c, Some("a"), Some("second"), "2026-01-01T00:00:02Z");
        served(&c, Some("b"), Some("other two"), "2026-01-01T00:00:03Z");

        let sessions = recent_sessions(&c, 10).expect("sessions");
        assert_eq!(sessions.len(), 2);
        assert_eq!(sessions[0].session_id, "b", "newest first: {sessions:?}");
        let a = sessions.iter().find(|s| s.session_id == "a").expect("a");
        assert_eq!(a.queries, vec!["first", "second"], "oldest turn first");
    }

    /// `store_queries = false` is a deliberate privacy choice that leaves
    /// nothing to embed. Such a session is absent, not reported as un-drifted.
    #[test]
    fn sessions_without_stored_queries_are_absent_not_clean() {
        let c = conn();
        served(&c, Some("a"), None, "2026-01-01T00:00:00Z");
        served(&c, Some("a"), None, "2026-01-01T00:00:01Z");
        assert!(recent_sessions(&c, 10).expect("sessions").is_empty());
    }

    /// A one-turn session has an anchor and nothing to compare it against.
    #[test]
    fn a_single_turn_session_is_not_reported() {
        let c = conn();
        served(&c, Some("a"), Some("only turn"), "2026-01-01T00:00:00Z");
        assert!(recent_sessions(&c, 10).expect("sessions").is_empty());
    }

    /// A host that reports no session id has no session to measure.
    #[test]
    fn turns_without_a_session_id_are_skipped() {
        let c = conn();
        served(&c, None, Some("a query"), "2026-01-01T00:00:00Z");
        served(&c, None, Some("another"), "2026-01-01T00:00:01Z");
        assert!(recent_sessions(&c, 10).expect("sessions").is_empty());
    }
}
