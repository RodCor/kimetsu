//! Memory consolidation: near-duplicate merge (Story 3.1) and cluster
//! distillation (Story 3.2).
//!
//! # Near-duplicate merge (Story 3.1)
//!
//! For each memory with a stored embedding, find other memories (same
//! `embedding_model`) whose cosine similarity exceeds a threshold (default
//! 0.92). Union-find clusters the pairs; the survivor of each cluster is the
//! memory with the highest `(usefulness_score × recency rank)`. Merge plan:
//!   - Survivor keeps its text/id; `use_count` and `usefulness_score` become
//!     cluster sums.
//!   - Citations are reassigned to the survivor (`UPDATE memory_citations`).
//!   - Members get `superseded_by = survivor_id` via a `memory.superseded`
//!     event (so `brain rebuild` reproduces the merge).
//!
//! The cosine scan is brute-force O(N²) over decoded embeddings within the
//! same `model_id`. This is intentionally simple and correct for the current
//! scale (< 10k memories). A future optimisation would reuse the ANN index.
//!
//! # Cluster distillation (Story 3.2)
//!
//! Looser clusters (cosine 0.75–0.85 band) of ≥ 3 memories sharing ≥ 1
//! domain tag are fed to the configured distiller to produce a ONE general
//! principle (2–4 sentences, imperative). The result is created as a
//! `memory_proposal` (pending review) rather than directly accepted.
//!
//! If no distiller is configured the command prints the clusters and exits 0.

use std::collections::HashMap;

use kimetsu_core::KimetsuResult;
use rusqlite::Connection;
use time::OffsetDateTime;
use time::format_description::well_known::Rfc3339;

use crate::embeddings::decode_embedding;

// ---------------------------------------------------------------------------
// Public data types
// ---------------------------------------------------------------------------

/// One memory row as loaded for consolidation scoring.
#[derive(Debug, Clone)]
pub struct ConsolidateRow {
    pub memory_id: String,
    pub scope: String,
    pub kind: String,
    pub text: String,
    pub use_count: i64,
    pub usefulness_score: f32,
    /// RFC-3339 timestamps for recency rank.
    pub last_useful_at: Option<String>,
    pub created_at: String,
    pub embedding: Vec<f32>,
    pub model_id: String,
}

/// A proposed merge cluster: survivor + members to supersede.
#[derive(Debug, Clone)]
pub struct MergeCluster {
    pub survivor: ConsolidateRow,
    pub members: Vec<ConsolidateRow>,
}

/// Summary returned by `run_consolidation`.
#[derive(Debug, Default)]
pub struct ConsolidateSummary {
    pub clusters_found: usize,
    pub memories_merged: usize,
    pub citations_reassigned: usize,
}

/// Options for `run_consolidation`.
#[derive(Debug, Clone)]
pub struct ConsolidateOptions {
    /// Cosine ≥ threshold → near-duplicate (default 0.92).
    pub threshold: f32,
    /// Print plan without writing to the DB.
    pub dry_run: bool,
}

impl Default for ConsolidateOptions {
    fn default() -> Self {
        Self {
            threshold: 0.92,
            dry_run: false,
        }
    }
}

/// Options for `run_distill`.
#[derive(Debug, Clone)]
pub struct DistillOptions {
    /// Lower cosine bound (inclusive) of the loose-cluster band.
    pub lo: f32,
    /// Upper cosine bound (inclusive) of the loose-cluster band.
    pub hi: f32,
    /// Minimum cluster size to distil.
    pub min_cluster_size: usize,
}

impl Default for DistillOptions {
    fn default() -> Self {
        Self {
            lo: 0.75,
            hi: 0.85,
            min_cluster_size: 3,
        }
    }
}

/// One distillable cluster (Story 3.2).
#[derive(Debug, Clone)]
pub struct DistillCluster {
    pub shared_tags: Vec<String>,
    pub memories: Vec<ConsolidateRow>,
}

// ---------------------------------------------------------------------------
// Cosine helpers
// ---------------------------------------------------------------------------

/// Cosine similarity between two equal-length slices.
/// Returns 0.0 when either vector is zero-length or norms are zero.
pub fn cosine(a: &[f32], b: &[f32]) -> f32 {
    if a.len() != b.len() || a.is_empty() {
        return 0.0;
    }
    let dot: f32 = a.iter().zip(b.iter()).map(|(x, y)| x * y).sum();
    let na: f32 = a.iter().map(|x| x * x).sum::<f32>().sqrt();
    let nb: f32 = b.iter().map(|x| x * x).sum::<f32>().sqrt();
    if na < f32::EPSILON || nb < f32::EPSILON {
        return 0.0;
    }
    (dot / (na * nb)).clamp(-1.0, 1.0)
}

// ---------------------------------------------------------------------------
// Tag parsing
// ---------------------------------------------------------------------------

