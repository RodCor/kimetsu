//! v3.0: asking the brain what it believed at a point in time.
//!
//! A memory has two independent time axes, and Kimetsu has only ever had one
//! of them working:
//!
//! * **Valid time** — when the fact was true in the world. `valid_from` /
//!   `valid_to`, added in v2.5 for temporal validity, and what default
//!   retrieval filters on: a memory whose `valid_to` is in the past is
//!   excluded.
//! * **Transaction time** — when the brain *learned* it. `created_at` for the
//!   write, and `invalidated_at` / the loser's stamped `valid_to` for the
//!   retraction.
//!
//! With only the first, you can ask "what is true now?" and (via
//! [`crate::context::search_memories_including_expired`]) "what was ever
//! recorded?". You cannot ask **"what did the brain believe on the 3rd?"**,
//! which is the question that matters when a past decision looks wrong and you
//! need to know whether the agent had the information at the time.
//!
//! That is the query Zep's bitemporal graph is built around, and the reason a
//! contradicting fact there invalidates rather than overwrites: history stays
//! answerable.
//!
//! ## The shape here
//!
//! Kimetsu is already most of the way there without a schema change, because
//! **nothing is ever destroyed**. Supersession stamps `superseded_by`,
//! invalidation stamps `invalidated_at`, and automatic contradiction resolution
//! stamps the loser's `valid_to` — every one of them a tombstone with a
//! timestamp, not a delete. The rows to answer an as-of query are all present;
//! there was simply no query that read them that way.
//!
//! So [`as_of_predicate`] is a WHERE clause rather than a migration:
//!
//! ```text
//! created_at      <= T                    -- the brain knew it by then
//! (invalidated_at IS NULL OR > T)         -- and had not retracted it
//! (valid_from     IS NULL OR <= T)        -- and it had taken effect
//! (valid_to       IS NULL OR  > T)        -- and had not expired
//! ```
//!
//! Superseded memories are deliberately *included*: a memory merged into a
//! survivor last week was a live belief the week before, and excluding it would
//! misreport what the brain knew.

use kimetsu_core::KimetsuResult;
use rusqlite::Connection;

use crate::context::ContextCapsule;

/// SQL predicate selecting memories the brain believed at `?1`.
///
/// Takes the as-of timestamp as a single bound parameter, repeated — callers
/// bind it once per placeholder. Written as a fragment rather than a whole
/// query so the several candidate paths can share exactly one definition of
/// "believed at T"; two subtly different versions of this clause would be a
/// bug nobody would ever notice.
pub const AS_OF_PREDICATE: &str = "created_at <= ?1 \
     AND (invalidated_at IS NULL OR invalidated_at > ?1) \
     AND (valid_from IS NULL OR valid_from <= ?1) \
     AND (valid_to IS NULL OR valid_to > ?1)";

/// Human-readable form of [`AS_OF_PREDICATE`], for `--explain` output and docs.
pub fn as_of_predicate() -> &'static str {
    AS_OF_PREDICATE
}

/// One memory as the brain held it at the as-of time.
#[derive(Debug, Clone)]
pub struct AsOfMemory {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub created_at: String,
    /// Present when this memory has *since* been retired — the as-of view
    /// shows it as live, and this says what became of it. The whole point of
    /// the query is usually to see a belief that is no longer current.
    pub retired_at: Option<String>,
    pub retired_reason: Option<String>,
}

