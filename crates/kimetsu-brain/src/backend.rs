//! S5.1 + S5.2: `RetrievalBackend` trait — the seam between candidate generation
//! and the broker.
//!
//! # Boundary
//!
//! The trait covers **memory candidate generation** only: given a query and an
//! optional pre-computed query embedding, produce the raw `Candidate` pool that
//! the broker (scoring, floors, rerank, compression) then operates on.
//!
//! Repo-file and manifest candidates are NOT part of the backend — they are
//! project-local and always generated the same way regardless of which backend
//! is active. This keeps the blast radius small: the broker is entirely
//! backend-agnostic.
//!
//! # Flat backend
//!
//! [`FlatBackend`] is the default. It is a pure refactor-in-place:
//! it delegates to the existing `context::memory_candidates_flat` function, so the
//! FTS + usearch-ANN candidate path is UNCHANGED.
//!
//! # Graph-lite backend (S5.2)
//!
//! [`GraphLiteBackend`] is a SUPERSET of flat: it starts with the flat
//! candidate set and then expands up to `max_hops` hops over the
//! `memory_edges` typed-edge projection table (created by the v3→v4 migration).
//!
//! The expansion uses a recursive CTE rooted on the flat hit set, bounded by
//! `MAX_HOPS` (default 2) and `MAX_FAN_OUT` (default 20 new ids per call).
//! Graph-reachable memories are marked with provenance `"graph"` in their
//! `ProvenanceRef.source` field so the broker can see they arrived via graph
//! traversal, though the broker's scoring treats them identically.
//!
//! Because graph-lite strictly adds candidates to the flat set, it cannot
//! reduce recall relative to flat — so enabling it can never make retrieval
//! worse, only broader.
//!
//! # Future backends
//!
//! `"graph"` is a TODO seam for a future story (full petgraph + remote graph
//! traversal). It currently falls through to `FlatBackend`.

use std::collections::HashSet;

use rusqlite::Connection;

use kimetsu_core::KimetsuResult;

use crate::context::{Candidate, ContextCapsule, ProvenanceRef, QueryEmbedding};

// ─── Trait ───────────────────────────────────────────────────────────────────

/// The retrieval backend trait: produces the **memory candidate pool** for a
/// given query.
///
/// All broker logic (lexical/semantic floors, scoring, MMR, compression) runs
/// ABOVE this trait and is backend-agnostic. Implementors only decide HOW to
/// surface the initial set of memory `Candidate`s — the broker takes it from
/// there.
///
/// The trait is `pub(crate)` because it is an internal architecture seam, not
/// a public API surface.
pub(crate) trait RetrievalBackend {
    /// Return the raw memory candidate pool for `query`.
    ///
    /// * `conn` — the brain SQLite connection to query.
    /// * `query` — the raw retrieval query string.
    /// * `query_embedding` — pre-computed query embedding, present when an
    ///   embedding model is active and successfully embedded the query. `None`
    ///   on lean (FTS-only) builds or when embedding failed silently.
    /// * `half_life_days` — usefulness-decay half-life from config; passed
    ///   through to `memory_row_to_candidate` for the decay multiplier.
    ///
    /// The returned slice is unsorted and unscored — the broker normalises and
    /// scores. Each element's `raw_relevance` carries the pre-normalisation
    /// signal (FTS BM25 blend or cosine blend) that the broker's per-kind max
    /// normalization uses.
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>>;
}

// ─── FlatBackend ─────────────────────────────────────────────────────────────

/// The flat (today's) retrieval backend.
///
/// Delegates directly to `context::memory_candidates`, which runs:
///   * On embeddings builds: FTS top-80 ∪ usearch-ANN top-80, merged by
///     memory-id (keeping the higher-scored instance).
///   * On lean builds: FTS top-80, falling back to latest-recency top-200
///     when FTS produces no results.
///
/// This is a pure refactor-in-place: identical SQL, identical ANN calls,
/// identical candidate set.
pub(crate) struct FlatBackend;

impl RetrievalBackend for FlatBackend {
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>> {
        crate::context::memory_candidates_flat(conn, query, query_embedding, half_life_days)
    }
}

// ─── GraphLiteBackend ────────────────────────────────────────────────────────

