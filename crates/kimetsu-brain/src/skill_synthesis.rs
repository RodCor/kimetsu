//! Flagship 2 — Memory → Skill synthesis: candidate detection.
//!
//! # Trigger (2.1)
//!
//! A memory becomes a "synthesis candidate" when either:
//! - It has been explicitly cited via `memory.cited` events **≥ 3 times**
//!   across distinct runs (`CITATION_THRESHOLD`), OR
//! - It participates in a loose semantic cluster (`find_distill_clusters`)
//!   of ≥ 3 related lessons sharing at least one domain tag — indicating a
//!   repeated domain pattern worth promoting to a reusable skill.
//!
//! Detection is **pure query / counter** — no model cost.
//!
//! # Proposal store (2.3)
//!
//! Accepted candidates land in `skill_proposals` (v6 schema migration).
//! The proposal carries the source memory ids as provenance.
//!
//! # Staleness (2.4)
//!
//! A skill is STALE when any source memory in its provenance list has been
//! superseded or invalidated.  `staleness_check` returns which ids are stale.

use std::collections::HashMap;

use rusqlite::{Connection, OptionalExtension, params};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use kimetsu_core::KimetsuResult;

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// Minimum number of distinct-run citations for a memory to become a synthesis
/// candidate via the citation path.
pub const CITATION_THRESHOLD: i64 = 3;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// A memory that qualifies for skill synthesis.
#[derive(Debug, Clone)]
pub struct SynthesisCandidate {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    /// `"citations"` — cited ≥ CITATION_THRESHOLD times across distinct runs.
    /// `"cluster"` — member of a tight semantic cluster of ≥ 3 lessons.
    pub trigger_kind: String,
    /// Citation count (for `"citations"` trigger) or cluster size.
    pub trigger_count: i64,
}

/// A persisted skill proposal (pending or decided).
#[derive(Debug, Clone)]
pub struct SkillProposalRow {
    pub proposal_id: String,
    pub skill_name: String,
    pub description: String,
    /// The drafted SKILL.md content, `None` in report-only (no-model) mode.
    pub draft_content: Option<String>,
    /// JSON-serialized list of source memory ids.
    pub source_memory_ids: Vec<String>,
    pub trigger_kind: String,
    pub trigger_count: i64,
    /// `"pending"` | `"accepted"` | `"rejected"`
    pub status: String,
    pub decided_at: Option<String>,
    pub installed_path: Option<String>,
    pub created_at: String,
}

/// Result of a staleness check for an installed skill.
#[derive(Debug, Clone)]
pub struct StalenessReport {
    pub proposal_id: String,
    pub skill_name: String,
    pub installed_path: Option<String>,
    /// Memory ids from the skill's provenance that are now stale
    /// (superseded or invalidated).
    pub stale_memory_ids: Vec<String>,
    pub is_stale: bool,
}

// ---------------------------------------------------------------------------
// 2.1 — Candidate detection (pure query, no model cost)
// ---------------------------------------------------------------------------

/// v2.6: what the skills loop is waiting on, as one line for the warm start.
///
/// Detection is a pure query and has run on a schedule since the maintenance
/// daemon landed — but its result went into a log file nobody opens. So a
/// memory could cross the citation threshold, sit there indefinitely, and never
/// become a skill, because closing the loop needed a person who was never told
/// there was anything to close. `find_synthesis_candidates` having exactly one
/// caller (the `brain skills` CLI) was the same problem stated as a call graph.
///
/// Two things are worth a session's attention, and nothing else is:
///
/// - **Candidates with no proposal yet** — memories that have earned skill
///   status and are waiting for `brain skills --detect` to draft them.
/// - **Pending proposals** — drafts waiting for an accept or reject.
///
/// A candidate that already has a pending or accepted proposal is *not*
/// reported as a candidate: the loop has moved on, and repeating it would
/// double-count the same memory in two halves of the same line. That is the
/// same rule `run_skill_synthesis` uses to stay idempotent.
///
/// Returns `None` when there is nothing to act on, which is the common case —
/// the warm start pays for this block on every session, so it must be silent
/// unless it has something to say.
pub fn graduation_notice(conn: &Connection) -> Option<String> {
    let pending = conn
        .query_row(
            "SELECT COUNT(*) FROM skill_proposals WHERE status = 'pending'",
            [],
            |row| row.get::<_, i64>(0),
        )
        .unwrap_or(0);

    let undrafted = find_synthesis_candidates(conn)
        .unwrap_or_default()
        .into_iter()
        .filter(|candidate| !has_open_proposal(conn, &candidate.memory_id))
        .count();

    let mut parts: Vec<String> = Vec::new();
    if undrafted > 0 {
        parts.push(format!(
            "{undrafted} {} earned skill status (`kimetsu brain skills --detect`)",
            plural(undrafted, "memory has", "memories have"),
        ));
    }
    if pending > 0 {
        parts.push(format!(
            "{pending} skill {} awaiting review (`kimetsu brain skills --list`)",
            plural(pending as usize, "proposal is", "proposals are"),
        ));
    }
    if parts.is_empty() {
        return None;
    }
    Some(parts.join("; "))
}