/// Every memory the brain believed at `as_of` (RFC 3339), newest first.
///
/// `limit` of 0 means no limit.
pub fn memories_as_of(
    conn: &Connection,
    as_of: &str,
    limit: u32,
) -> KimetsuResult<Vec<AsOfMemory>> {
    let sql = format!(
        "SELECT memory_id, scope, kind, text, created_at,
                invalidated_at, invalidated_reason, valid_to, superseded_by
         FROM memories
         WHERE {AS_OF_PREDICATE}
         ORDER BY created_at DESC
         {}",
        if limit == 0 {
            String::new()
        } else {
            format!("LIMIT {limit}")
        }
    );
    let mut stmt = conn.prepare(&sql)?;
    let rows = stmt
        .query_map(rusqlite::params![as_of], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, Option<String>>(5)?,
                row.get::<_, Option<String>>(6)?,
                row.get::<_, Option<String>>(7)?,
                row.get::<_, Option<String>>(8)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;

    Ok(rows
        .into_iter()
        .map(
            |(
                memory_id,
                scope,
                kind,
                text,
                created_at,
                invalidated_at,
                invalidated_reason,
                valid_to,
                superseded_by,
            )| {
                // What became of it, in the order the brain would have applied
                // it: an explicit invalidation, else an expiry, else a merge.
                let (retired_at, retired_reason) = match (invalidated_at, valid_to, superseded_by) {
                    (Some(at), _, _) => (
                        Some(at),
                        Some(invalidated_reason.unwrap_or_else(|| "invalidated".to_string())),
                    ),
                    (None, Some(until), _) => (Some(until), Some("expired".to_string())),
                    (None, None, Some(survivor)) => (None, Some(format!("merged into {survivor}"))),
                    (None, None, None) => (None, None),
                };
                AsOfMemory {
                    memory_id,
                    scope,
                    kind,
                    text,
                    created_at,
                    retired_at,
                    retired_reason,
                }
            },
        )
        .collect())
}

/// Render as-of memories as context capsules, so an as-of view can be handed to
/// a reader the same way a live bundle is.
pub fn as_of_capsules(memories: &[AsOfMemory]) -> Vec<ContextCapsule> {
    memories
        .iter()
        .map(|m| ContextCapsule {
            id: String::new(),
            kind: "memory".to_string(),
            summary: format!("{}:{} - {}", m.scope, m.kind, m.text),
            token_estimate: (m.text.len() / 4) as u32 + 8,
            expansion_handle: format!("memory:{}", m.memory_id),
            provenance: Vec::new(),
            confidence: 1.0,
            freshness: 0.0,
            relevance: 0.0,
            scope_weight: 0.0,
            score: 0.0,
        })
        .collect()
}

/// How the corpus changed between two points in time.
#[derive(Debug, Clone)]
pub struct BeliefDelta {
    /// Believed at `to` but not at `from`.
    pub learned: Vec<AsOfMemory>,
    /// Believed at `from` but not at `to`.
    pub retired: Vec<AsOfMemory>,
}

/// What the brain learned and retired between `from` and `to`.
///
/// The reason an as-of query is usually worth running: not "what did it know"
/// in the abstract, but "what changed around the time this went wrong".
pub fn belief_delta(conn: &Connection, from: &str, to: &str) -> KimetsuResult<BeliefDelta> {
    use std::collections::HashSet;

    let before = memories_as_of(conn, from, 0)?;
    let after = memories_as_of(conn, to, 0)?;
    let before_ids: HashSet<&str> = before.iter().map(|m| m.memory_id.as_str()).collect();
    let after_ids: HashSet<&str> = after.iter().map(|m| m.memory_id.as_str()).collect();

    Ok(BeliefDelta {
        learned: after
            .iter()
            .filter(|m| !before_ids.contains(m.memory_id.as_str()))
            .cloned()
            .collect(),
        retired: before
            .iter()
            .filter(|m| !after_ids.contains(m.memory_id.as_str()))
            .cloned()
            .collect(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("schema");
        conn
    }

    #[allow(clippy::too_many_arguments)]
    fn insert(
        conn: &Connection,
        id: &str,
        text: &str,
        created_at: &str,
        invalidated_at: Option<&str>,
        valid_from: Option<&str>,
        valid_to: Option<&str>,
        superseded_by: Option<&str>,
    ) {
        conn.execute(
            "INSERT INTO memories
             (memory_id, scope, kind, text, normalized_text, confidence,
              provenance_snapshot_json, created_at, invalidated_at,
              valid_from, valid_to, superseded_by)
             VALUES (?1, 'project', 'fact', ?2, ?2, 0.9, '{}', ?3, ?4, ?5, ?6, ?7)",
            rusqlite::params![
                id,
                text,
                created_at,
                invalidated_at,
                valid_from,
                valid_to,
                superseded_by
            ],
        )
        .expect("insert");
    }

    fn ids(memories: &[AsOfMemory]) -> Vec<&str> {
        let mut v: Vec<&str> = memories.iter().map(|m| m.memory_id.as_str()).collect();
        v.sort_unstable();
        v
    }

    /// A memory the brain had not written yet was not believed.
    #[test]
    fn a_memory_written_later_is_not_in_the_past_view() {
        let c = conn();
        insert(
            &c,
            "early",
            "a",
            "2026-01-01T00:00:00Z",
            None,
            None,
            None,
            None,
        );
        insert(
            &c,
            "late",
            "b",
            "2026-06-01T00:00:00Z",
            None,
            None,
            None,
            None,
        );

        assert_eq!(
            ids(&memories_as_of(&c, "2026-03-01T00:00:00Z", 0).unwrap()),
            vec!["early"]
        );
        assert_eq!(
            ids(&memories_as_of(&c, "2026-09-01T00:00:00Z", 0).unwrap()),
            vec!["early", "late"]
        );
    }

    /// The question this exists to answer: a belief that has since been
    /// retracted must still show up in a view from before the retraction —
    /// otherwise you cannot tell whether the agent had the information.
    #[test]
    fn a_retracted_memory_is_still_visible_before_its_retraction() {
        let c = conn();
        insert(
            &c,
            "retracted",
            "the schema is v10",
            "2026-01-01T00:00:00Z",
            Some("2026-05-01T00:00:00Z"),
            None,
            None,
            None,
        );

        let before = memories_as_of(&c, "2026-03-01T00:00:00Z", 0).unwrap();
        assert_eq!(ids(&before), vec!["retracted"], "believed at the time");
        assert_eq!(
            before[0].retired_at.as_deref(),
            Some("2026-05-01T00:00:00Z"),
            "and the view says what became of it"
        );

        let after = memories_as_of(&c, "2026-07-01T00:00:00Z", 0).unwrap();
        assert!(after.is_empty(), "no longer believed: {:?}", ids(&after));
    }

    /// Valid time and transaction time are independent: a fact recorded in
    /// January but only true from March was not believed in February.
    #[test]
    fn valid_time_is_independent_of_when_it_was_recorded() {
        let c = conn();
        insert(
            &c,
            "future-effective",
            "the new API lands in March",
            "2026-01-01T00:00:00Z",
            None,
            Some("2026-03-01T00:00:00Z"),
            None,
            None,
        );
        assert!(
            memories_as_of(&c, "2026-02-01T00:00:00Z", 0)
                .unwrap()
                .is_empty(),
            "recorded, but not yet in effect"
        );
        assert_eq!(
            ids(&memories_as_of(&c, "2026-04-01T00:00:00Z", 0).unwrap()),
            vec!["future-effective"]
        );
    }

    #[test]
    fn an_expired_memory_drops_out_after_its_valid_to() {
        let c = conn();
        insert(
            &c,
            "expired",
            "we are on rust 1.85",
            "2026-01-01T00:00:00Z",
            None,
            None,
            Some("2026-04-01T00:00:00Z"),
            None,
        );
        assert_eq!(
            ids(&memories_as_of(&c, "2026-02-01T00:00:00Z", 0).unwrap()),
            vec!["expired"]
        );
        assert!(
            memories_as_of(&c, "2026-05-01T00:00:00Z", 0)
                .unwrap()
                .is_empty()
        );
    }

    /// A memory merged into a survivor last week was a live belief the week
    /// before. Excluding it would misreport what the brain knew.
    #[test]
    fn a_superseded_memory_still_counts_as_a_past_belief() {
        let c = conn();
        insert(
            &c,
            "member",
            "checkpoint the wal",
            "2026-01-01T00:00:00Z",
            None,
            None,
            None,
            Some("survivor"),
        );
        let view = memories_as_of(&c, "2026-03-01T00:00:00Z", 0).unwrap();
        assert_eq!(ids(&view), vec!["member"]);
        assert_eq!(
            view[0].retired_reason.as_deref(),
            Some("merged into survivor"),
            "and the view explains where it went"
        );
    }

    #[test]
    fn belief_delta_reports_what_was_learned_and_retired() {
        let c = conn();
        insert(
            &c,
            "kept",
            "a",
            "2026-01-01T00:00:00Z",
            None,
            None,
            None,
            None,
        );
        insert(
            &c,
            "dropped",
            "b",
            "2026-01-01T00:00:00Z",
            Some("2026-04-01T00:00:00Z"),
            None,
            None,
            None,
        );
        insert(
            &c,
            "added",
            "c",
            "2026-03-01T00:00:00Z",
            None,
            None,
            None,
            None,
        );

        let delta = belief_delta(&c, "2026-02-01T00:00:00Z", "2026-06-01T00:00:00Z").unwrap();
        assert_eq!(ids(&delta.learned), vec!["added"]);
        assert_eq!(ids(&delta.retired), vec!["dropped"]);
    }

    #[test]
    fn the_limit_is_respected_and_zero_means_all() {
        let c = conn();
        for i in 0..5 {
            insert(
                &c,
                &format!("m{i}"),
                "x",
                &format!("2026-01-0{}T00:00:00Z", i + 1),
                None,
                None,
                None,
                None,
            );
        }
        assert_eq!(
            memories_as_of(&c, "2026-09-01T00:00:00Z", 0).unwrap().len(),
            5
        );
        assert_eq!(
            memories_as_of(&c, "2026-09-01T00:00:00Z", 2).unwrap().len(),
            2
        );
    }

    #[test]
    fn as_of_capsules_render_the_scope_and_kind_prefix() {
        let c = conn();
        insert(
            &c,
            "m",
            "checkpoint the wal",
            "2026-01-01T00:00:00Z",
            None,
            None,
            None,
            None,
        );
        let capsules = as_of_capsules(&memories_as_of(&c, "2026-02-01T00:00:00Z", 0).unwrap());
        assert_eq!(capsules.len(), 1);
        assert!(capsules[0].summary.starts_with("project:fact - "));
        assert_eq!(capsules[0].expansion_handle, "memory:m");
    }
}