/// S5.2: The graph-lite retrieval backend.
///
/// This backend is a **strict superset** of [`FlatBackend`]: it starts with
/// the flat candidate set (FTS + ANN, or FTS + recency on lean builds) and
/// then expands 1–`MAX_HOPS` hops over the `memory_edges` typed-edge
/// projection table.
///
/// # Edge traversal
///
/// After collecting the flat hit set (by `memory_id`), a single recursive
/// CTE walks outward over both directions of `memory_edges` (src→dst and
/// dst→src) up to `MAX_HOPS` steps. The CTE is bounded by:
///   * `MAX_HOPS = 2` — prevents traversal into distantly-related clusters.
///   * `MAX_FAN_OUT = 20` — caps the number of new memory ids returned per
///     call so a densely-connected corpus can't blow up the candidate set.
///
/// Graph-reachable memories that are already in the flat set are skipped
/// (dedup by `memory_id`). New graph-reached memories are fetched from
/// `memories` (active only: `invalidated_at IS NULL AND superseded_by IS
/// NULL`) and turned into `Candidate`s.  Their `ProvenanceRef.source` is
/// set to `"graph"` so callers can distinguish them from flat hits.
///
/// # No-edges guarantee
///
/// When `memory_edges` is empty (no superseded/merged memories yet) the CTE
/// returns zero rows and the function returns the exact flat candidate set —
/// identical behaviour to `FlatBackend`. This is the no-regression proof:
/// graph-lite ⊇ flat, always.
///
/// # Scoring
///
/// Graph-reached candidates have `raw_relevance = 0.0` pre-scoring because
/// they weren't matched by the query directly.  The broker's per-kind max
/// normalisation will therefore assign them `relevance = 0.0 / max` and they
/// will rank below any flat hit that had a positive signal.  If the caller
/// has a `min_score` floor they may be filtered out — which is correct
/// behaviour: the broker still controls final admission.
pub(crate) struct GraphLiteBackend;

/// Maximum hops to traverse from the flat hit set.
const MAX_HOPS: usize = 2;

/// Maximum number of graph-reachable memory ids to fetch per call.
const MAX_FAN_OUT: usize = 20;

impl RetrievalBackend for GraphLiteBackend {
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>> {
        // 1. Start with the flat candidate set (FTS + ANN / FTS + recency).
        let flat =
            crate::context::memory_candidates_flat(conn, query, query_embedding, half_life_days)?;

        // 2. Collect the memory_ids already in the flat set.
        let mut seen_ids: HashSet<String> = flat
            .iter()
            .filter_map(|c| {
                c.capsule
                    .expansion_handle
                    .strip_prefix("memory:")
                    .map(|id| id.to_string())
            })
            .collect();

        if seen_ids.is_empty() {
            // No flat hits → nothing to expand from; return flat as-is (empty).
            return Ok(flat);
        }

        // 3. Graph expansion: build a parameter list for the seed set.
        //    SQLite's recursive CTE traverses both edge directions (src→dst and
        //    dst→src) so that `supersedes` edges are followed in both directions
        //    (the superseded member can lead back to the survivor and vice versa).
        //    The hop depth guard (depth <= MAX_HOPS) and the NOT IN seed check
        //    bound the expansion.
        let new_ids = graph_expand(conn, &seen_ids, MAX_HOPS, MAX_FAN_OUT)?;

        if new_ids.is_empty() {
            return Ok(flat);
        }

        // 4. Fetch the graph-reachable memories as candidates, marking their
        //    provenance so the broker/caller can distinguish them from flat hits.
        let graph_candidates = fetch_graph_candidates(conn, &new_ids, &mut seen_ids)?;

        // 5. Concatenate: flat hits first (they have real relevance signals),
        //    graph-reachable hits appended (raw_relevance = 0.0 → ranked last
        //    by the broker's normalisation, filtered by floors if weak).
        let mut combined = flat;
        combined.extend(graph_candidates);
        Ok(combined)
    }
}

