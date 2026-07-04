//! Brain maintenance: prune, compact, projection rebuild, lock clearing.
//! Split out of `project.rs` (v2.5.1); re-exported by [`crate::project`].

use std::fs;
use std::path::Path;

use kimetsu_core::KimetsuResult;
use kimetsu_core::paths::ProjectPaths;
use rusqlite::params;

use crate::lock::ProjectLock;
use crate::project::*;
use crate::projector;
use crate::trace::{self};

/// MP-6: bulk prune of memories whose outcome-attribution data says they
/// are net-negative. Selection rules:
///   use_count >= min_uses
///   usefulness_score / use_count <= max_ratio
///   invalidated_at IS NULL
///   scope filter optional
///
/// `apply = false` is the default at the CLI layer so the user sees
/// what would be touched before any writes. `apply = true` invalidates
/// each match via the existing `invalidate_memory` path so every
/// removal still emits a canonical `memory.invalidated` event.
#[derive(Debug, Clone)]
pub struct PruneOptions {
    pub scope: Option<String>,
    pub min_uses: u32,
    pub max_ratio: f32,
    pub apply: bool,
}

impl Default for PruneOptions {
    fn default() -> Self {
        Self {
            scope: None,
            min_uses: 3,
            max_ratio: -0.2,
            apply: false,
        }
    }
}

#[derive(Debug, Clone)]
pub struct PruneCandidate {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    pub use_count: u32,
    pub usefulness_score: f32,
    pub text: String,
}

#[derive(Debug, Clone, Default)]
pub struct PruneSummary {
    pub candidates: Vec<PruneCandidate>,
    pub invalidated: u32,
    pub failed: u32,
}

