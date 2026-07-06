//! Consolidation v1 (v2.5.2): model-free reinforcement structures built from
//! citation outcomes. Two mechanisms, both $0 / no LLM:
//!
//! * **Co-citation stapling** — memories cited TOGETHER (same `brain cite`
//!   call, i.e. same citation run) at least [`scoring::STAPLE_MIN_CO_CITES`]
//!   times are stapled into ONE new fact memory (concatenated text, merged
//!   metadata, provenance carrying the part ids). This precomputes the
//!   multi-hop join: future retrieval finds the complete evidence in one hit.
//!   Originals are KEPT (two-tier, CLS-style: staple = index entry, not a
//!   replacement). Lineage: Small 1973 co-citation; SOAR chunking (success-
//!   gated, mechanical); HeLa-Mem density hubs.
//!
//! * **Query-association routing** — citations recorded with `--query` build
//!   a derived `query_routes` table (query text + embedding -> memory ids +
//!   cite counts). At retrieval time, [`apply_query_routing`] gives memories
//!   associated with SIMILAR past successful queries a bounded additive
//!   boost. Lineage: Rocchio relevance feedback; click-graph routing
//!   (Craswell & Szummer); ACT-R associative strength with a fixed source-
//!   activation budget and power-law decay.
//!
//! Both run OFFLINE via `kimetsu brain reinforce` (never in the retrieval
//! hot path); the boost lookup at retrieval time is one indexed SQL read.

use std::collections::BTreeMap;
use std::path::Path;

use kimetsu_core::KimetsuResult;
use kimetsu_core::memory::{MemoryKind, MemoryScope};
use rusqlite::{Connection, params};
use time::OffsetDateTime;

use crate::context::QueryEmbedding;
use crate::embeddings::{decode_embedding, encode_embedding};
use crate::project::{add_memory, load_project};
use crate::scoring::{
    ROUTE_DECAY_EXPONENT, ROUTE_MIN_CITES, ROUTE_QUERY_SIM_FLOOR, ROUTING_BOOST_CAP,
    ROUTING_BUDGET, STAPLE_MIN_CO_CITES,
};

/// Cap on how many memories one staple may bind. Components larger than this
/// are truncated to the most co-cited members — a giant staple is a summary
/// nobody asked for, not a precomputed join.
const STAPLE_MAX_PARTS: usize = 4;

/// (query_norm, memory_id, cites, last_cited_at, query_embedding)
type RouteRow = (String, String, i64, String, Option<Vec<u8>>);

#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct ReinforceSummary {
    /// Qualifying co-citation components found.
    pub staple_candidates: usize,
    /// Staples actually written (dedup'd re-runs write nothing).
    pub staples_created: usize,
    /// Distinct (query, memory) routes in the rebuilt table.
    pub routes_built: usize,
    /// Routes whose query got an embedding (0 on lean builds).
    pub routes_embedded: usize,
}

/// Run the offline consolidation pass: staple qualifying co-citations and/or
/// rebuild the query-routes table. Both idempotent; safe to run every
/// session end or between benchmark iterations.
pub fn reinforce(start: &Path, staple: bool, routes: bool) -> KimetsuResult<ReinforceSummary> {
    let mut summary = ReinforceSummary::default();
    if staple {
        let (cands, created) = staple_co_citations(start)?;
        summary.staple_candidates = cands;
        summary.staples_created = created;
    }
    if routes {
        let (built, embedded) = build_query_routes(start)?;
        summary.routes_built = built;
        summary.routes_embedded = embedded;
    }
    Ok(summary)
}

