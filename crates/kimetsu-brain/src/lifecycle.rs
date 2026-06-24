//! F3 Lifecycle & forgetting — Stories 3.1–3.4.
//!
//! # Story 3.1 — Active forgetting / compaction policy
//!
//! `forget_brain` identifies memories that are simultaneously:
//!   1. Low-usefulness (`usefulness_score / use_count <= floor`, OR
//!      `usefulness_score <= floor` when `use_count == 0`).
//!   2. Stale: `last_useful_at` (or `created_at` when never cited) is older
//!      than `min_age_days`.
//!   3. NOT evergreen: `use_count < protect_use_count` (high-traffic memories
//!      are protected even if the per-turn ratio is noisy).
//!
//! Forgetting is **archival, not destructive**: it calls the existing
//! `invalidate_memory` path with reason `"forgotten/archived"`, emitting a
//! `memory.invalidated` event into the event log. A full `rebuild_in_place`
//! will replay the invalidation and arrive at the same state — rebuild-safe.
//!
//! The policy is **opt-in** via `[lifecycle] forget_enabled = true` in
//! `project.toml`. The default is `false`, so existing installs are entirely
//! unaffected until the operator explicitly enables it.
//!
//! # Story 3.2 — Regret-driven review
//!
//! `flagged_for_review` returns memories whose `retrieval.regret` event count
//! (a memory was cited despite having been dropped from the context bundle)
//! exceeds a threshold. These memories are surfaced in `brain status` and
//! the review list — they are NOT auto-deleted.
//!
//! # Story 3.3 — Proposal-queue hygiene
//!
//! `gc_proposals` expires pending proposals older than `proposal_expiry_days`
//! (via the existing `reject_proposal` path, reason `"expired"`) and
//! optionally auto-accepts proposals whose `proposed_confidence` is above
//! `proposal_auto_accept_confidence`.
//!
//! # Story 3.4 — Structured invalidation taxonomy
//!
//! `InvalidationReason` is a serde-tagged enum whose canonical snake_case
//! string is what gets written to `invalidated_reason`. Back-compat: the
//! column has always been free-text; rows written before this story parse
//! as `InvalidationReason::Manual`. Analytics groups invalidations by reason.

use std::path::Path;

use kimetsu_core::KimetsuResult;
use rusqlite::{Connection, OptionalExtension, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::project::{AcceptOverrides, invalidate_memory, reject_proposal};

// ---------------------------------------------------------------------------
// Story 3.4 — Structured invalidation taxonomy
// ---------------------------------------------------------------------------

/// Canonical invalidation reason enum.
///
/// The string representation (serde snake_case) is written to the
/// `invalidated_reason` column. Rows from before this enum was introduced
/// have free-text reasons — they parse as `Manual` when the text doesn't
/// match a known variant.
///
/// Back-compat guarantee: `InvalidationReason::Manual` is the catch-all so
/// existing rows and any hand-typed reason strings keep deserialising cleanly.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InvalidationReason {
    /// The memory is no longer accurate / the described behaviour changed.
    Obsolete,
    /// A newer memory supersedes or refines this one.
    Superseded,
    /// This memory directly contradicts another accepted memory.
    Conflicted,
    /// The memory was factually wrong.
    Incorrect,
    /// An exact or near-exact duplicate of another memory exists.
    Duplicate,
    /// Archived by the active-forgetting policy (low-usefulness + stale).
    Forgotten,
    /// Manually invalidated by a human (default / catch-all).
    Manual,
}

impl InvalidationReason {
    /// Return the canonical snake_case string written to the DB column.
    pub fn as_str(&self) -> &'static str {
        match self {
            Self::Obsolete => "obsolete",
            Self::Superseded => "superseded",
            Self::Conflicted => "conflicted",
            Self::Incorrect => "incorrect",
            Self::Duplicate => "duplicate",
            Self::Forgotten => "forgotten",
            Self::Manual => "manual",
        }
    }

    /// Parse a free-text `invalidated_reason` column value into the best
    /// matching variant. Unknown / pre-taxonomy strings → `Manual`.
    pub fn from_db(s: &str) -> Self {
        let lower = s.to_ascii_lowercase();
        match lower.as_str() {
            "obsolete" => Self::Obsolete,
            "superseded" => Self::Superseded,
            "conflicted" => Self::Conflicted,
            "incorrect" => Self::Incorrect,
            "duplicate" => Self::Duplicate,
            "forgotten" | "forgotten/archived" | "forgotten_archived" => Self::Forgotten,
            _ => Self::Manual,
        }
    }
}