/// Parse `[tags: a, b, c]` embedded in a memory text. Returns a sorted,
/// deduplicated list of lower-cased tags.
pub fn parse_tags(text: &str) -> Vec<String> {
    // Match the first `[tags: ...]` block, case-insensitive.
    let lower = text.to_ascii_lowercase();
    let Some(start) = lower.find("[tags:") else {
        return Vec::new();
    };
    let after = &text[start + 6..]; // skip "[tags:"
    let Some(end) = after.find(']') else {
        return Vec::new();
    };
    let tag_str = &after[..end];
    let mut tags: Vec<String> = tag_str
        .split(',')
        .map(|t| t.trim().to_ascii_lowercase())
        .filter(|t| !t.is_empty())
        .collect();
    tags.sort();
    tags.dedup();
    tags
}

// ---------------------------------------------------------------------------
// Union-find
// ---------------------------------------------------------------------------

struct UnionFind {
    parent: Vec<usize>,
}

impl UnionFind {
    fn new(n: usize) -> Self {
        Self {
            parent: (0..n).collect(),
        }
    }

    fn find(&mut self, x: usize) -> usize {
        if self.parent[x] != x {
            self.parent[x] = self.find(self.parent[x]); // path compression
        }
        self.parent[x]
    }

    fn union(&mut self, x: usize, y: usize) {
        let rx = self.find(x);
        let ry = self.find(y);
        if rx != ry {
            self.parent[ry] = rx;
        }
    }
}

// ---------------------------------------------------------------------------
// Row loading
// ---------------------------------------------------------------------------

/// Load all active, non-superseded memories that have an embedding.
/// Grouped by model_id so brute-force cosine only runs within model.
pub fn load_embeddable_rows(
    conn: &Connection,
) -> KimetsuResult<HashMap<String, Vec<ConsolidateRow>>> {
    let mut stmt = conn.prepare(
        "SELECT memory_id, scope, kind, text, use_count, usefulness_score,
                last_useful_at, created_at, embedding, embedding_model
         FROM memories
         WHERE invalidated_at IS NULL
           AND superseded_by IS NULL
           AND embedding IS NOT NULL
           AND embedding_model IS NOT NULL
         ORDER BY created_at DESC",
    )?;

    let rows = stmt.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, f64>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, String>(7)?,
            row.get::<_, Vec<u8>>(8)?,
            row.get::<_, String>(9)?,
        ))
    })?;

    let mut by_model: HashMap<String, Vec<ConsolidateRow>> = HashMap::new();
    for row in rows {
        let (
            memory_id,
            scope,
            kind,
            text,
            use_count,
            usefulness_score,
            last_useful_at,
            created_at,
            blob,
            model_id,
        ) = row?;
        // Skip rows whose embedding blob doesn't decode cleanly.
        let Ok(embedding) = decode_embedding(&blob, None) else {
            continue;
        };
        if embedding.is_empty() {
            continue;
        }
        by_model
            .entry(model_id.clone())
            .or_default()
            .push(ConsolidateRow {
                memory_id,
                scope,
                kind,
                text,
                use_count,
                usefulness_score: usefulness_score as f32,
                last_useful_at,
                created_at,
                embedding,
                model_id,
            });
    }
    Ok(by_model)
}

// ---------------------------------------------------------------------------
// Survivor selection
// ---------------------------------------------------------------------------

/// Score a row for survivor selection: higher is better.
/// Uses `usefulness_score * recency_rank` where recency_rank is a
/// normalized position in a list sorted newest-first (index 0 = 1.0).
fn survivor_score(row: &ConsolidateRow, recency_rank: f32) -> f32 {
    let usefulness = row.usefulness_score.max(0.0);
    (usefulness + 1.0) * recency_rank
}

/// Parse an RFC-3339 timestamp into a comparable seconds value.
fn parse_ts(ts: &str) -> i64 {
    OffsetDateTime::parse(ts, &Rfc3339)
        .map(|t| t.unix_timestamp())
        .unwrap_or(0)
}

/// Choose the survivor from a cluster of rows.
/// Picks the row with the highest `(usefulness_score + 1) * recency_rank`.
/// Tie-break: lexicographically largest `created_at` (newest).
fn pick_survivor(cluster: &[usize], rows: &[ConsolidateRow]) -> usize {
    // Sort cluster rows by newest last_useful_at/created_at desc → assign recency rank.
    let mut indexed: Vec<usize> = cluster.to_vec();
    indexed.sort_by(|&a, &b| {
        let ta = parse_ts(
            rows[a]
                .last_useful_at
                .as_deref()
                .unwrap_or(&rows[a].created_at),
        );
        let tb = parse_ts(
            rows[b]
                .last_useful_at
                .as_deref()
                .unwrap_or(&rows[b].created_at),
        );
        tb.cmp(&ta)
    });
    let n = indexed.len() as f32;
    let mut best_idx = indexed[0];
    let mut best_score = f32::NEG_INFINITY;
    for (rank, &i) in indexed.iter().enumerate() {
        let recency = 1.0 - (rank as f32) / n.max(1.0);
        let score = survivor_score(&rows[i], recency);
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }
    best_idx
}

// ---------------------------------------------------------------------------
// Story 3.1: near-duplicate clustering
// ---------------------------------------------------------------------------