/// Find groups of memories repeatedly cited together and staple each group
/// into one consolidated fact memory. Returns (candidate_components,
/// staples_created).
fn staple_co_citations(start: &Path) -> KimetsuResult<(usize, usize)> {
    let (_paths, _config, conn) = load_project(start)?;

    // Pairs cited together in the same citation group (same run_id), counted
    // by DISTINCT group so one giant group doesn't inflate the count. The
    // legacy nil sentinel run is excluded: every pre-v2.5.2 CLI citation
    // shares it, which would co-cite everything with everything.
    let mut stmt = conn.prepare(
        "
        SELECT a.memory_id, b.memory_id, COUNT(DISTINCT a.run_id) AS co
        FROM memory_citations a
        JOIN memory_citations b
          ON a.run_id = b.run_id AND a.memory_id < b.memory_id
        WHERE a.run_id != '00000000000000000000000000'
        GROUP BY a.memory_id, b.memory_id
        HAVING co >= ?1
        ",
    )?;
    let pairs: Vec<(String, String)> = stmt
        .query_map(params![STAPLE_MIN_CO_CITES], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<_, _>>()?;
    if pairs.is_empty() {
        return Ok((0, 0));
    }

    // Union-find over qualifying pairs -> connected components. A trio that
    // answers together becomes ONE staple, not three overlapping ones.
    let mut parent: BTreeMap<String, String> = BTreeMap::new();
    fn find(parent: &mut BTreeMap<String, String>, x: &str) -> String {
        let p = parent.get(x).cloned().unwrap_or_else(|| x.to_string());
        if p == x {
            return p;
        }
        let root = find(parent, &p);
        parent.insert(x.to_string(), root.clone());
        root
    }
    for (a, b) in &pairs {
        parent.entry(a.clone()).or_insert_with(|| a.clone());
        parent.entry(b.clone()).or_insert_with(|| b.clone());
        let (ra, rb) = (find(&mut parent, a), find(&mut parent, b));
        if ra != rb {
            parent.insert(ra, rb);
        }
    }
    let mut components: BTreeMap<String, Vec<String>> = BTreeMap::new();
    let members: Vec<String> = parent.keys().cloned().collect();
    for m in members {
        let root = find(&mut parent, &m);
        components.entry(root).or_default().push(m);
    }

    let candidates = components.len();
    let mut created = 0usize;
    for (_root, mut ids) in components {
        ids.sort();
        ids.truncate(STAPLE_MAX_PARTS);
        if ids.len() < 2 {
            continue;
        }
        // Fetch active parts in a stable order; skip the component if any
        // part is gone (invalidated/superseded) — a staple must never
        // resurrect retired facts.
        let mut parts: Vec<(String, String)> = Vec::new(); // (id, text)
        let mut all_active = true;
        for id in &ids {
            let row: Option<String> = conn
                .query_row(
                    "SELECT text FROM memories
                     WHERE memory_id = ?1
                       AND invalidated_at IS NULL AND superseded_by IS NULL",
                    params![id],
                    |r| r.get(0),
                )
                .ok();
            match row {
                Some(text) => parts.push((id.clone(), text)),
                None => {
                    all_active = false;
                    break;
                }
            }
        }
        if !all_active || parts.len() < 2 {
            continue;
        }

        // Non-generative merge: the staple text is the parts joined, nothing
        // rewritten. add_memory's normalized-text dedup makes re-runs no-ops.
        let text = parts
            .iter()
            .map(|(_, t)| t.trim())
            .collect::<Vec<_>>()
            .join("\n");
        let provenance = serde_json::json!({
            "source": "staple",
            "parts": ids,
            "created_by": "brain reinforce",
        });
        let _scope = crate::packs::ImportProvenanceScope::new(provenance);
        let before: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;
        // add_memory opens its own connection; ours above is read-only use.
        let _id = add_memory(start, MemoryScope::Project, MemoryKind::Fact, &text)?;
        let after: i64 = conn.query_row("SELECT COUNT(*) FROM memories", [], |r| r.get(0))?;
        if after > before {
            created += 1;
        }
    }
    Ok((candidates, created))
}

/// Rebuild the `query_routes` table from citation history. Full rebuild
/// (small table, derived data): every citation that recorded a `query`
/// becomes a (normalized query, memory) route with a cite count; distinct
/// queries get an embedding when an embedder is available so similar future
/// queries can match semantically (lean builds fall back to exact text
/// match). Returns (routes, embedded).
fn build_query_routes(start: &Path) -> KimetsuResult<(usize, usize)> {
    let (_paths, config, conn) = load_project(start)?;

    conn.execute("DELETE FROM query_routes", [])?;
    let mut stmt = conn.prepare(
        "
        SELECT lower(trim(query)) AS q, memory_id,
               COUNT(*) AS cites, MAX(cited_at) AS last
        FROM memory_citations
        WHERE query IS NOT NULL AND trim(query) != ''
        GROUP BY q, memory_id
        ",
    )?;
    let rows: Vec<(String, String, i64, String)> = stmt
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, String>(3)?,
            ))
        })?
        .collect::<Result<_, _>>()?;

    let embedder = crate::embeddings::open_embedder_for(config.embedder.enabled);
    let mut embed_cache: BTreeMap<String, Option<Vec<f32>>> = BTreeMap::new();
    let mut built = 0usize;
    let mut embedded = 0usize;
    for (q, memory_id, cites, last) in rows {
        let emb = embed_cache
            .entry(q.clone())
            .or_insert_with(|| {
                if embedder.is_noop() {
                    None
                } else {
                    embedder
                        .embed(&q)
                        .ok()
                        .filter(|v| v.len() == embedder.dim())
                }
            })
            .clone();
        let (blob, model): (Option<Vec<u8>>, Option<String>) = match emb {
            Some(v) => {
                embedded += 1;
                (
                    Some(encode_embedding(&v)),
                    Some(embedder.model_id().to_string()),
                )
            }
            None => (None, None),
        };
        conn.execute(
            "INSERT OR REPLACE INTO query_routes
             (query_norm, memory_id, cites, last_cited_at, query_embedding, embedding_model)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![q, memory_id, cites, last, blob, model],
        )?;
        built += 1;
    }
    Ok((built, embedded))
}