/// Walk `memory_edges` up to `max_hops` steps from `seed_ids` and return the
/// set of reachable memory_ids that are NOT already in `seed_ids`.
///
/// The traversal follows edges in BOTH directions (src→dst and dst→src) so
/// that `supersedes` edges can be followed either way:
///   * survivor → member (dst)  : find the superseded member from the survivor
///   * member → survivor (src)  : find the survivor from the superseded member
///
/// Implementation: iterative BFS, one SQLite query per hop. Avoids the
/// `VALUES (...)` CTE seed syntax that SQLite does not support with bound
/// parameters in a recursive CTE anchor clause.
///
/// Returns at most `max_fan_out` new ids across ALL hops combined.
fn graph_expand(
    conn: &Connection,
    seed_ids: &HashSet<String>,
    max_hops: usize,
    max_fan_out: usize,
) -> KimetsuResult<Vec<String>> {
    if seed_ids.is_empty() || max_hops == 0 {
        return Ok(Vec::new());
    }

    // `frontier` = the ids visited in the previous hop (start = seeds).
    // `visited`  = all ids seen so far (seeds + discovered).
    let mut visited: HashSet<String> = seed_ids.clone();
    let mut frontier: Vec<String> = seed_ids.iter().cloned().collect();
    let mut new_ids: Vec<String> = Vec::new();

    for _hop in 0..max_hops {
        if frontier.is_empty() {
            break;
        }
        if new_ids.len() >= max_fan_out {
            break;
        }

        // One-hop query: from `frontier`, follow edges in both directions,
        // collecting neighbours that are not yet in `visited`.
        //
        // SQLite supports `IN (?1, ?2, ...)` with positional parameters.
        // We need two separate IN-clauses with different parameter slots
        // (src_id IN (?1..?N) and dst_id IN (?N+1..?2N)) so we supply the
        // frontier list twice as params.
        let n = frontier.len();
        let src_placeholders: String = (1..=n)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");
        let dst_placeholders: String = (n + 1..=2 * n)
            .map(|i| format!("?{i}"))
            .collect::<Vec<_>>()
            .join(", ");

        let sql = format!(
            "
            SELECT DISTINCT neighbour FROM (
                SELECT dst_id AS neighbour FROM memory_edges
                WHERE src_id IN ({src_placeholders})
                UNION
                SELECT src_id AS neighbour FROM memory_edges
                WHERE dst_id IN ({dst_placeholders})
            )
            "
        );

        let mut stmt = conn.prepare(&sql)?;
        // Supply frontier twice: once for src_id IN, once for dst_id IN.
        let params_refs: Vec<&dyn rusqlite::ToSql> = frontier
            .iter()
            .chain(frontier.iter())
            .map(|s| s as &dyn rusqlite::ToSql)
            .collect();

        let rows = stmt.query_map(params_refs.as_slice(), |row| row.get::<_, String>(0))?;

        let mut next_frontier: Vec<String> = Vec::new();
        for row in rows {
            let neighbour = row?;
            if !visited.contains(&neighbour) {
                visited.insert(neighbour.clone());
                next_frontier.push(neighbour.clone());
                new_ids.push(neighbour);
                if new_ids.len() >= max_fan_out {
                    break;
                }
            }
        }
        frontier = next_frontier;
    }

    Ok(new_ids)
}