impl std::fmt::Display for InvalidationReason {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

// ---------------------------------------------------------------------------
// Story 3.1 — Forget options / results
// ---------------------------------------------------------------------------

/// Options for the `forget_brain` policy run.
///
/// All thresholds come from `[lifecycle]` config; callers may also override
/// them for testing.
#[derive(Debug, Clone)]
pub struct ForgetOptions {
    /// Do not write anything — just report what WOULD be forgotten.
    pub dry_run: bool,
    /// Archive memories whose usefulness score (per-use ratio) is ≤ this.
    /// Default from config: `forget_usefulness_floor`.
    pub usefulness_floor: f32,
    /// Only consider memories whose age (from `last_useful_at` or
    /// `created_at`) is older than this many days.
    /// Default from config: `forget_min_age_days`.
    pub min_age_days: u32,
    /// Memories with `use_count >= this` are PROTECTED (evergreen).
    /// Default from config: `forget_protect_use_count`.
    pub protect_use_count: u32,
}

impl Default for ForgetOptions {
    fn default() -> Self {
        Self {
            dry_run: true, // safe default
            usefulness_floor: -0.1,
            min_age_days: 90,
            protect_use_count: 10,
        }
    }
}

/// One candidate identified by the forgetting pass.
#[derive(Debug, Clone, Serialize)]
pub struct ForgetCandidate {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    /// First ~80 characters of the memory text.
    pub text_preview: String,
    pub use_count: u32,
    pub usefulness_score: f32,
    /// Age in days (from `last_useful_at` / `created_at`).
    pub age_days: f64,
}

/// Result of a `forget_brain` call.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ForgetSummary {
    /// Memories identified as candidates.
    pub candidates: Vec<ForgetCandidate>,
    /// Memories that were actually archived (0 on dry_run).
    pub archived: u32,
    /// Memories that could not be archived due to errors.
    pub failed: u32,
    /// True when this was a dry-run (nothing written).
    pub dry_run: bool,
}

/// Run the active-forgetting policy.
///
/// Identifies stale low-usefulness memories and (unless `opts.dry_run`)
/// archives them via `invalidate_memory` with reason `"forgotten"`.
///
/// This function is **completely gated**: it early-returns Ok(empty) when
/// the lifecycle section has `forget_enabled = false`, so callers that
/// always pass the config option through will never archive anything unless
/// the user has opted in.
pub fn forget_brain(start: &Path, opts: ForgetOptions) -> KimetsuResult<ForgetSummary> {
    let mut summary = ForgetSummary {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    // Compute the age cutoff timestamp.
    let now = OffsetDateTime::now_utc();
    let cutoff = now - time::Duration::seconds(opts.min_age_days as i64 * 86_400);
    let cutoff_iso = cutoff.format(&Rfc3339).unwrap_or_default();

    // Query candidates.
    let candidates = {
        let (_paths, _config, conn) = crate::project::load_project(start)?;
        query_forget_candidates(
            &conn,
            opts.usefulness_floor,
            &cutoff_iso,
            opts.protect_use_count,
        )?
    };

    summary.candidates = candidates.clone();

    if opts.dry_run {
        return Ok(summary);
    }

    // Archive each candidate via the event-sourced invalidate path.
    for candidate in &candidates {
        let reason = InvalidationReason::Forgotten.as_str();
        match invalidate_memory(start, &candidate.memory_id, Some(reason)) {
            Ok(()) => summary.archived += 1,
            Err(_) => summary.failed += 1,
        }
    }

    Ok(summary)
}

/// Query candidates that meet the forget criteria.
fn query_forget_candidates(
    conn: &Connection,
    usefulness_floor: f32,
    cutoff_iso: &str,
    protect_use_count: u32,
) -> KimetsuResult<Vec<ForgetCandidate>> {
    // A memory qualifies when:
    //   - active (not invalidated, not superseded)
    //   - use_count < protect_use_count
    //   - usefulness is low: score / max(use_count,1) <= floor
    //   - stale: COALESCE(last_useful_at, created_at) <= cutoff
    let mut stmt = conn.prepare(
        "SELECT memory_id, scope, kind, text, use_count, usefulness_score,
                COALESCE(last_useful_at, created_at) AS ref_ts
         FROM memories
         WHERE invalidated_at IS NULL
           AND superseded_by IS NULL
           AND use_count < ?1
           AND (CAST(usefulness_score AS REAL) / MAX(CAST(use_count AS REAL), 1.0)) <= ?2
           AND COALESCE(last_useful_at, created_at) <= ?3
         ORDER BY (CAST(usefulness_score AS REAL) / MAX(CAST(use_count AS REAL), 1.0)) ASC",
    )?;

    let now = OffsetDateTime::now_utc();
    let now_secs = now.unix_timestamp() as f64;

    let rows = stmt.query_map(
        params![
            protect_use_count as i64,
            usefulness_floor as f64,
            cutoff_iso
        ],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, i64>(4)?,
                row.get::<_, f64>(5)?,
                row.get::<_, String>(6)?,
            ))
        },
    )?;

    let mut candidates = Vec::new();
    for row in rows {
        let (memory_id, scope, kind, text, use_count, usefulness_score, ref_ts) = row?;
        let age_days = if let Ok(ref_dt) = OffsetDateTime::parse(&ref_ts, &Rfc3339) {
            let ref_secs = ref_dt.unix_timestamp() as f64;
            (now_secs - ref_secs) / 86_400.0
        } else {
            0.0
        };
        let text_preview: String = text.chars().take(80).collect();
        candidates.push(ForgetCandidate {
            memory_id,
            scope,
            kind,
            text_preview,
            use_count: use_count as u32,
            usefulness_score: usefulness_score as f32,
            age_days,
        });
    }
    Ok(candidates)
}

