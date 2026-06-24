//! C1–C4: read-only proof-of-value analytics.
//!
//! `compute_insights` opens the project brain (via `load_project`, which
//! migrates on the way through) and runs a set of read-only SQL queries to
//! produce an `InsightsReport`. No writes are performed.
//!
//! Metrics left as `None` pending C7 (`context.served` events):
//!   - `RetrievalStats::hit_rate` / `with_hit` / `avg_top_score`
//!   - `TokenEconomy::skip_rate`

use std::path::Path;

use kimetsu_core::KimetsuResult;
use rusqlite::{OptionalExtension, params};
use serde::Serialize;

// ---------------------------------------------------------------------------
// Public option / report types
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct InsightsOptions {
    /// Number of most-recent runs to include in the rolling window.
    /// Default 50 when set to 0.
    pub last_n_runs: u32,
    /// ISO-8601 lower bound on `runs.started_at`. When set, overrides
    /// `last_n_runs`.
    pub since: Option<String>,
    /// How many items to include in ranked lists (top_useful,
    /// prune_candidates). Default 10 when set to 0.
    pub top_n: u32,
}

impl Default for InsightsOptions {
    fn default() -> Self {
        Self {
            last_n_runs: 50,
            since: None,
            top_n: 10,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct InsightsReport {
    pub retrieval: RetrievalStats,
    pub citation: CitationStats,
    pub proposals: ProposalStats,
    pub usefulness: UsefulnessTrend,
    pub harvest: HarvestStats,
    pub corpus: CorpusHealth,
    pub token_economy: TokenEconomy,
}

/// Lightweight memory reference for ranked lists.
#[derive(Debug, Clone, Serialize)]
pub struct MemoryRef {
    pub memory_id: String,
    pub text_preview: String,
    pub usefulness_score: f32,
    pub use_count: u32,
}

/// C1 — retrieval hit-rate. Populated once C7 lands `context.served` events;
/// all fields are stub values / `None` for now.
#[derive(Debug, Clone, Serialize)]
pub struct RetrievalStats {
    /// Total `context.served` events in the window (C7).
    pub served: u64,
    /// Served events whose bundle contained ≥1 capsule (C7).
    pub with_hit: u64,
    /// `with_hit / served`; `None` until C7 populates served.
    pub hit_rate: Option<f64>,
    /// Average `top_score` across served events (C7).
    pub avg_top_score: Option<f64>,
}

/// C4 — citation signal: what fraction of retrieved memories were actually
/// cited by the model?
#[derive(Debug, Clone, Serialize)]
pub struct CitationStats {
    /// Runs in the window with at least one `context.injected` event.
    pub runs_considered: u32,
    /// Distinct memory_ids surfaced across all `context.injected` events in
    /// those runs (from the `memory_ids` JSON array).
    pub retrieved_total: u64,
    /// Distinct memory_ids cited via `memory.cited` events in those runs.
    pub cited_total: u64,
    /// `cited_total / retrieved_total`; `None` when retrieved_total == 0.
    pub citation_rate: Option<f64>,
}

/// C2 — proposal acceptance funnel.
#[derive(Debug, Clone, Serialize)]
pub struct ProposalStats {
    pub accepted: u64,
    pub rejected: u64,
    pub pending: u64,
    /// `accepted / (accepted + rejected)`; `None` when denom == 0.
    pub acceptance_rate: Option<f64>,
}

/// C3 — memory usefulness distribution and run-outcome trend.
#[derive(Debug, Clone, Serialize)]
pub struct UsefulnessTrend {
    /// `SUM(usefulness_score)` across all active memories.
    pub sum_usefulness: f64,
    /// `AVG(usefulness_score / use_count)` over active rows with
    /// `use_count > 0`; `None` when no such rows exist.
    pub avg_ratio: Option<f64>,
    /// `run.finished` events in the window.
    pub window_finished: u64,
    /// `run.failed` events in the window whose payload `category != "Gate"`.
    pub window_failed_nongate: u64,
    /// `window_finished − window_failed_nongate`.
    pub window_net: i64,
}

/// C3 — memory harvest yield.
#[derive(Debug, Clone, Serialize)]
pub struct HarvestStats {
    /// Memories whose `created_at` falls inside the window.
    pub created_in_window: u64,
    /// Breakdown by `json_extract(provenance_snapshot_json, '$.source')`.
    pub by_source: Vec<(String, u64)>,
    /// `created_in_window / distinct_runs_in_window`; `None` when 0 runs.
    pub yield_per_run: Option<f64>,
}

/// C2 — corpus health snapshot.
#[derive(Debug, Clone, Serialize)]
pub struct CorpusHealth {
    pub active: u64,
    pub invalidated: u64,
    pub by_scope: Vec<(String, u64)>,
    pub by_kind: Vec<(String, u64)>,
    pub top_useful: Vec<MemoryRef>,
    pub prune_candidates: Vec<MemoryRef>,
    pub open_conflicts: u64,
    pub pending_proposals: u64,
    /// F3 Story 3.4: invalidations grouped by structured reason.
    /// Empty when no memories have been invalidated.
    pub invalidations_by_reason: Vec<(String, u64)>,
    /// F3 Story 3.2: memories flagged for review due to repeated retrieval regrets.
    /// Only populated when there are memories above the regret threshold.
    pub regret_flagged_count: u64,
}

/// C4 — token economy from `context.injected` events.
#[derive(Debug, Clone, Serialize)]
pub struct TokenEconomy {
    /// Average `used_tokens` across `context.injected` events in the window
    /// that carry the field. `None` when no event carries it (old-style).
    pub avg_injected_tokens: Option<f64>,
    /// Average `capsule_count` across events in the window that carry the
    /// field. `None` when no event carries it.
    pub avg_capsules: Option<f64>,
    /// Skip-rate (skipped / served); `None` until C7 adds `context.served`.
    pub skip_rate: Option<f64>,
    /// F3 — overhead ratio: brain-injected tokens ÷ total run tokens, averaged
    /// over runs in the window that carry both `context.injected` `used_tokens`
    /// AND `run.finished` `total_prompt_tokens` (or equivalent).
    ///
    /// Currently `None` because the pipeline does not yet record
    /// `total_prompt_tokens` in `run.finished` events. When that field is
    /// added, this metric will be computed from it. The fixture-level
    /// guarantee (brain overhead ratio falls on larger tasks) is proven in
    /// the `f3_overhead_ratio_falls_on_larger_task` unit test in
    /// `kimetsu_agent::pipeline` using `adaptive_budget` + simulated totals.
    pub overhead_ratio: Option<f64>,
}

// ---------------------------------------------------------------------------
// Entry point
// ---------------------------------------------------------------------------

pub fn compute_insights(start: &Path, opts: InsightsOptions) -> KimetsuResult<InsightsReport> {
    let (_paths, _config, conn) = crate::project::load_project(start)?;

    let last_n = if opts.last_n_runs == 0 {
        50u32
    } else {
        opts.last_n_runs
    };
    let top_n = if opts.top_n == 0 { 10u32 } else { opts.top_n };

    // Determine the window boundary. When `since` is set use that timestamp;
    // otherwise derive it from the N most-recent runs by started_at DESC.
    let window_since: Option<String> = if let Some(ref ts) = opts.since {
        Some(ts.clone())
    } else {
        conn.query_row(
            "SELECT started_at FROM runs ORDER BY started_at DESC LIMIT 1 OFFSET ?1",
            params![last_n as i64 - 1],
            |row| row.get::<_, String>(0),
        )
        .optional()?
    };

    // -----------------------------------------------------------------------
    // C1/C7 — RetrievalStats from context.served events.
    //
    // Hook-emitted events have run_id = all-zero ULID (sentinel "hook"),
    // so we do NOT filter by run_id-in-window. Instead we filter by the
    // event's ts column, which is always the real wall-clock time of the
    // hook call. Pipeline events are also included (their ts falls in the
    // same window). The window_since bound applies uniformly.
    // -----------------------------------------------------------------------
    let retrieval = {
        // Total context.served events in the window.
        let served: u64 = match &window_since {
            Some(ts) => conn.query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE kind = 'context.served' AND ts >= ?1",
                params![ts],
                |row| row.get(0),
            )?,
            None => conn.query_row(
                "SELECT COUNT(*) FROM events WHERE kind = 'context.served'",
                [],
                |row| row.get(0),
            )?,
        };

        // with_hit: capsule_count >= 1 AND skipped = false (JSON values).
        // Treat missing/null fields defensively (old-style events fall through
        // to 0/null → excluded from with_hit, which is correct).
        let with_hit: u64 = match &window_since {
            Some(ts) => conn.query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE kind = 'context.served' AND ts >= ?1 \
                   AND CAST(COALESCE(json_extract(payload_json,'$.capsule_count'),0) AS INTEGER) >= 1 \
                   AND COALESCE(json_extract(payload_json,'$.skipped'),'false') != 'true' \
                   AND COALESCE(json_extract(payload_json,'$.skipped'),0) != 1",
                params![ts],
                |row| row.get(0),
            )?,
            None => conn.query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE kind = 'context.served' \
                   AND CAST(COALESCE(json_extract(payload_json,'$.capsule_count'),0) AS INTEGER) >= 1 \
                   AND COALESCE(json_extract(payload_json,'$.skipped'),'false') != 'true' \
                   AND COALESCE(json_extract(payload_json,'$.skipped'),0) != 1",
                [],
                |row| row.get(0),
            )?,
        };

        let hit_rate = if served > 0 {
            Some(with_hit as f64 / served as f64)
        } else {
            None
        };

        // avg_top_score over events where capsule_count >= 1 (hits only).
        let avg_top_score: Option<f64> = match &window_since {
            Some(ts) => conn
                .query_row(
                    "SELECT AVG(CAST(json_extract(payload_json,'$.top_score') AS REAL)) \
                     FROM events \
                     WHERE kind = 'context.served' AND ts >= ?1 \
                       AND CAST(COALESCE(json_extract(payload_json,'$.capsule_count'),0) AS INTEGER) >= 1",
                    params![ts],
                    |row| row.get::<_, Option<f64>>(0),
                )
                .optional()?
                .flatten(),
            None => conn
                .query_row(
                    "SELECT AVG(CAST(json_extract(payload_json,'$.top_score') AS REAL)) \
                     FROM events \
                     WHERE kind = 'context.served' \
                       AND CAST(COALESCE(json_extract(payload_json,'$.capsule_count'),0) AS INTEGER) >= 1",
                    [],
                    |row| row.get::<_, Option<f64>>(0),
                )
                .optional()?
                .flatten(),
        };

        RetrievalStats {
            served,
            with_hit,
            hit_rate,
            avg_top_score,
        }
    };