/// True when this memory already has a pending or accepted proposal.
///
/// The substring match on the JSON column mirrors `run_skill_synthesis`'s
/// existing duplicate check; a ULID is long enough that a false match is not a
/// practical concern, and the cost of one is a line that under-reports by one.
fn has_open_proposal(conn: &Connection, memory_id: &str) -> bool {
    conn.query_row(
        "SELECT COUNT(*) FROM skill_proposals
         WHERE status IN ('pending', 'accepted')
           AND source_memory_ids_json LIKE ?1",
        params![format!("%{memory_id}%")],
        |row| row.get::<_, i64>(0),
    )
    .unwrap_or(0)
        > 0
}

fn plural(n: usize, one: &'static str, many: &'static str) -> &'static str {
    if n == 1 { one } else { many }
}

/// Find all synthesis candidates in `conn`.
///
/// Path 1 — citation count: memories cited ≥ `CITATION_THRESHOLD` times
/// across distinct runs, not superseded or invalidated.
///
/// Path 2 — cluster trigger: for any tight semantic cluster of ≥ 3 memories
/// sharing a domain tag (from `find_distill_clusters`), the representative
/// memory of each cluster is returned as a cluster candidate, carrying all
/// cluster member ids in a comma-separated `text` prefix. This path only
/// fires when embeddings are present.
///
/// Results are deduplicated by `memory_id` (citation path wins on conflict).
pub fn find_synthesis_candidates(conn: &Connection) -> KimetsuResult<Vec<SynthesisCandidate>> {
    let mut candidates: HashMap<String, SynthesisCandidate> = HashMap::new();

    // --- Path 1: citation count ≥ CITATION_THRESHOLD --------------------
    let mut stmt = conn.prepare(
        "SELECT mc.memory_id,
                COUNT(DISTINCT mc.run_id) AS cite_count,
                m.scope, m.kind, m.text
         FROM memory_citations mc
         JOIN memories m ON m.memory_id = mc.memory_id
         WHERE m.invalidated_at IS NULL
           AND m.superseded_by IS NULL
         GROUP BY mc.memory_id
         HAVING COUNT(DISTINCT mc.run_id) >= ?1
         ORDER BY cite_count DESC",
    )?;
    let rows = stmt.query_map(params![CITATION_THRESHOLD], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
        ))
    })?;
    for row in rows {
        let (memory_id, cite_count, scope, kind, text) = row?;
        candidates
            .entry(memory_id.clone())
            .or_insert(SynthesisCandidate {
                memory_id,
                scope,
                kind,
                text,
                trigger_kind: "citations".to_string(),
                trigger_count: cite_count,
            });
    }

    // --- Path 2: tight semantic cluster (embeddings optional) -----------
    // Load embeddable rows and run find_distill_clusters.  If no
    // embeddings are present, by_model will be empty and this path is
    // silently skipped — no hard failure.
    if let Ok(by_model) = crate::consolidate::load_embeddable_rows(conn) {
        let all_rows: Vec<crate::consolidate::ConsolidateRow> =
            by_model.into_values().flatten().collect();
        let opts = crate::consolidate::DistillOptions {
            lo: 0.75,
            hi: 0.92,
            min_cluster_size: 3,
        };
        let clusters = crate::consolidate::find_distill_clusters(&all_rows, &opts);
        for cluster in clusters {
            // Use the first memory in the cluster as the representative.
            if let Some(rep) = cluster.memories.first() {
                if !candidates.contains_key(&rep.memory_id) {
                    candidates.insert(
                        rep.memory_id.clone(),
                        SynthesisCandidate {
                            memory_id: rep.memory_id.clone(),
                            scope: rep.scope.clone(),
                            kind: rep.kind.clone(),
                            text: rep.text.clone(),
                            trigger_kind: "cluster".to_string(),
                            trigger_count: cluster.memories.len() as i64,
                        },
                    );
                }
            }
        }
    }

    let mut result: Vec<SynthesisCandidate> = candidates.into_values().collect();
    // Stable order: citations first, then cluster; within each group by count desc.
    result.sort_by(|a, b| {
        a.trigger_kind
            .cmp(&b.trigger_kind)
            .then_with(|| b.trigger_count.cmp(&a.trigger_count))
    });
    Ok(result)
}

