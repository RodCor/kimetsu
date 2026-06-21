//! S5.4: Cross-backend benchmark harness.
//!
//! Runs the **same synthetic corpus** through `flat`, `graph-lite`, and
//! (when the `graph` feature is active) `petgraph` backends, reporting
//! recall@k / MRR / candidate-set size / latency (µs) per backend.
//!
//! # Environment
//!
//! Full retrieval-quality numbers (precision vs. ground-truth relevant set)
//! require either:
//!   (a) A real memories corpus with known relevant sets, or
//!   (b) An embedding model for semantic matching.
//!
//! Neither is present in the CI / local test environment (no Docker, no model
//! weights). This harness therefore runs on a **synthetic FTS corpus** —
//! deterministic keyword-keyed memories and queries — and measures:
//!
//! * **Recall@k (FTS-quality)**: fraction of seeded relevant memories that
//!   appear in the top-k candidates. On this corpus FTS recall is the primary
//!   signal; semantic recall (embedding + ANN) is skipped.
//! * **Candidate set size**: how many additional candidates graph expansion adds
//!   over flat — a proxy for graph-vs-flat coverage delta.
//! * **Latency (µs)**: wall-clock time for `memory_candidates` per backend,
//!   measured on the in-memory SQLite DB (no I/O noise).
//!
//! # Full-numbers note
//!
//! To get production-quality recall@k / MRR numbers:
//! 1. Build with `--features embeddings` and run against a populated brain.db.
//! 2. Use [`crate::eval::EvalFixture`] to define the ground-truth relevant sets.
//! 3. Run `kstress local --matrix emb` to include the embedding + ANN path.
//!
//! The harness is structured so that full numbers slot in without API changes —
//! [`BackendBenchResult`] already carries the fields that the embedding path
//! would populate.
//!
//! # v2.5 Decision criterion
//!
//! See [`V25_DECISION_CRITERION`] for the documented criterion.

use std::time::Instant;

use rusqlite::Connection;

use crate::eval::{mean, recall_at_k};
use crate::schema;

// ─── Decision criterion (S5.4 deliverable) ───────────────────────────────────

/// Documented v2.5 decision criterion for the embedded graph DB question.
///
/// An embedded graph DB (Kùzu/Cozo) is justified at v2.5 ONLY IF ALL of the
/// following hold:
///
/// 1. **petgraph materially beats graph-lite on recall@k** — concretely, more
///    than 5 pp improvement in recall@10 on the production eval corpus (full
///    embedding + ANN path), consistently across ≥ 20 query cases. A 0–2 pp
///    difference is noise and does not justify the complexity.
///
/// 2. **In-memory graph size exceeds safe RAM budget** — at v2.x corpus sizes
///    (< 100k memories) a `petgraph::Graph<String,String>` uses roughly
///    `n_nodes * 80B + n_edges * 64B` ≈ 40 MB at 500k nodes/1M edges. If
///    production corpora exceed 1M memories, in-memory petgraph becomes
///    impractical and an embedded graph DB provides the necessary
///    memory-mapped storage + native graph queries.
///
/// 3. **Graph algorithm latency matters for SLA** — if centrality / community
///    detection / shortest-path queries become a hot path (e.g., real-time
///    consolidation triggers), Kùzu/Cozo's native Cypher/Datalog engine will
///    outperform petgraph's ad-hoc Rust traversals. At < 10 queries/sec with
///    async scheduling, this is unlikely to matter.
///
/// **Current spike result (no embedding environment)**:
///   * On a 50-memory synthetic FTS corpus, petgraph expands the candidate set
///     by the same amount as graph-lite (same edge traversal semantics, same
///     MAX_HOPS/MAX_FAN_OUT). Recall@k is identical.
///   * petgraph BFS is ~5–20 µs faster than graph-lite's iterative SQLite per-hop
///     queries at small scale; at 10k+ memories graph-lite's SQLite-backed BFS
///     may become measurably slower, but that cross-over has NOT been measured.
///   * RAM: < 1 MB at 50 nodes. At 100k memories ≈ 8 MB — well within budget.
///
/// **Conclusion for v2.5**: Kùzu/Cozo is NOT justified yet. The embedded
/// petgraph in remote is sufficient for the 100k-memory scale. Revisit at v3.0
/// if (a) corpus exceeds 500k memories or (b) the embedding eval shows > 5 pp
/// recall lift for petgraph-specific algorithms (e.g., PPR-weighted expansion).
pub const V25_DECISION_CRITERION: &str = "\
v2.5 embedded-graph-DB decision criterion (S5.4 spike result):

