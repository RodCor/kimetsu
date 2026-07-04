//! Per-run memory attribution (blame) + usefulness leaderboard (top).
//! Split out of `project.rs` (v2.5.1); re-exported by [`crate::project`].

use std::path::Path;

use kimetsu_core::KimetsuResult;
use rusqlite::{Connection, OptionalExtension, params};

use crate::project::*;
use crate::user_brain;

// v0.5.1: blame surface — per-run memory attribution. Both the CLI
// (`kimetsu brain memory blame <run-id>`) and the MCP tool
// (`kimetsu_brain_memory_blame`) consume `BlameReport`.

#[derive(Debug, Clone, serde::Serialize)]
pub struct BlameReport {
    pub run_id: String,
    /// Terminal outcome of the run: "success" (run.finished),
    /// "failed" (run.failed), "aborted" (run.aborted), or "unknown"
    /// (no terminal event found yet).
    pub outcome: String,
    /// Failure category when outcome is "failed" (e.g. "Gate",
    /// "Implementation"). None otherwise.
    pub failure_category: Option<String>,
    /// Memories the model explicitly cited via `cite_memory`,
    /// ordered by turn.
    pub cited: Vec<CitedMemory>,
    /// Memories that were retrieved into the run's context but
    /// never cited. They got the weak ±0.1 signal instead of ±1.0.
    pub silent_passengers: Vec<SilentMemory>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct CitedMemory {
    pub memory_id: String,
    pub turn: i64,
    pub rationale: Option<String>,
    pub cited_at: String,
    /// Truncated memory text for human-readable output.
    pub text_preview: String,
    pub scope: String,
    pub kind: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct SilentMemory {
    pub memory_id: String,
    pub text_preview: String,
    pub scope: String,
    pub kind: String,
}

/// `BlameReport` that surfaces which memories the model actually
/// reasoned with vs which were silent passengers.
///
/// Lookups across user + project brains are merged so a cited
/// user-scope memory shows its text even when the run lived in a
/// project brain.
pub fn blame_run(start: &Path, run_id: &str) -> KimetsuResult<BlameReport> {
    let (_paths, config, conn) = load_project(start)?;
    // W3.3: honor config.kimetsu.use_user_brain with env override.

    let user_conn = user_brain::open_user_brain_readonly_for_config(config.kimetsu.use_user_brain)?;

    // 1. Terminal outcome.
    let (outcome, failure_category) = run_outcome(&conn, run_id)?;

    // 2. Cited memories — ordered by turn.
    let cited_rows: Vec<(String, i64, Option<String>, String)> = {
        let mut stmt = conn.prepare(
            "
            SELECT memory_id, turn, rationale, cited_at
            FROM memory_citations
            WHERE run_id = ?1
            ORDER BY turn ASC, cited_at ASC
            ",
        )?;
        let rows = stmt.query_map(rusqlite::params![run_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?;
        let mut out = Vec::new();
        for row in rows {
            out.push(row?);
        }
        out
    };

    let mut cited: Vec<CitedMemory> = Vec::with_capacity(cited_rows.len());
    let mut cited_set: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    for (memory_id, turn, rationale, cited_at) in cited_rows {
        cited_set.insert(memory_id.clone());
        let (text, scope, kind) = resolve_memory(&conn, user_conn.as_ref(), &memory_id);
        cited.push(CitedMemory {
            memory_id,
            turn,
            rationale,
            cited_at,
            text_preview: text_preview(&text, 120),
            scope,
            kind,
        });
    }

    // 3. Silent passengers — retrieved but not cited.
    let retrieved_ids = collect_injected_memory_ids_for_blame(&conn, run_id)?;
    let mut silent: Vec<SilentMemory> = Vec::new();
    for memory_id in retrieved_ids {
        if cited_set.contains(&memory_id) {
            continue;
        }
        let (text, scope, kind) = resolve_memory(&conn, user_conn.as_ref(), &memory_id);
        silent.push(SilentMemory {
            memory_id,
            text_preview: text_preview(&text, 120),
            scope,
            kind,
        });
    }

    Ok(BlameReport {
        run_id: run_id.to_string(),
        outcome,
        failure_category,
        cited,
        silent_passengers: silent,
    })
}

fn run_outcome(conn: &Connection, run_id: &str) -> KimetsuResult<(String, Option<String>)> {
    // Pull the most recent terminal event for the run, if any.
    let row: Option<(String, String)> = conn
        .query_row(
            "
            SELECT kind, payload_json
            FROM events
            WHERE run_id = ?1
              AND kind IN ('run.finished', 'run.failed', 'run.aborted')
            ORDER BY ts DESC
            LIMIT 1
            ",
            rusqlite::params![run_id],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    Ok(match row {
        Some((kind, payload_json)) => {
            let outcome = match kind.as_str() {
                "run.finished" => "success".to_string(),
                "run.failed" => "failed".to_string(),
                "run.aborted" => "aborted".to_string(),
                other => other.to_string(),
            };
            let category = if kind == "run.failed" {
                serde_json::from_str::<serde_json::Value>(&payload_json)
                    .ok()
                    .and_then(|v| {
                        v.get("category")
                            .and_then(|c| c.as_str())
                            .map(str::to_string)
                    })
            } else {
                None
            };
            (outcome, category)
        }
        None => ("unknown".to_string(), None),
    })
}

fn collect_injected_memory_ids_for_blame(
    conn: &Connection,
    run_id: &str,
) -> KimetsuResult<Vec<String>> {
    let mut stmt = conn.prepare(
        "
        SELECT payload_json
        FROM events
        WHERE run_id = ?1 AND kind = 'context.injected'
        ",
    )?;
    let rows = stmt.query_map(rusqlite::params![run_id], |row| row.get::<_, String>(0))?;
    let mut seen = std::collections::BTreeSet::new();
    for row in rows {
        let payload_json = row?;
        let payload: serde_json::Value = serde_json::from_str(&payload_json)?;
        if let Some(ids) = payload.get("memory_ids").and_then(|v| v.as_array()) {
            for id in ids {
                if let Some(s) = id.as_str()
                    && !s.is_empty()
                {
                    seen.insert(s.to_string());
                }
            }
        }
    }
    Ok(seen.into_iter().collect())
}

/// Look up a memory's (text, scope, kind) across the project conn
/// and the optional user-brain conn. Returns
/// ("<unknown — deleted?>", "", "") when the row isn't found in
/// either DB (e.g. invalidated + GC'd, or a typo'd memory_id in
/// the citation).
fn resolve_memory(
    project_conn: &Connection,
    user_conn: Option<&Connection>,
    memory_id: &str,
) -> (String, String, String) {
    let q = "SELECT text, scope, kind FROM memories WHERE memory_id = ?1";
    let try_conn = |conn: &Connection| -> Option<(String, String, String)> {
        conn.query_row(q, rusqlite::params![memory_id], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })
        .optional()
        .ok()
        .flatten()
    };
    try_conn(project_conn)
        .or_else(|| user_conn.and_then(try_conn))
        .unwrap_or_else(|| {
            (
                "<unknown — deleted or invalid memory_id>".to_string(),
                String::new(),
                String::new(),
            )
        })
}

fn text_preview(text: &str, max_chars: usize) -> String {
    let trimmed = text.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let head: String = trimmed.chars().take(max_chars).collect();
        format!("{head}…")
    }
}

/// MP-6: ranked list of memories sorted by the same usefulness ratio the
/// broker uses for retrieval scoring (`usefulness_score / use_count`).
/// Filters out invalidated rows and any memory with `use_count < min_uses`
/// (the small-sample guard; default 3 matches the broker's
/// SMALL_SAMPLE_THRESHOLD). Optional scope filter narrows to a single
/// memory class. Lets the user see which memories are actually doing
/// work so they can prune the rest with `memory prune`.
#[derive(Debug, Clone, Default)]
pub struct TopOptions {
    pub scope: Option<String>,
    pub min_uses: u32,
    pub limit: u32,
}

pub fn list_memories_top(start: &Path, opts: TopOptions) -> KimetsuResult<Vec<MemoryRow>> {
    let (_paths, _config, conn) = load_project(start)?;
    let min_uses = opts.min_uses.max(1) as i64;
    let limit = if opts.limit == 0 { 20 } else { opts.limit } as i64;

    let (sql, scope_param): (&str, Option<String>) = if let Some(scope) = opts.scope.as_deref() {
        (
            "
            SELECT memory_id, scope, kind, text, confidence, use_count, usefulness_score
            FROM memories
            WHERE invalidated_at IS NULL
              AND superseded_by IS NULL
              AND use_count >= ?1
              AND lower(scope) = lower(?2)
            ORDER BY (usefulness_score / CAST(use_count AS REAL)) DESC, use_count DESC
            LIMIT ?3
            ",
            Some(scope.to_string()),
        )
    } else {
        (
            "
            SELECT memory_id, scope, kind, text, confidence, use_count, usefulness_score
            FROM memories
            WHERE invalidated_at IS NULL
              AND superseded_by IS NULL
              AND use_count >= ?1
            ORDER BY (usefulness_score / CAST(use_count AS REAL)) DESC, use_count DESC
            LIMIT ?2
            ",
            None,
        )
    };

    let mut stmt = conn.prepare(sql)?;
    let mut rows = if let Some(scope) = scope_param {
        stmt.query_map(params![min_uses, scope, limit], map_memory_row)?
            .collect::<Result<Vec<_>, _>>()?
    } else {
        stmt.query_map(params![min_uses, limit], map_memory_row)?
            .collect::<Result<Vec<_>, _>>()?
    };

    // SQLite's NaN-from-zero protection: a freshly-created memory with
    // use_count=0 would division-zero, but the WHERE clause guards
    // min_uses >= 1, so we never see a NaN here. Sort is a defensive
    // tie-breaker only.
    rows.sort_by(|a, b| {
        let ra = a.usefulness_score as f64 / a.use_count.max(1) as f64;
        let rb = b.usefulness_score as f64 / b.use_count.max(1) as f64;
        rb.partial_cmp(&ra).unwrap_or(std::cmp::Ordering::Equal)
    });
    Ok(rows)
}

pub(crate) fn map_memory_row(row: &rusqlite::Row) -> rusqlite::Result<MemoryRow> {
    Ok(MemoryRow {
        memory_id: row.get(0)?,
        scope: row.get(1)?,
        kind: row.get(2)?,
        text: row.get(3)?,
        confidence: row.get(4)?,
        use_count: row.get(5)?,
        usefulness_score: row.get::<_, f64>(6)? as f32,
    })
}