// ---------------------------------------------------------------------------
// 2.1 helper: fetch the cited memory texts for a set of memory ids
// ---------------------------------------------------------------------------

/// Return (memory_id, text) pairs for the given ids, excluding superseded /
/// invalidated rows.  Used by the drafter to assemble the grounded prompt.
pub fn load_memory_texts(
    conn: &Connection,
    memory_ids: &[String],
) -> KimetsuResult<Vec<(String, String)>> {
    if memory_ids.is_empty() {
        return Ok(Vec::new());
    }
    let placeholders: Vec<String> = (1..=memory_ids.len()).map(|i| format!("?{i}")).collect();
    let sql = format!(
        "SELECT memory_id, text FROM memories
         WHERE memory_id IN ({})
           AND invalidated_at IS NULL
           AND superseded_by IS NULL",
        placeholders.join(", ")
    );
    let mut stmt = conn.prepare(&sql)?;
    let params_iter: Vec<&dyn rusqlite::types::ToSql> = memory_ids
        .iter()
        .map(|s| s as &dyn rusqlite::types::ToSql)
        .collect();
    let rows = stmt.query_map(params_iter.as_slice(), |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Load memory texts for all memories that are cited ≥ `CITATION_THRESHOLD`
/// times for a given `memory_id` candidate. Also returns the distinct-run
/// citation count for that memory.
pub fn load_candidate_with_related(
    conn: &Connection,
    memory_id: &str,
) -> KimetsuResult<(i64, Vec<(String, String)>)> {
    // Citation count for this specific memory.
    let cite_count: i64 = conn
        .query_row(
            "SELECT COUNT(DISTINCT run_id) FROM memory_citations WHERE memory_id = ?1",
            params![memory_id],
            |r| r.get(0),
        )
        .unwrap_or(0);

    // The candidate memory + any memories in the same cluster (same tags
    // and cosine band). For the simple path we just load the candidate itself.
    let texts = load_memory_texts(conn, &[memory_id.to_string()])?;
    Ok((cite_count, texts))
}

// ---------------------------------------------------------------------------
// 2.3 — Proposal CRUD
// ---------------------------------------------------------------------------

/// Insert a new skill proposal into the database.  Returns the proposal_id.
pub fn insert_skill_proposal(
    conn: &Connection,
    skill_name: &str,
    description: &str,
    draft_content: Option<&str>,
    source_memory_ids: &[String],
    trigger_kind: &str,
    trigger_count: i64,
) -> KimetsuResult<String> {
    use ulid::Ulid;

    let proposal_id = Ulid::new().to_string();
    let source_ids_json =
        serde_json::to_string(source_memory_ids).unwrap_or_else(|_| "[]".to_string());
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());

    conn.execute(
        "INSERT INTO skill_proposals
             (proposal_id, skill_name, description, draft_content,
              source_memory_ids_json, trigger_kind, trigger_count,
              status, created_at)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, 'pending', ?8)",
        params![
            proposal_id,
            skill_name,
            description,
            draft_content,
            source_ids_json,
            trigger_kind,
            trigger_count,
            now,
        ],
    )?;
    Ok(proposal_id)
}