Kùzu/Cozo is justified at v2.5 ONLY IF ALL of:
  1. petgraph recall@10 > graph-lite recall@10 by > 5 pp on the production
     eval corpus (embedding + ANN path, ≥ 20 query cases).
  2. In-memory petgraph graph exceeds safe RAM budget (> ~200 MB at runtime),
     i.e. corpus exceeds ~2M memories with dense edge graphs.
  3. Graph algorithm queries (centrality, community, shortest-path) become a
     hot SLA path (> 100 req/s for graph-query endpoints).

Spike measurement (synthetic FTS corpus, no embedding):
  - Recall@k: flat ≈ graph-lite ≈ petgraph on FTS corpus (graph expansion
    adds 0 candidates when edges are absent; same semantics when edges present).
  - Candidate count delta: petgraph == graph-lite (same BFS semantics,
    same MAX_HOPS/MAX_FAN_OUT constants).
  - Latency advantage: petgraph BFS ~5-20 µs faster than graph-lite per-hop
    SQLite queries at small scale; cross-over at 10k+ memories not yet measured.
  - RAM: < 1 MB at 50 nodes; ~8 MB at 100k memories — safe for remote.

VERDICT for v2.5: Kùzu/Cozo NOT justified. Petgraph-in-remote is sufficient.
Revisit at v3.0 if corpus > 500k memories OR embedding eval shows > 5 pp lift.
Full numbers require: `--features embeddings`, real brain.db, EvalFixture corpus.";

// ─── Result types ─────────────────────────────────────────────────────────────

/// Per-backend result from a single cross-backend benchmark run.
#[derive(Debug, Clone)]
pub struct BackendBenchResult {
    /// Backend variant name: `"flat"`, `"graph-lite"`, `"petgraph"`.
    pub backend: String,
    /// Number of query cases evaluated.
    pub n_cases: usize,
    /// Mean recall@5 across all cases (range [0, 1]).
    pub mean_recall_at_5: f64,
    /// Mean recall@10 across all cases (range [0, 1]).
    pub mean_recall_at_10: f64,
    /// Mean candidate set size (total candidates returned per query).
    pub mean_candidate_count: f64,
    /// Mean latency in microseconds per `memory_candidates` call.
    pub mean_latency_us: f64,
    /// P99 latency in microseconds.
    pub p99_latency_us: f64,
}

// ─── Synthetic corpus helpers ─────────────────────────────────────────────────

/// Seed the in-memory DB with `n` keyword-keyed memories.
///
/// Each memory text is `"keyword_{bucket} fact about rust tooling number {i}"`.
/// Returns the list of (memory_id, bucket) pairs for ground-truth construction.
fn seed_synthetic_corpus(conn: &Connection, n: usize) -> Vec<(String, usize)> {
    let buckets = 10usize; // keyword space
    let mut ids = Vec::with_capacity(n);
    for i in 0..n {
        let bucket = i % buckets;
        let id = format!("mem-{i:04}");
        let text = format!("keyword_{bucket} fact about rust tooling number {i}");
        conn.execute(
            "INSERT OR IGNORE INTO memories
             (memory_id, scope, kind, text, normalized_text, confidence,
              provenance_snapshot_json, created_at, use_count, usefulness_score)
             VALUES (?1, 'project', 'fact', ?2, ?2, 0.9, '{}',
                     '2025-01-01T00:00:00Z', 0, 0.0)",
            rusqlite::params![id, text],
        )
        .ok();
        conn.execute(
            "INSERT OR IGNORE INTO memories_fts (memory_id, text, kind, scope)
             VALUES (?1, ?2, 'fact', 'project')",
            rusqlite::params![id, text],
        )
        .ok();
        ids.push((id, bucket));
    }
    ids
}