    // -----------------------------------------------------------------------
    // C2 — ProposalStats
    // -----------------------------------------------------------------------
    let proposals = {
        let mut accepted: u64 = 0;
        let mut rejected: u64 = 0;
        let mut pending: u64 = 0;
        let mut stmt =
            conn.prepare("SELECT status, COUNT(*) FROM memory_proposals GROUP BY status")?;
        let rows = stmt.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
        })?;
        for row in rows {
            let (status, count) = row?;
            match status.as_str() {
                "accepted" => accepted = count,
                "rejected" => rejected = count,
                "pending" => pending = count,
                _ => {}
            }
        }
        let acceptance_rate = if accepted + rejected > 0 {
            Some(accepted as f64 / (accepted + rejected) as f64)
        } else {
            None
        };
        ProposalStats {
            accepted,
            rejected,
            pending,
            acceptance_rate,
        }
    };

    // -----------------------------------------------------------------------
    // C2 — CorpusHealth
    // -----------------------------------------------------------------------
    let corpus = {
        // active vs invalidated.
        // "Active" = not invalidated AND not superseded (superseded rows are
        // retired by consolidation and excluded from retrieval, so including
        // them in health counts would disagree with what users actually see).
        let active: u64 = conn.query_row(
            "SELECT COUNT(*) FROM memories \
             WHERE invalidated_at IS NULL AND superseded_by IS NULL",
            [],
            |row| row.get(0),
        )?;
        let invalidated: u64 = conn.query_row(
            "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NOT NULL",
            [],
            |row| row.get(0),
        )?;

        // by_scope — active only (same superseded_by filter)
        let by_scope: Vec<(String, u64)> = {
            let mut stmt = conn.prepare(
                "SELECT scope, COUNT(*) FROM memories \
                 WHERE invalidated_at IS NULL AND superseded_by IS NULL \
                 GROUP BY scope ORDER BY COUNT(*) DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        // by_kind — active only
        let by_kind: Vec<(String, u64)> = {
            let mut stmt = conn.prepare(
                "SELECT kind, COUNT(*) FROM memories \
                 WHERE invalidated_at IS NULL AND superseded_by IS NULL \
                 GROUP BY kind ORDER BY COUNT(*) DESC",
            )?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        // top_useful: reuse list_memories_top (it opens its own project conn).
        // We already have `conn`, so run the same SQL directly to avoid a
        // second load_project call.
        let top_useful: Vec<MemoryRef> = {
            let limit = top_n as i64;
            let mut stmt = conn.prepare(
                "
                SELECT memory_id, text, usefulness_score, use_count
                FROM memories
                WHERE invalidated_at IS NULL
                  AND superseded_by IS NULL
                  AND use_count >= 1
                ORDER BY (usefulness_score / CAST(use_count AS REAL)) DESC, use_count DESC
                LIMIT ?1
                ",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                let text: String = row.get(1)?;
                let preview = text_preview(&text, 120);
                Ok(MemoryRef {
                    memory_id: row.get(0)?,
                    text_preview: preview,
                    usefulness_score: row.get::<_, f64>(2)? as f32,
                    use_count: row.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        // prune_candidates: use the same SQL as prune_low_usefulness dry-run.
        let prune_candidates: Vec<MemoryRef> = {
            let limit = top_n as i64;
            let mut stmt = conn.prepare(
                "
                SELECT memory_id, text, usefulness_score, use_count
                FROM memories
                WHERE invalidated_at IS NULL
                  AND superseded_by IS NULL
                  AND use_count >= 3
                  AND (usefulness_score / CAST(use_count AS REAL)) <= -0.2
                ORDER BY (usefulness_score / CAST(use_count AS REAL)) ASC
                LIMIT ?1
                ",
            )?;
            let rows = stmt.query_map(params![limit], |row| {
                let text: String = row.get(1)?;
                let preview = text_preview(&text, 120);
                Ok(MemoryRef {
                    memory_id: row.get(0)?,
                    text_preview: preview,
                    usefulness_score: row.get::<_, f64>(2)? as f32,
                    use_count: row.get(3)?,
                })
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        // open_conflicts via list_conflicts (counts project + user brain)
        // We only have the project conn here; call list_conflicts which opens
        // both. Since list_conflicts needs `start`, we use a count query on
        // the project conn (user-brain conflicts are rare; analytics is
        // best-effort here).
        let open_conflicts: u64 = conn.query_row(
            "SELECT COUNT(*) FROM memory_conflicts WHERE resolved_at IS NULL",
            [],
            |row| row.get(0),
        )?;

        // pending_proposals
        let pending_proposals: u64 = conn.query_row(
            "SELECT COUNT(*) FROM memory_proposals WHERE status = 'pending'",
            [],
            |row| row.get(0),
        )?;

        // F3 Story 3.4: invalidations by structured reason.
        let invalidations_by_reason: Vec<(String, u64)> =
            crate::lifecycle::invalidations_by_reason(&conn)
                .unwrap_or_default()
                .into_iter()
                .map(|r| (r.reason, r.count))
                .collect();

        // F3 Story 3.2: count regret-flagged memories.
        // Use the default threshold from config (5); analytics always uses the
        // default since it reads from DB and doesn't take a lifecycle config arg.
        let regret_flag_threshold = 5u64;
        let regret_flagged_count =
            crate::lifecycle::regret_flagged_memories(&conn, regret_flag_threshold)
                .map(|v| v.len() as u64)
                .unwrap_or(0);

        CorpusHealth {
            active,
            invalidated,
            by_scope,
            by_kind,
            top_useful,
            prune_candidates,
            open_conflicts,
            pending_proposals,
            invalidations_by_reason,
            regret_flagged_count,
        }
    };

    // -----------------------------------------------------------------------
    // C3 — HarvestStats
    // -----------------------------------------------------------------------
    let harvest = {
        // Memories created in the window.
        let created_in_window: u64 = match &window_since {
            Some(ts) => conn.query_row(
                "SELECT COUNT(*) FROM memories WHERE created_at >= ?1",
                params![ts],
                |row| row.get(0),
            )?,
            None => conn.query_row("SELECT COUNT(*) FROM memories", [], |row| row.get(0))?,
        };

        // by_source — group by provenance_snapshot_json $.source
        let by_source: Vec<(String, u64)> = {
            let sql = match &window_since {
                Some(ts) => {
                    format!(
                        "SELECT COALESCE(json_extract(provenance_snapshot_json,'$.source'),'unknown'), COUNT(*) \
                         FROM memories WHERE created_at >= '{}' \
                         GROUP BY json_extract(provenance_snapshot_json,'$.source') \
                         ORDER BY COUNT(*) DESC",
                        ts.replace('\'', "''")
                    )
                }
                None => "SELECT COALESCE(json_extract(provenance_snapshot_json,'$.source'),'unknown'), COUNT(*) \
                          FROM memories \
                          GROUP BY json_extract(provenance_snapshot_json,'$.source') \
                          ORDER BY COUNT(*) DESC"
                    .to_string(),
            };
            let mut stmt = conn.prepare(&sql)?;
            let rows = stmt.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, u64>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };

        // distinct runs in window
        let distinct_runs_in_window: u64 = match &window_since {
            Some(ts) => conn.query_row(
                "SELECT COUNT(*) FROM runs WHERE started_at >= ?1",
                params![ts],
                |row| row.get(0),
            )?,
            None => conn.query_row("SELECT COUNT(*) FROM runs", [], |row| row.get(0))?,
        };

        let yield_per_run = if distinct_runs_in_window > 0 {
            Some(created_in_window as f64 / distinct_runs_in_window as f64)
        } else {
            None
        };

        HarvestStats {
            created_in_window,
            by_source,
            yield_per_run,
        }
    };

    // -----------------------------------------------------------------------
    // C3 — UsefulnessTrend
    // -----------------------------------------------------------------------
    let usefulness = {
        // S4.1: exclude superseded rows from usefulness aggregates so that
        // the numbers align with what retrieval actually surfaces.
        let sum_usefulness: f64 = conn.query_row(
            "SELECT COALESCE(SUM(usefulness_score), 0.0) FROM memories \
             WHERE invalidated_at IS NULL AND superseded_by IS NULL",
            [],
            |row| row.get(0),
        )?;

        let avg_ratio: Option<f64> = conn
            .query_row(
                "SELECT AVG(usefulness_score / CAST(use_count AS REAL)) \
                 FROM memories \
                 WHERE invalidated_at IS NULL AND superseded_by IS NULL AND use_count > 0",
                [],
                |row| row.get::<_, Option<f64>>(0),
            )
            .optional()?
            .flatten();

        // window_finished / window_failed_nongate
        let (window_finished, window_failed_nongate): (u64, u64) = match &window_since {
            Some(ts) => {
                let finished: u64 = conn.query_row(
                    "SELECT COUNT(*) FROM events \
                     WHERE kind = 'run.finished' AND ts >= ?1",
                    params![ts],
                    |row| row.get(0),
                )?;
                // run.failed events whose payload category != 'Gate'
                let failed_nongate: u64 = conn.query_row(
                    "SELECT COUNT(*) FROM events \
                     WHERE kind = 'run.failed' AND ts >= ?1 \
                       AND COALESCE(json_extract(payload_json,'$.category'),'') != 'Gate'",
                    params![ts],
                    |row| row.get(0),
                )?;
                (finished, failed_nongate)
            }
            None => {
                let finished: u64 = conn.query_row(
                    "SELECT COUNT(*) FROM events WHERE kind = 'run.finished'",
                    [],
                    |row| row.get(0),
                )?;
                let failed_nongate: u64 = conn.query_row(
                    "SELECT COUNT(*) FROM events \
                     WHERE kind = 'run.failed' \
                       AND COALESCE(json_extract(payload_json,'$.category'),'') != 'Gate'",
                    [],
                    |row| row.get(0),
                )?;
                (finished, failed_nongate)
            }
        };

        let window_net = window_finished as i64 - window_failed_nongate as i64;

        UsefulnessTrend {
            sum_usefulness,
            avg_ratio,
            window_finished,
            window_failed_nongate,
            window_net,
        }
    };

    // -----------------------------------------------------------------------
    // C4 — CitationStats
    // -----------------------------------------------------------------------
    let citation = {
        // Collect run_ids in window that have at least one context.injected.
        let run_ids_with_injection: Vec<String> = match &window_since {
            Some(ts) => {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT run_id FROM events \
                     WHERE kind = 'context.injected' AND ts >= ?1",
                )?;
                let rows = stmt.query_map(params![ts], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = conn.prepare(
                    "SELECT DISTINCT run_id FROM events WHERE kind = 'context.injected'",
                )?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };

        let runs_considered = run_ids_with_injection.len() as u32;

        // retrieved_total: distinct memory_ids across all context.injected payloads.
        let mut retrieved_set = std::collections::BTreeSet::new();
        for run_id in &run_ids_with_injection {
            let mut stmt = conn.prepare(
                "SELECT payload_json FROM events \
                 WHERE run_id = ?1 AND kind = 'context.injected'",
            )?;
            let rows = stmt.query_map(params![run_id], |row| row.get::<_, String>(0))?;
            for row in rows {
                let payload_json = row?;
                let payload: serde_json::Value = serde_json::from_str(&payload_json)?;
                if let Some(ids) = payload.get("memory_ids").and_then(|v| v.as_array()) {
                    for id in ids {
                        if let Some(s) = id.as_str() {
                            if !s.is_empty() {
                                retrieved_set.insert(s.to_string());
                            }
                        }
                    }
                }
            }
        }
        let retrieved_total = retrieved_set.len() as u64;

        // cited_total: distinct memory_ids from memory_citations for these runs.
        let mut cited_set = std::collections::BTreeSet::new();
        for run_id in &run_ids_with_injection {
            let mut stmt =
                conn.prepare("SELECT DISTINCT memory_id FROM memory_citations WHERE run_id = ?1")?;
            let rows = stmt.query_map(params![run_id], |row| row.get::<_, String>(0))?;
            for row in rows {
                cited_set.insert(row?);
            }
        }
        let cited_total = cited_set.len() as u64;

        let citation_rate = if retrieved_total > 0 {
            Some(cited_total as f64 / retrieved_total as f64)
        } else {
            None
        };

        CitationStats {
            runs_considered,
            retrieved_total,
            cited_total,
            citation_rate,
        }
    };

    // -----------------------------------------------------------------------
    // C4 — TokenEconomy
    // -----------------------------------------------------------------------
    let token_economy = {
        // Collect used_tokens and capsule_count values from context.injected
        // events in the window that carry those fields.
        let injected_payloads: Vec<String> = match &window_since {
            Some(ts) => {
                let mut stmt = conn.prepare(
                    "SELECT payload_json FROM events \
                     WHERE kind = 'context.injected' AND ts >= ?1",
                )?;
                let rows = stmt.query_map(params![ts], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
            None => {
                let mut stmt = conn
                    .prepare("SELECT payload_json FROM events WHERE kind = 'context.injected'")?;
                let rows = stmt.query_map([], |row| row.get::<_, String>(0))?;
                rows.collect::<Result<Vec<_>, _>>()?
            }
        };

        let mut token_sum: f64 = 0.0;
        let mut token_count: u64 = 0;
        let mut capsule_sum: f64 = 0.0;
        let mut capsule_count: u64 = 0;

        for payload_json in &injected_payloads {
            let payload: serde_json::Value = serde_json::from_str(payload_json)?;
            if let Some(t) = payload.get("used_tokens").and_then(|v| v.as_f64()) {
                token_sum += t;
                token_count += 1;
            }
            if let Some(c) = payload.get("capsule_count").and_then(|v| v.as_f64()) {
                capsule_sum += c;
                capsule_count += 1;
            }
        }

        let avg_injected_tokens = if token_count > 0 {
            Some(token_sum / token_count as f64)
        } else {
            None
        };

        let avg_capsules = if capsule_count > 0 {
            Some(capsule_sum / capsule_count as f64)
        } else {
            None
        };

        // C7: skip_rate = count(context.served where skipped=true) / served.
        // Reuse the `served` count already computed above.
        let skipped_count: u64 = match &window_since {
            Some(ts) => conn.query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE kind = 'context.served' AND ts >= ?1 \
                   AND (json_extract(payload_json,'$.skipped') = 1 \
                     OR json_extract(payload_json,'$.skipped') = 'true')",
                params![ts],
                |row| row.get(0),
            )?,
            None => conn.query_row(
                "SELECT COUNT(*) FROM events \
                 WHERE kind = 'context.served' \
                   AND (json_extract(payload_json,'$.skipped') = 1 \
                     OR json_extract(payload_json,'$.skipped') = 'true')",
                [],
                |row| row.get(0),
            )?,
        };
        let skip_rate = if retrieval.served > 0 {
            Some(skipped_count as f64 / retrieval.served as f64)
        } else {
            None
        };

        TokenEconomy {
            avg_injected_tokens,
            avg_capsules,
            skip_rate,
            // F3: overhead_ratio requires total_prompt_tokens from run.finished
            // events, which the pipeline does not yet record. See field doc.
            overhead_ratio: None,
        }
    };

    Ok(InsightsReport {
        retrieval,
        citation,
        proposals,
        usefulness,
        harvest,
        corpus,
        token_economy,
    })
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn text_preview(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        project::{
            AcceptOverrides, accept_proposal, add_memory, init_project, propose_memory,
            reject_proposal,
        },
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
        let root = std::env::temp_dir().join(format!("kimetsu-analytics-test-{}", Ulid::new()));
        kimetsu_core::paths::git_init_boundary(&root);
        root
    }

    // -----------------------------------------------------------------------
    // 1. ProposalStats
    // -----------------------------------------------------------------------

    #[test]
    fn proposal_stats_acceptance_rate_and_pending() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Seed 2 accepted + 1 rejected + 1 pending.
            let p1 = propose_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "alpha fact",
                0.5,
                "r1",
            )
            .expect("propose 1");
            let p2 = propose_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "beta fact",
                0.5,
                "r2",
            )
            .expect("propose 2");
            let p3 = propose_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "gamma fact",
                0.5,
                "r3",
            )
            .expect("propose 3");
            let _p4 = propose_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "delta fact",
                0.5,
                "r4",
            )
            .expect("propose 4");

            accept_proposal(&root, &p1, AcceptOverrides::default()).expect("accept p1");
            accept_proposal(&root, &p2, AcceptOverrides::default()).expect("accept p2");
            reject_proposal(&root, &p3, Some("not useful")).expect("reject p3");
            // p4 stays pending.

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let ps = &report.proposals;
            assert_eq!(ps.accepted, 2, "accepted count");
            assert_eq!(ps.rejected, 1, "rejected count");
            assert_eq!(ps.pending, 1, "pending count");
            let rate = ps.acceptance_rate.expect("acceptance_rate must be Some");
            let expected = 2.0 / 3.0;
            assert!(
                (rate - expected).abs() < 1e-9,
                "acceptance_rate expected {expected}, got {rate}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // 2. CorpusHealth
    // -----------------------------------------------------------------------

    #[test]
    fn corpus_health_counts_active_vs_invalidated() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let _m1 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "active fact one",
            )
            .expect("m1");
            let _m2 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Command,
                "active command",
            )
            .expect("m2");
            let m3 = add_memory(
                &root,
                MemoryScope::Repo,
                MemoryKind::Convention,
                "repo convention",
            )
            .expect("m3");
            // Invalidate m3.
            crate::project::invalidate_memory(&root, &m3, Some("test")).expect("invalidate");

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let ch = &report.corpus;
            assert_eq!(ch.active, 2, "active count");
            assert_eq!(ch.invalidated, 1, "invalidated count");

            // by_scope must include "project" with count 2.
            let project_scope = ch.by_scope.iter().find(|(s, _)| s == "project");
            assert!(project_scope.is_some(), "project scope missing");
            assert_eq!(project_scope.unwrap().1, 2);

            // by_kind must include "fact" with count 1 (m3 was repo, invalidated).
            let fact_kind = ch.by_kind.iter().find(|(k, _)| k == "fact");
            assert!(fact_kind.is_some(), "fact kind missing");
            assert_eq!(fact_kind.unwrap().1, 1);

            // top_useful may be empty (use_count < 1) but must not error.
            let _ = &ch.top_useful;
        });
    }

    // -----------------------------------------------------------------------
    // 3. HarvestStats
    // -----------------------------------------------------------------------

    #[test]
    fn harvest_stats_by_source_and_yield() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // add_memory uses source "manual_cli" in provenance.
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "harvest fact A",
            )
            .expect("A");
            add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "harvest fact B",
            )
            .expect("B");

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let hs = &report.harvest;
            assert!(
                hs.created_in_window >= 2,
                "created_in_window must be >= 2; got {}",
                hs.created_in_window
            );
            // Both from "manual_cli"
            let manual = hs.by_source.iter().find(|(s, _)| s == "manual_cli");
            assert!(manual.is_some(), "manual_cli source missing");
            assert!(manual.unwrap().1 >= 2);
            // yield_per_run must be Some (runs were created by add_memory).
            assert!(hs.yield_per_run.is_some(), "yield_per_run must be Some");
        });
    }

    // -----------------------------------------------------------------------
    // 4. UsefulnessTrend — Gate-excluded run.failed
    // -----------------------------------------------------------------------

    #[test]
    fn usefulness_trend_gate_failure_excluded_from_window_net() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            let run_id1 = RunId::new();
            let run_id2 = RunId::new();
            let run_id3 = RunId::new();

            // run.finished
            let started1 = Event::new(
                run_id1,
                "run.started",
                serde_json::json!({"mode":"agent","task":"t","project_id":"test","repo_root":root.to_string_lossy(),"model":null,"platform":"test","kimetsu_version":"0","config_hash":"0"}),
            );
            let finished1 = Event::new(
                run_id1,
                "run.finished",
                serde_json::json!({"status":"success","total_cost_usd":0,"total_tool_calls":0}),
            );
            // run.failed category=Gate (excluded)
            let started2 = Event::new(
                run_id2,
                "run.started",
                serde_json::json!({"mode":"agent","task":"t","project_id":"test","repo_root":root.to_string_lossy(),"model":null,"platform":"test","kimetsu_version":"0","config_hash":"0"}),
            );
            let failed_gate = Event::new(
                run_id2,
                "run.failed",
                serde_json::json!({"category":"Gate","total_cost_usd":0}),
            );
            // run.failed category=Implementation (non-Gate, counts)
            let started3 = Event::new(
                run_id3,
                "run.started",
                serde_json::json!({"mode":"agent","task":"t","project_id":"test","repo_root":root.to_string_lossy(),"model":null,"platform":"test","kimetsu_version":"0","config_hash":"0"}),
            );
            let failed_impl = Event::new(
                run_id3,
                "run.failed",
                serde_json::json!({"category":"Implementation","total_cost_usd":0}),
            );

            projector::apply_events(&conn, &[started1, finished1]).expect("apply run1");
            projector::apply_events(&conn, &[started2, failed_gate]).expect("apply run2");
            projector::apply_events(&conn, &[started3, failed_impl]).expect("apply run3");

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let ut = &report.usefulness;
            assert_eq!(ut.window_finished, 1, "window_finished");
            assert_eq!(
                ut.window_failed_nongate, 1,
                "window_failed_nongate (Gate excluded)"
            );
            assert_eq!(ut.window_net, 0, "window_net = 1 - 1 = 0");
        });
    }

    // -----------------------------------------------------------------------
    // 5. CitationStats
    // -----------------------------------------------------------------------

    #[test]
    fn citation_stats_rate_correct() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let m1 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "citation fact A",
            )
            .expect("m1");
            let m2 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "citation fact B",
            )
            .expect("m2");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            let run_id = RunId::new();

            // context.injected with both memory_ids
            let injected = Event::new(
                run_id,
                "context.injected",
                serde_json::json!({
                    "stage": "localization",
                    "memory_ids": [&m1, &m2],
                }),
            );
            // memory.cited for m1 only
            let cited = Event::new(
                run_id,
                "memory.cited",
                serde_json::json!({
                    "memory_id": &m1,
                    "turn": 1,
                }),
            );
            let finished = Event::new(
                run_id,
                "run.finished",
                serde_json::json!({
                    "status": "success",
                    "total_cost_usd": 0,
                    "total_tool_calls": 0,
                }),
            );
            projector::apply_events(&conn, &[injected, cited, finished]).expect("project");

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let cs = &report.citation;
            assert_eq!(cs.retrieved_total, 2, "retrieved_total");
            assert_eq!(cs.cited_total, 1, "cited_total");
            let rate = cs.citation_rate.expect("citation_rate must be Some");
            assert!(
                (rate - 0.5).abs() < 1e-9,
                "citation_rate expected 0.5, got {rate}"
            );
        });
    }

    // -----------------------------------------------------------------------
    // 6. TokenEconomy — with and without used_tokens/capsule_count
    // -----------------------------------------------------------------------

    #[test]
    fn token_economy_averages_new_events_and_tolerates_old_events() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            let run_id1 = RunId::new();
            let run_id2 = RunId::new();

            // New-style event WITH used_tokens + capsule_count
            let new_event = Event::new(
                run_id1,
                "context.injected",
                serde_json::json!({
                    "stage": "localization",
                    "memory_ids": [],
                    "used_tokens": 400,
                    "capsule_count": 3,
                }),
            );
            // Old-style event WITHOUT those fields — must not crash; excluded from average
            let old_event = Event::new(
                run_id2,
                "context.injected",
                serde_json::json!({
                    "stage": "localization",
                    "memory_ids": [],
                }),
            );

            projector::apply_events(&conn, &[new_event]).expect("apply new");
            projector::apply_events(&conn, &[old_event]).expect("apply old");

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let te = &report.token_economy;

            let avg_tok = te
                .avg_injected_tokens
                .expect("avg_injected_tokens must be Some (one event has it)");
            assert!(
                (avg_tok - 400.0).abs() < 1e-6,
                "avg_injected_tokens expected 400.0, got {avg_tok}"
            );
            let avg_cap = te.avg_capsules.expect("avg_capsules must be Some");
            assert!(
                (avg_cap - 3.0).abs() < 1e-6,
                "avg_capsules expected 3.0, got {avg_cap}"
            );
            // No context.served events seeded → served==0 → skip_rate is None.
            assert!(
                te.skip_rate.is_none(),
                "skip_rate must be None when no context.served events exist"
            );
        });
    }

    #[test]
    fn token_economy_all_old_events_returns_none() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            let run_id = RunId::new();

            // Only old-style events
            let old_event = Event::new(
                run_id,
                "context.injected",
                serde_json::json!({
                    "stage": "localization",
                    "memory_ids": [],
                }),
            );
            projector::apply_events(&conn, &[old_event]).expect("apply");

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let te = &report.token_economy;
            assert!(
                te.avg_injected_tokens.is_none(),
                "must be None when no event has used_tokens"
            );
            assert!(
                te.avg_capsules.is_none(),
                "must be None when no event has capsule_count"
            );
        });
    }

    // -----------------------------------------------------------------------
    // 7. C7 — RetrievalStats from context.served events
    // -----------------------------------------------------------------------

    /// Helper: seed a `context.served` event directly into the DB.
    fn seed_context_served(
        conn: &rusqlite::Connection,
        capsule_count: u64,
        top_score: f32,
        skipped: bool,
    ) {
        let run_id = RunId::new();
        let event = Event::new(
            run_id,
            "context.served",
            serde_json::json!({
                "query_hash": "testhash",
                "capsule_count": capsule_count,
                "top_score": top_score,
                "skipped": skipped,
                "stage": "localization",
            }),
        );
        projector::apply_events(conn, &[event]).expect("seed context.served");
    }

    #[test]
    fn retrieval_stats_counts_hits_and_misses() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");

            // 2 hits (capsule_count >= 1, skipped=false)
            seed_context_served(&conn, 3, 0.85, false);
            seed_context_served(&conn, 1, 0.60, false);
            // 1 miss (capsule_count == 0, skipped=true)
            seed_context_served(&conn, 0, 0.0, true);
            // 1 explicit skip (skipped=true but some top_score)
            seed_context_served(&conn, 0, 0.10, true);

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let rs = &report.retrieval;

            assert_eq!(
                rs.served, 4,
                "served should count all context.served events"
            );
            assert_eq!(
                rs.with_hit, 2,
                "with_hit should count events with capsule_count>=1 and skipped=false"
            );
            let hr = rs.hit_rate.expect("hit_rate must be Some when served>0");
            assert!(
                (hr - 0.5).abs() < 1e-9,
                "hit_rate should be 2/4 = 0.5; got {hr}"
            );

            // avg_top_score over hits only (0.85 + 0.60) / 2 = 0.725
            let avg = rs
                .avg_top_score
                .expect("avg_top_score must be Some when hits exist");
            assert!(
                (avg - 0.725).abs() < 0.001,
                "avg_top_score expected ~0.725; got {avg}"
            );

            // skip_rate: 2 skipped / 4 served = 0.5
            let sr = report
                .token_economy
                .skip_rate
                .expect("skip_rate must be Some when served>0");
            assert!(
                (sr - 0.5).abs() < 1e-9,
                "skip_rate should be 2/4 = 0.5; got {sr}"
            );
        });
    }

    #[test]
    fn retrieval_stats_no_context_served_events_returns_none() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // No context.served events — old-DB case.
            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let rs = &report.retrieval;

            assert_eq!(rs.served, 0, "served must be 0 with no events");
            assert_eq!(rs.with_hit, 0, "with_hit must be 0 with no events");
            assert!(
                rs.hit_rate.is_none(),
                "hit_rate must be None when served==0"
            );
            assert!(
                rs.avg_top_score.is_none(),
                "avg_top_score must be None when no hits"
            );
            assert!(
                report.token_economy.skip_rate.is_none(),
                "skip_rate must be None when served==0"
            );
        });
    }

    #[test]
    fn retrieval_stats_all_hits_skip_rate_zero() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");

            // 3 hits, no skips
            seed_context_served(&conn, 2, 0.90, false);
            seed_context_served(&conn, 5, 0.75, false);
            seed_context_served(&conn, 1, 0.55, false);

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let rs = &report.retrieval;

            assert_eq!(rs.served, 3);
            assert_eq!(rs.with_hit, 3);
            let hr = rs.hit_rate.expect("hit_rate");
            assert!(
                (hr - 1.0).abs() < 1e-9,
                "all hits → hit_rate = 1.0; got {hr}"
            );

            let sr = report
                .token_economy
                .skip_rate
                .expect("skip_rate must be Some");
            assert!(
                (sr - 0.0).abs() < 1e-9,
                "no skips → skip_rate = 0.0; got {sr}"
            );
        });
    }

    #[test]
    fn log_telemetry_event_writes_context_served_to_db() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Write via log_telemetry_event (the helper used by the hook).
            crate::project::log_telemetry_event(
                &root,
                "context.served",
                serde_json::json!({
                    "query_hash": "abc123",
                    "capsule_count": 0,
                    "top_score": 0.0,
                    "skipped": true,
                    "stage": "localization",
                }),
            )
            .expect("log_telemetry_event must succeed");

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let rs = &report.retrieval;
            assert_eq!(rs.served, 1, "log_telemetry_event event must be counted");
            assert_eq!(rs.with_hit, 0, "skipped event is not a hit");
            assert!(rs.hit_rate.is_some());
            assert!(
                (rs.hit_rate.unwrap() - 0.0).abs() < 1e-9,
                "0 hits / 1 served = 0.0"
            );
        });
    }

    // -----------------------------------------------------------------------
    // S4.1 — superseded memories must be excluded from active counts
    // -----------------------------------------------------------------------

    /// A memory that has been superseded (its `superseded_by` column is set to
    /// a survivor memory_id) is RETIRED by consolidation — retrieval already
    /// excludes it.  The analytics `active` count, `by_scope`, `by_kind`, and
    /// `UsefulnessTrend` aggregates must agree with retrieval and exclude
    /// superseded rows, so the health dashboard shows the same corpus the user
    /// actually gets back when they run a query.
    #[test]
    fn superseded_memory_excluded_from_active_count() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");

            // Add two memories.
            let _m1 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "active fact stays",
            )
            .expect("m1");
            let m2 = add_memory(
                &root,
                MemoryScope::Project,
                MemoryKind::Fact,
                "superseded fact goes",
            )
            .expect("m2");

            // Mark m2 as superseded by m1 (simulates what the consolidation
            // projector does when it merges two contradicting memories).
            let (_paths, _config, conn) = crate::project::load_project(&root).expect("load");
            conn.execute(
                "UPDATE memories SET superseded_by = ?1 WHERE memory_id = ?2",
                rusqlite::params![_m1, m2],
            )
            .expect("stamp superseded_by");

            let report = compute_insights(&root, InsightsOptions::default()).expect("insights");
            let ch = &report.corpus;

            // Only the non-superseded memory must count as active.
            assert_eq!(
                ch.active, 1,
                "active count must exclude superseded memories; got {}",
                ch.active
            );
            // by_scope must also reflect only 1 active project-scope memory.
            let project_scope = ch.by_scope.iter().find(|(s, _)| s == "project");
            assert_eq!(
                project_scope.map(|(_, n)| *n),
                Some(1),
                "by_scope[project] must be 1 (superseded excluded)"
            );
            // by_kind must reflect only 1 active fact.
            let fact_kind = ch.by_kind.iter().find(|(k, _)| k == "fact");
            assert_eq!(
                fact_kind.map(|(_, n)| *n),
                Some(1),
                "by_kind[fact] must be 1 (superseded excluded)"
            );
        });
    }
}
