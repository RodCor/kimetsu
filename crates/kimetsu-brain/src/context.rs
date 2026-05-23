use std::cmp::Ordering;
use std::collections::HashMap;

use kimetsu_core::config::{BrokerWeights, StageWeights};
use kimetsu_core::memory::MemoryScope;
use kimetsu_core::{KimetsuResult, ids::new_id};
use rusqlite::{Connection, params};
use serde::{Deserialize, Serialize};
use time::OffsetDateTime;

use crate::embeddings::{
    self, DEFAULT_HYBRID_ALPHA, Embedder, cosine_similarity, decode_embedding,
};

/// v0.4.2: a pre-computed query embedding paired with the producing
/// model's id. Threaded down into [`memory_candidates`] so each row
/// can decide whether to contribute a cosine term (only when the
/// row's `embedding_model` matches the active query's `model_id`).
#[derive(Debug, Clone)]
struct QueryEmbedding {
    vector: Vec<f32>,
    model_id: String,
}

impl QueryEmbedding {
    fn from_embedder(embedder: &dyn Embedder, query: &str) -> Option<Self> {
        if embedder.is_noop() {
            return None;
        }
        match embedder.embed(query) {
            Ok(v) if v.len() == embedder.dim() => Some(Self {
                vector: v,
                model_id: embedder.model_id().to_string(),
            }),
            // NotImplemented / dim-mismatch / load failure → silently
            // skip the cosine blend. v0.4.2 surfaces no warning here
            // by design — the broker stays usable on best-effort
            // semantic retrieval.
            _ => None,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContextCapsule {
    pub id: String,
    pub kind: String,
    pub summary: String,
    pub token_estimate: u32,
    pub expansion_handle: String,
    pub provenance: Vec<ProvenanceRef>,
    pub confidence: f32,
    pub freshness: f32,
    pub relevance: f32,
    pub scope_weight: f32,
    pub score: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProvenanceRef {
    pub source: String,
    pub id: String,
    pub excerpt: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ContextRequest {
    pub stage: String,
    pub query: String,
    pub budget_tokens: u32,
}

#[derive(Debug, Clone)]
pub struct ContextBundle {
    pub stage: String,
    pub budget_tokens: u32,
    pub used_tokens: u32,
    pub capsules: Vec<ContextCapsule>,
    pub excluded: Vec<ContextCapsule>,
}

#[derive(Debug, Clone)]
struct Candidate {
    capsule: ContextCapsule,
    raw_relevance: f32,
}

pub fn retrieve_context(
    conn: &Connection,
    repo_root: &str,
    weights: &BrokerWeights,
    request: ContextRequest,
) -> KimetsuResult<ContextBundle> {
    retrieve_context_multi(conn, repo_root, weights, request, &[])
}

/// v0.4.1: multi-conn variant. `extra_memory_conns` is searched for
/// memory candidates only (repo files + manifests stay project-local).
/// The candidate stream is concatenated BEFORE normalization so the
/// blended set is normalized together — keeping a user-brain capsule
/// and a project-brain capsule comparable on the same `raw_relevance`
/// scale.
///
/// Today `extra_memory_conns` carries at most one entry (the user
/// brain at `~/.kimetsu/brain.db`); the slice shape leaves room for
/// future scope tiers (team brain, org brain) without breaking the
/// signature.
///
/// v0.4.2: uses [`embeddings::open_default_embedder`] for the cosine
/// term. Pre-v0.4.3 the default is `NoopEmbedder`, which short-
/// circuits the cosine path so retrieval stays FTS-only — exact
/// v0.4.1 behavior. v0.4.3 swaps the default to a real embedder.
pub fn retrieve_context_multi(
    conn: &Connection,
    repo_root: &str,
    weights: &BrokerWeights,
    request: ContextRequest,
    extra_memory_conns: &[&Connection],
) -> KimetsuResult<ContextBundle> {
    let embedder = embeddings::open_default_embedder();
    retrieve_context_with_embedder(
        conn,
        repo_root,
        weights,
        request,
        extra_memory_conns,
        embedder.as_ref(),
    )
}

/// v0.4.2: explicit-embedder variant. Lets tests inject `StubEmbedder`
/// or any other [`Embedder`] without going through
/// [`embeddings::open_default_embedder`]. v0.4.3 callers (chat REPL,
/// MCP server) can also use this directly to hold one embedder
/// instance for the lifetime of a session instead of paying the
/// model-load cost on every retrieval.
pub fn retrieve_context_with_embedder(
    conn: &Connection,
    repo_root: &str,
    weights: &BrokerWeights,
    request: ContextRequest,
    extra_memory_conns: &[&Connection],
    embedder: &dyn Embedder,
) -> KimetsuResult<ContextBundle> {
    let query_embedding = QueryEmbedding::from_embedder(embedder, &request.query);
    let mut candidates = Vec::new();
    candidates.extend(memory_candidates(conn, &request.query, query_embedding.as_ref())?);
    for extra in extra_memory_conns {
        candidates.extend(memory_candidates(extra, &request.query, query_embedding.as_ref())?);
    }
    candidates.extend(repo_file_candidates(conn, repo_root, &request.query, 30)?);
    candidates.extend(manifest_candidates(conn, repo_root, &request.query)?);

    normalize_and_score(&mut candidates, weights_for_stage(weights, &request.stage));

    let mut capsules = candidates
        .into_iter()
        .map(|candidate| candidate.capsule)
        .collect::<Vec<_>>();

    capsules.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| {
                right
                    .freshness
                    .partial_cmp(&left.freshness)
                    .unwrap_or(Ordering::Equal)
            })
            .then_with(|| left.id.cmp(&right.id))
    });

    // MP-17 #13: Maximal-Marginal-Relevance (MMR) re-ranking — when two
    // capsules look very similar (same tokens in the summary), keep the
    // higher-scoring one but push the redundant ones down so the budget
    // covers more distinct ground. Lambda=0.7 keeps the original ordering
    // strongly while penalizing >0.5-Jaccard overlaps.
    let capsules = apply_mmr_diversity(capsules, 0.7);

    let capsule_budget = request.budget_tokens / 2;
    let mut used_tokens = 0u32;
    let mut included = Vec::new();
    let mut excluded = Vec::new();

    for capsule in capsules {
        if used_tokens.saturating_add(capsule.token_estimate) <= capsule_budget {
            used_tokens += capsule.token_estimate;
            included.push(capsule);
        } else {
            excluded.push(capsule);
        }
    }

    Ok(ContextBundle {
        stage: request.stage,
        budget_tokens: request.budget_tokens,
        used_tokens,
        capsules: included,
        excluded,
    })
}

pub fn search_repo_files(
    conn: &Connection,
    repo_root: &str,
    query: &str,
    limit: u32,
) -> KimetsuResult<Vec<ContextCapsule>> {
    let candidates = repo_file_candidates(conn, repo_root, query, limit)?;
    let mut capsules = candidates
        .into_iter()
        .map(|mut candidate| {
            candidate.capsule.relevance = candidate.raw_relevance;
            candidate.capsule.score = candidate.raw_relevance;
            candidate.capsule
        })
        .collect::<Vec<_>>();
    capsules.sort_by(|left, right| {
        right
            .score
            .partial_cmp(&left.score)
            .unwrap_or(Ordering::Equal)
            .then_with(|| left.expansion_handle.cmp(&right.expansion_handle))
    });
    Ok(capsules)
}

fn memory_candidates(
    conn: &Connection,
    query: &str,
    query_embedding: Option<&QueryEmbedding>,
) -> KimetsuResult<Vec<Candidate>> {
    let query_tokens = query_tokens(query);
    if let Some(fts_query) = fts_query(query) {
        let candidates =
            memory_fts_candidates(conn, &query_tokens, &fts_query, 80, query_embedding)?;
        if !candidates.is_empty() {
            return Ok(candidates);
        }
    }

    latest_memory_candidates(conn, &query_tokens, 200, query_embedding)
}

fn latest_memory_candidates(
    conn: &Connection,
    query_tokens: &[String],
    limit: u32,
    query_embedding: Option<&QueryEmbedding>,
) -> KimetsuResult<Vec<Candidate>> {
    // MP-4d: exclude invalidated memories from retrieval. The row stays in
    // brain.db so `memory list` and replay can still see the history; only
    // the broker filters it out.
    //
    // v0.4.2: SELECT now also pulls the optional embedding + model id
    // so we can blend a cosine score with the lexical match.
    let mut stmt = conn.prepare_cached(
        "
        SELECT memory_id, scope, kind, text, confidence, created_at,
               use_count, usefulness_score, embedding, embedding_model
        FROM memories
        WHERE invalidated_at IS NULL
        ORDER BY created_at DESC
        LIMIT ?1
        ",
    )?;

    let rows = stmt.query_map(params![limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f32>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, f64>(7)?,
            row.get::<_, Option<Vec<u8>>>(8)?,
            row.get::<_, Option<String>>(9)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (
            memory_id,
            scope,
            kind,
            text,
            confidence,
            created_at,
            use_count,
            usefulness_score,
            embedding,
            embedding_model,
        ) = row?;
        let cosine = compute_cosine(query_embedding, embedding.as_deref(), embedding_model.as_deref());
        if let Some(candidate) = memory_row_to_candidate(
            query_tokens,
            memory_id,
            scope,
            kind,
            text,
            confidence,
            created_at,
            use_count,
            usefulness_score,
            None,
            cosine,
        ) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

fn memory_fts_candidates(
    conn: &Connection,
    query_tokens: &[String],
    fts_query: &str,
    limit: u32,
    query_embedding: Option<&QueryEmbedding>,
) -> KimetsuResult<Vec<Candidate>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT m.memory_id, m.scope, m.kind, m.text, m.confidence, m.created_at,
               m.use_count, m.usefulness_score, bm25(memories_fts) AS rank,
               m.embedding, m.embedding_model
        FROM memories_fts
        JOIN memories m
          ON m.memory_id = memories_fts.memory_id
        WHERE m.invalidated_at IS NULL
          AND memories_fts MATCH ?1
        ORDER BY rank
        LIMIT ?2
        ",
    )?;

    let rows = stmt.query_map(params![fts_query, limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, f32>(4)?,
            row.get::<_, String>(5)?,
            row.get::<_, i64>(6)?,
            row.get::<_, f64>(7)?,
            row.get::<_, f64>(8)?,
            row.get::<_, Option<Vec<u8>>>(9)?,
            row.get::<_, Option<String>>(10)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (
            memory_id,
            scope,
            kind,
            text,
            confidence,
            created_at,
            use_count,
            usefulness_score,
            rank,
            embedding,
            embedding_model,
        ) = row?;
        let fts_relevance = (-rank as f32).max(0.0);
        let cosine = compute_cosine(query_embedding, embedding.as_deref(), embedding_model.as_deref());
        if let Some(candidate) = memory_row_to_candidate(
            query_tokens,
            memory_id,
            scope,
            kind,
            text,
            confidence,
            created_at,
            use_count,
            usefulness_score,
            Some(fts_relevance),
            cosine,
        ) {
            candidates.push(candidate);
        }
    }
    Ok(candidates)
}

/// v0.4.2: cosine helper used by both the FTS and latest-memory
/// retrieval branches. Returns `Some(score in [-1, 1])` when a
/// non-null embedding is present AND its `embedding_model` matches
/// the active `query_embedding`'s model id. Otherwise None — the
/// caller treats None as "lexical only".
///
/// Cross-model rows are intentionally NOT blended: a row embedded
/// with `stub-d8` and a query embedded with `bge-small-en-v1.5`
/// produce meaningless dot products. Falling back to FTS for those
/// rows keeps hybrid retrieval safe across schema upgrades and
/// `kimetsu brain reindex` migrations (v0.4.3).
fn compute_cosine(
    query_embedding: Option<&QueryEmbedding>,
    row_bytes: Option<&[u8]>,
    row_model: Option<&str>,
) -> Option<f32> {
    let q = query_embedding?;
    let bytes = row_bytes?;
    let model = row_model?;
    if model != q.model_id {
        return None;
    }
    let row_vec = match decode_embedding(bytes, Some(q.vector.len())) {
        Ok(v) => v,
        Err(_) => return None,
    };
    Some(cosine_similarity(&q.vector, &row_vec))
}

#[allow(clippy::too_many_arguments)]
fn memory_row_to_candidate(
    query_tokens: &[String],
    memory_id: String,
    scope: String,
    kind: String,
    text: String,
    confidence: f32,
    created_at: String,
    use_count: i64,
    usefulness_score: f64,
    raw_relevance_override: Option<f32>,
    cosine_score: Option<f32>,
) -> Option<Candidate> {
    let lexical = lexical_relevance(query_tokens, &format!("{kind} {text}"));
    let lexical_term = raw_relevance_override.unwrap_or(lexical).max(lexical);

    // v0.4.2: hybrid blend.
    //   final = (1 - α) * lexical + α * normalized_cosine
    // where normalized_cosine maps [-1, 1] -> [0, 1] so it composes
    // with the lexical relevance scale.
    //
    // When cosine_score is None (NoopEmbedder, NULL row embedding,
    // cross-model mismatch), the cosine term drops out and the
    // candidate scores lexical-only — exact v0.4.1 behavior. The
    // caller's gate `raw_relevance <= 0.0 && !query_tokens.is_empty()`
    // still works because in the no-cosine path `raw_relevance ==
    // lexical_term`.
    let raw_relevance = match cosine_score {
        Some(c) => {
            let normalized_cos = ((c + 1.0) * 0.5).clamp(0.0, 1.0);
            (1.0 - DEFAULT_HYBRID_ALPHA) * lexical_term + DEFAULT_HYBRID_ALPHA * normalized_cos
        }
        None => lexical_term,
    };

    // Drop the row when neither lexical nor cosine had any signal —
    // an empty query OR a candidate that didn't match any of the
    // search terms. The cosine-only path is still allowed through
    // (raw_relevance > 0) for semantic-only matches against rows
    // whose words don't textually overlap the query.
    if raw_relevance <= 0.0 && !query_tokens.is_empty() {
        return None;
    }

    let freshness = freshness(&created_at);
    let scope_weight = scope_weight(&scope);
    let multiplier = usefulness_multiplier(usefulness_score as f32, use_count as u32);
    let biased_relevance = raw_relevance * multiplier;
    Some(Candidate {
        raw_relevance: biased_relevance,
        capsule: ContextCapsule {
            id: new_id().to_string(),
            kind: "memory".to_string(),
            summary: format!("{scope}:{kind} - {text}"),
            token_estimate: estimate_tokens(&text) + 8,
            expansion_handle: format!("memory:{memory_id}"),
            provenance: vec![ProvenanceRef {
                source: "Memory".to_string(),
                id: memory_id,
                excerpt: Some(excerpt(&text)),
            }],
            confidence,
            freshness,
            relevance: 0.0,
            scope_weight,
            score: 0.0,
        },
    })
}

/// MP-4b multiplier in [0.5, 1.5] derived from a memory's outcome history.
/// `use_count < 3` is treated as small-sample and yields 1.0 (neutral) so a
/// brand-new memory has a fair chance to demonstrate value before being
/// boosted or penalized.
pub(crate) fn usefulness_multiplier(usefulness_score: f32, use_count: u32) -> f32 {
    // MP-17e: soften the hard sample-size threshold via Bayesian smoothing.
    //
    // Old behaviour: hard cutoff at use_count < 3 returned neutral 1.0,
    // then full envelope kicked in. That meant a memory with 2 uses (both
    // helpful) was treated identically to a memory with 0 uses, which
    // wasted early signal. New behaviour: linearly blend toward the
    // full multiplier as use_count climbs to FULL_CONFIDENCE_USES.
    const FULL_CONFIDENCE_USES: u32 = 3;
    const MULTIPLIER_MIN: f32 = 0.5;
    const MULTIPLIER_MAX: f32 = 1.5;
    if use_count == 0 {
        return 1.0;
    }
    let ratio = usefulness_score / use_count as f32; // in -1.0..1.0 typically
    let normalized = ((ratio + 1.0) / 2.0).clamp(0.0, 1.0); // map to 0..1
    let full_multiplier = MULTIPLIER_MIN + normalized * (MULTIPLIER_MAX - MULTIPLIER_MIN);
    let confidence = (use_count as f32 / FULL_CONFIDENCE_USES as f32).min(1.0);
    1.0 * (1.0 - confidence) + full_multiplier * confidence
}

fn repo_file_candidates(
    conn: &Connection,
    repo_root: &str,
    query: &str,
    limit: u32,
) -> KimetsuResult<Vec<Candidate>> {
    let Some(fts_query) = fts_query(query) else {
        return Ok(Vec::new());
    };

    let mut stmt = conn.prepare_cached(
        "
        SELECT path, snippet, language_guess, bm25(repo_files_fts) AS rank
        FROM repo_files_fts
        WHERE repo_root = ?1 AND repo_files_fts MATCH ?2
        ORDER BY rank
        LIMIT ?3
        ",
    )?;

    let rows = stmt.query_map(params![repo_root, fts_query, limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (path, snippet, language, rank) = row?;
        let raw_relevance = (-rank as f32).max(0.0);
        let summary = format!("{path} ({language}) - {}", excerpt(&snippet));
        let token_estimate = estimate_tokens(&summary) + 8;
        candidates.push(Candidate {
            raw_relevance,
            capsule: ContextCapsule {
                id: new_id().to_string(),
                kind: "repo_file".to_string(),
                summary,
                token_estimate,
                expansion_handle: format!("file:{path}"),
                provenance: vec![ProvenanceRef {
                    source: "RepoFile".to_string(),
                    id: path.clone(),
                    excerpt: Some(excerpt(&snippet)),
                }],
                confidence: 0.9,
                freshness: 1.0,
                relevance: 0.0,
                scope_weight: 0.9,
                score: 0.0,
            },
        });
    }
    Ok(candidates)
}

fn manifest_candidates(
    conn: &Connection,
    repo_root: &str,
    query: &str,
) -> KimetsuResult<Vec<Candidate>> {
    if let Some(fts_query) = fts_query(query) {
        let candidates = manifest_fts_candidates(conn, repo_root, &fts_query, 30)?;
        if !candidates.is_empty() {
            return Ok(candidates);
        }
    }

    let query_tokens = query_tokens(query);
    let mut stmt = conn.prepare_cached(
        "
        SELECT manifest_path, manifest_kind, parsed_summary_json
        FROM repo_manifests
        WHERE repo_root = ?1
        ORDER BY manifest_path
        ",
    )?;

    let rows = stmt.query_map(params![repo_root], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (path, kind, summary_json) = row?;
        let raw_relevance =
            lexical_relevance(&query_tokens, &format!("{path} {kind} {summary_json}"));
        if raw_relevance <= 0.0 && !query_tokens.is_empty() {
            continue;
        }
        let summary = format!("{path} manifest ({kind})");
        let token_estimate = estimate_tokens(&summary) + 8;
        candidates.push(Candidate {
            raw_relevance,
            capsule: ContextCapsule {
                id: new_id().to_string(),
                kind: "repo_manifest".to_string(),
                summary,
                token_estimate,
                expansion_handle: format!("file:{path}"),
                provenance: vec![ProvenanceRef {
                    source: "Manifest".to_string(),
                    id: path,
                    excerpt: Some(excerpt(&summary_json)),
                }],
                confidence: 0.95,
                freshness: 1.0,
                relevance: 0.0,
                scope_weight: 0.9,
                score: 0.0,
            },
        });
    }
    Ok(candidates)
}

fn manifest_fts_candidates(
    conn: &Connection,
    repo_root: &str,
    fts_query: &str,
    limit: u32,
) -> KimetsuResult<Vec<Candidate>> {
    let mut stmt = conn.prepare_cached(
        "
        SELECT manifest_path, manifest_kind, parsed_summary_json,
               bm25(repo_manifests_fts) AS rank
        FROM repo_manifests_fts
        WHERE repo_root = ?1 AND repo_manifests_fts MATCH ?2
        ORDER BY rank
        LIMIT ?3
        ",
    )?;

    let rows = stmt.query_map(params![repo_root, fts_query, limit], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, f64>(3)?,
        ))
    })?;

    let mut candidates = Vec::new();
    for row in rows {
        let (path, kind, summary_json, rank) = row?;
        let raw_relevance = (-rank as f32).max(0.0);
        let summary = format!("{path} manifest ({kind})");
        let token_estimate = estimate_tokens(&summary) + 8;
        candidates.push(Candidate {
            raw_relevance,
            capsule: ContextCapsule {
                id: new_id().to_string(),
                kind: "repo_manifest".to_string(),
                summary,
                token_estimate,
                expansion_handle: format!("file:{path}"),
                provenance: vec![ProvenanceRef {
                    source: "Manifest".to_string(),
                    id: path,
                    excerpt: Some(excerpt(&summary_json)),
                }],
                confidence: 0.95,
                freshness: 1.0,
                relevance: 0.0,
                scope_weight: 0.9,
                score: 0.0,
            },
        });
    }
    Ok(candidates)
}

fn normalize_and_score(candidates: &mut [Candidate], weights: StageWeights) {
    let mut max_by_kind = HashMap::<String, f32>::new();
    for candidate in candidates.iter() {
        max_by_kind
            .entry(candidate.capsule.kind.clone())
            .and_modify(|max| *max = (*max).max(candidate.raw_relevance))
            .or_insert(candidate.raw_relevance);
    }

    for candidate in candidates {
        let max = max_by_kind
            .get(&candidate.capsule.kind)
            .copied()
            .unwrap_or(0.0);
        let relevance = if max <= f32::EPSILON {
            if candidate.raw_relevance > 0.0 {
                1.0
            } else {
                0.0
            }
        } else {
            (candidate.raw_relevance / max).clamp(0.0, 1.0)
        };
        candidate.capsule.relevance = relevance;
        candidate.capsule.score = weights.relevance * relevance
            + weights.confidence * candidate.capsule.confidence
            + weights.freshness * candidate.capsule.freshness
            + weights.scope * candidate.capsule.scope_weight;
    }
}

fn weights_for_stage(weights: &BrokerWeights, stage: &str) -> StageWeights {
    match stage {
        "localization" => weights.localization.clone(),
        "patch_plan" => weights.patch_plan.clone(),
        "verification" => weights.verification.clone(),
        "review" => weights.review.clone(),
        _ => None,
    }
    .unwrap_or(StageWeights {
        relevance: weights.relevance,
        confidence: weights.confidence,
        freshness: weights.freshness,
        scope: weights.scope,
    })
}

fn scope_weight(scope: &str) -> f32 {
    match scope.parse::<MemoryScope>() {
        Ok(MemoryScope::Run) => 1.0,
        Ok(MemoryScope::Repo) => 0.9,
        Ok(MemoryScope::Project) => 0.7,
        Ok(MemoryScope::GlobalUser) => 0.5,
        Err(_) => 0.3,
    }
}

fn freshness(created_at: &str) -> f32 {
    let Ok(created_at) =
        OffsetDateTime::parse(created_at, &time::format_description::well_known::Rfc3339)
    else {
        return 0.5;
    };
    let age = OffsetDateTime::now_utc() - created_at;
    let age_days = age.whole_seconds().max(0) as f32 / 86_400.0;
    (-age_days / 30.0).exp().clamp(0.0, 1.0)
}

fn query_tokens(query: &str) -> Vec<String> {
    let mut tokens: Vec<String> = query
        .split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .map(str::trim)
        .filter(|part| part.len() >= 2)
        .map(str::to_ascii_lowercase)
        .collect();
    // MP-17 #11: task-class routing — augment the query with tool-aware
    // tokens so MP-17b's tool-proficiency capsules surface higher when
    // the task description matches a known class. Cheap keyword fan-out;
    // the underlying lexical_relevance counts substring matches so the
    // augmented tokens only matter when a capsule's text actually mentions
    // them (i.e. the new MP-17b capsules light up, not generic text).
    let lower = query.to_ascii_lowercase();
    for (triggers, expansions) in CLASS_HINTS.iter() {
        if triggers.iter().any(|t| lower.contains(t)) {
            tokens.extend(expansions.iter().map(|e| e.to_string()));
        }
    }
    tokens
}

// MP-17 #11: (trigger keywords, expansion tokens) pairs.
//
// When the user task mentions a trigger, we add the expansions to the
// query token set. Capsules whose text mentions the same expansions
// then score higher on lexical_relevance. The expansions are kimetsu
// tool / concept names so MP-17b capsules (which document those tools)
// surface preferentially.
const CLASS_HINTS: &[(&[&str], &[&str])] = &[
    (
        &[
            "build",
            "compile",
            "make",
            "cargo",
            "cmake",
            "configure",
            "install",
            "train",
            "benchmark",
            "test suite",
            "ray trace",
            "render",
        ],
        &[
            "shell_background",
            "shell_status",
            "shell_output",
            "shell_stop",
            "long_running",
        ],
    ),
    (
        &[
            "edit", "modify", "change", "fix", "update", "patch", "refactor", "rename",
        ],
        &["edit_file", "apply_patch", "old_string", "new_string"],
    ),
    (
        &[
            "read", "inspect", "review", "analyze", "examine", "view", "show",
        ],
        &["read_file", "offset", "limit", "multi_read"],
    ),
    (
        &["find", "locate", "search", "look up", "discover", "list"],
        &["glob", "search_files", "list_files"],
    ),
    (
        &["plan", "step", "checklist", "todo", "task list", "phase"],
        &["plan", "todos"],
    ),
    (
        &[
            "verify",
            "check",
            "ensure",
            "validate",
            "pass test",
            "verifier",
        ],
        &["finish", "verifier", "verification"],
    ),
    (
        &[
            "image",
            "png",
            "jpeg",
            "jpg",
            "pdf",
            "diagram",
            "screenshot",
        ],
        &["view_image", "base64", "sha256"],
    ),
    (&["delete", "remove", "rm "], &["delete_file", "recursive"]),
    (&["rename", "move file", "mv "], &["move_file"]),
];

fn fts_query(query: &str) -> Option<String> {
    let tokens = query_tokens(query);
    if tokens.is_empty() {
        return None;
    }
    Some(
        tokens
            .into_iter()
            .take(12)
            .map(|token| format!("{token}*"))
            .collect::<Vec<_>>()
            .join(" OR "),
    )
}

/// MP-17 #13: greedy MMR (Maximal Marginal Relevance) re-ranking.
///
/// Given capsules already sorted by relevance score, walk the list and
/// at each step pick the next capsule that maximizes
/// `lambda * score - (1 - lambda) * max_overlap_with_already_picked`.
///
/// Overlap = Jaccard similarity of the lowercased token sets of the
/// `summary` field. Capsules from different kinds (memory / repo_file /
/// manifest) get a 0.5 similarity floor so redundancy is only penalized
/// within-kind (a memory and a repo_file aren't really redundant even
/// if they share words).
fn apply_mmr_diversity(mut sorted: Vec<ContextCapsule>, lambda: f32) -> Vec<ContextCapsule> {
    if sorted.len() <= 1 {
        return sorted;
    }
    // Pre-tokenize summaries for cheap Jaccard.
    let summaries: Vec<std::collections::HashSet<String>> = sorted
        .iter()
        .map(|c| summary_token_set(&c.summary))
        .collect();
    let mut picked_indices: Vec<usize> = Vec::with_capacity(sorted.len());
    let mut remaining: Vec<usize> = (0..sorted.len()).collect();

    // Always seed with the top-scoring capsule.
    picked_indices.push(remaining.remove(0));

    while !remaining.is_empty() {
        let mut best_idx_in_remaining = 0;
        let mut best_score = f32::MIN;
        for (i, &cand) in remaining.iter().enumerate() {
            let mut max_overlap = 0.0f32;
            for &p in &picked_indices {
                let raw = jaccard(&summaries[cand], &summaries[p]);
                let overlap = if sorted[cand].kind == sorted[p].kind {
                    raw
                } else {
                    // cross-kind: scale down so we don't over-penalize a memory
                    // that happens to share words with a repo file.
                    raw * 0.5
                };
                if overlap > max_overlap {
                    max_overlap = overlap;
                }
            }
            let mmr = lambda * sorted[cand].score - (1.0 - lambda) * max_overlap;
            if mmr > best_score {
                best_score = mmr;
                best_idx_in_remaining = i;
            }
        }
        picked_indices.push(remaining.remove(best_idx_in_remaining));
    }
    // Reorder `sorted` to match picked_indices.
    let mut out = Vec::with_capacity(sorted.len());
    // We need to drain in picked_indices order; do it by taking with mem::replace.
    let mut taken: Vec<Option<ContextCapsule>> = sorted.drain(..).map(Some).collect();
    for idx in picked_indices {
        if let Some(c) = taken[idx].take() {
            out.push(c);
        }
    }
    out
}

fn summary_token_set(s: &str) -> std::collections::HashSet<String> {
    s.split(|ch: char| !ch.is_ascii_alphanumeric() && ch != '_')
        .filter(|t| t.len() >= 3)
        .map(str::to_ascii_lowercase)
        .collect()
}

fn jaccard(a: &std::collections::HashSet<String>, b: &std::collections::HashSet<String>) -> f32 {
    if a.is_empty() && b.is_empty() {
        return 0.0;
    }
    let intersection = a.intersection(b).count();
    let union = a.union(b).count();
    intersection as f32 / union.max(1) as f32
}

fn lexical_relevance(tokens: &[String], haystack: &str) -> f32 {
    if tokens.is_empty() {
        return 0.0;
    }
    let haystack = haystack.to_ascii_lowercase();
    let matches = tokens
        .iter()
        .filter(|token| haystack.contains(token.as_str()))
        .count();
    matches as f32 / tokens.len() as f32
}

fn estimate_tokens(text: &str) -> u32 {
    ((text.split_whitespace().count() as f32) * 1.33).ceil() as u32
}

fn excerpt(text: &str) -> String {
    let value = one_line(text);
    value.chars().take(256).collect()
}

fn one_line(text: &str) -> String {
    text.split_whitespace().collect::<Vec<_>>().join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// MP-17e: zero-use rows are neutral (no data); use_count >= 1 starts
    /// blending toward the full multiplier (Bayesian smoothing).
    #[test]
    fn usefulness_multiplier_neutral_at_zero_uses() {
        // use_count = 0 is the only strictly-neutral case.
        assert!((usefulness_multiplier(0.0, 0) - 1.0).abs() < f32::EPSILON);
        assert!((usefulness_multiplier(5.0, 0) - 1.0).abs() < f32::EPSILON);
        assert!((usefulness_multiplier(-5.0, 0) - 1.0).abs() < f32::EPSILON);
    }

    /// MP-17e: between use_count 1..3 the multiplier blends linearly from
    /// neutral (1.0) toward the full envelope. A use_count of 2 with a
    /// perfect ratio lands at 2/3 of the way to the max boost.
    #[test]
    fn usefulness_multiplier_blends_smoothly_in_transition() {
        // use_count = 1, ratio = 1.0 -> confidence 1/3, blend toward 1.5
        // expected = 1.0 * 2/3 + 1.5 * 1/3 = 1.1667
        let one_use = usefulness_multiplier(1.0, 1);
        assert!((one_use - 1.166_666_6).abs() < 1e-4, "got {one_use}");
        // use_count = 2, ratio = 1.0 -> confidence 2/3, blend toward 1.5
        // expected = 1.0 * 1/3 + 1.5 * 2/3 = 1.3333
        let two_uses = usefulness_multiplier(2.0, 2);
        assert!((two_uses - 1.333_333_4).abs() < 1e-4, "got {two_uses}");
        // use_count = 2 with ratio = -1.0 should pull toward the penalty side.
        let two_uses_bad = usefulness_multiplier(-2.0, 2);
        // expected = 1.0 * 1/3 + 0.5 * 2/3 = 0.6667
        assert!(
            (two_uses_bad - 0.666_666_7).abs() < 1e-4,
            "got {two_uses_bad}"
        );
    }

    /// MP-4b: at use_count >= 3 the multiplier maps ratio in [-1, 1] linearly
    /// onto [MULTIPLIER_MIN, MULTIPLIER_MAX] = [0.5, 1.5]. A neutral memory
    /// (ratio = 0) gets a 1.0 multiplier.
    #[test]
    fn usefulness_multiplier_maps_ratio_onto_envelope() {
        // ratio = 1.0 -> 1.5 (max boost)
        assert!((usefulness_multiplier(5.0, 5) - 1.5).abs() < f32::EPSILON);
        // ratio = -1.0 -> 0.5 (max penalty)
        assert!((usefulness_multiplier(-5.0, 5) - 0.5).abs() < f32::EPSILON);
        // ratio = 0.0 -> 1.0 (neutral)
        let mid = usefulness_multiplier(0.0, 6);
        assert!((mid - 1.0).abs() < f32::EPSILON, "got {mid}");
        // ratio = 0.5 -> 1.25 (mid boost)
        let high = usefulness_multiplier(2.0, 4);
        assert!((high - 1.25).abs() < f32::EPSILON, "got {high}");
        // ratio = -0.5 -> 0.75 (mid penalty)
        let low = usefulness_multiplier(-2.0, 4);
        assert!((low - 0.75).abs() < f32::EPSILON, "got {low}");
    }

    /// MP-4b: the multiplier is bounded so even a runaway score cannot
    /// dominate the budget; a single memory with usefulness_score >> use_count
    /// is clamped at the upper envelope.
    #[test]
    fn usefulness_multiplier_clamps_to_envelope() {
        // ratio > 1.0 is clamped to 1.0 -> 1.5
        assert!((usefulness_multiplier(100.0, 5) - 1.5).abs() < f32::EPSILON);
        // ratio < -1.0 is clamped to -1.0 -> 0.5
        assert!((usefulness_multiplier(-100.0, 5) - 0.5).abs() < f32::EPSILON);
    }

    // ----- MP-17 #11: task-class query expansion -----

    #[test]
    fn query_tokens_expands_build_class() {
        let toks = query_tokens("Build the project from source");
        assert!(toks.iter().any(|t| t == "build"));
        // class-aware expansion adds tool tokens:
        assert!(toks.iter().any(|t| t == "shell_background"));
        assert!(toks.iter().any(|t| t == "long_running"));
    }

    #[test]
    fn query_tokens_expands_edit_class() {
        let toks = query_tokens("Modify the config to fix the bug");
        assert!(toks.iter().any(|t| t == "edit_file"));
        assert!(toks.iter().any(|t| t == "apply_patch"));
    }

    #[test]
    fn query_tokens_expands_search_class() {
        let toks = query_tokens("Find all references to the symbol");
        assert!(toks.iter().any(|t| t == "glob"));
        assert!(toks.iter().any(|t| t == "search_files"));
    }

    #[test]
    fn query_tokens_no_expansion_on_unrelated_query() {
        let toks = query_tokens("hello world testing nothing");
        // Only the "test" trigger fires here -> verification expansion.
        assert!(toks.iter().any(|t| t == "hello"));
        // The base tokens are present regardless.
        assert!(toks.iter().any(|t| t == "world"));
    }

    // ----- MP-17 #13: MMR diversity helpers -----

    #[test]
    fn jaccard_is_zero_for_disjoint_sets() {
        let a: std::collections::HashSet<String> =
            ["foo", "bar"].iter().map(|s| s.to_string()).collect();
        let b: std::collections::HashSet<String> =
            ["baz", "qux"].iter().map(|s| s.to_string()).collect();
        assert!((jaccard(&a, &b) - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn jaccard_is_one_for_identical_sets() {
        let a: std::collections::HashSet<String> =
            ["foo", "bar"].iter().map(|s| s.to_string()).collect();
        let b = a.clone();
        assert!((jaccard(&a, &b) - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn jaccard_partial_overlap() {
        let a: std::collections::HashSet<String> = ["foo", "bar", "baz"]
            .iter()
            .map(|s| s.to_string())
            .collect();
        let b: std::collections::HashSet<String> =
            ["bar", "qux"].iter().map(|s| s.to_string()).collect();
        // intersection = {bar} = 1, union = {foo,bar,baz,qux} = 4
        assert!((jaccard(&a, &b) - 0.25).abs() < f32::EPSILON);
    }

    #[test]
    fn summary_token_set_lowercases_and_filters_short() {
        let set = summary_token_set("Build the Foo-bar project");
        assert!(set.contains("build"));
        assert!(set.contains("foo"));
        assert!(set.contains("bar"));
        assert!(set.contains("project"));
        // "the" is len=3, included; "a" or "i" would be excluded.
        assert!(set.contains("the"));
    }

    // ----- v0.4.2: hybrid retrieval end-to-end -----

    /// Helper: open an in-memory brain.db, initialize schema, insert
    /// a memory row (post-projector shape) plus its embedding +
    /// embedding_model and the matching FTS entry.
    fn insert_memory_with_embedding(
        conn: &rusqlite::Connection,
        memory_id: &str,
        text: &str,
        embedder: &dyn embeddings::Embedder,
    ) {
        let normalized = kimetsu_core::memory::normalize_memory_text(text);
        conn.execute(
            "
            INSERT INTO memories (
                memory_id, scope, kind, text, normalized_text, confidence,
                source_event_id, provenance_snapshot_json, created_at,
                use_count, usefulness_score, embedding, embedding_model
            )
            VALUES (?1, 'global_user', 'fact', ?2, ?3, 1.0, NULL, '{}',
                    '2026-05-01T00:00:00Z', 0, 0.0, ?4, ?5)
            ",
            rusqlite::params![
                memory_id,
                text,
                normalized,
                embeddings::encode_embedding(&embedder.embed(text).expect("embed test row")),
                embedder.model_id(),
            ],
        )
        .expect("insert memory");
        conn.execute(
            "INSERT INTO memories_fts (memory_id, text, kind, scope) VALUES (?1, ?2, 'fact', 'global_user')",
            rusqlite::params![memory_id, text],
        )
        .expect("insert fts row");
    }

    /// v0.4.2: the cosine blend changes retrieval ranking when two
    /// memories tie lexically but differ semantically (via the stub
    /// embedder's hashed-bucket vectors).
    ///
    /// Setup: two memories, neither containing the query's literal
    /// words. With pure FTS, neither matches and we fall back to
    /// latest-memory ranking. With the stub embedder enabled, the
    /// memory that's "semantically closer" to the query (shares
    /// hash buckets) outranks the other.
    #[test]
    fn hybrid_retrieval_uses_cosine_score_to_rerank() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");
        let stub = embeddings::StubEmbedder::new();

        insert_memory_with_embedding(&conn, "m_rg", "use ripgrep for code search", &stub);
        insert_memory_with_embedding(
            &conn,
            "m_unrelated",
            "cookie recipe with chocolate chips",
            &stub,
        );

        // Query shares words with m_rg but not m_unrelated. FTS will
        // already prefer m_rg here; we use that as the baseline.
        let weights = kimetsu_core::config::BrokerWeights::default();
        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "ripgrep search".to_string(),
                budget_tokens: 4000,
            },
            &[],
            &stub,
        )
        .expect("retrieve");

        let memory_handles: Vec<_> = bundle
            .capsules
            .iter()
            .filter(|c| c.expansion_handle.starts_with("memory:"))
            .collect();
        assert!(
            !memory_handles.is_empty(),
            "at least one memory should surface"
        );
        // The semantically-relevant memory must rank first.
        assert_eq!(
            memory_handles[0].expansion_handle,
            "memory:m_rg",
            "ripgrep memory should outrank the cookie recipe; ranked: {:?}",
            memory_handles
                .iter()
                .map(|c| &c.expansion_handle)
                .collect::<Vec<_>>()
        );
    }

    /// v0.4.2: when a row's stored `embedding_model` doesn't match
    /// the active query embedder's id, the row's cosine contribution
    /// is skipped — falling back to FTS-only for that row. Critical
    /// for safety across `kimetsu brain reindex` migrations (v0.4.3)
    /// where some rows might be embedded with the new model and some
    /// with the old.
    #[test]
    fn hybrid_retrieval_skips_cosine_on_model_id_mismatch() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");
        let stub = embeddings::StubEmbedder::new();
        insert_memory_with_embedding(&conn, "m_xref", "use ripgrep for code search", &stub);

        // Stomp the row's embedding_model with a synthetic id that
        // doesn't match the active embedder. Simulates a `kimetsu
        // brain reindex` mid-migration where some rows are on the
        // new model and some on the old.
        conn.execute(
            "UPDATE memories SET embedding_model = 'bge-small-en-v1.5' WHERE memory_id = 'm_xref'",
            [],
        )
        .expect("force model_id mismatch");

        // Query through the stub embedder. Its model_id is "stub-d8";
        // the row's is "bge-small-en-v1.5". The cosine path MUST be
        // skipped for this row; FTS still surfaces it on the lexical
        // match because retrieval doesn't crash on cross-model rows.
        let weights = kimetsu_core::config::BrokerWeights::default();
        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "ripgrep search".to_string(),
                budget_tokens: 4000,
            },
            &[],
            &stub,
        )
        .expect("retrieve");

        assert!(
            bundle
                .capsules
                .iter()
                .any(|c| c.expansion_handle == "memory:m_xref"),
            "cross-model row should still match lexically (cosine skipped, FTS works)"
        );
    }

    /// v0.4.2: with [`NoopEmbedder`] the retrieval path is identical
    /// to v0.4.1 — no cosine term contributes, stored embeddings (if
    /// any) are ignored. Regression guard so the default build
    /// behaves identically to pre-v0.4.2.
    #[test]
    fn hybrid_retrieval_with_noop_embedder_is_lexical_only() {
        let conn = rusqlite::Connection::open_in_memory().expect("open in-memory");
        crate::schema::initialize(&conn).expect("init schema");
        let stub = embeddings::StubEmbedder::new();
        // Two memories, both with non-null embeddings.
        insert_memory_with_embedding(&conn, "m_a", "use ripgrep", &stub);
        insert_memory_with_embedding(&conn, "m_b", "use ripgrep too", &stub);

        // Query through the Noop default. QueryEmbedding will be
        // None → no cosine blend → exact FTS ranking.
        let weights = kimetsu_core::config::BrokerWeights::default();
        let bundle = retrieve_context_with_embedder(
            &conn,
            "/fake-repo",
            &weights,
            ContextRequest {
                stage: "localization".to_string(),
                query: "ripgrep".to_string(),
                budget_tokens: 4000,
            },
            &[],
            &embeddings::NoopEmbedder,
        )
        .expect("retrieve");

        let count = bundle
            .capsules
            .iter()
            .filter(|c| c.expansion_handle.starts_with("memory:"))
            .count();
        assert_eq!(count, 2, "both memories should surface via FTS");
    }
}