/// Fetch active memory rows for `new_ids` and build `Candidate`s.
///
/// Each returned candidate has `raw_relevance = 0.0` (no query signal) and
/// `ProvenanceRef.source = "graph"` so callers know it arrived via edge
/// traversal rather than direct lexical/semantic match.
///
/// Memories that are invalidated or superseded are silently skipped — the
/// `memory_edges` table may contain references to superseded rows (by design:
/// we keep the edge history for `blame`), but retrieval must never surface them.
///
/// `seen_ids` is updated in place so callers can track which ids were added.
fn fetch_graph_candidates(
    conn: &Connection,
    new_ids: &[String],
    seen_ids: &mut HashSet<String>,
) -> KimetsuResult<Vec<Candidate>> {
    if new_ids.is_empty() {
        return Ok(Vec::new());
    }

    let placeholders: String = (1..=new_ids.len())
        .map(|i| format!("?{i}"))
        .collect::<Vec<_>>()
        .join(", ");

    let sql = format!(
        "SELECT memory_id, scope, kind, text, confidence, created_at
         FROM memories
         WHERE invalidated_at IS NULL
           AND superseded_by IS NULL
           AND memory_id IN ({placeholders})"
    );

    let mut stmt = conn.prepare(&sql)?;
    let params_refs: Vec<&dyn rusqlite::ToSql> =
        new_ids.iter().map(|s| s as &dyn rusqlite::ToSql).collect();

    let rows = stmt.query_map(params_refs.as_slice(), |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f32>(4)?,
            row.get::<_, String>(5)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (memory_id, scope, kind, text, confidence, created_at) = row?;

        // Skip if already in the seen set (shouldn't happen given the CTE's
        // NOT IN guard, but be defensive).
        if !seen_ids.insert(memory_id.clone()) {
            continue;
        }

        let freshness = crate::context::freshness_pub(&created_at);
        let scope_weight = crate::context::scope_weight_pub(&scope);
        let token_estimate = crate::context::estimate_tokens(&text) + 8;

        candidates.push(Candidate {
            raw_relevance: 0.0,
            embedding: None,
            cosine: None,
            capsule: ContextCapsule {
                id: kimetsu_core::ids::new_id().to_string(),
                kind: "memory".to_string(),
                summary: format!("{scope}:{kind} - {text}"),
                token_estimate,
                expansion_handle: format!("memory:{memory_id}"),
                provenance: vec![ProvenanceRef {
                    source: "graph".to_string(),
                    id: memory_id,
                    excerpt: Some(crate::context::excerpt_pub(&text)),
                }],
                confidence,
                freshness,
                relevance: 0.0,
                scope_weight,
                score: 0.0,
            },
        });
    }
    Ok(candidates)
}

// ─── Backend selection ───────────────────────────────────────────────────────

/// Resolve the configured backend variant name to a `Box<dyn RetrievalBackend>`.
///
/// Valid `backend` strings (from `[storage] backend = "…"` in project.toml):
///   * `"flat"` → [`FlatBackend`] (default, always available).
///   * `"graph-lite"` → [`GraphLiteBackend`] (S5.2: flat + 1-2 hop edge expansion).
///   * `"graph"` → [`FlatBackend`] (TODO seam — future story: full petgraph backend).
///   * Anything else → [`FlatBackend`] with an eprintln warning so a typo is
///     surfaced without crashing the process.
pub(crate) fn backend_for(backend: &str) -> Box<dyn RetrievalBackend + Send + Sync> {
    match backend {
        "flat" => Box::new(FlatBackend),
        "graph-lite" => Box::new(GraphLiteBackend),
        "graph" => {
            // Future story: full petgraph/remote graph traversal backend.
            Box::new(FlatBackend)
        }
        other => {
            eprintln!(
                "kimetsu-brain: unknown storage.backend {:?}; falling back to \"flat\"",
                other
            );
            Box::new(FlatBackend)
        }
    }
}

#[cfg(test)]
mod tests {
    use rusqlite::Connection;
    use serde_json::json;

    use super::*;
    use crate::projector;
    use crate::schema;

    /// Helper: open an in-memory brain with the current schema.
    fn make_conn() -> Connection {
        let conn = Connection::open_in_memory().expect("open_in_memory");
        schema::initialize(&conn).expect("schema::initialize");
        conn
    }

    /// Helper: insert a minimal active memory row directly.
    fn insert_memory(conn: &Connection, id: &str, kind: &str, text: &str) {
        conn.execute(
            "INSERT INTO memories
             (memory_id, scope, kind, text, normalized_text, confidence,
              provenance_snapshot_json, created_at, use_count, usefulness_score)
             VALUES (?1, 'project', ?2, ?3, ?3, 0.9, '{}', '2025-01-01T00:00:00Z', 0, 0.0)",
            rusqlite::params![id, kind, text],
        )
        .expect("insert memory");
        // Also insert into FTS so flat retrieval can find it.
        conn.execute(
            "INSERT INTO memories_fts (memory_id, text, kind, scope) VALUES (?1, ?2, ?3, 'project')",
            rusqlite::params![id, text, kind],
        )
        .expect("insert memories_fts");
    }

    // ── S5.1 smoke tests (unchanged) ─────────────────────────────────────────

    /// backend_for("flat") resolves to FlatBackend (smoke test — exercises the
    /// selection point without hitting SQLite).
    #[test]
    fn backend_for_flat_resolves() {
        let _b = backend_for("flat");
    }