/// List skill proposals, optionally filtered by `status`.
pub fn list_skill_proposals(
    conn: &Connection,
    status_filter: Option<&str>,
) -> KimetsuResult<Vec<SkillProposalRow>> {
    let sql = if status_filter.is_some() {
        "SELECT proposal_id, skill_name, description, draft_content,
                source_memory_ids_json, trigger_kind, trigger_count,
                status, decided_at, installed_path, created_at
         FROM skill_proposals
         WHERE status = ?1
         ORDER BY created_at DESC"
    } else {
        "SELECT proposal_id, skill_name, description, draft_content,
                source_memory_ids_json, trigger_kind, trigger_count,
                status, decided_at, installed_path, created_at
         FROM skill_proposals
         ORDER BY created_at DESC"
    };

    let mut stmt = conn.prepare(sql)?;
    let rows = if let Some(sf) = status_filter {
        stmt.query_map(params![sf], parse_proposal_row)?
    } else {
        stmt.query_map([], parse_proposal_row)?
    };

    let mut out = Vec::new();
    for row in rows {
        out.push(row?);
    }
    Ok(out)
}

/// Load one proposal by id.
pub fn load_skill_proposal(
    conn: &Connection,
    proposal_id: &str,
) -> KimetsuResult<Option<SkillProposalRow>> {
    conn.query_row(
        "SELECT proposal_id, skill_name, description, draft_content,
                source_memory_ids_json, trigger_kind, trigger_count,
                status, decided_at, installed_path, created_at
         FROM skill_proposals
         WHERE proposal_id = ?1",
        params![proposal_id],
        parse_proposal_row,
    )
    .optional()
    .map_err(Into::into)
}

/// Mark a proposal as accepted and record where the skill was installed.
pub fn accept_skill_proposal(
    conn: &Connection,
    proposal_id: &str,
    installed_path: &str,
) -> KimetsuResult<()> {
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    let updated = conn.execute(
        "UPDATE skill_proposals
         SET status = 'accepted', decided_at = ?1, installed_path = ?2
         WHERE proposal_id = ?3 AND status = 'pending'",
        params![now, installed_path, proposal_id],
    )?;
    if updated == 0 {
        return Err(format!("proposal `{proposal_id}` not found or already decided").into());
    }
    Ok(())
}