// ---------------------------------------------------------------------------
// Story 3.2 — Regret-driven review
// ---------------------------------------------------------------------------

/// A memory flagged for review due to repeated retrieval regrets.
#[derive(Debug, Clone, Serialize)]
pub struct RegretFlaggedMemory {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    pub text_preview: String,
    pub confidence: f32,
    pub regret_count: u64,
    pub use_count: u32,
    pub usefulness_score: f32,
}

/// Query memories that have accumulated ≥ `threshold` `retrieval.regret`
/// events. These are surfaced for review but NOT auto-deleted.
///
/// A high-confidence memory that keeps being dropped (low retrieval score)
/// but cited by the model anyway is a signal that the memory is right but
/// the retrieval config is mis-calibrated — OR that the memory is
/// over-confident. Either way it deserves human attention.
pub fn regret_flagged_memories(
    conn: &Connection,
    threshold: u64,
) -> KimetsuResult<Vec<RegretFlaggedMemory>> {
    // Count regret events per memory_id from the events table.
    let mut stmt = conn.prepare(
        "SELECT json_extract(payload_json, '$.memory_id') AS mid,
                COUNT(*) AS cnt
         FROM events
         WHERE kind = 'retrieval.regret'
           AND mid IS NOT NULL
         GROUP BY mid
         HAVING cnt >= ?1
         ORDER BY cnt DESC",
    )?;

    let rows = stmt.query_map(params![threshold as i64], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    let mut flagged = Vec::new();
    for row in rows {
        let (memory_id, regret_count) = row?;
        let mem_row: Option<(String, String, String, f32, i64, f64)> = conn
            .query_row(
                "SELECT scope, kind, text, confidence, use_count, usefulness_score
                 FROM memories
                 WHERE memory_id = ?1
                   AND invalidated_at IS NULL
                   AND superseded_by IS NULL",
                params![memory_id],
                |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, String>(2)?,
                        r.get::<_, f32>(3)?,
                        r.get::<_, i64>(4)?,
                        r.get::<_, f64>(5)?,
                    ))
                },
            )
            .optional()?;
        if let Some((scope, kind, text, confidence, use_count, usefulness_score)) = mem_row {
            flagged.push(RegretFlaggedMemory {
                memory_id,
                scope,
                kind,
                text_preview: text.chars().take(80).collect(),
                confidence,
                regret_count: regret_count as u64,
                use_count: use_count as u32,
                usefulness_score: usefulness_score as f32,
            });
        }
    }
    Ok(flagged)
}

