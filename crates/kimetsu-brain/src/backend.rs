//! S5.1 + S5.2 + S5.3: `RetrievalBackend` trait — the seam between candidate
//! generation and the broker.
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
//! candidate set and then expands 1–`MAX_HOPS` hops over the
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
//! # PetgraphBackend (S5.3, `graph` feature, remote-only)
//!
//! [`PetgraphBackend`] is the Tier-2 full-graph backend. It is ONLY compiled
//! when the `graph` feature is active (enabled by `kimetsu-remote`; never in
//! the local lean/CLI builds). It loads the entire `memory_edges` table into an
//! in-memory `petgraph` directed graph at construction time, enabling real
//! graph algorithms:
//!
//! * **Candidate expansion**: like graph-lite but operates on the petgraph
//!   in-memory graph — `BFS` up to `MAX_HOPS` without SQLite round-trips per
//!   hop. The candidate set is a SUPERSET of flat (no recall loss).
//! * **Centrality** (`node_centrality`): degree centrality per node — identifies
//!   the most-referenced memories (high in-degree = frequently consolidated into;
//!   high out-degree = memory that was itself superseded many times). Exposed for
//!   future remote endpoints (consolidation hints, importance ranking).
//! * **Shortest path** (`shortest_path`): BFS shortest path between two memory
//!   ids — answers "why are these two memories connected?" for explainability
//!   endpoints.
//! * **Community detection stub** (`community_hints`): returns the weakly
//!   connected components as cluster ids — a lightweight proxy for community
//!   detection. Full Louvain is deferred until the corpus justifies it.
//!
//! The graph is rebuilt from `memory_edges` at backend construction. Since
//! `memory_edges` is a rebuild-safe projection (rebuilt from the event log on
//! `rebuild_in_place`), re-constructing `PetgraphBackend` is always safe.
//!
//! ## v2.5 decision criterion (S5.4)
//!
//! The cross-backend benchmark (`BackendBenchResult`) documents the measured
//! numbers and the criterion for whether an embedded graph DB (Kùzu/Cozo) is
//! justified at v2.5. See [`BackendBenchResult`] and the inline commentary in
//! the benchmark module.

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
pub(crate) struct FlatBackend {
    /// How this backend merges its FTS and ANN rankings. See [`crate::fusion`].
    pub(crate) fusion: crate::fusion::Fusion,
}