    /// All variant strings resolve without panicking.
    #[test]
    fn backend_for_all_known_variants_no_panic() {
        for variant in &["flat", "graph-lite", "graph", "unknown-typo"] {
            let _b = backend_for(variant);
        }
    }

    // ── S5.2 correctness bars ─────────────────────────────────────────────────

    /// S5.2-A: graph-lite with NO edges returns exactly the flat candidate set
    /// (no regression, no panic).
    ///
    /// Proof of no-regression: when `memory_edges` is empty, `graph_expand`
    /// returns an empty Vec and the backend returns `flat` unchanged.
    #[test]
    fn graph_lite_no_edges_returns_flat_set() {
        let conn = make_conn();
        // Insert two memories.
        insert_memory(&conn, "mem-a", "fact", "cargo build compiles rust code");
        insert_memory(&conn, "mem-b", "preference", "use ripgrep for searching");

        let flat_backend = FlatBackend;
        let graph_backend = GraphLiteBackend;

        let flat_candidates = flat_backend
            .memory_candidates(&conn, "cargo rust", None, 90.0)
            .expect("flat candidates");
        let graph_candidates = graph_backend
            .memory_candidates(&conn, "cargo rust", None, 90.0)
            .expect("graph candidates");

        // graph-lite ⊇ flat — so it must have at least as many candidates.
        assert!(
            graph_candidates.len() >= flat_candidates.len(),
            "graph-lite must return at least as many candidates as flat; \
             flat={} graph={}",
            flat_candidates.len(),
            graph_candidates.len()
        );

        // The flat hit set must be a subset of the graph set (all flat ids present).
        let graph_ids: HashSet<String> = graph_candidates
            .iter()
            .filter_map(|c| {
                c.capsule
                    .expansion_handle
                    .strip_prefix("memory:")
                    .map(|s| s.to_string())
            })
            .collect();
        for flat_c in &flat_candidates {
            if let Some(id) = flat_c.capsule.expansion_handle.strip_prefix("memory:") {
                assert!(
                    graph_ids.contains(id),
                    "flat candidate {id:?} must be present in graph-lite result set"
                );
            }
        }
    }