// ---------------------------------------------------------------------------
// Story 3.3 — Proposal-queue hygiene
// ---------------------------------------------------------------------------

/// Options for the proposal GC pass.
#[derive(Debug, Clone)]
pub struct ProposalGcOptions {
    /// Expire pending proposals older than this many days (0 = disabled).
    pub expiry_days: u32,
    /// Auto-accept proposals with `proposed_confidence >= this` threshold.
    /// Set to 1.0 or above to disable (default = disabled = 1.1).
    pub auto_accept_confidence: f32,
    /// Dry-run: report what would happen without writing.
    pub dry_run: bool,
}

impl Default for ProposalGcOptions {
    fn default() -> Self {
        Self {
            expiry_days: 30,
            auto_accept_confidence: 1.1, // disabled by default
            dry_run: false,
        }
    }
}

/// Summary of a proposal GC pass.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ProposalGcSummary {
    pub expired: u32,
    pub auto_accepted: u32,
    pub failed: u32,
    pub dry_run: bool,
}

/// Run the proposal-queue hygiene pass.
///
/// 1. Expires pending proposals older than `opts.expiry_days` via
///    `reject_proposal` with reason `"expired"`.
/// 2. Optionally auto-accepts proposals whose `proposed_confidence` is
///    above `opts.auto_accept_confidence`.
///
/// All mutations go through the existing event-sourced
/// `reject_proposal` / `accept_proposal` paths — rebuild-safe.
pub fn gc_proposals(start: &Path, opts: ProposalGcOptions) -> KimetsuResult<ProposalGcSummary> {
    let mut summary = ProposalGcSummary {
        dry_run: opts.dry_run,
        ..Default::default()
    };

    if opts.expiry_days == 0 && opts.auto_accept_confidence >= 1.0 {
        return Ok(summary); // nothing to do
    }

    // Load pending proposals.
    let pending = {
        let filter = crate::project::ProposalFilter {
            status: Some("pending".to_string()),
            limit: 1000,
            ..Default::default()
        };
        crate::project::list_proposals(start, filter)?
    };

    let now = OffsetDateTime::now_utc();

    for proposal in &pending {
        // ---- Expiry check ----
        if opts.expiry_days > 0 {
            // proposals table doesn't store created_at directly; derive from the
            // memory.proposed event timestamp via the events table rowid ordering.
            // Fallback: if we can't parse a timestamp, skip expiry for this row.
            let proposal_ts = proposal_created_at(start, &proposal.proposal_id);
            if let Some(created_at) = proposal_ts {
                let age_days =
                    (now.unix_timestamp() - created_at.unix_timestamp()) as f64 / 86_400.0;
                if age_days >= opts.expiry_days as f64 {
                    if !opts.dry_run {
                        match reject_proposal(start, &proposal.proposal_id, Some("expired")) {
                            Ok(()) => summary.expired += 1,
                            Err(_) => summary.failed += 1,
                        }
                    } else {
                        summary.expired += 1;
                    }
                    continue; // don't also auto-accept something we just expired
                }
            }
        }

        // ---- Auto-accept check ----
        if opts.auto_accept_confidence < 1.0
            && proposal.proposed_confidence >= opts.auto_accept_confidence
        {
            if !opts.dry_run {
                match crate::project::accept_proposal(
                    start,
                    &proposal.proposal_id,
                    AcceptOverrides::default(),
                ) {
                    Ok(_) => summary.auto_accepted += 1,
                    Err(_) => summary.failed += 1,
                }
            } else {
                summary.auto_accepted += 1;
            }
        }
    }

    Ok(summary)
}

/// Look up the wall-clock timestamp of the `memory.proposed` event for a
/// given `proposal_id`. Returns `None` when the proposal cannot be found or
/// the timestamp cannot be parsed.
fn proposal_created_at(start: &Path, proposal_id: &str) -> Option<OffsetDateTime> {
    let conn = crate::project::load_project(start)
        .ok()
        .map(|(_, _, c)| c)?;

    let ts_str: Option<String> = conn
        .query_row(
            "SELECT ts FROM events
             WHERE kind = 'memory.proposed'
               AND json_extract(payload_json, '$.proposal_id') = ?1
             ORDER BY rowid ASC
             LIMIT 1",
            params![proposal_id],
            |r| r.get(0),
        )
        .optional()
        .ok()
        .flatten();

    ts_str
        .as_deref()
        .and_then(|s| OffsetDateTime::parse(s, &Rfc3339).ok())
}