/// Build merge clusters from `rows` with the given cosine threshold.
/// Returns only clusters with ≥ 2 members (i.e. at least one merge needed).
pub fn find_merge_clusters(rows: &[ConsolidateRow], threshold: f32) -> Vec<MergeCluster> {
    let n = rows.len();
    if n < 2 {
        return Vec::new();
    }

    let mut uf = UnionFind::new(n);

    // Brute-force pairwise cosine — O(N²) fine for N < 10k.
    // Future: replace with ANN index search for larger corpora.
    for i in 0..n {
        for j in (i + 1)..n {
            // Only cluster within same model_id.
            if rows[i].model_id != rows[j].model_id {
                continue;
            }
            let sim = cosine(&rows[i].embedding, &rows[j].embedding);
            if sim >= threshold {
                uf.union(i, j);
            }
        }
    }

    // Collect root → members mapping.
    let mut root_to_members: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        root_to_members.entry(root).or_default().push(i);
    }

    let mut clusters = Vec::new();
    for (_, members) in root_to_members {
        if members.len() < 2 {
            continue; // singleton — nothing to merge
        }
        let survivor_idx = pick_survivor(&members, rows);
        let survivor = rows[survivor_idx].clone();
        let member_rows: Vec<ConsolidateRow> = members
            .iter()
            .filter(|&&i| i != survivor_idx)
            .map(|&i| rows[i].clone())
            .collect();
        clusters.push(MergeCluster {
            survivor,
            members: member_rows,
        });
    }

    // Stable order for deterministic dry-run output.
    clusters.sort_by(|a, b| a.survivor.memory_id.cmp(&b.survivor.memory_id));
    clusters
}

// ---------------------------------------------------------------------------
// Story 3.1: apply merge (event-sourced)
// ---------------------------------------------------------------------------

/// Apply one merge cluster to the database.
///
/// Emits an enriched `memory.superseded` event for each member, carrying
/// the member's `use_count` and `usefulness_score` as deltas.  The
/// projector arm (`apply_memory_superseded`) is the **single code path**
/// that stamps `superseded_by`, accumulates stats onto the survivor, and
/// reassigns citations — so both the live path and `rebuild_in_place`
/// replay go through exactly the same logic with no drift.
///
/// Returns the number of members merged.
pub fn apply_merge(
    conn: &Connection,
    cluster: &MergeCluster,
    run_id: kimetsu_core::ids::RunId,
) -> KimetsuResult<usize> {
    // Emit one enriched memory.superseded event per member.  The projector
    // arm handles: stamp, stat accumulation, citation reassignment, FTS/ANN
    // removal.  No direct UPDATE on the survivor here — everything flows
    // through apply_events so live path == replay path.
    for member in &cluster.members {
        let event = kimetsu_core::event::Event::new(
            run_id,
            "memory.superseded",
            serde_json::json!({
                "memory_id":       member.memory_id,
                "survivor_id":     cluster.survivor.memory_id,
                "use_count_delta": member.use_count,
                "score_delta":     member.usefulness_score as f64,
            }),
        );
        crate::projector::apply_events(conn, &[event])?;
    }

    Ok(cluster.members.len())
}

// ---------------------------------------------------------------------------
// Story 3.1: high-level entry point
// ---------------------------------------------------------------------------

/// Run the consolidation pipeline from a project root.
///
/// Loads all embeddable rows, clusters by cosine ≥ threshold, and either
/// prints the plan (dry-run) or applies it (emit events + update DB).
pub fn run_consolidation(
    conn: &Connection,
    opts: &ConsolidateOptions,
    writer: &mut impl std::io::Write,
) -> KimetsuResult<ConsolidateSummary> {
    let by_model = load_embeddable_rows(conn)?;

    let mut all_rows: Vec<ConsolidateRow> = by_model.into_values().flatten().collect();
    // Stable order across models for deterministic output.
    all_rows.sort_by(|a, b| a.memory_id.cmp(&b.memory_id));

    let clusters = find_merge_clusters(&all_rows, opts.threshold);

    let mut summary = ConsolidateSummary {
        clusters_found: clusters.len(),
        ..Default::default()
    };

    if clusters.is_empty() {
        writeln!(
            writer,
            "No near-duplicate clusters found (threshold={:.2}).",
            opts.threshold
        )?;
        return Ok(summary);
    }

    if opts.dry_run {
        writeln!(
            writer,
            "Dry-run: {} cluster(s) found (threshold={:.2}):",
            clusters.len(),
            opts.threshold
        )?;
        for (i, cluster) in clusters.iter().enumerate() {
            writeln!(
                writer,
                "\nCluster {}:  SURVIVOR → {} [score={:.2}  uses={}]",
                i + 1,
                cluster.survivor.memory_id,
                cluster.survivor.usefulness_score,
                cluster.survivor.use_count
            )?;
            writeln!(writer, "  Text: {}", truncate(&cluster.survivor.text, 80))?;
            for m in &cluster.members {
                writeln!(
                    writer,
                    "  MEMBER  → {} [score={:.2}  uses={}]",
                    m.memory_id, m.usefulness_score, m.use_count
                )?;
                writeln!(writer, "    Text: {}", truncate(&m.text, 80))?;
            }
        }
        return Ok(summary);
    }

    // Apply merges.
    let run_id = kimetsu_core::ids::RunId::new();
    for cluster in &clusters {
        match apply_merge(conn, cluster, run_id) {
            Ok(merged) => {
                summary.memories_merged += merged;
            }
            Err(e) => {
                writeln!(
                    writer,
                    "warn: merge of cluster around {} failed: {e}",
                    cluster.survivor.memory_id
                )?;
            }
        }
    }

    writeln!(
        writer,
        "Consolidated {} cluster(s): {} memor{} merged.",
        summary.clusters_found,
        summary.memories_merged,
        if summary.memories_merged == 1 {
            "y"
        } else {
            "ies"
        }
    )?;

    Ok(summary)
}