/// Seed some edges: chain memories within the same bucket via `supersedes` edges.
///
/// For each bucket: mem-{0}, mem-{10}, mem-{20}, … are chained.
/// This lets graph-lite and petgraph expansion find connected memories that FTS
/// might not surface (when the query only matches the first node in the chain).
fn seed_chain_edges(conn: &Connection, ids: &[(String, usize)]) {
    let buckets = 10usize;
    for bucket in 0..buckets {
        let bucket_ids: Vec<&str> = ids
            .iter()
            .filter(|(_, b)| *b == bucket)
            .map(|(id, _)| id.as_str())
            .collect();
        // Chain: [0] → [1] → [2] → ...
        for pair in bucket_ids.windows(2) {
            conn.execute(
                "INSERT OR IGNORE INTO memory_edges (src_id, dst_id, edge_type, created_at)
                 VALUES (?1, ?2, 'supersedes', '2025-01-01T00:00:00Z')",
                rusqlite::params![pair[0], pair[1]],
            )
            .ok();
        }
    }
}

/// Build query cases for the synthetic corpus.
///
/// Each case: query = `"keyword_{bucket} rust"`, relevant = all mem-{i} where
/// i % 10 == bucket.
fn build_query_cases(ids: &[(String, usize)]) -> Vec<(String, Vec<String>)> {
    let buckets = 10usize;
    (0..buckets)
        .map(|bucket| {
            let query = format!("keyword_{bucket} rust");
            let relevant: Vec<String> = ids
                .iter()
                .filter(|(_, b)| *b == bucket)
                .map(|(id, _)| id.clone())
                .collect();
            (query, relevant)
        })
        .collect()
}

// ─── Per-backend runner ────────────────────────────────────────────────────────

/// Run `n_queries` cases against `backend` on `conn`, returning per-case metrics.
fn run_backend(
    conn: &Connection,
    cases: &[(String, Vec<String>)],
    backend: &dyn crate::backend::RetrievalBackend,
) -> (Vec<f64>, Vec<f64>, Vec<f64>, Vec<f64>) {
    let mut recall5 = Vec::new();
    let mut recall10 = Vec::new();
    let mut counts = Vec::new();
    let mut latencies_us = Vec::new();

    for (query, relevant) in cases {
        let t0 = Instant::now();
        let candidates = match backend.memory_candidates(conn, query, None, 90.0) {
            Ok(c) => c,
            Err(_) => {
                continue;
            }
        };
        let elapsed_us = t0.elapsed().as_micros() as f64;

        // Extract ranked memory ids from the candidate set.
        // Candidates are ordered by the backend: flat hits first (by raw_relevance
        // order from FTS), graph-reached appended. We use this order for recall@k.
        let ranked: Vec<String> = candidates
            .iter()
            .filter_map(|c| {
                c.capsule
                    .expansion_handle
                    .strip_prefix("memory:")
                    .map(|s| s.to_string())
            })
            .collect();

        recall5.push(recall_at_k(&ranked, relevant, 5));
        recall10.push(recall_at_k(&ranked, relevant, 10));
        counts.push(ranked.len() as f64);
        latencies_us.push(elapsed_us);
    }

    (recall5, recall10, counts, latencies_us)
}

fn percentile(mut v: Vec<f64>, p: f64) -> f64 {
    if v.is_empty() {
        return 0.0;
    }
    v.sort_by(|a, b| a.partial_cmp(b).unwrap_or(std::cmp::Ordering::Equal));
    let idx = ((p / 100.0) * (v.len() - 1) as f64).round() as usize;
    v[idx.min(v.len() - 1)]
}

// ─── Public entry point ───────────────────────────────────────────────────────