/// Retrieval-time routing boost (called from the broker with the candidates
/// already scored). For every stored successful query similar to the
/// incoming one, the memories it cited receive a bounded additive relevance
/// gain:
///
///   gain_i = min(ROUTING_BOOST_CAP, share_i of ROUTING_BUDGET)
///   share_i weighted by query similarity x ln(1+cites) x age decay
///
/// The budget is FIXED per retrieval (ACT-R source activation): however many
/// routes match, the total added relevance never exceeds ROUTING_BUDGET, and
/// no single candidate gains more than ROUTING_BOOST_CAP — routing reorders
/// within a relevance band, it can never flood the pool (the failure mode we
/// measured with the unbounded citation boost).
pub(crate) fn apply_query_routing(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&QueryEmbedding>,
    candidates: &mut [crate::context::Candidate],
) {
    // Table may not exist on old brains mid-migration; treat errors as "no routes".
    let Ok(mut stmt) = conn.prepare(
        "SELECT query_norm, memory_id, cites, last_cited_at, query_embedding
         FROM query_routes WHERE cites >= ?1",
    ) else {
        return;
    };
    let rows: Vec<RouteRow> = match stmt.query_map(params![ROUTE_MIN_CITES], |row| {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
        ))
    }) {
        Ok(mapped) => mapped.flatten().collect(),
        Err(_) => return,
    };
    if rows.is_empty() {
        return;
    }

    let query_norm = query.trim().to_lowercase();
    let now = OffsetDateTime::now_utc();

    // Aggregate weight per memory across all matching routes.
    let mut weights: BTreeMap<String, f32> = BTreeMap::new();
    for (route_q, memory_id, cites, last_cited_at, blob) in rows {
        let sim = if route_q == query_norm {
            1.0
        } else {
            match (query_embedding, blob) {
                (Some(qe), Some(b)) => match decode_embedding(&b, Some(qe.vector.len())) {
                    Ok(v) => crate::consolidate::cosine(&qe.vector, &v),
                    Err(_) => continue,
                },
                _ => continue, // lean build: exact-match only
            }
        };
        if sim < ROUTE_QUERY_SIM_FLOOR {
            continue;
        }
        let age_days = OffsetDateTime::parse(
            &last_cited_at,
            &time::format_description::well_known::Rfc3339,
        )
        .map(|t| ((now - t).whole_seconds().max(0) as f32) / 86_400.0)
        .unwrap_or(0.0);
        let decay = (1.0 + age_days).powf(-ROUTE_DECAY_EXPONENT);
        let strength = sim * (1.0 + cites as f32).ln() * decay;
        *weights.entry(memory_id).or_insert(0.0) += strength;
    }
    if weights.is_empty() {
        return;
    }

    // Fixed budget split proportionally; per-candidate hard cap.
    let total: f32 = weights.values().sum();
    for cand in candidates.iter_mut() {
        let Some(id) = cand
            .capsule
            .expansion_handle
            .strip_prefix("memory:")
            .map(str::to_string)
        else {
            continue;
        };
        if let Some(w) = weights.get(&id) {
            let gain = (ROUTING_BUDGET * w / total).min(ROUTING_BOOST_CAP);
            cand.raw_relevance += gain;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::project::{init_project, record_citations};
    use crate::user_brain::with_user_brain_disabled;
    use ulid::Ulid;

    fn test_root() -> std::path::PathBuf {
        let root = std::env::temp_dir().join(format!("kimetsu-reinforce-{}", Ulid::new()));
        kimetsu_core::paths::git_init_boundary(&root);
        std::fs::create_dir_all(&root).expect("root");
        root
    }

    fn add(root: &Path, text: &str) -> String {
        add_memory(root, MemoryScope::Project, MemoryKind::Fact, text).expect("add")
    }

    fn mem_candidate(id: &str) -> crate::context::Candidate {
        crate::context::Candidate {
            raw_relevance: 0.50,
            embedding: None,
            cosine: None,
            capsule: crate::context::ContextCapsule {
                id: "x".into(),
                kind: "memory".into(),
                summary: "s".into(),
                token_estimate: 10,
                expansion_handle: format!("memory:{id}"),
                provenance: vec![],
                confidence: 0.8,
                freshness: 1.0,
                relevance: 0.0,
                scope_weight: 0.7,
                score: 0.0,
            },
        }
    }

    /// Two memories cited together twice -> ONE staple containing both texts,
    /// originals kept, provenance carries the part ids, re-run is a no-op.
    #[test]
    fn co_cited_pair_staples_once_and_keeps_originals() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let a = add(&root, "caroline moved to berlin in may");
            let b = add(&root, "caroline researches marine biology");
            let c = add(&root, "unrelated memory about rust builds");

            for _ in 0..2 {
                record_citations(
                    &root,
                    &[a.clone(), b.clone()],
                    None,
                    Some("what does caroline research and where does she live"),
                )
                .expect("cite");
            }
            record_citations(&root, std::slice::from_ref(&c), None, None).expect("cite c");

            let s = reinforce(&root, true, false).expect("reinforce");
            assert_eq!(s.staple_candidates, 1, "one qualifying component");
            assert_eq!(s.staples_created, 1, "one staple written");

            let (_p, _c, conn) = load_project(&root).expect("load");
            let (staple_text, prov): (String, String) = conn
                .query_row(
                    "SELECT text, provenance_snapshot_json FROM memories
                     WHERE provenance_snapshot_json LIKE '%staple%'",
                    [],
                    |r| Ok((r.get(0)?, r.get(1)?)),
                )
                .expect("staple exists");
            assert!(staple_text.contains("berlin") && staple_text.contains("marine"));
            assert!(
                prov.contains(&a) && prov.contains(&b),
                "provenance carries parts"
            );
            let active: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memories WHERE invalidated_at IS NULL",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(active, 4, "a, b, c + staple all active");

            let s2 = reinforce(&root, true, false).expect("re-run");
            assert_eq!(s2.staples_created, 0, "re-run must not duplicate");
        });
    }

    /// One co-cite is not enough (STAPLE_MIN_CO_CITES = 2).
    #[test]
    fn single_co_cite_does_not_staple() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let a = add(&root, "fact one");
            let b = add(&root, "fact two");
            record_citations(&root, &[a, b], None, None).expect("cite");
            let s = reinforce(&root, true, false).expect("reinforce");
            assert_eq!(s.staples_created, 0);
        });
    }

    /// Routes build from query-linked citations and boost the routed memory
    /// for the SAME query, with the gain bounded by ROUTING_BOOST_CAP.
    #[test]
    fn routes_build_and_boost_is_bounded() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let a = add(&root, "the deploy script lives under scripts");
            for _ in 0..2 {
                record_citations(
                    &root,
                    std::slice::from_ref(&a),
                    None,
                    Some("where is the deploy script"),
                )
                .expect("cite");
            }
            let s = reinforce(&root, false, true).expect("routes");
            assert!(s.routes_built >= 1, "route row built");

            let (_p, _c, conn) = load_project(&root).expect("load");
            let mut candidates = vec![mem_candidate(&a)];
            apply_query_routing(&conn, "Where is the deploy script", None, &mut candidates);
            let boosted = candidates[0].raw_relevance;
            assert!(boosted > 0.50, "routed memory must gain relevance");
            assert!(
                boosted <= 0.50 + ROUTING_BOOST_CAP + f32::EPSILON,
                "gain must respect the cap, got {boosted}"
            );
        });
    }

    /// Below min-support (one citation), the route must NOT fire.
    #[test]
    fn route_below_min_support_does_not_fire() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let a = add(&root, "single-cite memory");
            record_citations(
                &root,
                std::slice::from_ref(&a),
                None,
                Some("one off question"),
            )
            .expect("cite");
            reinforce(&root, false, true).expect("routes");
            let (_p, _c, conn) = load_project(&root).expect("load");
            let mut candidates = vec![mem_candidate(&a)];
            apply_query_routing(&conn, "one off question", None, &mut candidates);
            assert!((candidates[0].raw_relevance - 0.50).abs() < f32::EPSILON);
        });
    }

    /// Grouped citations share one run_id (the co-citation basis) and carry
    /// the query into memory_citations.
    #[test]
    fn grouped_citations_share_run_and_persist_query() {
        with_user_brain_disabled(|| {
            let root = test_root();
            init_project(&root, false).expect("init");
            let a = add(&root, "alpha");
            let b = add(&root, "beta");
            record_citations(&root, &[a, b], None, Some("the question")).expect("cite");
            let (_p, _c, conn) = load_project(&root).expect("load");
            let distinct_runs: i64 = conn
                .query_row(
                    "SELECT COUNT(DISTINCT run_id) FROM memory_citations",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(distinct_runs, 1, "one call = one citation group");
            let with_query: i64 = conn
                .query_row(
                    "SELECT COUNT(*) FROM memory_citations WHERE query = 'the question'",
                    [],
                    |r| r.get(0),
                )
                .unwrap();
            assert_eq!(with_query, 2, "both rows carry the query");
        });
    }
}