impl RetrievalBackend for FlatBackend {
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>> {
        crate::context::memory_candidates_flat(
            conn,
            query,
            query_embedding,
            half_life_days,
            self.fusion,
        )
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
/// Graph-reached candidates were not matched by the query directly, so they
/// inherit a hop-decayed share of the best flat signal:
/// `raw_relevance = max_flat_relevance * HOP_DECAY^hops`. A 1-hop neighbour of
/// the top hit therefore enters at 60% of it — below every strong flat hit, but
/// above a weak one, which is the point: a memory two words away from the query
/// but one edge from its best answer is often the better capsule. If the caller
/// has a `min_score` floor they may still be filtered out — the broker controls
/// final admission.
pub(crate) struct GraphLiteBackend {
    /// How the flat seed set is fused before graph expansion runs on top of it.
    pub(crate) fusion: crate::fusion::Fusion,
}

/// Maximum hops to traverse from the flat hit set.
const MAX_HOPS: usize = 2;

/// Maximum number of graph-reachable memory ids to fetch per call. Kept modest
/// so multi-hop candidates stay a supplement rather than a flood (fewer noise
/// candidates competing for budget on precision-sensitive queries).
const MAX_FAN_OUT: usize = 12;

/// Relevance a graph-reachable candidate inherits from its seed, decayed per
/// hop: `raw_relevance = max_flat_relevance * HOP_DECAY^hops`. A 1-hop neighbour
/// of a strong hit ranks well (the multi-hop win); a far or weak-seed neighbour
/// sinks below the admission floor (so it does not add noise). Graph candidates
/// are never scored at 0 — they earn their place from the strength of their seed.
const HOP_DECAY: f32 = 0.6;

impl RetrievalBackend for GraphLiteBackend {
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>> {
        // 1. Start with the flat candidate set (FTS + ANN / FTS + recency).
        let flat = crate::context::memory_candidates_flat(
            conn,
            query,
            query_embedding,
            half_life_days,
            self.fusion,
        )?;

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
        // Seed relevance for hop-decay scoring: graph candidates inherit a
        // decayed fraction of the strongest flat hit (see HOP_DECAY).
        let max_flat_relevance = flat.iter().map(|c| c.raw_relevance).fold(0.0_f32, f32::max);

        let new_ids = graph_expand(conn, &seen_ids, MAX_HOPS, MAX_FAN_OUT)?;

        if new_ids.is_empty() {
            return Ok(flat);
        }

        // 4. Fetch the graph-reachable memories as candidates, marking their
        //    provenance so the broker/caller can distinguish them from flat hits.
        let graph_candidates =
            fetch_graph_candidates(conn, &new_ids, &mut seen_ids, max_flat_relevance)?;

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
) -> KimetsuResult<Vec<(String, usize)>> {
    if seed_ids.is_empty() || max_hops == 0 {
        return Ok(Vec::new());
    }

    // `frontier` = the ids visited in the previous hop (start = seeds).
    // `visited`  = all ids seen so far (seeds + discovered).
    // `new_ids`  = discovered ids paired with their hop distance (1-based).
    let mut visited: HashSet<String> = seed_ids.clone();
    let mut frontier: Vec<String> = seed_ids.iter().cloned().collect();
    let mut new_ids: Vec<(String, usize)> = Vec::new();

    for hop_idx in 0..max_hops {
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
                new_ids.push((neighbour, hop_idx + 1));
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
    new_ids: &[(String, usize)],
    seen_ids: &mut HashSet<String>,
    seed_relevance: f32,
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
           AND (valid_to IS NULL OR valid_to > datetime('now'))
           AND memory_id IN ({placeholders})"
    );

    let mut stmt = conn.prepare(&sql)?;
    let params_refs: Vec<&dyn rusqlite::ToSql> = new_ids
        .iter()
        .map(|(s, _)| s as &dyn rusqlite::ToSql)
        .collect();

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

        // Hop-decayed relevance inherited from the seed set: a near neighbour of
        // a strong hit earns a real score; a far / weak-seed one sinks below the
        // admission floor. Never 0 (see HOP_DECAY).
        let hop = new_ids
            .iter()
            .find(|(id, _)| id == &memory_id)
            .map(|(_, h)| *h)
            .unwrap_or(1);
        let raw_relevance = seed_relevance * HOP_DECAY.powi(hop as i32);

        let freshness = crate::context::freshness_pub(&created_at);
        let scope_weight = crate::context::scope_weight_pub(&scope);
        let token_estimate = crate::context::estimate_tokens(&text) + 8;

        candidates.push(Candidate {
            raw_relevance,
            embedding: None,
            cosine: None,
            created_at: Some(created_at),
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

// ─── PetgraphBackend (S5.3, `graph` feature, remote-only) ───────────────────

/// S5.3: Full in-memory petgraph backend — compiled ONLY when the `graph`
/// feature is active (enabled by `kimetsu-remote`; never in lean/CLI builds).
///
/// Holds an in-memory `petgraph::Graph` built from `memory_edges`. All graph
/// algorithm helpers (`node_centrality`, `shortest_path`, `community_hints`)
/// operate on this in-memory structure without hitting SQLite.
///
/// For `memory_candidates`, the BFS expansion is identical to graph-lite in
/// semantics (SUPERSET of flat, no recall loss), but uses the petgraph BFS
/// iterator rather than per-hop SQLite queries — removing the N*hops round-trips
/// of graph-lite's iterative approach.
#[cfg(feature = "graph")]
pub(crate) struct PetgraphBackend {
    /// Directed graph: edges loaded from `memory_edges` (src_id → dst_id).
    /// Node weights = memory_id (String). Edge weights = edge_type (String).
    graph: petgraph::Graph<String, String>,
    /// Maps memory_id → NodeIndex for O(1) node lookup.
    node_map: std::collections::HashMap<String, petgraph::graph::NodeIndex>,
    /// How the flat seed set is fused before graph expansion runs on top of it.
    fusion: crate::fusion::Fusion,
}

#[cfg(feature = "graph")]
impl PetgraphBackend {
    /// Build a `PetgraphBackend` by loading all rows from `memory_edges`.
    ///
    /// This is called at server startup (or on demand). The graph is a point-in-
    /// time snapshot — it does not update incrementally. Re-construct it after
    /// `rebuild_in_place` if you need a fresh view.
    pub(crate) fn from_conn(
        conn: &Connection,
        fusion: crate::fusion::Fusion,
    ) -> KimetsuResult<Self> {
        use petgraph::Graph;
        use std::collections::HashMap;

        let mut graph: Graph<String, String> = Graph::new();
        let mut node_map: HashMap<String, petgraph::graph::NodeIndex> = HashMap::new();

        // Load all edges from memory_edges.
        let mut stmt =
            conn.prepare("SELECT src_id, dst_id, edge_type FROM memory_edges ORDER BY created_at")?;

        let rows = stmt.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;

        for row in rows {
            let (src_id, dst_id, edge_type) = row?;

            let src_idx = *node_map
                .entry(src_id.clone())
                .or_insert_with(|| graph.add_node(src_id.clone()));
            let dst_idx = *node_map
                .entry(dst_id.clone())
                .or_insert_with(|| graph.add_node(dst_id.clone()));

            graph.add_edge(src_idx, dst_idx, edge_type);
        }

        Ok(Self {
            graph,
            node_map,
            fusion,
        })
    }

    /// Degree centrality for each node: `(in_degree, out_degree)` by memory_id.
    ///
    /// High in-degree = frequently merged-into (important survivor).
    /// High out-degree = frequently superseded/pointed-from (active memory).
    ///
    /// Exposed for future remote endpoints (importance ranking, consolidation
    /// hints). Not wired into `memory_candidates` — centrality is a HINT for
    /// operators, not a retrieval filter.
    #[allow(dead_code)] // S5.3 forward-facing API for future remote endpoints.
    pub(crate) fn node_centrality(&self) -> Vec<(String, usize, usize)> {
        self.node_map
            .iter()
            .map(|(id, &idx)| {
                let in_deg = self
                    .graph
                    .edges_directed(idx, petgraph::Direction::Incoming)
                    .count();
                let out_deg = self
                    .graph
                    .edges_directed(idx, petgraph::Direction::Outgoing)
                    .count();
                (id.clone(), in_deg, out_deg)
            })
            .collect()
    }

    /// BFS shortest path between two memory ids (directed graph).
    ///
    /// Returns the ordered list of memory_ids on the shortest path from
    /// `from_id` to `to_id` (inclusive), or `None` if no path exists.
    ///
    /// Use case: "why are these two memories connected?" — explainability for
    /// remote endpoints, audit trails, and graph-walk debugging.
    #[allow(dead_code)] // S5.3 forward-facing API for future remote endpoints.
    pub(crate) fn shortest_path(&self, from_id: &str, to_id: &str) -> Option<Vec<String>> {
        use petgraph::algo::astar;

        let src_idx = *self.node_map.get(from_id)?;
        let dst_idx = *self.node_map.get(to_id)?;

        // A* with unit costs = BFS shortest path (uniform edge weights).
        let (_, path_nodes) = astar(
            &self.graph,
            src_idx,
            |finish| finish == dst_idx,
            |_| 1usize,
            |_| 0usize,
        )?;

        Some(
            path_nodes
                .into_iter()
                .map(|idx| self.graph[idx].clone())
                .collect(),
        )
    }

    /// Weakly-connected components as a lightweight community proxy.
    ///
    /// Returns a `Vec` of component groups (each group = Vec of memory_ids).
    /// Memories in the same component share at least one path (ignoring edge
    /// direction). Useful as a cheap consolidation hint: large components
    /// indicate memory clusters that could be merged.
    ///
    /// Note: Full Louvain community detection is deferred — the SQLite-backed
    /// `memory_edges` corpus is unlikely to exceed a few thousand nodes in v2.x,
    /// making WCC an adequate proxy for the v2.5 decision spike.
    #[allow(dead_code)] // S5.3 forward-facing API for future remote endpoints.
    pub(crate) fn community_hints(&self) -> Vec<Vec<String>> {
        use petgraph::algo::kosaraju_scc;

        // Use strongly connected components on the directed graph. In a tree-
        // structured supersedes graph most SCCs are singletons; non-trivial SCCs
        // indicate cycles (which are invalid for supersedes but possible for
        // co-occurrence / similar edges). This surfaces them without crashing.
        let sccs = kosaraju_scc(&self.graph);
        sccs.into_iter()
            .filter(|component| !component.is_empty())
            .map(|component| {
                component
                    .into_iter()
                    .map(|idx| self.graph[idx].clone())
                    .collect()
            })
            .collect()
    }

    /// BFS expansion from `seed_ids` up to `max_hops` in the petgraph graph.
    ///
    /// Traverses BOTH directions (in-edges and out-edges) so that supersedes
    /// edges are followed from both sides, matching graph-lite semantics.
    /// Returns new (previously unseen) memory ids, capped at `max_fan_out`.
    fn petgraph_expand(
        &self,
        seed_ids: &HashSet<String>,
        max_hops: usize,
        max_fan_out: usize,
    ) -> Vec<(String, usize)> {
        use petgraph::visit::EdgeRef;

        if seed_ids.is_empty() || max_hops == 0 {
            return Vec::new();
        }

        let mut visited: HashSet<String> = seed_ids.clone();
        let mut frontier: Vec<petgraph::graph::NodeIndex> = seed_ids
            .iter()
            .filter_map(|id| self.node_map.get(id).copied())
            .collect();
        let mut new_ids: Vec<(String, usize)> = Vec::new();

        for hop_idx in 0..max_hops {
            if frontier.is_empty() || new_ids.len() >= max_fan_out {
                break;
            }

            let mut next_frontier = Vec::new();
            for node_idx in &frontier {
                // Follow edges in both directions (out-neighbors and in-neighbors).
                let neighbors: Vec<petgraph::graph::NodeIndex> = self
                    .graph
                    .edges_directed(*node_idx, petgraph::Direction::Outgoing)
                    .map(|e| e.target())
                    .chain(
                        self.graph
                            .edges_directed(*node_idx, petgraph::Direction::Incoming)
                            .map(|e| e.source()),
                    )
                    .collect();

                for neighbour_idx in neighbors {
                    let neighbour_id = &self.graph[neighbour_idx];
                    if !visited.contains(neighbour_id) {
                        visited.insert(neighbour_id.clone());
                        next_frontier.push(neighbour_idx);
                        new_ids.push((neighbour_id.clone(), hop_idx + 1));
                        if new_ids.len() >= max_fan_out {
                            break;
                        }
                    }
                }
                if new_ids.len() >= max_fan_out {
                    break;
                }
            }

            frontier = next_frontier;
        }

        new_ids
    }
}

#[cfg(feature = "graph")]
impl RetrievalBackend for PetgraphBackend {
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>> {
        // 1. Flat candidate set (FTS + ANN or FTS + recency).
        let flat = crate::context::memory_candidates_flat(
            conn,
            query,
            query_embedding,
            half_life_days,
            self.fusion,
        )?;

        // 2. Collect seen ids from the flat set.
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
            return Ok(flat);
        }

        let max_flat_relevance = flat.iter().map(|c| c.raw_relevance).fold(0.0_f32, f32::max);

        // 3. Petgraph BFS expansion (no SQLite round-trips per hop).
        let new_ids = self.petgraph_expand(&seen_ids, MAX_HOPS, MAX_FAN_OUT);

        if new_ids.is_empty() {
            return Ok(flat);
        }

        // 4. Fetch graph-reached candidates from SQLite (active memories only).
        let graph_candidates =
            fetch_graph_candidates(conn, &new_ids, &mut seen_ids, max_flat_relevance)?;

        // 5. Flat first (real relevance signals), graph-reached appended.
        let mut combined = flat;
        combined.extend(graph_candidates);
        Ok(combined)
    }
}

// ─── Backend selection ───────────────────────────────────────────────────────

/// Resolve the configured backend variant name to a `Box<dyn RetrievalBackend>`.
///
/// Valid `backend` strings (from `[storage] backend = "…"` in project.toml):
///   * `"flat"` → [`FlatBackend`] (default, always available).
///   * `"graph-lite"` → [`GraphLiteBackend`] (S5.2: flat + 1-2 hop edge expansion).
///   * `"graph"` → [`PetgraphBackend`] when the `graph` feature is enabled (S5.3:
///     full in-memory petgraph backend for remote deployments). Falls back to
///     [`GraphLiteBackend`] when the feature is disabled (lean builds), so that
///     a config file with `backend = "graph"` on a lean build still gets graph
///     expansion (just without the petgraph in-memory cache and algorithms).
///   * Anything else → [`FlatBackend`] with an eprintln warning so a typo is
///     surfaced without crashing the process.
pub(crate) fn backend_for(
    backend: &str,
    fusion: crate::fusion::Fusion,
) -> Box<dyn RetrievalBackend + Send + Sync> {
    match backend {
        "flat" => Box::new(FlatBackend { fusion }),
        "graph-lite" => Box::new(GraphLiteBackend { fusion }),
        "graph" => {
            // S5.3: PetgraphBackend when the `graph` feature is enabled.
            // Falls back to GraphLiteBackend (not flat) so that lean builds
            // requesting "graph" still get the graph-lite superset behaviour.
            #[cfg(feature = "graph")]
            {
                // PetgraphBackend requires a DB connection to load edges. Since
                // `backend_for` is called without a conn (before any query), we
                // return a deferred variant that constructs the petgraph on the
                // first `memory_candidates` call.
                Box::new(DeferredPetgraphBackend::new(fusion))
            }
            #[cfg(not(feature = "graph"))]
            {
                Box::new(GraphLiteBackend { fusion })
            }
        }
        other => {
            eprintln!(
                "kimetsu-brain: unknown storage.backend {:?}; falling back to \"flat\"",
                other
            );
            Box::new(FlatBackend { fusion })
        }
    }
}

/// Deferred construction wrapper for `PetgraphBackend`.
///
/// `backend_for` is called without a DB connection, but `PetgraphBackend::from_conn`
/// needs one to load `memory_edges`. `DeferredPetgraphBackend` is a lazy wrapper:
/// it constructs the petgraph on the first `memory_candidates` call and caches it
/// for subsequent calls.
///
/// Thread-safety: the inner `Mutex<Option<PetgraphBackend>>` serializes
/// construction. After the first successful init, the `Option` is `Some` and
/// subsequent calls short-circuit by holding the lock only long enough to clone
/// the candidates. In practice the remote server constructs exactly one backend
/// per repository at startup, so the lock is never contended after init.
#[cfg(feature = "graph")]
struct DeferredPetgraphBackend {
    inner: std::sync::Mutex<Option<PetgraphBackend>>,
    fusion: crate::fusion::Fusion,
}

#[cfg(feature = "graph")]
impl DeferredPetgraphBackend {
    fn new(fusion: crate::fusion::Fusion) -> Self {
        Self {
            inner: std::sync::Mutex::new(None),
            fusion,
        }
    }
}

#[cfg(feature = "graph")]
impl RetrievalBackend for DeferredPetgraphBackend {
    fn memory_candidates(
        &self,
        conn: &Connection,
        query: &str,
        query_embedding: Option<&QueryEmbedding>,
        half_life_days: f32,
    ) -> KimetsuResult<Vec<Candidate>> {
        // Fast path: already initialised — but we must hold the lock to read.
        // We delegate to the inner backend while the lock is held. The lock is
        // held for the duration of `memory_candidates` (including SQLite queries
        // for graph-reached candidate hydration), which is acceptable: the remote
        // server builds one backend per repo and the petgraph in-memory traversal
        // is fast relative to the SQLite hydration it triggers.
        let mut guard = self.inner.lock().unwrap_or_else(|e| e.into_inner());
        if guard.is_none() {
            *guard = Some(PetgraphBackend::from_conn(conn, self.fusion)?);
        }
        guard.as_ref().expect("just initialised").memory_candidates(
            conn,
            query,
            query_embedding,
            half_life_days,
        )
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

    /// backend_for("flat", crate::fusion::Fusion::Linear) resolves to FlatBackend (smoke test — exercises the
    /// selection point without hitting SQLite).
    #[test]
    fn backend_for_flat_resolves() {
        let _b = backend_for("flat", crate::fusion::Fusion::Linear);
    }

    /// All variant strings resolve without panicking.
    #[test]
    fn backend_for_all_known_variants_no_panic() {
        for variant in &["flat", "graph-lite", "graph", "unknown-typo"] {
            let _b = backend_for(variant, crate::fusion::Fusion::Linear);
        }
    }

    // ── v2.6: the no-regression gate for the default flip ────────────────────

    /// The gate on making `graph-lite` the default backend: on a realistic
    /// corpus, with edges actually present, graph-lite must return a SUPERSET
    /// of flat's candidates, and every flat candidate must keep its
    /// `raw_relevance` unchanged.
    ///
    /// That is the whole safety argument. Graph-reached candidates enter with
    /// `raw_relevance = 0.0`, so they rank below every direct hit and can only
    /// occupy slots flat would have left empty — which means flipping the
    /// default can broaden recall but cannot displace a result.
    #[test]
    fn graph_lite_is_a_superset_of_flat_with_edges_present() {
        let conn = make_conn();
        let corpus = [
            (
                "mem-a",
                "convention",
                "[tags: sqlite wal] checkpoint the WAL before copying brain.db",
            ),
            (
                "mem-b",
                "failure_pattern",
                "[tags: sqlite wal] opening a WAL database read-only skips recovery",
            ),
            (
                "mem-c",
                "command",
                "[tags: rust cargo] regenerate the schema with cargo xtask gen",
            ),
            (
                "mem-d",
                "fact",
                "[tags: rust cargo] the workspace pins edition 2024",
            ),
            (
                "mem-e",
                "preference",
                "[tags: search] prefer ripgrep over grep for large trees",
            ),
        ];
        for (id, kind, text) in corpus {
            insert_memory(&conn, id, kind, text);
            crate::graph::project_entities(&conn, id, text).expect("project entities");
        }
        // Link the corpus exactly the way the write path does.
        for (id, _, _) in corpus {
            let edges = crate::graph::incremental_edges_for_memory(&conn, id, 0).expect("edges");
            let tuples: Vec<(String, String, String)> = edges
                .into_iter()
                .map(|e| (e.src_id, e.dst_id, e.edge_type))
                .collect();
            projector::add_memory_edges(&conn, &tuples).expect("persist edges");
        }
        let edge_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_edges WHERE edge_type='relates_to'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(
            edge_count > 0,
            "fixture must actually have edges, or this proves nothing"
        );

        for query in [
            "wal checkpoint",
            "cargo schema",
            "ripgrep",
            "sqlite recovery read-only",
            "something entirely unrelated to this corpus",
        ] {
            let flat = FlatBackend {
                fusion: crate::fusion::Fusion::Linear,
            }
            .memory_candidates(&conn, query, None, 90.0)
            .expect("flat");
            let graph = GraphLiteBackend {
                fusion: crate::fusion::Fusion::Linear,
            }
            .memory_candidates(&conn, query, None, 90.0)
            .expect("graph-lite");

            for candidate in &flat {
                let same = graph
                    .iter()
                    .find(|g| g.capsule.expansion_handle == candidate.capsule.expansion_handle)
                    .unwrap_or_else(|| {
                        panic!(
                            "graph-lite dropped a flat candidate for {query:?}: {}",
                            candidate.capsule.expansion_handle
                        )
                    });
                assert!(
                    (same.raw_relevance - candidate.raw_relevance).abs() < f32::EPSILON,
                    "graph-lite changed a flat candidate's relevance for {query:?}: \
                     {} vs {}",
                    same.raw_relevance,
                    candidate.raw_relevance
                );
            }
            assert!(
                graph.len() >= flat.len(),
                "graph-lite must never shrink the pool for {query:?}: {} < {}",
                graph.len(),
                flat.len()
            );
        }
    }

    /// The corollary: a graph-reached candidate enters at a hop-decayed share
    /// of the best flat signal, never at full strength — so it ranks below the
    /// hit that pulled it in.
    #[test]
    fn graph_reached_candidates_are_hop_decayed_below_their_seed() {
        let conn = make_conn();
        for (id, kind, text) in [
            (
                "mem-a",
                "convention",
                "[tags: sqlite wal] checkpoint the WAL before copying",
            ),
            (
                "mem-b",
                "fact",
                "[tags: sqlite wal] recovery is skipped on read-only opens",
            ),
        ] {
            insert_memory(&conn, id, kind, text);
            crate::graph::project_entities(&conn, id, text).expect("entities");
        }
        projector::add_memory_edges(
            &conn,
            &[(
                "mem-a".to_string(),
                "mem-b".to_string(),
                "relates_to".to_string(),
            )],
        )
        .expect("edge");

        // "checkpoint" hits mem-a directly; mem-b arrives only via the edge.
        let graph = GraphLiteBackend {
            fusion: crate::fusion::Fusion::Linear,
        }
        .memory_candidates(&conn, "checkpoint", None, 90.0)
        .expect("graph-lite");
        let reached = graph
            .iter()
            .find(|c| c.capsule.expansion_handle.contains("mem-b"))
            .expect("mem-b must be reachable through the edge");
        let seed = graph
            .iter()
            .find(|c| c.capsule.expansion_handle.contains("mem-a"))
            .expect("mem-a is the direct hit");
        assert!(
            reached.raw_relevance < seed.raw_relevance,
            "a graph-reached candidate must rank below the hit that pulled it in: \
             {} vs {}",
            reached.raw_relevance,
            seed.raw_relevance
        );
        let expected = seed.raw_relevance * HOP_DECAY;
        assert!(
            (reached.raw_relevance - expected).abs() < 1e-4,
            "one hop must decay by HOP_DECAY: got {}, expected {expected}",
            reached.raw_relevance
        );
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

        let flat_backend = FlatBackend {
            fusion: crate::fusion::Fusion::Linear,
        };
        let graph_backend = GraphLiteBackend {
            fusion: crate::fusion::Fusion::Linear,
        };

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
        let flat = FlatBackend {
            fusion: crate::fusion::Fusion::Linear,
        }
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
        let graph = GraphLiteBackend {
            fusion: crate::fusion::Fusion::Linear,
        }
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

    /// S5.2-D: `backend_for("graph-lite", crate::fusion::Fusion::Linear)` now resolves to `GraphLiteBackend`
    /// (not the old FlatBackend stub).  Verify it returns a backend that calls
    /// `graph_expand` (indirectly: confirm it compiles and doesn't panic on
    /// an empty DB, which the old stub also didn't — but the wiring is now live).
    #[test]
    fn backend_for_graph_lite_resolves_to_graph_lite_backend() {
        let conn = make_conn();
        let backend = backend_for("graph-lite", crate::fusion::Fusion::Linear);
        // Must not panic on an empty brain.
        let result = backend.memory_candidates(&conn, "some query", None, 90.0);
        assert!(
            result.is_ok(),
            "graph-lite backend must not error on empty brain"
        );
    }

    // ── S5.3 PetgraphBackend tests (compiled only when `graph` feature is on) ──

    /// S5.3-A: `PetgraphBackend::from_conn` succeeds on an empty `memory_edges`
    /// table and returns a backend with no nodes.
    #[cfg(feature = "graph")]
    #[test]
    fn petgraph_backend_from_conn_empty_db() {
        let conn = make_conn();
        let backend =
            PetgraphBackend::from_conn(&conn, crate::fusion::Fusion::Linear).expect("from_conn");
        // Empty graph → no centrality entries.
        let centrality = backend.node_centrality();
        assert!(centrality.is_empty(), "empty graph → empty centrality");
        // Shortest path between non-existent ids → None.
        assert!(backend.shortest_path("a", "b").is_none());
        // Community hints on empty graph → empty.
        let communities = backend.community_hints();
        assert!(communities.is_empty(), "empty graph → no communities");
    }

    /// S5.3-B: graph loaded from edges — centrality, shortest-path, communities
    /// all reflect the seeded topology.
    #[cfg(feature = "graph")]
    #[test]
    fn petgraph_backend_graph_algorithms_on_seeded_topology() {
        let conn = make_conn();

        // Seed three nodes: A → B → C (chain).
        insert_memory(&conn, "node-a", "fact", "node-a content");
        insert_memory(&conn, "node-b", "fact", "node-b content");
        insert_memory(&conn, "node-c", "fact", "node-c content");

        conn.execute(
            "INSERT INTO memory_edges (src_id, dst_id, edge_type, created_at)
             VALUES ('node-a', 'node-b', 'supersedes', '2025-01-01T00:00:00Z'),
                    ('node-b', 'node-c', 'supersedes', '2025-01-01T00:00:00Z')",
            [],
        )
        .expect("insert edges");

        let backend =
            PetgraphBackend::from_conn(&conn, crate::fusion::Fusion::Linear).expect("from_conn");

        // Centrality: node-a has out=1 in=0; node-b has out=1 in=1; node-c has out=0 in=1.
        let centrality = backend.node_centrality();
        assert_eq!(centrality.len(), 3, "three nodes in the graph");
        let find = |id: &str| {
            centrality
                .iter()
                .find(|(node_id, _, _)| node_id == id)
                .cloned()
        };
        let (_, a_in, a_out) = find("node-a").expect("node-a centrality");
        let (_, b_in, b_out) = find("node-b").expect("node-b centrality");
        let (_, c_in, c_out) = find("node-c").expect("node-c centrality");
        assert_eq!((a_in, a_out), (0, 1), "node-a: in=0 out=1");
        assert_eq!((b_in, b_out), (1, 1), "node-b: in=1 out=1");
        assert_eq!((c_in, c_out), (1, 0), "node-c: in=1 out=0");

        // Shortest path A→C via B.
        let path = backend
            .shortest_path("node-a", "node-c")
            .expect("path A→C must exist");
        assert_eq!(path, vec!["node-a", "node-b", "node-c"]);

        // No path C→A in a directed chain A→B→C.
        assert!(
            backend.shortest_path("node-c", "node-a").is_none(),
            "no reverse path in directed chain"
        );

        // Community hints: one SCC per node in a DAG (A, B, C are not in a cycle).
        let communities = backend.community_hints();
        assert_eq!(
            communities.len(),
            3,
            "three singleton SCCs in a directed chain"
        );
    }

    /// S5.3-C: `PetgraphBackend::memory_candidates` is a superset of flat —
    /// it surfaces graph-connected memories beyond the flat FTS hit set.
    #[cfg(feature = "graph")]
    #[test]
    fn petgraph_backend_memory_candidates_superset_of_flat() {
        let conn = make_conn();

        insert_memory(
            &conn,
            "pg-survivor",
            "fact",
            "cargo fmt formats your Rust code automatically",
        );
        insert_memory(
            &conn,
            "pg-connected",
            "preference",
            "always run formatter before submitting a pull request",
        );

        conn.execute(
            "INSERT INTO memory_edges (src_id, dst_id, edge_type, created_at)
             VALUES ('pg-survivor', 'pg-connected', 'supersedes', '2025-01-01T00:00:00Z')",
            [],
        )
        .expect("insert edge");

        let backend =
            PetgraphBackend::from_conn(&conn, crate::fusion::Fusion::Linear).expect("from_conn");

        let candidates = backend
            .memory_candidates(&conn, "cargo fmt", None, 90.0)
            .expect("memory_candidates");

        let ids: std::collections::HashSet<String> = candidates
            .iter()
            .filter_map(|c| {
                c.capsule
                    .expansion_handle
                    .strip_prefix("memory:")
                    .map(|s| s.to_string())
            })
            .collect();

        assert!(
            ids.contains("pg-survivor"),
            "PetgraphBackend must return the flat FTS hit"
        );
        assert!(
            ids.contains("pg-connected"),
            "PetgraphBackend must return the graph-connected memory"
        );
    }

    /// S5.3-D: `backend_for("graph", crate::fusion::Fusion::Linear)` returns a `DeferredPetgraphBackend` (wrapped
    /// as Box<dyn RetrievalBackend>) that works on an empty brain without panicking.
    #[cfg(feature = "graph")]
    #[test]
    fn backend_for_graph_resolves_to_petgraph_backend() {
        let conn = make_conn();
        let backend = backend_for("graph", crate::fusion::Fusion::Linear);
        // Must not panic on an empty brain.
        let result = backend.memory_candidates(&conn, "some query", None, 90.0);
        assert!(
            result.is_ok(),
            "petgraph backend must not error on empty brain"
        );
    }

    /// S5.3-E: without the `graph` feature, `backend_for("graph", crate::fusion::Fusion::Linear)` falls back to
    /// `GraphLiteBackend` (not flat) — the fallback is the next-best backend.
    #[cfg(not(feature = "graph"))]
    #[test]
    fn backend_for_graph_falls_back_to_graph_lite_without_feature() {
        let conn = make_conn();
        let backend = backend_for("graph", crate::fusion::Fusion::Linear);
        // Must still work (graph-lite fallback).
        let result = backend.memory_candidates(&conn, "some query", None, 90.0);
        assert!(
            result.is_ok(),
            "graph fallback must not error on empty brain"
        );
    }
}