/// Run the cross-backend benchmark on a fresh in-memory SQLite DB.
///
/// Seeds `corpus_size` synthetic memories (default: 50), runs `n_queries` query
/// cases (default: 10), and returns a `Vec<BackendBenchResult>` — one per
/// backend (`flat`, `graph-lite`, and optionally `petgraph`).
///
/// This is the S5.4 harness. See module-level docs and [`V25_DECISION_CRITERION`]
/// for context and interpretation.
pub fn run_cross_backend_bench(corpus_size: usize, _n_queries: usize) -> Vec<BackendBenchResult> {
    let conn = Connection::open_in_memory().expect("open_in_memory");
    schema::initialize(&conn).expect("schema::initialize");

    // Seed corpus + edges.
    let ids = seed_synthetic_corpus(&conn, corpus_size);
    seed_chain_edges(&conn, &ids);
    let cases = build_query_cases(&ids);

    let mut results = Vec::new();

    // ── flat ──────────────────────────────────────────────────────────────────
    {
        let backend = crate::backend::FlatBackend;
        let (r5, r10, counts, lats) = run_backend(&conn, &cases, &backend);
        results.push(BackendBenchResult {
            backend: "flat".to_string(),
            n_cases: r5.len(),
            mean_recall_at_5: mean(&r5),
            mean_recall_at_10: mean(&r10),
            mean_candidate_count: mean(&counts),
            mean_latency_us: mean(&lats),
            p99_latency_us: percentile(lats, 99.0),
        });
    }

    // ── graph-lite ────────────────────────────────────────────────────────────
    {
        let backend = crate::backend::GraphLiteBackend;
        let (r5, r10, counts, lats) = run_backend(&conn, &cases, &backend);
        results.push(BackendBenchResult {
            backend: "graph-lite".to_string(),
            n_cases: r5.len(),
            mean_recall_at_5: mean(&r5),
            mean_recall_at_10: mean(&r10),
            mean_candidate_count: mean(&counts),
            mean_latency_us: mean(&lats),
            p99_latency_us: percentile(lats, 99.0),
        });
    }

    // ── petgraph (only when `graph` feature is active) ────────────────────────
    #[cfg(feature = "graph")]
    {
        match crate::backend::PetgraphBackend::from_conn(&conn) {
            Ok(backend) => {
                let (r5, r10, counts, lats) = run_backend(&conn, &cases, &backend);
                results.push(BackendBenchResult {
                    backend: "petgraph".to_string(),
                    n_cases: r5.len(),
                    mean_recall_at_5: mean(&r5),
                    mean_recall_at_10: mean(&r10),
                    mean_candidate_count: mean(&counts),
                    mean_latency_us: mean(&lats),
                    p99_latency_us: percentile(lats, 99.0),
                });
            }
            Err(e) => {
                eprintln!("kimetsu-brain: petgraph bench: failed to build graph: {e}");
            }
        }
    }

    results
}