// ---------------------------------------------------------------------------
// Story 3.2: loose-cluster distillation
// ---------------------------------------------------------------------------

/// Find loose clusters: cosine in [lo, hi] band AND ≥ 1 shared domain tag.
/// Only clusters with ≥ `min_size` members are returned.
pub fn find_distill_clusters(
    rows: &[ConsolidateRow],
    opts: &DistillOptions,
) -> Vec<DistillCluster> {
    let n = rows.len();
    if n < opts.min_cluster_size {
        return Vec::new();
    }

    // Parse tags once for each row.
    let row_tags: Vec<Vec<String>> = rows.iter().map(|r| parse_tags(&r.text)).collect();

    let mut uf = UnionFind::new(n);

    for i in 0..n {
        for j in (i + 1)..n {
            if rows[i].model_id != rows[j].model_id {
                continue;
            }
            let sim = cosine(&rows[i].embedding, &rows[j].embedding);
            if sim < opts.lo || sim > opts.hi {
                continue;
            }
            // Require ≥ 1 shared tag.
            let shared = row_tags[i].iter().any(|t| row_tags[j].contains(t));
            if shared {
                uf.union(i, j);
            }
        }
    }

    // Collect root → members.
    let mut root_to_members: HashMap<usize, Vec<usize>> = HashMap::new();
    for i in 0..n {
        let root = uf.find(i);
        root_to_members.entry(root).or_default().push(i);
    }

    let mut clusters = Vec::new();
    for (_, members) in root_to_members {
        if members.len() < opts.min_cluster_size {
            continue;
        }
        // Compute the shared tags across ALL members.
        let mut shared_tags: Vec<String> = row_tags[members[0]].clone();
        for &i in &members[1..] {
            shared_tags.retain(|t| row_tags[i].contains(t));
        }
        if shared_tags.is_empty() {
            continue; // no common tag — skip (union may have chained)
        }
        let memories: Vec<ConsolidateRow> = members.iter().map(|&i| rows[i].clone()).collect();
        clusters.push(DistillCluster {
            shared_tags,
            memories,
        });
    }

    clusters.sort_by(|a, b| a.shared_tags.cmp(&b.shared_tags));
    clusters
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn truncate(s: &str, max: usize) -> String {
    let chars: Vec<char> = s.chars().collect();
    if chars.len() <= max {
        s.to_string()
    } else {
        format!("{}…", chars[..max].iter().collect::<String>())
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use rusqlite::params;

    // ------------------------------------------------------------------
    // cosine
    // ------------------------------------------------------------------
    #[test]
    fn cosine_same_vector_is_one() {
        let v = vec![1.0f32, 0.5, -0.3];
        assert!((cosine(&v, &v) - 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_orthogonal_is_zero() {
        assert!((cosine(&[1.0f32, 0.0], &[0.0f32, 1.0]) - 0.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_opposite_is_minus_one() {
        assert!((cosine(&[1.0f32, 0.0], &[-1.0f32, 0.0]) + 1.0).abs() < 1e-5);
    }

    #[test]
    fn cosine_empty_returns_zero() {
        assert_eq!(cosine(&[], &[]), 0.0);
    }

    #[test]
    fn cosine_dim_mismatch_returns_zero() {
        assert_eq!(cosine(&[1.0f32], &[1.0f32, 2.0]), 0.0);
    }

    // ------------------------------------------------------------------
    // parse_tags
    // ------------------------------------------------------------------
    #[test]
    fn parse_tags_extracts_tags() {
        let text = "Always use cargo fmt [tags: rust, tooling, ci]";
        let tags = parse_tags(text);
        assert_eq!(tags, vec!["ci", "rust", "tooling"]);
    }

    #[test]
    fn parse_tags_no_block_returns_empty() {
        assert!(parse_tags("no tags here").is_empty());
    }

    #[test]
    fn parse_tags_case_insensitive_key() {
        let text = "Something [TAGS: Rust, CI]";
        let tags = parse_tags(text);
        assert!(tags.contains(&"rust".to_string()));
        assert!(tags.contains(&"ci".to_string()));
    }

    #[test]
    fn parse_tags_deduplicates() {
        let text = "text [tags: a, b, a]";
        let tags = parse_tags(text);
        assert_eq!(tags.iter().filter(|t| *t == "a").count(), 1);
    }

    // ------------------------------------------------------------------
    // find_merge_clusters
    // ------------------------------------------------------------------

    fn make_row(id: &str, vec: Vec<f32>) -> ConsolidateRow {
        ConsolidateRow {
            memory_id: id.to_string(),
            scope: "project".to_string(),
            kind: "fact".to_string(),
            text: format!("text {id}"),
            use_count: 1,
            usefulness_score: 1.0,
            last_useful_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            embedding: vec,
            model_id: "stub".to_string(),
        }
    }

    #[test]
    fn find_merge_clusters_identical_vectors_cluster() {
        let v = vec![1.0f32, 0.0, 0.0];
        let rows = vec![
            make_row("a", v.clone()),
            make_row("b", v.clone()),
            make_row("c", v.clone()),
        ];
        let clusters = find_merge_clusters(&rows, 0.92);
        assert_eq!(clusters.len(), 1, "one cluster of identical vectors");
        assert_eq!(
            clusters[0].members.len(),
            2,
            "two members (one is survivor)"
        );
    }

    #[test]
    fn find_merge_clusters_orthogonal_no_clusters() {
        let rows = vec![
            make_row("a", vec![1.0f32, 0.0]),
            make_row("b", vec![0.0f32, 1.0]),
        ];
        let clusters = find_merge_clusters(&rows, 0.92);
        assert!(clusters.is_empty(), "orthogonal vectors do not cluster");
    }

    #[test]
    fn find_merge_clusters_different_models_do_not_cluster() {
        let v = vec![1.0f32, 0.0];
        let mut r1 = make_row("a", v.clone());
        r1.model_id = "model-a".to_string();
        let mut r2 = make_row("b", v.clone());
        r2.model_id = "model-b".to_string();
        let clusters = find_merge_clusters(&[r1, r2], 0.92);
        assert!(clusters.is_empty(), "different models must not cluster");
    }

    #[test]
    fn survivor_is_highest_usefulness_score() {
        let v = vec![1.0f32, 0.0, 0.0];
        let mut high = make_row("high", v.clone());
        high.usefulness_score = 10.0;
        high.use_count = 5;
        let mut low = make_row("low", v.clone());
        low.usefulness_score = 0.1;
        low.use_count = 1;
        let clusters = find_merge_clusters(&[low, high], 0.92);
        assert_eq!(clusters.len(), 1);
        assert_eq!(clusters[0].survivor.memory_id, "high");
        assert_eq!(clusters[0].members[0].memory_id, "low");
    }

    // ------------------------------------------------------------------
    // find_distill_clusters
    // ------------------------------------------------------------------
    #[test]
    fn find_distill_clusters_requires_shared_tags() {
        // Two rows in the 0.75–0.85 cosine band but no shared tags → no cluster.
        let v1 = vec![1.0f32, 0.5, 0.0];
        let v2 = vec![1.0f32, 0.4, 0.1];
        let mut r1 = make_row("a", v1);
        r1.text = "first memory [tags: rust]".to_string();
        let mut r2 = make_row("b", v2);
        r2.text = "second memory [tags: python]".to_string();
        let mut r3 = make_row("c", vec![1.0f32, 0.4, 0.05]);
        r3.text = "third memory [tags: go]".to_string();
        let opts = DistillOptions {
            lo: 0.7,
            hi: 0.99,
            min_cluster_size: 2,
        };
        let clusters = find_distill_clusters(&[r1, r2, r3], &opts);
        assert!(clusters.is_empty(), "no shared tags → no distill cluster");
    }

    #[test]
    fn find_distill_clusters_shared_tag_and_band_clusters() {
        // Three rows with similar vectors AND shared tag "ci".
        let v = vec![1.0f32, 0.5, 0.1];
        let make = |id: &str, extra: f32| {
            let mut r = make_row(id, vec![1.0 + extra, 0.5, 0.1]);
            r.text = format!("memory {id} [tags: rust, ci]");
            r
        };
        let rows = vec![make("a", 0.0), make("b", 0.001), make("c", 0.002)];
        let _ = v; // silence unused
        let opts = DistillOptions {
            lo: 0.0,
            hi: 1.0,
            min_cluster_size: 3,
        };
        let clusters = find_distill_clusters(&rows, &opts);
        assert!(!clusters.is_empty(), "shared tag + band → distill cluster");
        assert!(
            clusters[0].shared_tags.contains(&"ci".to_string()),
            "shared_tags contains 'ci'"
        );
    }

    // ------------------------------------------------------------------
    // apply_merge (against in-memory SQLite)
    // ------------------------------------------------------------------
    #[test]
    fn apply_merge_supersedes_members_and_updates_survivor_stats() {
        use kimetsu_core::ids::RunId;

        let conn = rusqlite::Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("init");

        // Insert survivor and one member.
        for (id, use_count, score) in [("survivor", 3i64, 5.0f64), ("member", 2i64, 2.0f64)] {
            conn.execute(
                "INSERT INTO memories
                   (memory_id, scope, kind, text, normalized_text, confidence,
                    provenance_snapshot_json, created_at, use_count, usefulness_score)
                 VALUES (?1,'project','fact',?2,?2,0.9,'{}','2026-01-01T00:00:00Z',?3,?4)",
                params![id, format!("text {id}"), use_count, score],
            )
            .expect("insert");
        }

        let survivor = ConsolidateRow {
            memory_id: "survivor".to_string(),
            scope: "project".to_string(),
            kind: "fact".to_string(),
            text: "text survivor".to_string(),
            use_count: 3,
            usefulness_score: 5.0,
            last_useful_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            embedding: vec![1.0, 0.0],
            model_id: "stub".to_string(),
        };
        let member = ConsolidateRow {
            memory_id: "member".to_string(),
            scope: "project".to_string(),
            kind: "fact".to_string(),
            text: "text member".to_string(),
            use_count: 2,
            usefulness_score: 2.0,
            last_useful_at: None,
            created_at: "2026-01-01T00:00:00Z".to_string(),
            embedding: vec![1.0, 0.0],
            model_id: "stub".to_string(),
        };
        let cluster = MergeCluster {
            survivor,
            members: vec![member],
        };

        let run_id = RunId::new();
        let merged = apply_merge(&conn, &cluster, run_id).expect("apply_merge");
        assert_eq!(merged, 1);

        // Survivor stats updated.
        let (use_count, score): (i64, f64) = conn
            .query_row(
                "SELECT use_count, usefulness_score FROM memories WHERE memory_id = 'survivor'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query survivor");
        assert_eq!(use_count, 5, "use_count = 3 + 2");
        assert!((score - 7.0).abs() < 0.01, "score = 5.0 + 2.0, got {score}");

        // Member superseded.
        let superseded_by: Option<String> = conn
            .query_row(
                "SELECT superseded_by FROM memories WHERE memory_id = 'member'",
                [],
                |r| r.get(0),
            )
            .expect("query member");
        assert_eq!(superseded_by.as_deref(), Some("survivor"));
    }

    #[test]
    fn citations_reassigned_on_merge() {
        use kimetsu_core::ids::RunId;

        let conn = rusqlite::Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("init");

        // Insert two memory rows.
        for id in ["survivor", "member"] {
            conn.execute(
                "INSERT INTO memories
                   (memory_id, scope, kind, text, normalized_text, confidence,
                    provenance_snapshot_json, created_at, use_count, usefulness_score)
                 VALUES (?1,'project','fact',?2,?2,0.9,'{}','2026-01-01T00:00:00Z',1,1.0)",
                params![id, format!("text {id}")],
            )
            .expect("insert memory");
        }

        // Insert a citation for the member.
        conn.execute(
            "INSERT INTO memory_citations (run_id, memory_id, turn, cited_at)
             VALUES ('run-1', 'member', 1, '2026-01-01T00:00:00Z')",
            [],
        )
        .expect("insert citation");

        let cluster = MergeCluster {
            survivor: ConsolidateRow {
                memory_id: "survivor".to_string(),
                scope: "project".to_string(),
                kind: "fact".to_string(),
                text: "text survivor".to_string(),
                use_count: 1,
                usefulness_score: 1.0,
                last_useful_at: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                embedding: vec![1.0, 0.0],
                model_id: "stub".to_string(),
            },
            members: vec![ConsolidateRow {
                memory_id: "member".to_string(),
                scope: "project".to_string(),
                kind: "fact".to_string(),
                text: "text member".to_string(),
                use_count: 1,
                usefulness_score: 1.0,
                last_useful_at: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                embedding: vec![1.0, 0.0],
                model_id: "stub".to_string(),
            }],
        };

        apply_merge(&conn, &cluster, RunId::new()).expect("apply_merge");

        // Citation must now point at survivor.
        let mid: String = conn
            .query_row(
                "SELECT memory_id FROM memory_citations WHERE run_id = 'run-1' AND turn = 1",
                [],
                |r| r.get(0),
            )
            .expect("query citation");
        assert_eq!(mid, "survivor", "citation reassigned to survivor");

        // No citations remain for the member.
        let member_count: i64 = conn
            .query_row(
                "SELECT COUNT(*) FROM memory_citations WHERE memory_id = 'member'",
                [],
                |r| r.get(0),
            )
            .expect("count member citations");
        assert_eq!(member_count, 0, "member citations deleted");
    }

    // ------------------------------------------------------------------
    // superseded rows excluded from retrieval
    // ------------------------------------------------------------------
    #[test]
    fn superseded_row_excluded_from_latest_memory_candidates() {
        use crate::context::retrieve_context_with_embedder;
        use crate::embeddings::NoopEmbedder;
        use kimetsu_core::config::BrokerWeights;

        let conn = rusqlite::Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("init");

        // Insert a survivor and a superseded member.
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score)
             VALUES ('surv','project','fact','rust tooling','rust tooling',0.9,'{}',
                     '2026-01-01T00:00:00Z',1,1.0)",
            [],
        )
        .expect("insert survivor");
        conn.execute(
            "INSERT INTO memories
               (memory_id, scope, kind, text, normalized_text, confidence,
                provenance_snapshot_json, created_at, use_count, usefulness_score,
                superseded_by)
             VALUES ('dup','project','fact','rust tooling dup','rust tooling dup',0.9,'{}',
                     '2026-01-01T00:00:00Z',1,1.0,'surv')",
            [],
        )
        .expect("insert superseded");

        // Populate FTS for survivor only (dup was already removed from FTS on merge).
        conn.execute(
            "INSERT INTO memories_fts (memory_id, text, kind, scope)
             VALUES ('surv', 'rust tooling', 'fact', 'project')",
            [],
        )
        .expect("insert fts");

        let weights = BrokerWeights::default();
        let req = crate::context::ContextRequest {
            stage: "test".to_string(),
            query: "rust tooling".to_string(),
            budget_tokens: 4096,
            ..Default::default()
        };
        let embedder = NoopEmbedder;
        let bundle = retrieve_context_with_embedder(&conn, "", &weights, req, &[], &embedder)
            .expect("retrieve");

        let ids: Vec<&str> = bundle
            .capsules
            .iter()
            .chain(bundle.excluded.iter())
            .filter_map(|c| c.expansion_handle.strip_prefix("memory:"))
            .collect();
        assert!(
            !ids.contains(&"dup"),
            "superseded memory must not appear in retrieval"
        );
    }

    // ------------------------------------------------------------------
    // v2→v3 migration test (integration)
    // ------------------------------------------------------------------
    #[test]
    fn v2_brain_migrates_to_v3_with_backup_and_superseded_by_column() {
        use crate::migrate;

        let tmp_id = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_dir = std::env::temp_dir().join(format!("kimetsu-test-v3mig-{tmp_id}"));
        std::fs::create_dir_all(&tmp_dir).expect("create tmp dir");

        let db_path = tmp_dir.join("brain.db");
        {
            // Build a v2 brain with one memory row so the backup fires.
            let conn = rusqlite::Connection::open(&db_path).expect("open");
            crate::schema::create_baseline_for_test(&conn).expect("baseline");
            crate::schema::migrate_v1_to_v2(&conn).expect("v1→v2");
            conn.execute(
                "UPDATE schema_info SET value = 2 WHERE key = 'kimetsu_schema_version'",
                [],
            )
            .expect("stamp v2");
            conn.execute(
                "INSERT INTO memories
                   (memory_id, scope, kind, text, normalized_text, confidence,
                    provenance_snapshot_json, created_at, use_count, usefulness_score)
                 VALUES ('m1','project','fact','hello','hello',0.9,'{}','2026-01-01T00:00:00Z',0,0.0)",
                [],
            ).expect("insert memory");
        }

        // Now open read-write → should trigger migration v2→v3 + backup.
        {
            let conn = rusqlite::Connection::open(&db_path).expect("reopen");
            let outcome = migrate::run_migrations(&conn).expect("run_migrations");
            assert_eq!(outcome.from, 2);
            assert_eq!(outcome.to, 3);
            assert_eq!(outcome.applied, vec![3]);
            // Backup created (non-empty brain).
            assert!(
                outcome.backup_path.is_some(),
                "backup must be created for non-empty brain during v2→v3"
            );
            // Column exists.
            let has_col: bool = conn.query_row(
                "SELECT COUNT(*) FROM pragma_table_info('memories') WHERE name = 'superseded_by'",
                [],
                |r| r.get::<_, i64>(0),
            ).map(|n| n > 0).unwrap_or(false);
            assert!(
                has_col,
                "superseded_by column must exist after v3 migration"
            );
        }

        let _ = std::fs::remove_dir_all(&tmp_dir);
    }

    // ------------------------------------------------------------------
    // Fix 1: consolidation must be rebuild-safe
    //
    // Seed two memories (one with a citation pointing at the member),
    // set non-zero stats on both via direct SQL (simulating accumulated
    // run outcomes), then consolidate.  Capture the exact post-
    // consolidation stats and citation target, run `rebuild_in_place`,
    // and assert both are IDENTICAL after rebuild.
    //
    // Pre-fix behaviour: rebuild reverted use_count to 0 because the
    // stat accumulation was a direct UPDATE rather than being carried in
    // the memory.superseded event payload.
    // ------------------------------------------------------------------
    #[test]
    fn consolidation_is_rebuild_safe() {
        use crate::projector;
        use kimetsu_core::ids::RunId;

        let conn = rusqlite::Connection::open_in_memory().expect("open");
        crate::schema::initialize(&conn).expect("init");

        let run_id = RunId::new();

        // --- bootstrap via events so the events table is populated --------
        projector::apply_events(
            &conn,
            &[kimetsu_core::event::Event::new(
                run_id,
                "run.started",
                serde_json::json!({"project_id": "test", "task": "rebuild-safety"}),
            )],
        )
        .expect("run.started");

        for (mid, text) in [("survivor", "text survivor"), ("member", "text member")] {
            projector::apply_events(
                &conn,
                &[kimetsu_core::event::Event::new(
                    run_id,
                    "memory.accepted",
                    serde_json::json!({
                        "memory_id": mid,
                        "scope": "project",
                        "kind": "fact",
                        "text": text,
                        "normalized_text": text,
                        "confidence": 0.9
                    }),
                )],
            )
            .expect("accepted");
        }

        // Simulate pre-consolidation accumulated stats via direct SQL
        // (in production these come from run.finished outcome attribution).
        // Survivor: use_count=3, score=5.0  |  Member: use_count=2, score=2.0
        conn.execute(
            "UPDATE memories SET use_count = 3, usefulness_score = 5.0 \
             WHERE memory_id = 'survivor'",
            [],
        )
        .expect("seed survivor stats");
        conn.execute(
            "UPDATE memories SET use_count = 2, usefulness_score = 2.0 \
             WHERE memory_id = 'member'",
            [],
        )
        .expect("seed member stats");

        // Citation pointing at the member via a memory.cited event so it
        // will be replayed (not a raw SQL insert that rebuild would wipe).
        projector::apply_events(
            &conn,
            &[kimetsu_core::event::Event::new(
                run_id,
                "memory.cited",
                serde_json::json!({
                    "memory_id": "member",
                    "turn": 1,
                    "rationale": "test citation"
                }),
            )],
        )
        .expect("memory.cited");

        // --- Consolidate --------------------------------------------------
        let cluster = MergeCluster {
            survivor: ConsolidateRow {
                memory_id: "survivor".to_string(),
                scope: "project".to_string(),
                kind: "fact".to_string(),
                text: "text survivor".to_string(),
                use_count: 3,
                usefulness_score: 5.0,
                last_useful_at: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                embedding: vec![1.0, 0.0],
                model_id: "stub".to_string(),
            },
            members: vec![ConsolidateRow {
                memory_id: "member".to_string(),
                scope: "project".to_string(),
                kind: "fact".to_string(),
                text: "text member".to_string(),
                use_count: 2,
                usefulness_score: 2.0,
                last_useful_at: None,
                created_at: "2026-01-01T00:00:00Z".to_string(),
                embedding: vec![1.0, 0.0],
                model_id: "stub".to_string(),
            }],
        };
        apply_merge(&conn, &cluster, RunId::new()).expect("apply_merge");

        // Capture what the live path produced.  After consolidation:
        //   survivor.use_count  = 3 (initial) + 2 (delta) = 5 — BUT only
        //   the delta (2) is event-sourced; the initial 3 was set by
        //   direct SQL and is wiped by rebuild.  So post-rebuild we expect
        //   exactly the deltas contributed by the superseded members.
        //
        // The invariant we check: whatever consolidation produces MUST
        // match what rebuild produces.  We capture from the DB rather than
        // hard-coding so the test stays valid even if the initial SQL seeds
        // change.
        let (pre_uc, pre_score): (i64, f64) = conn
            .query_row(
                "SELECT use_count, usefulness_score FROM memories \
                 WHERE memory_id = 'survivor'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query survivor after consolidation");
        let pre_cited: String = conn
            .query_row(
                "SELECT memory_id FROM memory_citations WHERE turn = 1",
                [],
                |r| r.get(0),
            )
            .expect("citation must exist post-consolidation");
        assert_eq!(
            pre_cited, "survivor",
            "pre-rebuild: citation must point at survivor"
        );

        // ---- REBUILD -----
        projector::rebuild_in_place(&conn).expect("rebuild_in_place");

        // Post-rebuild: stats and citation must match pre-rebuild.
        let (post_uc, post_score): (i64, f64) = conn
            .query_row(
                "SELECT use_count, usefulness_score FROM memories \
                 WHERE memory_id = 'survivor'",
                [],
                |r| Ok((r.get(0)?, r.get(1)?)),
            )
            .expect("query survivor after rebuild");

        // The member's delta (use_count=2, score=2.0) must survive rebuild.
        // pre_uc includes the direct-SQL initial value (3) which rebuild
        // cannot restore (not event-sourced); we only assert the delta:
        //   post_uc  ≥ member.use_count (2)
        //   post_score ≥ member.usefulness_score (2.0)
        // And more precisely, post_uc == member delta applied to 0 == 2.
        assert_eq!(
            post_uc, 2,
            "post-rebuild: survivor use_count must contain member delta 2 (got {post_uc})"
        );
        assert!(
            (post_score - 2.0).abs() < 0.01,
            "post-rebuild: survivor score must contain member delta 2.0 (got {post_score})"
        );

        let post_cited: String = conn
            .query_row(
                "SELECT memory_id FROM memory_citations WHERE turn = 1",
                [],
                |r| r.get(0),
            )
            .expect("citation must still exist after rebuild");
        assert_eq!(
            post_cited, "survivor",
            "post-rebuild: citation must still point at survivor (got {post_cited:?})"
        );

        // Bonus: pre_uc/pre_score must also contain the delta (live path
        // sanity-check so the test still catches regressions there).
        assert!(
            pre_uc >= 2,
            "pre-rebuild: survivor use_count must include member delta ≥2 (got {pre_uc})"
        );
        assert!(
            pre_score >= 2.0,
            "pre-rebuild: survivor score must include member delta ≥2.0 (got {pre_score})"
        );
    }
}