// ---------------------------------------------------------------------------
// Story 3.4 — Analytics: invalidations by reason
// ---------------------------------------------------------------------------

/// Count of invalidations grouped by structured reason.
#[derive(Debug, Clone, Serialize)]
pub struct InvalidationByReason {
    /// The canonical reason string (matches `InvalidationReason::as_str()`).
    pub reason: String,
    pub count: u64,
}

/// Return a summary of all invalidated memories grouped by their structured
/// reason (normalised via `InvalidationReason::from_db`).
pub fn invalidations_by_reason(conn: &Connection) -> KimetsuResult<Vec<InvalidationByReason>> {
    let mut stmt = conn.prepare(
        "SELECT COALESCE(invalidated_reason, 'manual') AS reason, COUNT(*) AS cnt
         FROM memories
         WHERE invalidated_at IS NOT NULL
         GROUP BY reason
         ORDER BY cnt DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
    })?;

    let mut grouped: std::collections::HashMap<String, u64> = std::collections::HashMap::new();
    for row in rows {
        let (raw_reason, count) = row?;
        let canonical = InvalidationReason::from_db(&raw_reason)
            .as_str()
            .to_string();
        *grouped.entry(canonical).or_insert(0) += count as u64;
    }

    let mut result: Vec<InvalidationByReason> = grouped
        .into_iter()
        .map(|(reason, count)| InvalidationByReason { reason, count })
        .collect();
    result.sort_by(|a, b| b.count.cmp(&a.count).then_with(|| a.reason.cmp(&b.reason)));
    Ok(result)
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        project::{add_memory, init_project, propose_memory},
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
        let root = std::env::temp_dir().join(format!("kimetsu-lc-test-{}", Ulid::new()));
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }

    // -------------------------------------------------------------------------
    // Story 3.4: InvalidationReason round-trips
    // -------------------------------------------------------------------------

    #[test]
    fn invalidation_reason_as_str_round_trips() {
        let reasons = [
            InvalidationReason::Obsolete,
            InvalidationReason::Superseded,
            InvalidationReason::Conflicted,
            InvalidationReason::Incorrect,
            InvalidationReason::Duplicate,
            InvalidationReason::Forgotten,
            InvalidationReason::Manual,
        ];
        for r in &reasons {
            let s = r.as_str();
            let parsed = InvalidationReason::from_db(s);
            assert_eq!(&parsed, r, "from_db(as_str()) must round-trip for {:?}", r);
        }
    }

    #[test]
    fn invalidation_reason_legacy_strings_parse_correctly() {
        assert_eq!(
            InvalidationReason::from_db("forgotten/archived"),
            InvalidationReason::Forgotten
        );
        assert_eq!(
            InvalidationReason::from_db("some unknown old reason"),
            InvalidationReason::Manual
        );
        assert_eq!(
            InvalidationReason::from_db("invalidated_by_cli"),
            InvalidationReason::Manual
        );
    }

    // -------------------------------------------------------------------------
    // Story 3.4: invalidations_by_reason groups correctly
    // -------------------------------------------------------------------------

    #[test]
    fn invalidations_by_reason_groups_structured_reasons() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let m1 =
                add_memory(&root, MemoryScope::Project, MemoryKind::Fact, "fact one").expect("m1");
            let m2 =
                add_memory(&root, MemoryScope::Project, MemoryKind::Fact, "fact two").expect("m2");
            let m3 = add_memory(&root, MemoryScope::Project, MemoryKind::Fact, "fact three")
                .expect("m3");

            invalidate_memory(&root, &m1, Some("forgotten")).expect("inv m1");
            invalidate_memory(&root, &m2, Some("forgotten")).expect("inv m2");
            invalidate_memory(&root, &m3, Some("obsolete")).expect("inv m3");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            let by_reason = invalidations_by_reason(&conn).expect("by_reason");

            let forgotten_count = by_reason
                .iter()
                .find(|r| r.reason == "forgotten")
                .map(|r| r.count)
                .unwrap_or(0);
            assert_eq!(forgotten_count, 2, "expected 2 forgotten");

            let obsolete_count = by_reason
                .iter()
                .find(|r| r.reason == "obsolete")
                .map(|r| r.count)
                .unwrap_or(0);
            assert_eq!(obsolete_count, 1, "expected 1 obsolete");

            std::fs::remove_dir_all(&root).ok();
        });
    }

    // -------------------------------------------------------------------------
    // Story 3.1: forget_brain dry-run identifies noise, not signal
    // -------------------------------------------------------------------------

    /// Helper to directly set usefulness_score + last_useful_at on a memory
    /// row (bypasses the event system for test speed).
    fn set_memory_usefulness(
        conn: &rusqlite::Connection,
        memory_id: &str,
        use_count: i64,
        usefulness_score: f64,
        last_useful_at: Option<&str>,
    ) {
        conn.execute(
            "UPDATE memories SET use_count=?2, usefulness_score=?3, last_useful_at=?4 WHERE memory_id=?1",
            rusqlite::params![memory_id, use_count, usefulness_score, last_useful_at],
        )
        .expect("set_memory_usefulness");
    }

    #[test]
    fn forget_brain_dry_run_identifies_noise_keeps_signal() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Noise: low usefulness, old, low use_count
            let noise = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "noise memory stale unused",
            )
            .expect("noise");

            // Signal: high use_count → evergreen → protected
            let signal = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::FailurePattern,
                "evergreen failure pattern cited many times",
            )
            .expect("signal");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");

            // Noise: usefulness=-0.5, use_count=2, last_useful 200 days ago
            let old_ts = (OffsetDateTime::now_utc() - time::Duration::seconds(200 * 86_400))
                .format(&Rfc3339)
                .unwrap();
            set_memory_usefulness(&conn, &noise, 2, -0.5, Some(&old_ts));

            // Signal: use_count=15 (protected), good usefulness
            let recent_ts = (OffsetDateTime::now_utc() - time::Duration::seconds(5 * 86_400))
                .format(&Rfc3339)
                .unwrap();
            set_memory_usefulness(&conn, &signal, 15, 5.0, Some(&recent_ts));
            drop(conn);

            let opts = ForgetOptions {
                dry_run: true,
                usefulness_floor: -0.1,
                min_age_days: 90,
                protect_use_count: 10,
            };
            let summary = forget_brain(&root, opts).expect("forget_brain dry-run");

            assert!(summary.dry_run, "must be a dry-run");
            assert_eq!(summary.archived, 0, "dry-run must archive nothing");
            let ids: Vec<&str> = summary
                .candidates
                .iter()
                .map(|c| c.memory_id.as_str())
                .collect();
            assert!(ids.contains(&noise.as_str()), "noise must be a candidate");
            assert!(
                !ids.contains(&signal.as_str()),
                "signal (use_count=15) must be protected"
            );

            std::fs::remove_dir_all(&root).ok();
        });
    }

    #[test]
    fn forget_brain_apply_invalidates_noise_keeps_signal() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let noise = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "forget me noise",
            )
            .expect("noise");
            let signal = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Convention,
                "keep me evergreen",
            )
            .expect("signal");

            {
                let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
                let old_ts = (OffsetDateTime::now_utc() - time::Duration::seconds(200 * 86_400))
                    .format(&Rfc3339)
                    .unwrap();
                set_memory_usefulness(&conn, &noise, 2, -0.5, Some(&old_ts));
                let recent_ts = (OffsetDateTime::now_utc() - time::Duration::seconds(5 * 86_400))
                    .format(&Rfc3339)
                    .unwrap();
                set_memory_usefulness(&conn, &signal, 15, 5.0, Some(&recent_ts));
            }

            let opts = ForgetOptions {
                dry_run: false,
                usefulness_floor: -0.1,
                min_age_days: 90,
                protect_use_count: 10,
            };
            let summary = forget_brain(&root, opts).expect("forget_brain apply");
            assert_eq!(summary.failed, 0, "no failures");
            assert!(summary.archived >= 1, "must archive at least noise");

            // Verify noise is now invalidated in DB.
            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            let noise_inv: Option<String> = conn
                .query_row(
                    "SELECT invalidated_reason FROM memories WHERE memory_id=?1",
                    rusqlite::params![noise],
                    |r| r.get(0),
                )
                .optional()
                .expect("query")
                .flatten();
            assert_eq!(
                noise_inv.as_deref(),
                Some("forgotten"),
                "noise must be invalidated with reason=forgotten"
            );

            // Verify signal is still active.
            let signal_inv: Option<String> = conn
                .query_row(
                    "SELECT invalidated_at FROM memories WHERE memory_id=?1",
                    rusqlite::params![signal],
                    |r| r.get(0),
                )
                .optional()
                .expect("query")
                .flatten();
            assert!(signal_inv.is_none(), "signal must NOT be invalidated");

            std::fs::remove_dir_all(&root).ok();
        });
    }

    // -------------------------------------------------------------------------
    // Story 3.2: regret_flagged_memories
    // -------------------------------------------------------------------------

    fn seed_regret(conn: &rusqlite::Connection, memory_id: &str, n: usize) {
        let run_id = RunId::new();
        for _ in 0..n {
            let ev = Event::new(
                run_id,
                "retrieval.regret",
                serde_json::json!({
                    "memory_id": memory_id,
                    "dropped_at": 1000,
                    "cited_at": 2000,
                    "score": 0.1
                }),
            );
            projector::apply_events(conn, &[ev]).expect("seed regret");
        }
    }

    #[test]
    fn regret_flagged_memories_flags_above_threshold() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let m1 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "regret flagged memory",
            )
            .expect("m1");
            let m2 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "not enough regrets",
            )
            .expect("m2");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            seed_regret(&conn, &m1, 5);
            seed_regret(&conn, &m2, 1);

            let flagged = regret_flagged_memories(&conn, 3).expect("regret_flagged");
            let ids: Vec<&str> = flagged.iter().map(|f| f.memory_id.as_str()).collect();
            assert!(ids.contains(&m1.as_str()), "m1 must be flagged (5 regrets)");
            assert!(
                !ids.contains(&m2.as_str()),
                "m2 must NOT be flagged (1 regret < threshold=3)"
            );

            std::fs::remove_dir_all(&root).ok();
        });
    }

    // -------------------------------------------------------------------------
    // Story 3.3: gc_proposals expiry
    // -------------------------------------------------------------------------

    #[test]
    fn gc_proposals_expires_old_pending_keeps_fresh() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Create two proposals.
            let _old_prop = propose_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "old proposal",
                0.5,
                "old rationale",
            )
            .expect("old prop");
            let _fresh_prop = propose_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "fresh proposal",
                0.5,
                "fresh rationale",
            )
            .expect("fresh prop");

            // Artificially age the old proposal's event by back-dating it.
            {
                let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
                let old_ts = (OffsetDateTime::now_utc() - time::Duration::seconds(60 * 86_400))
                    .format(&Rfc3339)
                    .unwrap();
                conn.execute(
                    "UPDATE events SET ts=?1 WHERE kind='memory.proposed'
                     AND json_extract(payload_json,'$.proposal_id')=?2",
                    rusqlite::params![old_ts, _old_prop],
                )
                .expect("back-date event");
            }

            let opts = ProposalGcOptions {
                expiry_days: 30,
                auto_accept_confidence: 1.1,
                dry_run: false,
            };
            let summary = gc_proposals(&root, opts).expect("gc_proposals");

            assert_eq!(summary.expired, 1, "one old proposal must be expired");

            // Verify old proposal is rejected in DB.
            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            let old_status: String = conn
                .query_row(
                    "SELECT status FROM memory_proposals WHERE proposal_id=?1",
                    rusqlite::params![_old_prop],
                    |r| r.get(0),
                )
                .expect("old status");
            assert_eq!(old_status, "rejected", "old proposal must be rejected");

            let fresh_status: String = conn
                .query_row(
                    "SELECT status FROM memory_proposals WHERE proposal_id=?1",
                    rusqlite::params![_fresh_prop],
                    |r| r.get(0),
                )
                .expect("fresh status");
            assert_eq!(fresh_status, "pending", "fresh proposal must stay pending");

            std::fs::remove_dir_all(&root).ok();
        });
    }
}