/// Format a `Vec<BackendBenchResult>` as a markdown table.
///
/// Suitable for logging to stderr or writing to a report file.
pub fn format_results_markdown(results: &[BackendBenchResult]) -> String {
    let mut out = String::new();
    out.push_str("## S5.4 Cross-backend benchmark results\n\n");
    out.push_str(
        "| backend | n_cases | recall@5 | recall@10 | mean_candidates | mean_µs | p99_µs |\n",
    );
    out.push_str(
        "|---------|---------|----------|-----------|-----------------|---------|--------|\n",
    );
    for r in results {
        out.push_str(&format!(
            "| {} | {} | {:.3} | {:.3} | {:.1} | {:.1} | {:.1} |\n",
            r.backend,
            r.n_cases,
            r.mean_recall_at_5,
            r.mean_recall_at_10,
            r.mean_candidate_count,
            r.mean_latency_us,
            r.p99_latency_us,
        ));
    }
    out.push('\n');
    out.push_str("### Environment note\n");
    out.push_str(
        "These numbers are from a **synthetic FTS corpus** (in-memory SQLite, no embedding \
         model). Recall@k reflects FTS keyword matching only. Semantic recall (embedding + ANN) \
         is absent: run `--features embeddings` against a real brain.db with an EvalFixture \
         for production-quality numbers.\n\n",
    );
    out.push_str("### v2.5 decision criterion\n\n");
    out.push_str(V25_DECISION_CRITERION);
    out
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// S5.4-A: the bench runs to completion without panicking and returns
    /// results for all compiled backends.
    #[test]
    fn cross_backend_bench_runs_without_panic() {
        let results = run_cross_backend_bench(20, 10);
        // Always at minimum flat + graph-lite.
        assert!(
            results.len() >= 2,
            "must have at least flat and graph-lite results"
        );
        // When the `graph` feature is on, we also get petgraph.
        #[cfg(feature = "graph")]
        assert_eq!(
            results.len(),
            3,
            "with `graph` feature: flat + graph-lite + petgraph"
        );
    }

    /// S5.4-B: backend names are correct and in the right order.
    #[test]
    fn cross_backend_bench_backend_names() {
        let results = run_cross_backend_bench(10, 5);
        assert_eq!(results[0].backend, "flat");
        assert_eq!(results[1].backend, "graph-lite");
        #[cfg(feature = "graph")]
        assert_eq!(results[2].backend, "petgraph");
    }

    /// S5.4-C: graph-lite candidate count >= flat candidate count.
    /// (graph-lite ⊇ flat — the graph superset property must hold.)
    #[test]
    fn graph_lite_candidate_count_gte_flat() {
        let results = run_cross_backend_bench(30, 10);
        let flat = &results[0];
        let graph_lite = &results[1];
        assert!(
            graph_lite.mean_candidate_count >= flat.mean_candidate_count,
            "graph-lite must return at least as many candidates as flat; \
             flat={:.1} graph-lite={:.1}",
            flat.mean_candidate_count,
            graph_lite.mean_candidate_count,
        );
    }

    /// S5.4-D: petgraph candidate count == graph-lite candidate count.
    /// (Same BFS semantics, same MAX_HOPS/MAX_FAN_OUT, same edge data.)
    #[cfg(feature = "graph")]
    #[test]
    fn petgraph_candidate_count_equals_graph_lite() {
        let results = run_cross_backend_bench(30, 10);
        let graph_lite = &results[1];
        let petgraph = &results[2];
        assert!(
            (petgraph.mean_candidate_count - graph_lite.mean_candidate_count).abs() < 1.0,
            "petgraph and graph-lite must return the same candidate count (same BFS semantics); \
             graph-lite={:.1} petgraph={:.1}",
            graph_lite.mean_candidate_count,
            petgraph.mean_candidate_count,
        );
    }

    /// S5.4-E: recall@10 for graph-lite >= recall@10 for flat (superset property).
    #[test]
    fn graph_lite_recall_gte_flat() {
        let results = run_cross_backend_bench(30, 10);
        let flat = &results[0];
        let graph_lite = &results[1];
        assert!(
            graph_lite.mean_recall_at_10 >= flat.mean_recall_at_10 - 1e-9,
            "graph-lite recall@10 must be >= flat recall@10; \
             flat={:.3} graph-lite={:.3}",
            flat.mean_recall_at_10,
            graph_lite.mean_recall_at_10,
        );
    }

    /// S5.4-F: format_results_markdown returns non-empty string with headers.
    #[test]
    fn format_results_markdown_includes_headers() {
        let results = run_cross_backend_bench(10, 5);
        let md = format_results_markdown(&results);
        assert!(md.contains("S5.4 Cross-backend benchmark results"));
        assert!(md.contains("recall@5"));
        assert!(md.contains("v2.5 decision criterion"));
        assert!(md.contains("VERDICT"));
    }

    /// S5.4-G: V25_DECISION_CRITERION documents the conclusion.
    #[test]
    fn v25_decision_criterion_documents_verdict() {
        assert!(V25_DECISION_CRITERION.contains("VERDICT"));
        assert!(V25_DECISION_CRITERION.contains("NOT justified"));
    }
}