pub fn prune_low_usefulness(start: &Path, opts: PruneOptions) -> KimetsuResult<PruneSummary> {
    let min_uses = opts.min_uses.max(1) as i64;

    let candidates = {
        let (_paths, _config, conn) = load_project(start)?;
        let (sql, scope_param): (&str, Option<String>) = if let Some(scope) = opts.scope.as_deref()
        {
            (
                "
                SELECT memory_id, scope, kind, text, use_count, usefulness_score
                FROM memories
                WHERE invalidated_at IS NULL
                  AND superseded_by IS NULL
                  AND use_count >= ?1
                  AND (usefulness_score / CAST(use_count AS REAL)) <= ?2
                  AND lower(scope) = lower(?3)
                ORDER BY (usefulness_score / CAST(use_count AS REAL)) ASC
                ",
                Some(scope.to_string()),
            )
        } else {
            (
                "
                SELECT memory_id, scope, kind, text, use_count, usefulness_score
                FROM memories
                WHERE invalidated_at IS NULL
                  AND superseded_by IS NULL
                  AND use_count >= ?1
                  AND (usefulness_score / CAST(use_count AS REAL)) <= ?2
                ORDER BY (usefulness_score / CAST(use_count AS REAL)) ASC
                ",
                None,
            )
        };
        let mut stmt = conn.prepare(sql)?;
        let max_ratio = opts.max_ratio as f64;
        let mut found: Vec<PruneCandidate> = if let Some(scope) = scope_param {
            stmt.query_map(params![min_uses, max_ratio, scope], |row| {
                Ok(PruneCandidate {
                    memory_id: row.get(0)?,
                    scope: row.get(1)?,
                    kind: row.get(2)?,
                    text: row.get(3)?,
                    use_count: row.get(4)?,
                    usefulness_score: row.get::<_, f64>(5)? as f32,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        } else {
            stmt.query_map(params![min_uses, max_ratio], |row| {
                Ok(PruneCandidate {
                    memory_id: row.get(0)?,
                    scope: row.get(1)?,
                    kind: row.get(2)?,
                    text: row.get(3)?,
                    use_count: row.get(4)?,
                    usefulness_score: row.get::<_, f64>(5)? as f32,
                })
            })?
            .collect::<Result<Vec<_>, _>>()?
        };
        // Stable tie-break: lowest ratio first, then highest use_count
        // first (penalize the long-running underperformers).
        found.sort_by(|a, b| {
            let ra = a.usefulness_score as f64 / a.use_count.max(1) as f64;
            let rb = b.usefulness_score as f64 / b.use_count.max(1) as f64;
            ra.partial_cmp(&rb)
                .unwrap_or(std::cmp::Ordering::Equal)
                .then_with(|| b.use_count.cmp(&a.use_count))
        });
        found
    };

    let mut summary = PruneSummary {
        candidates: candidates.clone(),
        invalidated: 0,
        failed: 0,
    };
    if !opts.apply {
        return Ok(summary);
    }

    for candidate in &candidates {
        let ratio = candidate.usefulness_score / candidate.use_count.max(1) as f32;
        let reason = format!(
            "pruned_by_usefulness ratio={:+.2} use_count={}",
            ratio, candidate.use_count
        );
        match invalidate_memory(start, &candidate.memory_id, Some(&reason)) {
            Ok(()) => summary.invalidated += 1,
            Err(_) => summary.failed += 1,
        }
    }
    Ok(summary)
}

pub fn rebuild_projection(start: &Path, from_traces: bool) -> KimetsuResult<usize> {
    let (paths, _config, conn) = load_project(start)?;
    let _lock = ProjectLock::acquire(&paths, "brain rebuild", None)?;

    // Explicit legacy import: rebuild from on-disk trace.jsonl files (inserts
    // any events missing from the table via OR IGNORE, then projects).
    if from_traces {
        let events = trace::read_all_traces(&paths)?;
        projector::rebuild(&conn, &events)?;
        return Ok(events.len());
    }

    // Auto-fallback: a brain whose events table was wiped by a pre-W1.1 rebuild
    // still has its history only in trace.jsonl. If the table is empty but
    // traces exist, import them first, then proceed.
    let event_count: i64 = conn.query_row("SELECT COUNT(*) FROM events", [], |r| r.get(0))?;
    if event_count == 0 {
        let events = trace::read_all_traces(&paths)?;
        if !events.is_empty() {
            eprintln!(
                "[kimetsu] events table empty; importing {} event(s) from legacy traces",
                events.len()
            );
            projector::rebuild(&conn, &events)?;
            return Ok(events.len());
        }
    }

    // Normal path: replay the durable events table in place.
    projector::rebuild_in_place(&conn)
}

pub fn clear_lock(start: &Path) -> KimetsuResult<bool> {
    let paths = ProjectPaths::discover(start)?;
    crate::lock::clear_force(&paths)
}

// ── Q8: brain compact ────────────────────────────────────────────────────────

/// Report returned by [`compact_brain`] describing what was freed.
#[derive(Debug, Clone, serde::Serialize)]
pub struct CompactReport {
    /// brain.db file size in bytes before compaction.
    pub bytes_before: u64,
    /// brain.db file size in bytes after compaction (WAL checkpointed first).
    pub bytes_after: u64,
    /// Number of events deleted by `--trim-events-older-than` (0 when not requested).
    pub events_trimmed: u64,
    /// Number of invalidated memory rows purged (0 when not requested).
    pub invalidated_memories_purged: u64,
}

/// Reclaim dead space in brain.db.
///
/// 1. Acquires the project lock (same as `rebuild_projection`).
/// 2. Optionally purges invalidated memory rows (`purge_invalidated`).
/// 3. Optionally trims old events (`trim_events_older_than`).
/// 4. Runs `VACUUM` (outside any transaction) to rebuild the file in-place.
/// 5. Checkpoints the WAL before measuring `bytes_after` so the measurement
///    reflects the on-disk file, not the shadow WAL.
pub fn compact_brain(
    start: &Path,
    trim_events_older_than: Option<std::time::Duration>,
    purge_invalidated: bool,
) -> KimetsuResult<CompactReport> {
    let (paths, _config, conn) = load_project(start)?;
    let _lock = ProjectLock::acquire(&paths, "brain compact", None)?;

    // Step 2: record bytes_before.
    let bytes_before = fs::metadata(&paths.brain_db).map(|m| m.len()).unwrap_or(0);

    // Step 3: purge invalidated memories (optional, gated by caller).
    let invalidated_memories_purged = if purge_invalidated {
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NOT NULL",
            [],
            |r| r.get(0),
        )?;
        conn.execute_batch(
            "DELETE FROM memories_fts WHERE memory_id IN (
                 SELECT memory_id FROM memories WHERE invalidated_at IS NOT NULL
             );
             DELETE FROM memories WHERE invalidated_at IS NOT NULL;",
        )?;
        count as u64
    } else {
        0
    };

    // Step 4: trim old events (optional, gated by caller).
    let events_trimmed = if let Some(dur) = trim_events_older_than {
        // Compute the cutoff as an RFC 3339 string (UTC) so it compares
        // correctly against the TEXT `ts` column.
        let cutoff_secs = dur.as_secs();
        let now_unix = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_secs())
            .unwrap_or(0);
        let cutoff_unix = now_unix.saturating_sub(cutoff_secs);
        // Format as a naive UTC RFC 3339 string (matches the stored format).
        let cutoff_rfc3339 = {
            let secs = cutoff_unix as i64;
            // Use the `time` crate (already a dependency of projector.rs).
            use time::OffsetDateTime;
            use time::format_description::well_known::Rfc3339;
            OffsetDateTime::from_unix_timestamp(secs)
                .map_err(|e| format!("compact_brain: invalid cutoff timestamp: {e}"))?
                .format(&Rfc3339)
                .map_err(|e| format!("compact_brain: failed to format cutoff: {e}"))?
        };
        let count: i64 = conn.query_row(
            "SELECT COUNT(*) FROM events WHERE ts < ?1",
            rusqlite::params![cutoff_rfc3339],
            |r| r.get(0),
        )?;
        conn.execute(
            "DELETE FROM events WHERE ts < ?1",
            rusqlite::params![cutoff_rfc3339],
        )?;
        count as u64
    } else {
        0
    };

    // Step 5: VACUUM — must run outside any active transaction.
    // `rusqlite::Connection` does not hold an implicit transaction here so
    // execute_batch is safe.
    conn.execute_batch("VACUUM;")?;

    // Step 6: Checkpoint the WAL so bytes_after reflects the real file size
    // (on systems without WAL mode this is a no-op).
    conn.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")?;

    let bytes_after = fs::metadata(&paths.brain_db).map(|m| m.len()).unwrap_or(0);

    Ok(CompactReport {
        bytes_before,
        bytes_after,
        events_trimmed,
        invalidated_memories_purged,
    })
}