/// Mark a proposal as rejected.
pub fn reject_skill_proposal(conn: &Connection, proposal_id: &str) -> KimetsuResult<()> {
    let now = OffsetDateTime::now_utc()
        .format(&Rfc3339)
        .unwrap_or_else(|_| "1970-01-01T00:00:00Z".to_string());
    let updated = conn.execute(
        "UPDATE skill_proposals
         SET status = 'rejected', decided_at = ?1
         WHERE proposal_id = ?2 AND status = 'pending'",
        params![now, proposal_id],
    )?;
    if updated == 0 {
        return Err(format!("proposal `{proposal_id}` not found or already decided").into());
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// 2.4 — Staleness check
// ---------------------------------------------------------------------------

/// Check all accepted skill proposals for staleness.
///
/// A skill is STALE when any source memory in its provenance is now
/// superseded (`superseded_by IS NOT NULL`) or invalidated
/// (`invalidated_at IS NOT NULL`).
pub fn check_staleness(conn: &Connection) -> KimetsuResult<Vec<StalenessReport>> {
    let accepted = list_skill_proposals(conn, Some("accepted"))?;
    let mut reports = Vec::new();

    for proposal in accepted {
        if proposal.source_memory_ids.is_empty() {
            reports.push(StalenessReport {
                proposal_id: proposal.proposal_id.clone(),
                skill_name: proposal.skill_name.clone(),
                installed_path: proposal.installed_path.clone(),
                stale_memory_ids: Vec::new(),
                is_stale: false,
            });
            continue;
        }

        let mut stale_ids = Vec::new();
        for mid in &proposal.source_memory_ids {
            let is_stale: bool = conn
                .query_row(
                    "SELECT (superseded_by IS NOT NULL OR invalidated_at IS NOT NULL)
                     FROM memories WHERE memory_id = ?1",
                    params![mid],
                    |r| r.get::<_, bool>(0),
                )
                .unwrap_or(false); // memory_id not found → not stale (may be user-brain memory)
            if is_stale {
                stale_ids.push(mid.clone());
            }
        }

        let is_stale = !stale_ids.is_empty();
        reports.push(StalenessReport {
            proposal_id: proposal.proposal_id,
            skill_name: proposal.skill_name,
            installed_path: proposal.installed_path,
            stale_memory_ids: stale_ids,
            is_stale,
        });
    }

    Ok(reports)
}

// ---------------------------------------------------------------------------
// Internal helpers
// ---------------------------------------------------------------------------

fn parse_proposal_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<SkillProposalRow> {
    let source_ids_json: String = row.get(4)?;
    let source_memory_ids: Vec<String> = serde_json::from_str(&source_ids_json).unwrap_or_default();
    Ok(SkillProposalRow {
        proposal_id: row.get(0)?,
        skill_name: row.get(1)?,
        description: row.get(2)?,
        draft_content: row.get(3)?,
        source_memory_ids,
        trigger_kind: row.get(5)?,
        trigger_count: row.get(6)?,
        status: row.get(7)?,
        decided_at: row.get(8)?,
        installed_path: row.get(9)?,
        created_at: row.get(10)?,
    })
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn init_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        crate::schema::initialize(&conn).expect("initialize");
        conn
    }

    // -----------------------------------------------------------------------
    // insert / list / load / accept / reject
    // -----------------------------------------------------------------------

    #[test]
    fn insert_and_list_pending_proposals() {
        let conn = init_conn();
        let id = insert_skill_proposal(
            &conn,
            "my-skill",
            "A test skill",
            Some("# My Skill\nDo the thing."),
            &["mem-1".to_string(), "mem-2".to_string()],
            "citations",
            4,
        )
        .expect("insert");
        assert!(!id.is_empty());

        let all = list_skill_proposals(&conn, None).expect("list all");
        assert_eq!(all.len(), 1);
        let row = &all[0];
        assert_eq!(row.proposal_id, id);
        assert_eq!(row.skill_name, "my-skill");
        assert_eq!(row.description, "A test skill");
        assert_eq!(
            row.draft_content.as_deref(),
            Some("# My Skill\nDo the thing.")
        );
        assert_eq!(row.source_memory_ids, vec!["mem-1", "mem-2"]);
        assert_eq!(row.trigger_kind, "citations");
        assert_eq!(row.trigger_count, 4);
        assert_eq!(row.status, "pending");
        assert!(row.installed_path.is_none());
    }

    #[test]
    fn accept_proposal_records_installed_path() {
        let conn = init_conn();
        let id = insert_skill_proposal(
            &conn,
            "install-me",
            "Install test",
            Some("# Install Me"),
            &["mem-a".to_string()],
            "citations",
            3,
        )
        .expect("insert");

        accept_skill_proposal(&conn, &id, "/path/to/.kimetsu/skills/install-me").expect("accept");

        let row = load_skill_proposal(&conn, &id)
            .expect("load")
            .expect("must exist");
        assert_eq!(row.status, "accepted");
        assert_eq!(
            row.installed_path.as_deref(),
            Some("/path/to/.kimetsu/skills/install-me")
        );
        assert!(row.decided_at.is_some());
    }

    #[test]
    fn reject_proposal_marks_rejected() {
        let conn = init_conn();
        let id = insert_skill_proposal(&conn, "reject-me", "Reject test", None, &[], "cluster", 3)
            .expect("insert");
        reject_skill_proposal(&conn, &id).expect("reject");

        let row = load_skill_proposal(&conn, &id)
            .expect("load")
            .expect("must exist");
        assert_eq!(row.status, "rejected");
    }

    #[test]
    fn accept_already_decided_proposal_errors() {
        let conn = init_conn();
        let id =
            insert_skill_proposal(&conn, "dup", "dup", None, &[], "citations", 3).expect("insert");
        accept_skill_proposal(&conn, &id, "/some/path").expect("first accept");
        let err = accept_skill_proposal(&conn, &id, "/other").expect_err("double-accept");
        assert!(
            err.to_string().contains("already decided"),
            "unexpected: {err}"
        );
    }

    #[test]
    fn report_only_proposal_has_no_draft_content() {
        let conn = init_conn();
        let id = insert_skill_proposal(
            &conn,
            "report-only",
            "Report only",
            None, // no draft
            &["mem-x".to_string()],
            "citations",
            5,
        )
        .expect("insert");
        let row = load_skill_proposal(&conn, &id)
            .expect("load")
            .expect("must exist");
        assert!(
            row.draft_content.is_none(),
            "report-only must have no draft"
        );
    }

    // -----------------------------------------------------------------------
    // find_synthesis_candidates — citation path
    // -----------------------------------------------------------------------

    #[test]
    fn candidate_detected_at_citation_threshold() {
        let conn = init_conn();

        // Insert one memory and cite it from 3 distinct runs.
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score)
             VALUES ('hot-mem', 'project', 'convention', 'Always run fmt', 'always run fmt',
                     0.9, '{}', '2026-01-01T00:00:00Z', 3, 3.0)",
            [],
        )
        .expect("insert memory");

        for run_id in ["run-1", "run-2", "run-3"] {
            conn.execute(
                "INSERT INTO memory_citations (run_id, memory_id, turn, cited_at)
                 VALUES (?1, 'hot-mem', 1, '2026-01-01T00:00:00Z')",
                params![run_id],
            )
            .expect("insert citation");
        }

        let candidates = find_synthesis_candidates(&conn).expect("find");
        assert!(
            candidates.iter().any(|c| c.memory_id == "hot-mem"),
            "hot-mem must be a synthesis candidate"
        );
        let hot = candidates
            .iter()
            .find(|c| c.memory_id == "hot-mem")
            .unwrap();
        assert_eq!(hot.trigger_kind, "citations");
        assert_eq!(hot.trigger_count, 3);
    }

    // -----------------------------------------------------------------------
    // graduation_notice — v2.6, closing the skills loop
    // -----------------------------------------------------------------------

    /// Cite `memory_id` from `n` distinct runs, which is what earns skill status.
    fn cite_from_distinct_runs(conn: &Connection, memory_id: &str, n: i64) {
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at)
             VALUES (?1, 'project', 'convention', 'Always run fmt', 'always run fmt',
                     0.9, '{}', '2026-01-01T00:00:00Z')",
            params![memory_id],
        )
        .expect("insert memory");
        for run in 0..n {
            conn.execute(
                "INSERT INTO memory_citations (run_id, memory_id, turn, cited_at)
                 VALUES (?1, ?2, 1, '2026-01-01T00:00:00Z')",
                params![format!("run-{memory_id}-{run}"), memory_id],
            )
            .expect("insert citation");
        }
    }

    /// The warm start pays for this block on every session, so a brain with
    /// nothing to graduate must say nothing at all.
    #[test]
    fn a_quiet_brain_gets_no_nudge() {
        let conn = init_conn();
        assert!(graduation_notice(&conn).is_none());
        cite_from_distinct_runs(&conn, "cold-mem", CITATION_THRESHOLD - 1);
        assert!(
            graduation_notice(&conn).is_none(),
            "below the threshold is not a graduation"
        );
    }

    /// The gap this closes: detection ran, found something, and told nobody.
    #[test]
    fn an_undrafted_candidate_is_surfaced_with_its_command() {
        let conn = init_conn();
        cite_from_distinct_runs(&conn, "hot-mem", CITATION_THRESHOLD);
        let notice = graduation_notice(&conn).expect("surfaced");
        assert!(notice.contains('1'), "got: {notice}");
        assert!(
            notice.contains("kimetsu brain skills --detect"),
            "a nudge without the command is not actionable; got: {notice}"
        );
    }

    /// Once a candidate has been drafted the loop has moved on, and reporting
    /// it as still-undrafted would double-count the same memory across both
    /// halves of the line.
    #[test]
    fn a_drafted_candidate_is_reported_as_pending_not_as_a_candidate() {
        let conn = init_conn();
        cite_from_distinct_runs(&conn, "hot-mem", CITATION_THRESHOLD);
        insert_skill_proposal(
            &conn,
            "always-run-fmt",
            "Run cargo fmt before committing",
            None,
            &["hot-mem".to_string()],
            "citations",
            CITATION_THRESHOLD,
        )
        .expect("insert proposal");

        let notice = graduation_notice(&conn).expect("surfaced");
        assert!(
            !notice.contains("--detect"),
            "nothing left to detect; got: {notice}"
        );
        assert!(
            notice.contains("awaiting review"),
            "the draft is what needs a decision now; got: {notice}"
        );
    }

    /// An accepted proposal is a closed loop — nothing to nudge about.
    #[test]
    fn an_accepted_proposal_ends_the_nudge() {
        let conn = init_conn();
        cite_from_distinct_runs(&conn, "hot-mem", CITATION_THRESHOLD);
        let proposal_id = insert_skill_proposal(
            &conn,
            "always-run-fmt",
            "Run cargo fmt before committing",
            None,
            &["hot-mem".to_string()],
            "citations",
            CITATION_THRESHOLD,
        )
        .expect("insert proposal");
        accept_skill_proposal(&conn, &proposal_id, "/skills/always-run-fmt").expect("accept");

        assert!(
            graduation_notice(&conn).is_none(),
            "an installed skill is a closed loop"
        );
    }

    #[test]
    fn below_threshold_not_a_candidate() {
        let conn = init_conn();

        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score)
             VALUES ('cold-mem', 'project', 'convention', 'Run tests', 'run tests',
                     0.9, '{}', '2026-01-01T00:00:00Z', 2, 2.0)",
            [],
        )
        .expect("insert memory");

        // Only 2 distinct runs — below CITATION_THRESHOLD.
        for run_id in ["run-a", "run-b"] {
            conn.execute(
                "INSERT INTO memory_citations (run_id, memory_id, turn, cited_at)
                 VALUES (?1, 'cold-mem', 1, '2026-01-01T00:00:00Z')",
                params![run_id],
            )
            .expect("insert citation");
        }

        let candidates = find_synthesis_candidates(&conn).expect("find");
        assert!(
            !candidates.iter().any(|c| c.memory_id == "cold-mem"),
            "cold-mem must NOT be a candidate (only 2 citations, threshold=3)"
        );
    }

    #[test]
    fn superseded_memory_excluded_from_candidates() {
        let conn = init_conn();

        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                superseded_by)
             VALUES ('super-mem', 'project', 'convention', 'Old lesson', 'old lesson',
                     0.9, '{}', '2026-01-01T00:00:00Z', 3, 3.0, 'survivor-mem')",
            [],
        )
        .expect("insert superseded memory");

        for run_id in ["run-x", "run-y", "run-z"] {
            conn.execute(
                "INSERT INTO memory_citations (run_id, memory_id, turn, cited_at)
                 VALUES (?1, 'super-mem', 1, '2026-01-01T00:00:00Z')",
                params![run_id],
            )
            .expect("insert citation");
        }

        let candidates = find_synthesis_candidates(&conn).expect("find");
        assert!(
            !candidates.iter().any(|c| c.memory_id == "super-mem"),
            "superseded memory must not be a candidate"
        );
    }

    // -----------------------------------------------------------------------
    // Staleness check (2.4)
    // -----------------------------------------------------------------------

    #[test]
    fn staleness_check_flags_superseded_source() {
        let conn = init_conn();

        // Insert a live memory (source) and an accepted skill proposal that
        // references it.
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                superseded_by)
             VALUES ('stale-src', 'project', 'convention', 'Old',  'old',
                     0.9, '{}', '2026-01-01T00:00:00Z', 1, 1.0, 'other-mem')",
            [],
        )
        .expect("insert stale source");

        let proposal_id = insert_skill_proposal(
            &conn,
            "stale-skill",
            "Uses stale source",
            Some("# Stale Skill"),
            &["stale-src".to_string()],
            "citations",
            3,
        )
        .expect("insert proposal");
        accept_skill_proposal(&conn, &proposal_id, "/tmp/stale-skill").expect("accept");

        let reports = check_staleness(&conn).expect("staleness");
        let stale = reports.iter().find(|r| r.proposal_id == proposal_id);
        assert!(stale.is_some(), "proposal must appear in staleness report");
        let stale = stale.unwrap();
        assert!(
            stale.is_stale,
            "skill with superseded source must be flagged stale"
        );
        assert!(
            stale.stale_memory_ids.contains(&"stale-src".to_string()),
            "stale-src must be in stale_memory_ids"
        );
    }

    #[test]
    fn staleness_check_ok_for_live_source() {
        let conn = init_conn();

        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score)
             VALUES ('live-src', 'project', 'convention', 'Current lesson', 'current lesson',
                     0.9, '{}', '2026-01-01T00:00:00Z', 3, 3.0)",
            [],
        )
        .expect("insert live source");

        let proposal_id = insert_skill_proposal(
            &conn,
            "live-skill",
            "Uses live source",
            Some("# Live Skill"),
            &["live-src".to_string()],
            "citations",
            3,
        )
        .expect("insert proposal");
        accept_skill_proposal(&conn, &proposal_id, "/tmp/live-skill").expect("accept");

        let reports = check_staleness(&conn).expect("staleness");
        let report = reports.iter().find(|r| r.proposal_id == proposal_id);
        assert!(report.is_some(), "proposal must appear in staleness report");
        let report = report.unwrap();
        assert!(
            !report.is_stale,
            "skill with live source must NOT be flagged stale"
        );
    }
}