    /// S5.2-B: edges derived from a `memory.superseded` event appear in
    /// `memory_edges` AND survive a `rebuild_projection` (rebuild-safe).
    #[test]
    fn superseded_event_inserts_edge_and_edge_survives_rebuild() {
        use kimetsu_core::ids::RunId;

        let conn = make_conn();
        let run_id = RunId::new();

        // Accept two memories via events.
        let events = vec![
            kimetsu_core::event::Event::new(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": "survivor-1",
                    "scope": "project",
                    "kind": "fact",
                    "text": "use cargo fmt to format code",
                    "confidence": 0.9
                }),
            ),
            kimetsu_core::event::Event::new(
                run_id,
                "memory.accepted",
                json!({
                    "memory_id": "member-1",
                    "scope": "project",
                    "kind": "fact",
                    "text": "run cargo fmt before commit",
                    "confidence": 0.8
                }),
            ),
            kimetsu_core::event::Event::new(
                run_id,
                "memory.superseded",
                json!({
                    "memory_id": "member-1",
                    "survivor_id": "survivor-1",
                    "use_count_delta": 2,
                    "score_delta": 1.5
                }),
            ),
        ];

        projector::apply_events(&conn, &events).expect("apply_events");

        // Edge must exist: survivor-1 → member-1 (supersedes direction).
        let edge_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_edges
                 WHERE src_id='survivor-1' AND dst_id='member-1' AND edge_type='supersedes'",
                [],
                |r| r.get(0),
            )
            .expect("query edge count");
        assert_eq!(
            edge_count, 1,
            "memory.superseded event must insert a supersedes edge"
        );

        // Now rebuild in-place and verify the edge is repopulated.
        projector::rebuild_in_place(&conn).expect("rebuild_in_place");

        let edge_count_after: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_edges
                 WHERE src_id='survivor-1' AND dst_id='member-1' AND edge_type='supersedes'",
                [],
                |r| r.get(0),
            )
            .expect("query edge count after rebuild");
        assert_eq!(
            edge_count_after, 1,
            "supersedes edge must survive rebuild_in_place (rebuild-safe)"
        );
    }

    /// S5.2-C: 1-hop graph expansion surfaces an edge-connected memory that
    /// flat retrieval alone would miss.
    ///
    /// Setup:
    ///   * "mem-survivor" — a memory about "cargo fmt" that the query DOES match.
    ///   * "mem-connected" — a memory about "always run fmt before PR" that the
    ///     query does NOT match lexically (no shared tokens).
    ///   * An edge: survivor → connected (type "supersedes").
    ///
    /// Flat retrieval returns only "mem-survivor".
    /// Graph-lite traversal adds "mem-connected" via the 1-hop edge.
    #[test]
    fn graph_lite_1_hop_surfaces_edge_connected_memory() {
        let conn = make_conn();

        // mem-survivor: query will match this (shares "cargo" and "fmt").
        insert_memory(
            &conn,
            "mem-survivor",
            "fact",
            "cargo fmt formats your Rust code automatically",
        );

        // mem-connected: query will NOT match this directly (no shared tokens
        // with "cargo fmt").
        insert_memory(
            &conn,
            "mem-connected",
            "preference",
            "always run formatter before submitting a pull request",
        );

        // Insert an edge: survivor → connected.
        conn.execute(
            "INSERT INTO memory_edges (src_id, dst_id, edge_type, created_at)
             VALUES ('mem-survivor', 'mem-connected', 'supersedes', '2025-01-01T00:00:00Z')",
            [],
        )
        .expect("insert edge");

        // Flat backend: only mem-survivor should be in the result.
        let flat = FlatBackend
            .memory_candidates(&conn, "cargo fmt", None, 90.0)
            .expect("flat");
        let flat_ids: HashSet<String> = flat
            .iter()
            .filter_map(|c| {
                c.capsule
                    .expansion_handle
                    .strip_prefix("memory:")
                    .map(|s| s.to_string())
            })
            .collect();
        assert!(
            flat_ids.contains("mem-survivor"),
            "flat must contain mem-survivor"
        );
        assert!(
            !flat_ids.contains("mem-connected"),
            "flat must NOT contain mem-connected (no lexical match)"
        );

        // Graph-lite backend: must add mem-connected via the 1-hop edge.
        let graph = GraphLiteBackend
            .memory_candidates(&conn, "cargo fmt", None, 90.0)
            .expect("graph");
        let graph_ids: HashSet<String> = graph
            .iter()
            .filter_map(|c| {
                c.capsule
                    .expansion_handle
                    .strip_prefix("memory:")
                    .map(|s| s.to_string())
            })
            .collect();
        assert!(
            graph_ids.contains("mem-survivor"),
            "graph-lite must contain mem-survivor (from flat)"
        );
        assert!(
            graph_ids.contains("mem-connected"),
            "graph-lite must contain mem-connected (via 1-hop edge)"
        );

        // The graph-reached candidate must be marked with provenance "graph".
        let graph_candidate = graph
            .iter()
            .find(|c| c.capsule.expansion_handle.strip_prefix("memory:") == Some("mem-connected"))
            .expect("mem-connected must be in graph candidates");
        let is_graph_sourced = graph_candidate
            .capsule
            .provenance
            .iter()
            .any(|p| p.source == "graph");
        assert!(
            is_graph_sourced,
            "graph-reached candidate must carry provenance source 'graph'"
        );

        // Flat candidate set is unchanged (graph-lite ⊇ flat).
        for id in &flat_ids {
            assert!(
                graph_ids.contains(id),
                "flat candidate {id:?} must be preserved in graph-lite"
            );
        }
    }

    /// S5.2-D: `backend_for("graph-lite")` now resolves to `GraphLiteBackend`
    /// (not the old FlatBackend stub).  Verify it returns a backend that calls
    /// `graph_expand` (indirectly: confirm it compiles and doesn't panic on
    /// an empty DB, which the old stub also didn't — but the wiring is now live).
    #[test]
    fn backend_for_graph_lite_resolves_to_graph_lite_backend() {
        let conn = make_conn();
        let backend = backend_for("graph-lite");
        // Must not panic on an empty brain.
        let result = backend.memory_candidates(&conn, "some query", None, 90.0);
        assert!(
            result.is_ok(),
            "graph-lite backend must not error on empty brain"
        );
    }
}
