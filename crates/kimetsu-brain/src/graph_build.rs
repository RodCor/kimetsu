//! Knowledge-graph edge building over active memories.
//! Split out of `project.rs` (v2.5.1); re-exported by [`crate::project`].

use std::path::Path;

use kimetsu_core::KimetsuResult;
use rusqlite::{Connection, OpenFlags};

use crate::projector;
use crate::schema;

// ── #2 knowledge graph: build relation edges ─────────────────────────────────

/// Summary of a `kimetsu brain graph build` run.
#[derive(Debug, Clone, Default, serde::Serialize, serde::Deserialize)]
pub struct GraphBuildSummary {
    /// Active (non-invalidated, non-superseded) memories scanned.
    pub active_memories: usize,
    /// Rule-derived `relates_to` edges proposed.
    pub rule_edges: usize,
    /// Enrichment (LLM typed) edges proposed by the caller.
    pub enrich_edges: usize,
    /// Edges actually written (0 when `dry_run`).
    pub written: usize,
    /// Proposed edge counts grouped by edge_type (rule + enrichment, pre-write).
    pub by_type: std::collections::BTreeMap<String, usize>,
    /// True when no edges were persisted (preview only).
    pub dry_run: bool,
}

/// Read every active memory as `(id, text)` for graph enrichment. Read-only.
/// Exposed so the CLI (which owns the cheap-model provider) can compute typed
/// enrichment edges before calling [`build_graph`].
pub fn active_memory_texts(start: &Path) -> KimetsuResult<Vec<(String, String)>> {
    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open_with_flags(&paths.brain_db, OpenFlags::SQLITE_OPEN_READ_ONLY)?;
    let mut stmt = conn.prepare(
        "SELECT memory_id, text
         FROM memories
         WHERE invalidated_at IS NULL AND superseded_by IS NULL
         ORDER BY memory_id",
    )?;
    let rows = stmt
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

/// #2: build the knowledge-graph edges for the workspace brain.
///
/// Combines the deterministic rule layer ([`crate::graph::build_relates_to_edges`])
/// with any caller-supplied `extra_edges` (LLM enrichment computed in the CLI),
/// de-duplicates, and — unless `dry_run` — persists them as rebuild-safe
/// `memory.edge` events via [`projector::add_memory_edges`]. Returns a summary.
///
/// `max_fan_out` caps rule edges per source memory (0 = the module default).
pub fn build_graph(
    start: &Path,
    extra_edges: &[(String, String, String)],
    max_fan_out: usize,
    dry_run: bool,
) -> KimetsuResult<GraphBuildSummary> {
    use std::collections::{BTreeMap, BTreeSet};

    let paths = kimetsu_core::paths::ProjectPaths::discover(start)?;
    let conn = Connection::open(&paths.brain_db)?;
    schema::initialize(&conn)?;

    let active_memories = conn.query_row(
        "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL AND superseded_by IS NULL",
        [],
        |r| r.get::<_, i64>(0),
    )? as usize;

    // v2.6: refresh the entity index before deriving edges. A brain that was
    // migrated with an unreadable corpus, or one whose extractor rules have
    // since changed, would otherwise build its graph from a stale index —
    // and this command is exactly what a user runs to fix that.
    if !dry_run {
        let _ = crate::graph::reproject_all_entities(&conn);
    }

    let rule = crate::graph::build_relates_to_edges(&conn, max_fan_out)?;
    let rule_edges = rule.len();
    let enrich_edges = extra_edges.len();

    // Merge rule + enrichment, de-duplicating on (src, dst, type). Self-loops are
    // dropped by add_memory_edges; we also drop them here for an accurate summary.
    let mut seen: BTreeSet<(String, String, String)> = BTreeSet::new();
    let mut by_type: BTreeMap<String, usize> = BTreeMap::new();
    let mut merged: Vec<(String, String, String)> = Vec::new();
    let push = |src: String,
                dst: String,
                ty: String,
                seen: &mut BTreeSet<(String, String, String)>,
                by_type: &mut BTreeMap<String, usize>,
                merged: &mut Vec<(String, String, String)>| {
        if src == dst {
            return;
        }
        let key = (src.clone(), dst.clone(), ty.clone());
        if seen.insert(key) {
            *by_type.entry(ty.clone()).or_insert(0) += 1;
            merged.push((src, dst, ty));
        }
    };
    for e in &rule {
        push(
            e.src_id.clone(),
            e.dst_id.clone(),
            e.edge_type.clone(),
            &mut seen,
            &mut by_type,
            &mut merged,
        );
    }
    for (src, dst, ty) in extra_edges {
        push(
            src.clone(),
            dst.clone(),
            ty.clone(),
            &mut seen,
            &mut by_type,
            &mut merged,
        );
    }

    let written = if dry_run {
        0
    } else {
        projector::add_memory_edges(&conn, &merged)?
    };

    Ok(GraphBuildSummary {
        active_memories,
        rule_edges,
        enrich_edges,
        written,
        by_type,
        dry_run,
    })
}
